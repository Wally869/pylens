//! Relation-driven vector shaping: nudges a generated input vector toward coherence with
//! `sig.param_relations` (see `analyze::collect::relations`) — pairing a `str` container with an
//! `int` scalar when the body compares them wastes a slot on a guaranteed `TypeError`. [`repair`]
//! fixes up a kind mismatch on an already-built vector; [`relative_vectors`] goes further and
//! seeds new vectors that place a related scalar below, above, at, and strictly between the
//! elements of its related container, so branches like binary search's below-range/above-range/
//! exact-match/interpolated-match outcomes get a candidate that can actually reach them.

use serde_json::Value;

use crate::model::{ParamRef, ParamRelation, RelationKind, Shape};

use super::{Candidate, ValueDomain, seeds};

/// The coarse value kind [`repair`] pairs on. Ints, floats and bools all collapse to `Number` —
/// Python compares and adds them freely, so distinguishing them would only reject pairings that
/// actually work at runtime. `Tagged` is a `{"__t__": ...}` object (the tuple/set/dict encoding
/// `seeds.rs` and `python/worker.py` share); a plain string-keyed object is `Object`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PairKind {
    Null,
    Number,
    Str,
    Array,
    Object,
    Tagged,
}

pub(super) fn kind_of(value: &Value) -> PairKind {
    match value {
        Value::Null => PairKind::Null,
        Value::Bool(_) | Value::Number(_) => PairKind::Number,
        Value::String(_) => PairKind::Str,
        Value::Array(_) => PairKind::Array,
        Value::Object(map) => {
            if super::tag_of(map).is_some() { PairKind::Tagged } else { PairKind::Object }
        }
    }
}

/// The kind `value`'s elements carry, for the `element: true` side of a [`ParamRef`] — `None`
/// when `value` has no discernible element kind (an empty array, or a non-container value).
pub(super) fn element_kind_of(value: &Value) -> Option<PairKind> {
    match value {
        Value::String(_) => Some(PairKind::Str),
        Value::Array(items) => items.first().map(kind_of),
        _ => None,
    }
}

/// The kind `reference` contributes given the value bound to its parameter — `None` means "no
/// opinion", compatible with any kind.
fn ref_kind(reference: &ParamRef, value: &Value) -> Option<PairKind> {
    if reference.element { element_kind_of(value) } else { Some(kind_of(value)) }
}

fn domain_allows(value: &Value, domain: Option<&ValueDomain>) -> bool {
    match domain {
        Some(d) => d.allows(value),
        None => true,
    }
}

/// The first of `candidates` whose value contributes `needed_kind` for `reference`, or a value
/// synthesized from the seed corpus when none qualifies — see [`repair`]. `None` when neither a
/// matching candidate nor a synthesizable one exists (e.g. `needed_kind` is `Null`/`Object`/
/// `Tagged`, which have no dedicated seed shape), or when a synthesized value fails `domain`.
fn pick_value(reference: &ParamRef, needed_kind: PairKind, candidates: &[Candidate], domain: Option<&ValueDomain>) -> Option<Value> {
    if !reference.element {
        if let Some(c) = candidates.iter().find(|c| kind_of(&c.value) == needed_kind) {
            return Some(c.value.clone());
        }
        let shape = match needed_kind {
            PairKind::Number => Shape::Int,
            PairKind::Str => Shape::Str,
            PairKind::Array => Shape::Seq(Box::new(Shape::Int)),
            _ => return None,
        };
        let value = seeds::candidates(&shape).into_iter().next()?.value;
        return domain_allows(&value, domain).then_some(value);
    }
    if let Some(c) = candidates.iter().find(|c| element_kind_of(&c.value) == Some(needed_kind)) {
        return Some(c.value.clone());
    }
    let elem_shape = match needed_kind {
        PairKind::Number => Shape::Int,
        PairKind::Str => Shape::Str,
        _ => return None,
    };
    let value = seeds::candidates(&Shape::Seq(Box::new(elem_shape)))
        .into_iter()
        .map(|c| c.value)
        .find(|v| matches!(v, Value::Array(items) if !items.is_empty()))?;
    domain_allows(&value, domain).then_some(value)
}

