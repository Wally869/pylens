//! Input generation directed by the analyzer's inferred parameter shapes. Produces a small,
//! diverse set of input vectors (including boundary values) per function, spending a limited
//! `--inputs` budget on the candidates most likely to reach new behavior: hold every parameter
//! at a typical value, vary one parameter at a time through its ranked candidates, then spend
//! whatever budget remains on combinations.
//!
//! Generation cannot break soundness: every value it produces is only ever *tried*, never
//! trusted. A wrong guess (structural or, since the [`Rank::Hint`] tier was added, domain-content
//! guessed) just makes the function return early or raise on a branch other than the one hoped
//! for, wasting a slot of the `--inputs` budget — the same outcome an unlucky generic candidate
//! already risks. So [`seeds`]'s heuristics — including the per-parameter domain tags
//! `analyze::collect::hints` infers — are justified by how cheap and common the evidence is, not
//! by how often the guess is right.

use serde_json::{Value, Map, json};

use crate::model::{EffectSignature, ParamInfo, ParamKind, Shape};

mod domain;
pub mod predicate;
mod relations;
mod seeds;

pub use domain::ValueDomain;

/// One generated call: a positional-argument vector plus a keyword-argument map, ready to hand
/// to the sandbox. `positional` lines up with `positional_params(sig)` by index; `kwargs` holds
/// one `(name, value)` pair per keyword-only parameter.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GenInput {
    pub positional: Vec<Value>,
    pub kwargs: Vec<(String, Value)>,
}

/// A generated value with its interest rank. Lower ranks are tried first: the sampler spends a
/// small `--inputs` budget on the values most likely to reach new behavior.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub value: Value,
    pub rank: Rank,
}

/// Interest ordering for a candidate value, low to high. [`gen_inputs`] samples in rank order —
/// every parameter gets its rank-1 value before any parameter gets its rank-2 value — so a small
/// budget still covers each parameter rather than exhausting itself on one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    /// The one typical value per shape. Used to hold a parameter fixed while another varies.
    Base,
    /// Pulled from the function's own guards, and the None injection for a defaulted parameter.
    Guard,
    /// Drawn from a domain corpus (`url`, `email`, `path`, `json`, ...) matching a tag
    /// `analyze::collect::hints` inferred for this parameter. Ranked above the structural
    /// `Edge`/`Filler`/`Property` tiers: an inferred hint is what unlocks a function's real
    /// body — an empty string or `0` usually just returns early, whereas a well-formed URL or
    /// JSON document reaches the parsing code the structural tiers never touch.
    Hint,
    /// A structural boundary: empty, zero, singleton, negative.
    Edge,
    /// A structural property of the value (reserved; no collector currently emits it).
    Property,
    /// Generic spread with no specific evidence.
    Filler,
}

