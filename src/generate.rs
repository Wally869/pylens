//! Input generation directed by the analyzer's inferred parameter shapes. Produces a small,
//! diverse set of input vectors (including boundary values) per function.

use serde_json::{Value, json};

use crate::model::{EffectSignature, ParamShape};

/// Generate up to `max_vectors` input vectors for `sig`, one `Value` per parameter. Vectors are
/// an even spread across the full cartesian product of per-parameter candidates (not just the
/// diagonal), so combinations of arguments — not only matched positions — get exercised.
pub fn gen_inputs(sig: &EffectSignature, max_vectors: usize) -> Vec<Vec<Value>> {
    let max = max_vectors.max(1);
    if sig.params.is_empty() {
        return vec![vec![]];
    }
    let per: Vec<Vec<Value>> = sig
        .params
        .iter()
        .map(|p| {
            let mut c = candidates(p.shape);
            // A defaulted parameter is likely Optional — exercise the None/default path.
            if p.has_default && !c.iter().any(Value::is_null) {
                c.insert(0, Value::Null);
            }
            c
        })
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
            per.iter()
                .map(|c| {
                    let sel = flat % c.len();
                    flat /= c.len();
                    c[sel].clone()
                })
                .collect()
        })
        .collect()
}

/// Candidate values for a parameter of a given shape (boundary cases first).
fn candidates(shape: ParamShape) -> Vec<Value> {
    match shape {
        ParamShape::Int => vec![json!(0), json!(1), json!(-3), json!(7)],
        ParamShape::Float => vec![json!(0.0), json!(1.5), json!(-2.0), json!(3.25)],
        ParamShape::Bool => vec![json!(true), json!(false)],
        ParamShape::Str => vec![
            json!(""),
            json!("hello world"),
            json!("a b a c b a"),
            json!("Word"),
        ],
        ParamShape::Sequence => vec![json!([]), json!([1]), json!([3, 1, 2]), json!([5, 5, 2, 8])],
        ParamShape::Mapping => vec![json!({}), json!({ "a": 1 }), json!({"a": 1, "b": 2})],
        ParamShape::Set => vec![set_val(&[]), set_val(&[1]), set_val(&[1, 2, 3])],
        // No discriminating usage: spread across the type spectrum, None included. Ordered so
        // the even-spaced sample (see `gen_inputs`) still hits the common happy-path types
        // (str/seq/None/float at a cap of 4) before the rarer ones.
        ParamShape::Any => vec![
            json!("ab"),
            json!(0),
            json!([1, 2, 3]),
            json!({ "k": 1 }),
            Value::Null,
            json!(true),
            json!(1.5),
            json!(-1),
            set_val(&[1, 2]),
        ],
    }
}

fn set_val(items: &[i64]) -> Value {
    json!({ "__t__": "set", "items": items })
}
