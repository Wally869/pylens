//! Input generation directed by the analyzer's inferred parameter shapes. Produces a small,
//! diverse set of input vectors (including boundary values) per function, spending a limited
//! `--inputs` budget on the candidates most likely to reach new behavior: hold every parameter
//! at a typical value, vary one parameter at a time through its ranked candidates, then spend
//! whatever budget remains on combinations.

use serde_json::{Value, Map, json};

use crate::model::{EffectSignature, ParamInfo, ParamKind, Shape};

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
/// whole budget before the others get a turn; (3) once every parameter's candidates are
/// exhausted, combination vectors spread evenly across the full cartesian product, so pairings
/// across positional AND keyword-only arguments — not only matched positions — get exercised.
/// Duplicate vectors (same positional values and same keyword pairs) are never emitted twice.
pub fn gen_inputs(sig: &EffectSignature, max_vectors: usize) -> Vec<GenInput> {
    let max = max_vectors.max(1);
    let positional = positional_params(sig);
    let kwonly = keyword_only_params(sig);
    if positional.is_empty() && kwonly.is_empty() {
        return vec![GenInput::default()];
    }
    let per: Vec<Vec<Candidate>> = positional
        .iter()
        .chain(kwonly.iter())
        .map(|p| candidates_for(p))
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

    let base_values: Vec<Value> = per.iter().map(|c| c[0].value.clone()).collect();
    let mut out: Vec<GenInput> = vec![to_input(&base_values)];

    let max_len = per.iter().map(Vec::len).max().unwrap_or(1);
    'round_robin: for i in 1..max_len {
        for (j, c) in per.iter().enumerate() {
            if out.len() >= max {
                break 'round_robin;
            }
            let Some(cand) = c.get(i) else { continue };
            let mut values = base_values.clone();
            values[j] = cand.value.clone();
            let candidate_input = to_input(&values);
            if !out.contains(&candidate_input) {
                out.push(candidate_input);
            }
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
            let candidate_input = to_input(&values);
            if !out.contains(&candidate_input) {
                out.push(candidate_input);
            }
        }
    }

    out
}