/// Generate up to `max_vectors` [`GenInput`]s for `sig`: one `Value` per *positional* parameter,
/// plus a keyword-argument map with one entry per *keyword-only* parameter. `*args`/`**kwargs`
/// params are excluded entirely (they receive nothing).
///
/// Sampling order: (1) the all-base vector, every parameter at its typical value; (2) one
/// parameter at a time, round-robined by candidate index so no single parameter can consume the
/// whole budget before the others get a turn; (3) for every relation that pairs a scalar
/// parameter against a related container parameter's element by `Order` or `Eq`
/// (`sig.param_relations`, see [`relations::relative_vectors`]), vectors that place the scalar
/// below the container's minimum, above its maximum, equal to a middle element, and strictly
/// between its widest-spaced adjacent elements — the positions a "binary search"/"find closest"
/// style body branches on, which independent round-robin sampling of the scalar and the container
/// essentially never lands on together; (4) once every parameter's candidates are exhausted,
/// combination vectors spread evenly across the full cartesian product, so pairings across
/// positional AND keyword-only arguments — not only matched positions — get exercised. Duplicate
/// vectors (same positional values and same keyword pairs) are never emitted twice. Phase (3)
/// reserves at most a quarter of `max_vectors` for itself (`min` of its own count and that
/// quarter), taken out of phase (2)'s share so the total budget is unaffected; a function with no
/// qualifying relation skips phase (3) and generates exactly as it did before this phase existed.
///
/// `domain`, when given, restricts every produced value to [`ValueDomain::allows`] — see
/// `pylens record --value-domain`. `None` means unrestricted generation (the default, and
/// always the case for `pylens validate`, which must not weaken the soundness harness).
///
/// Every vector built above also passes through [`relations::repair`], which nudges it toward
/// `sig.param_relations` coherence: when two parameters (or one parameter's element and another
/// parameter) meet as operands of a comparison or arithmetic expression in the body, pairing
/// e.g. a string with an int wastes a slot on a guaranteed `TypeError` before the real body is
/// ever reached. One side of the mismatch is kept (the parameter this step just varied, or the
/// left side for the base/combination vectors) and the other side is swapped for the first of
/// its own candidates with a matching value kind, or a synthesized one when none qualifies. A
/// relation is never treated as a hard constraint — repair can leave a vector unfixed — so a
/// wrong or unresolvable relation only wastes a slot, exactly like a wrong guard sample.
pub fn gen_inputs(sig: &EffectSignature, max_vectors: usize, domain: Option<&ValueDomain>) -> Vec<GenInput> {
    let max = max_vectors.max(1);
    let positional = positional_params(sig);
    let kwonly = keyword_only_params(sig);
    if positional.is_empty() && kwonly.is_empty() {
        return vec![GenInput::default()];
    }
    let names: Vec<&str> = positional.iter().chain(kwonly.iter()).map(|p| p.name.as_str()).collect();
    let per: Vec<Vec<Candidate>> = positional
        .iter()
        .chain(kwonly.iter())
        .map(|p| candidates_for(p, domain))
        .collect();
    // A pathological domain (e.g. an empty `scalars` array with no `list_elements`) can admit
    // no value at all; fall back to `Value::Null` rather than index an empty candidate list —
    // this one vector may itself violate the domain, but an unsatisfiable domain has no
    // in-domain vector to offer regardless.
    let per: Vec<Vec<Candidate>> = per
        .into_iter()
        .map(|c| if c.is_empty() { vec![base(Value::Null)] } else { c })
        .collect();

    let to_input = |values: &[Value]| -> GenInput {
        let (pos_values, kw_values) = values.split_at(positional.len());
        GenInput {
            positional: pos_values.to_vec(),
            kwargs: kwonly
                .iter()
                .zip(kw_values)
                .map(|(p, v)| (p.name.clone(), v.clone()))
                .collect(),
        }
    };

    let mut base_values: Vec<Value> = per.iter().map(|c| c[0].value.clone()).collect();
    relations::repair(&sig.param_relations, &names, &per, &mut base_values, None, domain);
    let mut out: Vec<GenInput> = vec![to_input(&base_values)];

    let relative = relations::relative_vectors(&sig.param_relations, &names, &per, &base_values, domain);
    let reserved = relative.len().min(max / 4);
    let round_robin_limit = max.saturating_sub(reserved);

    let max_len = per.iter().map(Vec::len).max().unwrap_or(1);
    'round_robin: for i in 1..max_len {
        for (j, c) in per.iter().enumerate() {
            if out.len() >= round_robin_limit {
                break 'round_robin;
            }
            let Some(cand) = c.get(i) else { continue };
            let mut values = base_values.clone();
            values[j] = cand.value.clone();
            relations::repair(&sig.param_relations, &names, &per, &mut values, Some(j), domain);
            let candidate_input = to_input(&values);
            if !out.contains(&candidate_input) {
                out.push(candidate_input);
            }
        }
    }

    for values in &relative {
        if out.len() >= max {
            break;
        }
        let candidate_input = to_input(values);
        if !out.contains(&candidate_input) {
            out.push(candidate_input);
        }
    }

    if out.len() < max {
        let total = per
            .iter()
            .map(Vec::len)
            .fold(1usize, |a, b| a.saturating_mul(b))
            .max(1);
        let k = max.min(total);
        for i in 0..k {
            if out.len() >= max {
                break;
            }
            // Even spacing across the product, decoded with the parameter order reversed so the
            // FIRST parameter is the one that varies fastest as the flat index steps forward.
            let mut flat = (i as u128 * total as u128 / k as u128) as usize;
            let mut values: Vec<Value> = Vec::with_capacity(per.len());
            for c in per.iter().rev() {
                let sel = flat % c.len();
                flat /= c.len();
                values.push(c[sel].value.clone());
            }
            values.reverse();
            relations::repair(&sig.param_relations, &names, &per, &mut values, None, domain);
            let candidate_input = to_input(&values);
            if !out.contains(&candidate_input) {
                out.push(candidate_input);
            }
        }
    }

    out
}

