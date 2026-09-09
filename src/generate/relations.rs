//! Relation-driven vector shaping: nudges a generated input vector toward coherence with
//! `sig.param_relations` (see `analyze::collect::relations`) — pairing a `str` container with an
//! `int` scalar when the body compares them wastes a slot on a guaranteed `TypeError`. [`repair`]
//! fixes up a kind mismatch on an already-built vector.

use serde_json::Value;

use crate::model::{ParamRef, ParamRelation, Shape};

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

