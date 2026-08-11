//! Per-shape candidate corpora: the concrete seed values [`candidates`] draws from for each
//! [`Shape`], the structural property corpora (sorted/palindrome/all-equal lists, primes, float
//! traps, ...) that give branches with no guard evidence a chance to fire, and the content-domain
//! corpora keyed by the tags `analyze::collect::hints` infers (`url`, `email`, `path`, `json`,
//! `date`, `numeric_str`, `regex`, `html`).
//!
//! The domain corpora exist for the same reason as the structural ones: generation can only ever
//! *widen* the set of behaviors an execution observes, never narrow the analyzer's may-set — a
//! wrong domain guess just wastes one of the (already capped) `--inputs` budget on a value that
//! makes the function return early, exactly like an unlucky structural candidate does. So these
//! corpora only need to be cheap and roughly right, not precise: each one mixes well-formed and
//! malformed members on purpose, since the malformed ones are what reach a function's error
//! branch and the well-formed ones are what reach its real body.

use serde_json::{Value, Map, json};

use crate::model::Shape;

use super::{Candidate, Rank, base, edge, filler, property};

/// The most a nested container candidate draws from its element/key/value candidates, and the
/// most elements a generated list/set/dict candidate holds — keeps recursive generation from
/// blowing up combinatorially on deeply nested shapes.
const BREADTH_CAP: usize = 3;

/// Candidate values for a parameter of a given shape, exactly one ranked [`Rank::Base`] (a
/// typical value, not a boundary one — a held-fixed parameter of `[]` or `0` makes most
/// functions return early and wastes the one-at-a-time sampling in [`super::gen_inputs`]) and
/// first in the returned order. Recursive: container shapes (`Seq`/`Map`/`Set`) build their
/// candidates out of a breadth-capped sample of their element/key/value shape's own candidates.
pub(super) fn candidates(shape: &Shape) -> Vec<Candidate> {
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

/// One well-formed and one malformed literal for each domain corpus, keyed by the
/// [`analyze::collect::hints`](crate::analyze) tag it seeds. `hint_candidates` unions the
/// corpora for a parameter that carries several tags, all at [`Rank::Hint`].
pub(super) fn hint_candidates(tag: &str) -> &'static [&'static str] {
    match tag {
        "url" => &[
            "http://example.com/path?query=1",
            "http://user:pass@example.com:8080",
            "ftp://",
            "not a url",
        ],
        "email" => &["user@example.com", "user@", "not-an-email"],
        "path" => &["/usr/local/bin/app", "relative/path/file.txt", "C:\\Users\\name\\file.txt", "/", ""],
        "json" => &[
            "{\"key\": \"value\"}",
            "[]",
            "null",
            "{\"key\": ",
            "not json at all",
        ],
        "date" => &["2024-01-15", "2024-13-45", "01/15/2024", ""],
        "numeric_str" => &["42", "-1", "0", "3.14", "not a number"],
        "regex" => &["abc.*", "(?P<name>\\w+)", "[a-z"],
        "html" => &["<p>text</p>", "<a href=\"x\">link</a>", "plain text"],
        _ => &[],
    }
}