/// Candidate values for a parameter, ranked, including the None/default-path injection for
/// defaulted parameters, the default's own literal value, and guard-derived literal samples (see
/// `analyze::collect::guards`) so generation is more likely to exercise both sides of a guarded
/// branch. Stably sorted by rank so the parameter's [`Rank::Base`] candidate stays first.
fn candidates_for(p: &ParamInfo) -> Vec<Candidate> {
    let mut c = candidates(&generation_shape(p));
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

/// The most a nested container candidate draws from its element/key/value candidates, and the
/// most elements a generated list/set/dict candidate holds — keeps recursive generation from
/// blowing up combinatorially on deeply nested shapes.
const BREADTH_CAP: usize = 3;

fn base(value: Value) -> Candidate {
    Candidate { value, rank: Rank::Base }
}

fn edge(value: Value) -> Candidate {
    Candidate { value, rank: Rank::Edge }
}

fn filler(value: Value) -> Candidate {
    Candidate { value, rank: Rank::Filler }
}

fn property(value: Value) -> Candidate {
    Candidate { value, rank: Rank::Property }
}

/// Candidate values for a parameter of a given shape, exactly one ranked [`Rank::Base`] (a
/// typical value, not a boundary one — a held-fixed parameter of `[]` or `0` makes most
/// functions return early and wastes the one-at-a-time sampling in [`gen_inputs`]) and first in
/// the returned order. Recursive: container shapes (`Seq`/`Map`/`Set`) build their candidates
/// out of a breadth-capped sample of their element/key/value shape's own candidates.
fn candidates(shape: &Shape) -> Vec<Candidate> {
    match shape {
        Shape::Int => {
            let mut c = vec![base(json!(1)), edge(json!(0)), edge(json!(-3)), filler(json!(7))];
            c.extend(int_properties());
            c
        }
        Shape::Float => {
            let mut c = vec![
                base(json!(1.5)),
                edge(json!(0.0)),
                edge(json!(-2.0)),
                filler(json!(3.25)),
            ];
            c.extend(float_properties());
            c
        }
        // `false` is the boundary/falsy counterpart to the typical `true`, the same role zero
        // plays for the numeric shapes.
        Shape::Bool => vec![base(json!(true)), edge(json!(false))],
        Shape::Str => {
            let mut c = vec![
                base(json!("hello world")),
                edge(json!("")),
                filler(json!("a b a c b a")),
                filler(json!("Word")),
            ];
            c.extend(str_properties());
            c
        }
        Shape::Bytes => {
            let mut c = vec![base(json!("abc")), edge(json!("")), filler(json!("hello"))];
            c.extend(bytes_properties());
            c
        }
        Shape::None => vec![base(Value::Null)],
        Shape::Seq(elem) => {
            let mut c = seq_candidates(elem);
            c.extend(seq_property_candidates(elem));
            c
        }
        Shape::Map(key, value) => map_candidates(key, value),
        Shape::Set(elem) => {
            let e = capped_values(elem);
            let mut out = vec![edge(set_val(&[])), base(set_val(&e[..2.min(e.len())]))];
            if e.len() > 2 {
                out.push(filler(set_val(&e)));
            }
            out
        }
        // No discriminating usage: spread across the type spectrum, None included. The base is
        // numeric (`1`), not the string `"ab"` — comparison/arithmetic guards are the most
        // common branch condition on an unconstrained parameter, and a string base makes every
        // one of them raise TypeError against whatever a sibling parameter holds it against, so
        // a string base poisons one-at-a-time sampling for every OTHER parameter that gets
        // varied while this one sits fixed. `0`, `-1`, `null`, `""` are the values most likely to
        // flip an unseen guard (falsy/negative/None checks are common even when the analyzer
        // can't pin the type down), so they're ranked `Edge` — a small `--inputs` budget must
        // reach them before it reaches the rest of the type spread.
        Shape::Any => vec![
            base(json!(1)),
            edge(json!(0)),
            edge(json!(-1)),
            edge(Value::Null),
            edge(json!("")),
            filler(json!("ab")),
            filler(json!([1, 2, 3])),
            filler(json!({ "k": 1 })),
            filler(json!(true)),
            filler(json!(1.5)),
            filler(set_val(&[json!(1), json!(2)])),
        ],
        // A union's candidates are the union of its members' candidates, so generation exercises
        // every branch a disjoint-shape param/return can take. Only the first member's typical
        // value stays `Base` — the invariant of exactly one `Base` candidate holds per parameter,
        // not per member — so the other members' typical values fall back to `Filler`.
        Shape::Union(members) => {
            let mut out: Vec<Candidate> = Vec::new();
            for (i, member) in members.iter().enumerate() {
                for cand in candidates(member) {
                    if i > 0 && cand.rank == Rank::Base {
                        out.push(filler(cand.value));
                    } else {
                        out.push(cand);
                    }
                }
            }
            out.sort_by_key(|cand| cand.rank);
            out
        }
        // The jail can build a receiver for a method under test from `__init__`, but it has no
        // way to construct an object for an ordinary parameter typed `Instance(C)` — generating
        // like `Any` is the honest stopping point until that gap closes (constructing `C` here
        // would need its own `__init__` probe, the same machinery `record.rs` already has for
        // the receiver, generalized to an arbitrary parameter position).
        Shape::Instance(_) => candidates(&Shape::Any),
    }
}

/// Property-shaped seed corpora, at [`Rank::Property`].
///
/// The guard collector (`analyze::collect::guards`) only extracts a literal when the guarded
/// parameter is a *direct* operand of a comparison — `param_root` resolves through Name,
/// Attribute and Subscript, nothing else. So a condition like `n % 2 == 0`, `len(xs) > 3`, or
/// `is_sorted(xs)` yields no guard sample at all, and a value assembled from the generic
/// candidates above is almost never even, long enough, sorted, a palindrome, or all-equal. These
/// fixed literals give exactly those branches a chance to fire even without guard evidence.
fn int_properties() -> Vec<Candidate> {
    vec![
        property(json!(97)),         // prime
        property(json!(64)),         // power of two
        property(json!(100)),        // perfect square
        property(json!(2147483648i64)), // beyond i32 range
    ]
}

fn float_properties() -> Vec<Candidate> {
    vec![
        property(json!(0.1)),   // the classic binary-representation trap
        property(json!(-0.0)),
        property(nan_val()),
        property(inf_val(false)),
        property(inf_val(true)),
    ]
}

fn str_properties() -> Vec<Candidate> {
    vec![
        property(json!("aba")),           // palindrome
        property(json!("42")),            // numeric-looking
        property(json!("-1")),            // negative numeric-looking
        property(json!("   ")),           // whitespace only
        property(json!("héllo wörld")),   // non-ASCII
        property(json!("a".repeat(256))), // long
    ]
}

fn bytes_properties() -> Vec<Candidate> {
    vec![property(json!("aba"))] // palindrome
}

/// A small ascending run of distinct literal values for an orderable scalar element shape, used
/// to build the `Seq` property corpus below. `None` for a non-orderable or container element —
/// the generic construction in `seq_candidates` already covers those.
fn ordered_examples(elem: &Shape) -> Option<Vec<Value>> {
    match elem {
        Shape::Int => Some(vec![json!(1), json!(2), json!(3), json!(4), json!(5)]),
        Shape::Float => Some(vec![json!(1.0), json!(2.0), json!(3.0), json!(4.0), json!(5.0)]),
        Shape::Str => Some(vec![json!("a"), json!("b"), json!("c"), json!("d"), json!("e")]),
        _ => None,
    }
}

/// A value far outside `ordered_examples(elem)`'s range, for the single-outlier corpus entry.
fn outlier_example(elem: &Shape) -> Value {
    match elem {
        Shape::Float => json!(1000.0),
        Shape::Str => json!("zzzzzz"),
        _ => json!(1000),
    }
}

/// Sorted, palindromic, all-equal, duplicate-heavy and outlier-containing lists for an orderable
/// scalar element — the shapes a randomly assembled list essentially never takes on, so branches
/// like "is this sorted" or "are all elements equal" never fire without a seeded example.
fn seq_property_candidates(elem: &Shape) -> Vec<Candidate> {
    let Some(vals) = ordered_examples(elem) else {
        return Vec::new();
    };
    let mut descending = vals.clone();
    descending.reverse();
    let palindrome = vec![vals[0].clone(), vals[1].clone(), vals[0].clone()];
    let all_equal = vec![vals[2].clone(); 4];
    let duplicate_heavy = vec![
        vals[0].clone(),
        vals[0].clone(),
        vals[1].clone(),
        vals[1].clone(),
        vals[2].clone(),
    ];
    let mut outlier = vals[..4].to_vec();
    outlier.push(outlier_example(elem));
    vec![
        property(Value::Array(vals)),
        property(Value::Array(descending)),
        property(Value::Array(palindrome)),
        property(Value::Array(all_equal)),
        property(Value::Array(duplicate_heavy)),
        property(Value::Array(outlier)),
    ]
}

/// A breadth-capped sample (at most `BREADTH_CAP` values, ranked candidates first) of `shape`'s
/// own candidates, to build nested container candidates from.
fn capped_values(shape: &Shape) -> Vec<Value> {
    candidates(shape)
        .into_iter()
        .take(BREADTH_CAP)
        .map(|c| c.value)
        .collect()
}

fn seq_candidates(elem: &Shape) -> Vec<Candidate> {
    let e = capped_values(elem);
    if e.is_empty() {
        return vec![base(json!([]))];
    }
    vec![
        base(Value::Array(e.clone())),
        edge(json!([])),
        filler(json!([e[0].clone()])),
    ]
}

/// Whether keys of this shape can be represented as plain JSON object string keys (the worker
/// treats a plain object as a `str`-keyed dict). Non-string-key shapes need the tagged
/// `{"__t__": "dict", "items": [[k, v], ...]}` encoding instead.
fn is_string_like_key(key: &Shape) -> bool {
    matches!(key, Shape::Str | Shape::Any)
}

fn map_candidates(key: &Shape, value: &Shape) -> Vec<Candidate> {
    let ks = capped_values(key);
    let vs = capped_values(value);
    if ks.is_empty() || vs.is_empty() {
        return vec![base(json!({}))];
    }
    let pairs = |n: usize| -> Vec<(Value, Value)> {
        (0..n)
            .map(|i| (ks[i % ks.len()].clone(), vs[i % vs.len()].clone()))
            .collect()
    };
    let build: Box<dyn Fn(usize) -> Value> = if is_string_like_key(key) {
        Box::new(move |n: usize| -> Value {
            let mut m = Map::new();
            for (k, v) in pairs(n) {
                let key_str = k.as_str().map(str::to_string).unwrap_or_else(|| format!("k{}", m.len()));
                m.insert(key_str, v);
            }
            Value::Object(m)
        })
    } else {
        Box::new(move |n: usize| -> Value {
            let items: Vec<Value> = pairs(n).into_iter().map(|(k, v)| json!([k, v])).collect();
            json!({ "__t__": "dict", "items": items })
        })
    };
    vec![base(build(1)), edge(build(0)), filler(build(2))]
}

fn set_val(items: &[Value]) -> Value {
    json!({ "__t__": "set", "items": items })
}

/// `serde_json::Value` cannot hold NaN, and the jail's `json.dumps(allow_nan=False)` would
/// reject a bare non-finite float anyway, so NaN/infinity travel through the wire in the same
/// tagged encoding as `set_val` (see `python/worker.py::serialize`/`deserialize`).
fn nan_val() -> Value {
    json!({ "__t__": "float", "v": "nan" })
}

fn inf_val(negative: bool) -> Value {
    json!({ "__t__": "float", "v": if negative { "-inf" } else { "inf" } })
}

/// Strictly-smaller candidate variants of `value`, for shrinking a failing case's input while
/// preserving its JSON kind (and, for tagged set/dict encodings, its `__t__` tag) — the worker
/// must be able to deserialize a shrink candidate exactly like any other generated value. Each
/// candidate is a *distinct* value considered strictly smaller than `value` by an obvious
/// per-kind measure (length, magnitude, member count); `value` itself is never returned.
/// Containers additionally propose one candidate per element that is itself shrunk (holding
/// container size fixed), so nested structure can shrink without dropping outer elements.
pub fn shrink_candidates(value: &Value) -> Vec<Value> {
    match value {
        Value::Null => Vec::new(),
        Value::Bool(_) => Vec::new(),
        Value::Number(n) => shrink_number(n),
        Value::String(s) => shrink_string(s),
        Value::Array(items) => shrink_array(items),
        Value::Object(map) => shrink_object(map),
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
/// `{"__t__": ..., "items": [...]}` encoding — see [`is_string_like_key`]/[`set_val`].
fn tag_of(map: &Map<String, Value>) -> Option<&str> {
    map.get("__t__").and_then(Value::as_str)
}

fn shrink_array(items: &[Value]) -> Vec<Value> {
    if items.is_empty() {
        return Vec::new();
    }
    let mut out = vec![json!([])];
    if items.len() > 1 {
        out.push(Value::Array(items[..items.len() / 2].to_vec()));
        out.push(Value::Array(items[..items.len() - 1].to_vec()));
    }
    for (i, item) in items.iter().enumerate() {
        for shrunk in shrink_candidates(item) {
            let mut variant = items.to_vec();
            variant[i] = shrunk;
            out.push(Value::Array(variant));
        }
    }
    out
}

fn shrink_object(map: &Map<String, Value>) -> Vec<Value> {
    match tag_of(map) {
        Some("dict") | Some("set") => shrink_tagged_items(map),
        Some(_) => Vec::new(),
        None => shrink_plain_dict(map),
    }
}

/// Shrink a tagged `{"__t__": "dict"|"set", "items": [...]}` encoding by shrinking its `items`
/// array (dropping/halving entries), preserving the tag.
fn shrink_tagged_items(map: &Map<String, Value>) -> Vec<Value> {
    let Some(Value::Array(items)) = map.get("items") else {
        return Vec::new();
    };
    let tag = map.get("__t__").cloned().unwrap_or(Value::Null);
    shrink_array(items)
        .into_iter()
        .map(|shrunk_items| {
            let mut m = Map::new();
            m.insert("__t__".to_string(), tag.clone());
            m.insert("items".to_string(), shrunk_items);
            Value::Object(m)
        })
        .collect()
}

fn shrink_plain_dict(map: &Map<String, Value>) -> Vec<Value> {
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
        for shrunk in shrink_candidates(v) {
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