/// Nudges `values` toward coherence with `relations`: for every relation whose two parameters are
/// both present in `names` and whose contributed kinds ([`ref_kind`]) disagree, one side is kept
/// (the parameter at `anchor`, when the relation names it; otherwise the left side) and the other
/// is replaced by [`pick_value`]. Two passes over `relations` so a chain `a ~ b ~ c` can settle;
/// a relation naming a parameter absent from `names` (`*args`/`**kwargs`, or untracked) is
/// ignored. Leaves `values` as-is wherever no candidate and no synthesized value qualifies — a
/// wasted slot, never a hard failure (see [`super::gen_inputs`]'s doc).
pub(super) fn repair(
    relations: &[ParamRelation],
    names: &[&str],
    per: &[Vec<Candidate>],
    values: &mut [Value],
    anchor: Option<usize>,
    domain: Option<&ValueDomain>,
) {
    for _ in 0..2 {
        let mut changed = false;
        for rel in relations {
            let (Some(li), Some(ri)) = (
                names.iter().position(|n| *n == rel.left.param),
                names.iter().position(|n| *n == rel.right.param),
            ) else {
                continue;
            };
            if li == ri {
                continue;
            }
            let (Some(lk), Some(rk)) = (ref_kind(&rel.left, &values[li]), ref_kind(&rel.right, &values[ri])) else {
                continue;
            };
            if lk == rk {
                continue;
            }
            let (target, target_ref, needed_kind) = if anchor == Some(ri) {
                (li, &rel.left, rk)
            } else {
                (ri, &rel.right, lk)
            };
            if let Some(new_value) = pick_value(target_ref, needed_kind, &per[target], domain)
                && values[target] != new_value
            {
                values[target] = new_value;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

/// New vectors that place a scalar related by [`RelationKind::Order`] or [`RelationKind::Eq`] to
/// a container's *element* at a few positions relative to that container: below its minimum, above
/// its maximum, equal to a middle element, and strictly between the two adjacent elements with the
/// widest gap (numbers get two such values: the midpoint and one just past it, so a closest-
/// neighbour comparison sees both its tie side and its other side). `Arith`-only relations don't qualify — arithmetic doesn't imply a comparison
/// boundary the way `Order`/`Eq` do.
///
/// For each qualifying relation (one processed per distinct container/scalar pair, even if
/// several relations name it — `Eq` and `Order` on the same two parameters would otherwise repeat
/// identical work), every candidate of the container parameter (from `per[container]`) that is an
/// all-number (no bools) or all-string array of at least two elements, or a string of at least two
/// distinct characters, contributes up to five scalar values built off its sorted, deduplicated
/// elements — widest-range candidate first (see [`range_of`]), so a Union-shaped container's more
/// interesting, wide-spread candidates aren't crowded out of a small reserved budget by low-signal
/// ones from an unrelated member. Each produced vector holds every other parameter at
/// `base_values`, sets the container slot to that candidate and the scalar slot to the built
/// value, then runs through [`repair`] (anchored on the container) so a third related parameter
/// stays coherent. A scalar that fails `domain` is skipped; a vector is only emitted when a scalar
/// was actually built.
pub(super) fn relative_vectors(
    relations: &[ParamRelation],
    names: &[&str],
    per: &[Vec<Candidate>],
    base_values: &[Value],
    domain: Option<&ValueDomain>,
) -> Vec<Vec<Value>> {
    let mut out: Vec<Vec<Value>> = Vec::new();
    let mut seen_pairs: Vec<(usize, usize)> = Vec::new();
    for rel in relations {
        if !matches!(rel.kind, RelationKind::Order | RelationKind::Eq) {
            continue;
        }
        let Some((container_idx, scalar_idx)) = container_scalar_pair(rel, names) else {
            continue;
        };
        if seen_pairs.contains(&(container_idx, scalar_idx)) {
            continue;
        }
        seen_pairs.push((container_idx, scalar_idx));

        // Widest-range candidates go first: a container instance whose elements span a narrow
        // range (e.g. `[1, 2, 1]`) barely separates "below min" from "above max" from "between",
        // whereas the outlier/wide-spread candidates give the four positions the most room to
        // land in genuinely different branches — the values most worth the reserved budget when
        // a Union-shaped container mixes in many low-signal candidates ahead of them.
        let mut ranked: Vec<(f64, &Candidate, Elements)> = per[container_idx]
            .iter()
            .filter_map(|cand| orderable_elements(&cand.value).map(|e| (range_of(&e), cand, e)))
            .collect();
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());

        for (_, cand, elements) in ranked {
            for scalar in scalar_positions(&elements) {
                if !domain_allows(&scalar, domain) {
                    continue;
                }
                let mut values = base_values.to_vec();
                values[container_idx] = cand.value.clone();
                values[scalar_idx] = scalar;
                repair(relations, names, per, &mut values, Some(container_idx), domain);
                if !out.contains(&values) {
                    out.push(values);
                }
            }
        }
    }
    out
}

/// The value span the four [`scalar_positions`] have to work with — `max - min` of the
/// deduplicated elements, in the same units [`num_scalar_positions`]/[`str_scalar_positions`]
/// build from ([`str_char_gap`]'s codepoint distance for strings).
fn range_of(elements: &Elements) -> f64 {
    match elements {
        Elements::Nums(d, _) => d.last().unwrap().as_f64().unwrap() - d.first().unwrap().as_f64().unwrap(),
        Elements::Strs(d) => str_char_gap(d.first().unwrap(), d.last().unwrap()) as f64,
    }
}

/// Resolves `rel` to `(container_index, scalar_index)` when exactly one side names a container
/// element and the other a bare scalar, both present in `names`. `None` otherwise (both sides
/// scalar, both sides elements, or a name outside `names`).
fn container_scalar_pair(rel: &ParamRelation, names: &[&str]) -> Option<(usize, usize)> {
    let li = names.iter().position(|n| *n == rel.left.param)?;
    let ri = names.iter().position(|n| *n == rel.right.param)?;
    if li == ri {
        return None;
    }
    match (rel.left.element, rel.right.element) {
        (true, false) => Some((li, ri)),
        (false, true) => Some((ri, li)),
        _ => None,
    }
}

/// A sorted, deduplicated element kind for `value`: `Nums` for an array of at least two numbers
/// (no bools), `Strs` for an array of at least two strings, or the one-character-string breakdown
/// of a `Value::String` of at least two distinct characters. `None` for anything else.
enum Elements {
    Nums(Vec<serde_json::Number>, bool),
    Strs(Vec<String>),
}

fn orderable_elements(value: &Value) -> Option<Elements> {
    match value {
        Value::Array(items) if items.len() >= 2 => {
            if items.iter().all(|v| matches!(v, Value::Number(_))) {
                let is_int = items.iter().all(|v| v.as_i64().is_some() || v.as_u64().is_some());
                let mut nums: Vec<serde_json::Number> =
                    items.iter().filter_map(|v| v.as_number().cloned()).collect();
                nums.sort_by(|a, b| a.as_f64().partial_cmp(&b.as_f64()).unwrap());
                nums.dedup_by(|a, b| a.as_f64() == b.as_f64());
                (nums.len() >= 2).then_some(Elements::Nums(nums, is_int))
            } else if items.iter().all(|v| matches!(v, Value::String(_))) {
                let mut strs: Vec<String> =
                    items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect();
                strs.sort();
                strs.dedup();
                (strs.len() >= 2).then_some(Elements::Strs(strs))
            } else {
                None
            }
        }
        Value::String(s) => {
            let mut chars: Vec<String> = s.chars().map(String::from).collect();
            chars.sort();
            chars.dedup();
            (chars.len() >= 2).then_some(Elements::Strs(chars))
        }
        _ => None,
    }
}

/// The below-min, above-max, middle-element, and widest-gap between scalar values for `elements`
/// (see [`relative_vectors`]'s doc for the exact rule per position). Fewer when a position has
/// no qualifying value (e.g. an empty string can't produce a below-min string).
fn scalar_positions(elements: &Elements) -> Vec<Value> {
    match elements {
        Elements::Nums(nums, is_int) => num_scalar_positions(nums, *is_int),
        Elements::Strs(strs) => str_scalar_positions(strs),
    }
}

fn num_scalar_positions(d: &[serde_json::Number], is_int: bool) -> Vec<Value> {
    let mut out = Vec::new();
    let to_value = |f: f64| -> Value {
        if is_int { Value::from(f as i64) } else { serde_json::json!(f) }
    };
    let min = d.first().unwrap().as_f64().unwrap();
    let max = d.last().unwrap().as_f64().unwrap();
    out.push(to_value(min - 1.0));
    out.push(to_value(max + 1.0));
    out.push(Value::Number(d[d.len() / 2].clone()));
    // Two between values for the widest gap: the exact midpoint, equidistant from both
    // neighbours, and one just past it toward the upper element. A "which neighbour is closer"
    // comparison takes its tie side on the first and the other side on the second. Ints need a
    // gap of at least 2 for the midpoint and at least 4 for the off-centre value.
    let mut best_gap = 0.0;
    let mut widest: Option<f64> = None;
    for w in d.windows(2) {
        let a = w[0].as_f64().unwrap();
        let b = w[1].as_f64().unwrap();
        if b - a > best_gap {
            best_gap = b - a;
            widest = Some(a);
        }
    }
    if let Some(a) = widest {
        if is_int {
            let mid = a + (best_gap / 2.0).floor();
            if best_gap >= 2.0 {
                out.push(to_value(mid));
            }
            if best_gap >= 4.0 {
                out.push(to_value(mid + 1.0));
            }
        } else {
            out.push(to_value(a + best_gap / 2.0));
            out.push(to_value(a + best_gap * 0.6));
        }
    }
    out
}

fn str_scalar_positions(d: &[String]) -> Vec<Value> {
    let mut out = Vec::new();
    let min = &d[0];
    let max = d.last().unwrap();
    if !min.is_empty() {
        out.push(Value::String(String::new()));
    }
    out.push(Value::String(format!("{max}z")));
    out.push(Value::String(d[d.len() / 2].clone()));
    // "Widest gap" between two strings has no canonical metric, so the first differing
    // character's codepoint distance stands in for it — a coarse, but order-respecting, proxy.
    let mut best_gap: i64 = -1;
    let mut between: Option<String> = None;
    for w in d.windows(2) {
        let a = &w[0];
        let b = &w[1];
        let candidate = format!("{a}a");
        if candidate.as_str() <= a.as_str() || candidate.as_str() >= b.as_str() {
            continue;
        }
        let gap = str_char_gap(a, b);
        if gap > best_gap {
            best_gap = gap;
            between = Some(candidate);
        }
    }
    if let Some(b) = between {
        out.push(Value::String(b));
    }
    out
}

fn str_char_gap(a: &str, b: &str) -> i64 {
    let mut ac = a.chars();
    let mut bc = b.chars();
    loop {
        match (ac.next(), bc.next()) {
            (Some(x), Some(y)) if x == y => continue,
            (Some(x), Some(y)) => return (y as i64) - (x as i64),
            (None, Some(y)) => return y as i64,
            _ => return 0,
        }
    }
}