/// Candidate values for a parameter, ranked, including the None/default-path injection for
/// defaulted parameters, the default's own literal value, guard-derived literal samples (see
/// `analyze::collect::guards`) so generation is more likely to exercise both sides of a guarded
/// branch, and — for a parameter carrying one or more inferred content-domain tags (see
/// `analyze::collect::hints`) — the union of the matching [`Rank::Hint`] corpora from
/// `seeds::hint_candidates`. Stably sorted by rank so the parameter's [`Rank::Base`] candidate
/// stays first.
///
/// When `domain` is given, every candidate from every source above is checked against
/// [`ValueDomain::allows`] before it's returned — this is the one place all of a parameter's
/// candidates converge, so filtering here covers shapes, guards, hints and defaults alike. If
/// filtering empties the list (e.g. a `Dict`-shaped parameter under a scalars-only domain),
/// [`ValueDomain::fallback_candidates`] fills it instead of leaving nothing for [`gen_inputs`]
/// to sample.
fn candidates_for(p: &ParamInfo, domain: Option<&ValueDomain>) -> Vec<Candidate> {
    let mut c = seeds::candidates(&generation_shape(p));
    // The author's own annotation (untrusted for inference, but a fine ranking hint) says which
    // concrete shape the sampler's limited early budget should reach first. Prepending — rather
    // than replacing — keeps every other candidate as a falsifier for a wrong or lying
    // annotation. See `ParamInfo::declared_shape_hint`.
    if let Some(hint) = &p.declared_shape_hint {
        let matching: Vec<Candidate> = seeds::candidates(hint)
            .into_iter()
            .filter(|cand| !c.iter().any(|existing| existing.value == cand.value))
            .collect();
        c.splice(0..0, matching);
    }
    // A defaulted parameter is likely Optional — exercise the None/default path.
    if p.has_default && !c.iter().any(|cand| cand.value.is_null()) {
        c.push(Candidate { value: Value::Null, rank: Rank::Guard });
    }
    // A parameter's own default is the function's own source, not an annotation — trustworthy
    // evidence of intent, unlike `declared` (which comes from an untrusted annotation).
    if let Some(default) = &p.default_literal
        && !default.is_null()
        && !c.iter().any(|cand| &cand.value == default)
    {
        c.push(Candidate { value: default.clone(), rank: Rank::Guard });
    }
    for sample in &p.guard_samples {
        if !c.iter().any(|cand| &cand.value == sample) {
            c.push(Candidate { value: sample.clone(), rank: Rank::Guard });
        }
    }
    for tag in &p.hints {
        for literal in seeds::hint_candidates(tag) {
            let value = json!(literal);
            if !c.iter().any(|cand| cand.value == value) {
                c.push(Candidate { value, rank: Rank::Hint });
            }
        }
    }
    if let Some(domain) = domain {
        c.retain(|cand| domain.allows(&cand.value));
        if c.is_empty() {
            c = domain.fallback_candidates();
        }
    }
    c.sort_by_key(|cand| cand.rank);
    c
}

