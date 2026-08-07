//! Input generation directed by the analyzer's inferred parameter shapes. Produces a small,
//! diverse set of input vectors (including boundary values) per function.

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

/// Generate up to `max_vectors` [`GenInput`]s for `sig`: one `Value` per *positional* parameter,
/// plus a keyword-argument map with one entry per *keyword-only* parameter. `*args`/`**kwargs`
/// params are excluded entirely (they receive nothing). Both groups are drawn from the same
/// mixed-radix even spread across the combined cartesian product of their candidates, so
/// combinations across positional AND keyword-only arguments — not only matched positions — get
/// exercised.
pub fn gen_inputs(sig: &EffectSignature, max_vectors: usize) -> Vec<GenInput> {
    let max = max_vectors.max(1);
    let positional = positional_params(sig);
    let kwonly = keyword_only_params(sig);
    if positional.is_empty() && kwonly.is_empty() {
        return vec![GenInput::default()];
    }
    let per: Vec<Vec<Value>> = positional
        .iter()
        .chain(kwonly.iter())
        .map(|p| candidates_for(p))
        .collect();

    let total = per
        .iter()
        .map(Vec::len)
        .fold(1usize, |a, b| a.saturating_mul(b))
        .max(1);
    let k = max.min(total);
    (0..k)
        .map(|i| {
            // Even spacing across the product, decoded as a mixed-radix index over the params.
            let mut flat = (i as u128 * total as u128 / k as u128) as usize;
            let values: Vec<Value> = per
                .iter()
                .map(|c| {
                    let sel = flat % c.len();
                    flat /= c.len();
                    c[sel].clone()
                })
                .collect();
            let (pos_values, kw_values) = values.split_at(positional.len());
            GenInput {
                positional: pos_values.to_vec(),
                kwargs: kwonly
                    .iter()
                    .zip(kw_values)
                    .map(|(p, v)| (p.name.clone(), v.clone()))
                    .collect(),
            }
        })
        .collect()
}

/// Candidate values for a parameter, including the None/default-path injection for defaulted
/// parameters and guard-derived literal samples (see `analyze::collect::guards`) so generation
/// is more likely to exercise both sides of a guarded branch.
fn candidates_for(p: &ParamInfo) -> Vec<Value> {
    let mut c = candidates(&p.shape);
    // A defaulted parameter is likely Optional — exercise the None/default path.
    if p.has_default && !c.iter().any(Value::is_null) {
        c.insert(0, Value::Null);
    }
    for sample in &p.guard_samples {
        if !c.contains(sample) {
            c.push(sample.clone());
        }
    }
    c
}

/// The most a nested container candidate draws from its element/key/value candidates, and the
/// most elements a generated list/set/dict candidate holds — keeps recursive generation from
/// blowing up combinatorially on deeply nested shapes.
const BREADTH_CAP: usize = 3;

/// Candidate values for a parameter of a given shape (boundary cases first). Recursive:
/// container shapes (`Seq`/`Map`/`Set`) build their candidates out of a breadth-capped sample of
/// their element/key/value shape's own candidates.
fn candidates(shape: &Shape) -> Vec<Value> {
    match shape {
        Shape::Int => vec![json!(0), json!(1), json!(-3), json!(7)],
        Shape::Float => vec![json!(0.0), json!(1.5), json!(-2.0), json!(3.25)],
        Shape::Bool => vec![json!(true), json!(false)],
        Shape::Str => vec![
            json!(""),
            json!("hello world"),
            json!("a b a c b a"),
            json!("Word"),
        ],
        Shape::Bytes => vec![json!(""), json!("hello"), json!("abc")],
        Shape::None => vec![Value::Null],
        Shape::Seq(elem) => seq_candidates(elem),
        Shape::Map(key, value) => map_candidates(key, value),
        Shape::Set(elem) => {
            let e = capped(candidates(elem));
            vec![
                set_val(&[]),
                set_val(&e[..1.min(e.len())]),
                set_val(&e),
            ]
        }
        // No discriminating usage: spread across the type spectrum, None included. Ordered so
        // the even-spaced sample (see `gen_inputs`) still hits the common happy-path types
        // (str/seq/None/float at a cap of 4) before the rarer ones.
        Shape::Any => vec![
            json!("ab"),
            json!(0),
            json!([1, 2, 3]),
            json!({ "k": 1 }),
            Value::Null,
            json!(true),
            json!(1.5),
            json!(-1),
            set_val(&[json!(1), json!(2)]),
        ],
        // A union's candidates are the union of its members' candidates, so generation exercises
        // every branch a disjoint-shape param/return can take.
        Shape::Union(members) => members.iter().flat_map(candidates).collect(),
    }
}

/// A breadth-capped sample of `c` (at most `BREADTH_CAP` candidates) to build nested container
/// candidates from.
fn capped(mut c: Vec<Value>) -> Vec<Value> {
    c.truncate(BREADTH_CAP);
    c
}

fn seq_candidates(elem: &Shape) -> Vec<Value> {
    let e = capped(candidates(elem));
    if e.is_empty() {
        return vec![json!([])];
    }
    let mut out = vec![json!([]), json!([e[0].clone()])];
    out.push(Value::Array(e.clone()));
    out
}

/// Whether keys of this shape can be represented as plain JSON object string keys (the worker
/// treats a plain object as a `str`-keyed dict). Non-string-key shapes need the tagged
/// `{"__t__": "dict", "items": [[k, v], ...]}` encoding instead.
fn is_string_like_key(key: &Shape) -> bool {
    matches!(key, Shape::Str | Shape::Any)
}

fn map_candidates(key: &Shape, value: &Shape) -> Vec<Value> {
    let ks = capped(candidates(key));
    let vs = capped(candidates(value));
    if ks.is_empty() || vs.is_empty() {
        return vec![json!({})];
    }
    let pairs = |n: usize| -> Vec<(Value, Value)> {
        (0..n)
            .map(|i| (ks[i % ks.len()].clone(), vs[i % vs.len()].clone()))
            .collect()
    };
    if is_string_like_key(key) {
        let build = |n: usize| -> Value {
            let mut m = Map::new();
            for (k, v) in pairs(n) {
                let key_str = k.as_str().map(str::to_string).unwrap_or_else(|| format!("k{}", m.len()));
                m.insert(key_str, v);
            }
            Value::Object(m)
        };
        vec![build(0), build(1), build(2)]
    } else {
        let build = |n: usize| -> Value {
            let items: Vec<Value> = pairs(n).into_iter().map(|(k, v)| json!([k, v])).collect();
            json!({ "__t__": "dict", "items": items })
        };
        vec![build(0), build(1), build(2)]
    }
}

fn set_val(items: &[Value]) -> Value {
    json!({ "__t__": "set", "items": items })
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