/// The shape used to draw generation candidates for `p` — normally `p.shape`, but when the
/// analyzer found no evidence at all (`Shape::Any`) and the parameter's own default is a literal
/// scalar, the default's type is stronger evidence than "no evidence": a parameter defaulting to
/// `0.5` draws from the `Float` candidates instead of the generic `Any` spread. Generation-only —
/// this never touches `p.shape` in the signature, so it can't affect the may-set or purity.
fn generation_shape(p: &ParamInfo) -> Shape {
    if p.shape != Shape::Any {
        return p.shape.clone();
    }
    match &p.default_literal {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => Shape::Int,
        Some(Value::Number(_)) => Shape::Float,
        Some(Value::String(_)) => Shape::Str,
        Some(Value::Bool(_)) => Shape::Bool,
        _ => Shape::Any,
    }
}

/// A generated value ranked as the one typical value for its shape — see [`Rank::Base`].
fn base(value: Value) -> Candidate {
    Candidate { value, rank: Rank::Base }
}

/// A generated value ranked as a structural boundary — see [`Rank::Edge`].
fn edge(value: Value) -> Candidate {
    Candidate { value, rank: Rank::Edge }
}

/// A generated value ranked as generic spread — see [`Rank::Filler`].
fn filler(value: Value) -> Candidate {
    Candidate { value, rank: Rank::Filler }
}

/// A generated value ranked as a seeded structural property — see [`Rank::Property`].
fn property(value: Value) -> Candidate {
    Candidate { value, rank: Rank::Property }
}

/// Strictly-smaller candidate variants of `value`, for shrinking a failing case's input while
/// preserving its JSON kind (and, for tagged set/dict encodings, its `__t__` tag) — the worker
/// must be able to deserialize a shrink candidate exactly like any other generated value. Each
/// candidate is a *distinct* value considered strictly smaller than `value` by an obvious
/// per-kind measure (length, magnitude, member count); `value` itself is never returned.
/// Containers additionally propose one candidate per element that is itself shrunk (holding
/// container size fixed), so nested structure can shrink without dropping outer elements.
///
/// `domain`, when given, filters out any candidate [`ValueDomain::allows`] rejects — shrinking
/// only ever makes a value structurally smaller (shorter, fewer elements, smaller magnitude) of
/// the same kind, so an in-domain `value` almost always produces in-domain candidates already;
/// this filter is the explicit guarantee, not a load-bearing narrowing.
pub fn shrink_candidates(value: &Value, domain: Option<&ValueDomain>) -> Vec<Value> {
    let raw = match value {
        Value::Null => Vec::new(),
        Value::Bool(_) => Vec::new(),
        Value::Number(n) => shrink_number(n),
        Value::String(s) => shrink_string(s),
        Value::Array(items) => shrink_array(items, domain),
        Value::Object(map) => shrink_object(map, domain),
    };
    match domain {
        Some(d) => raw.into_iter().filter(|v| d.allows(v)).collect(),
        None => raw,
    }
}

fn shrink_number(n: &serde_json::Number) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(i) = n.as_i64() {
        if i != 0 {
            out.push(json!(0));
        }
        if i.abs() > 1 {
            out.push(json!(i / 2));
        }
        if i > 0 {
            out.push(json!(i - 1));
        } else if i < 0 {
            out.push(json!(i + 1));
        }
    } else if let Some(f) = n.as_f64() {
        if f != 0.0 {
            out.push(json!(0.0));
        }
        if f.abs() > f64::EPSILON {
            out.push(json!(f / 2.0));
        }
    }
    out
}

fn shrink_string(s: &str) -> Vec<Value> {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let mut out = vec![json!("")];
    if chars.len() > 1 {
        out.push(json!(chars[..chars.len() / 2].iter().collect::<String>()));
        out.push(json!(chars[..chars.len() - 1].iter().collect::<String>()));
    }
    out
}

/// Whether `map` is a plain string-keyed JSON object (a dict candidate) rather than a tagged
/// `{"__t__": ..., "items": [...]}` encoding — see `seeds::is_string_like_key`/`seeds::set_val`.
pub(super) fn tag_of(map: &Map<String, Value>) -> Option<&str> {
    map.get("__t__").and_then(Value::as_str)
}

fn shrink_array(items: &[Value], domain: Option<&ValueDomain>) -> Vec<Value> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut out = vec![json!([])];
    if items.len() > 1 {
        out.push(Value::Array(items[..items.len() / 2].to_vec()));
        out.push(Value::Array(items[..items.len() - 1].to_vec()));
    }
    for (i, item) in items.iter().enumerate() {
        for shrunk in shrink_candidates(item, domain) {
            let mut variant = items.to_vec();
            variant[i] = shrunk;
            out.push(Value::Array(variant));
        }
    }
    out
}

fn shrink_object(map: &Map<String, Value>, domain: Option<&ValueDomain>) -> Vec<Value> {
    match tag_of(map) {
        Some("dict") | Some("set") => shrink_tagged_items(map, domain),
        Some(_) => Vec::new(),
        None => shrink_plain_dict(map, domain),
    }
}

/// Shrink a tagged `{"__t__": "dict"|"set", "items": [...]}` encoding by shrinking its `items`
/// array (dropping/halving entries), preserving the tag.
fn shrink_tagged_items(map: &Map<String, Value>, domain: Option<&ValueDomain>) -> Vec<Value> {
    let Some(Value::Array(items)) = map.get("items") else {
        return Vec::new();
    };
    let tag = map.get("__t__").cloned().unwrap_or(Value::Null);
    shrink_array(items, domain)
        .into_iter()
        .map(|shrunk_items| {
            let mut m = Map::new();
            m.insert("__t__".to_string(), tag.clone());
            m.insert("items".to_string(), shrunk_items);
            Value::Object(m)
        })
        .collect()
}

fn shrink_plain_dict(map: &Map<String, Value>, domain: Option<&ValueDomain>) -> Vec<Value> {
    if map.is_empty() {
        return Vec::new();
    }
    let entries: Vec<(&String, &Value)> = map.iter().collect();
    let mut out = vec![Value::Object(Map::new())];
    if entries.len() > 1 {
        let build = |n: usize| -> Value {
            let mut m = Map::new();
            for (k, v) in entries.iter().take(n) {
                m.insert((*k).clone(), (*v).clone());
            }
            Value::Object(m)
        };
        out.push(build(entries.len() / 2));
        out.push(build(entries.len() - 1));
    }
    for (k, v) in &entries {
        for shrunk in shrink_candidates(v, domain) {
            let mut variant = map.clone();
            variant.insert((*k).to_string(), shrunk);
            out.push(Value::Object(variant));
        }
    }
    out
}

/// The parameters that actually receive a positional slot in a generated call: everything
/// except `*args`/`**kwargs`, in declaration order. Callers that need to line up a generated
/// input vector against `sig.params` positionally (e.g. mutation diffing) must filter the same
/// way.
pub fn positional_params(sig: &EffectSignature) -> Vec<&ParamInfo> {
    sig.params
        .iter()
        .filter(|p| p.kind == ParamKind::Positional)
        .collect()
}

/// The keyword-only parameters (`def f(a, *, b)`), in declaration order — generated by shape
/// like positional params, but passed as named keyword arguments; see [`gen_inputs`].
pub fn keyword_only_params(sig: &EffectSignature) -> Vec<&ParamInfo> {
    sig.params
        .iter()
        .filter(|p| p.kind == ParamKind::KeywordOnly)
        .collect()
}
