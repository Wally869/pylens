//! Pure-Rust tests for input generation (no jail).

use pylens::analyze_source;
use pylens::generate::{gen_inputs, shrink_candidates};
use serde_json::{Value, json};

fn sig(sigs: &[pylens::model::EffectSignature], name: &str) -> pylens::model::EffectSignature {
    sigs.iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no signature named {name}"))
        .clone()
}

#[test]
fn varargs_and_kwargs_get_no_positional_slot() {
    let sigs = analyze_source("def f(a, *args, **kw):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 4);
    assert!(!vectors.is_empty());
    for v in &vectors {
        assert_eq!(v.positional.len(), 1, "only `a` should get a positional slot: {v:?}");
    }
}

#[test]
fn ordinary_params_unaffected() {
    let sigs = analyze_source("def f(a, b):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 4);
    assert!(!vectors.is_empty());
    for v in &vectors {
        assert_eq!(v.positional.len(), 2, "both ordinary params get a slot: {v:?}");
    }
}

#[test]
fn seq_int_param_receives_sorted_and_palindrome_property_lists_at_default_budget() {
    // `x - 1` pins the element shape to Int (`- * // % **` all pin Int; see
    // `analyze/passes/shapes/mod.rs`), so `xs` infers as Seq(Int); none of these branch
    // conditions bind a literal through the guard collector (`is_sorted`/comparisons on `xs`
    // itself aren't a direct param comparison), so only the property corpus can reach them.
    let sigs = analyze_source(
        "def f(xs):\n    for x in xs:\n        y = x - 1\n    return xs\n",
    )
    .expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 12);
    assert!(!vectors.is_empty());
    let ascending = json!([1, 2, 3, 4, 5]);
    let descending = json!([5, 4, 3, 2, 1]);
    let palindrome = json!([1, 2, 1]);
    let lists: Vec<&Value> = vectors.iter().filter_map(|v| v.positional.first()).collect();
    assert!(
        lists.contains(&&ascending),
        "expected a sorted-ascending list among generated inputs: {vectors:?}"
    );
    assert!(
        lists.contains(&&descending),
        "expected a sorted-descending list among generated inputs: {vectors:?}"
    );
    assert!(
        lists.contains(&&palindrome),
        "expected a palindrome list among generated inputs: {vectors:?}"
    );
}

#[test]
fn guard_samples_appear_in_generated_vectors() {
    let sigs = analyze_source("def f(x):\n    if x == 42:\n        return 1\n    return 0\n")
        .expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 32);
    assert!(!vectors.is_empty());
    assert!(
        vectors
            .iter()
            .any(|v| v.positional.first() == Some(&serde_json::json!(42))),
        "expected the guard literal 42 among generated inputs for `x`: {vectors:?}"
    );
}

#[test]
fn hinted_param_receives_a_corpus_value_at_default_budget() {
    let sigs = analyze_source(
        "from urllib.parse import urlparse\ndef f(url):\n    return urlparse(url)\n",
    )
    .expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 12);
    assert!(!vectors.is_empty());
    assert!(
        vectors.iter().any(|v| v
            .positional
            .first()
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with("http://"))),
        "expected a well-formed URL from the hint corpus among generated inputs: {vectors:?}"
    );
}

fn abs_num(v: &Value) -> f64 {
    v.as_f64().expect("expected a JSON number").abs()
}

#[test]
fn shrink_candidates_null_and_bool_have_no_variant() {
    assert!(shrink_candidates(&Value::Null).is_empty());
    assert!(shrink_candidates(&json!(true)).is_empty());
    assert!(shrink_candidates(&json!(false)).is_empty());
}

#[test]
fn shrink_candidates_int_are_strictly_smaller_in_magnitude() {
    for original in [json!(7), json!(-3), json!(1), json!(0)] {
        let cands = shrink_candidates(&original);
        let orig_abs = abs_num(&original);
        for c in &cands {
            assert!(c.is_number(), "shrink of an int must stay a number: {c:?}");
            assert!(c != &original, "candidate must differ from the original: {c:?}");
            assert!(
                abs_num(c) < orig_abs,
                "candidate {c:?} not smaller in magnitude than {original:?}"
            );
        }
    }
}

#[test]
fn shrink_candidates_float_are_strictly_smaller_in_magnitude() {
    let original = json!(3.25);
    let cands = shrink_candidates(&original);
    assert!(!cands.is_empty());
    for c in &cands {
        assert!(c.is_number());
        assert!(abs_num(c) < abs_num(&original));
    }
}

#[test]
fn shrink_candidates_string_are_strictly_shorter() {
    let original = json!("hello world");
    let cands = shrink_candidates(&original);
    assert!(!cands.is_empty());
    for c in &cands {
        let s = c.as_str().expect("shrink of a string must stay a string");
        assert!(s.len() < "hello world".len(), "candidate {s:?} is not shorter");
    }
}

#[test]
fn shrink_candidates_empty_string_has_no_variant() {
    assert!(shrink_candidates(&json!("")).is_empty());
}

#[test]
fn shrink_candidates_array_stays_array_and_shrinks() {
    let original = json!([1, 2, 3]);
    let cands = shrink_candidates(&original);
    assert!(!cands.is_empty());
    let has_empty = cands.iter().any(|c| c == &json!([]));
    assert!(has_empty, "expected the empty array among candidates: {cands:?}");
    for c in &cands {
        let arr = c.as_array().expect("shrink of an array must stay an array");
        assert!(c != &original, "candidate must differ from the original: {c:?}");
        assert!(
            arr.len() <= 3,
            "a shrunk array must never grow past the original length: {c:?}"
        );
    }
}

#[test]
fn shrink_candidates_array_is_bounded() {
    let original = json!([1, 2, 3, 4, 5]);
    let cands = shrink_candidates(&original);
    // Whole-array shrinks (empty/half/drop-last) plus one per-element shrink variant per
    // element's own candidate set — bounded, not combinatorial.
    assert!(
        cands.len() < 100,
        "shrink candidate count should stay small, got {}",
        cands.len()
    );
}

#[test]
fn shrink_candidates_empty_array_has_no_variant() {
    assert!(shrink_candidates(&json!([])).is_empty());
}

#[test]
fn shrink_candidates_plain_dict_stays_object_and_shrinks() {
    let mut original = serde_json::Map::new();
    original.insert("a".to_string(), json!(1));
    original.insert("b".to_string(), json!(2));
    let original = Value::Object(original);
    let cands = shrink_candidates(&original);
    assert!(!cands.is_empty());
    for c in &cands {
        let obj = c.as_object().expect("shrink of a dict must stay an object");
        assert!(c != &original);
        assert!(obj.len() <= 2, "a shrunk dict must never grow: {c:?}");
    }
}

#[test]
fn shrink_candidates_tagged_set_keeps_tag() {
    let original = json!({ "__t__": "set", "items": [1, 2, 3] });
    let cands = shrink_candidates(&original);
    assert!(!cands.is_empty());
    for c in &cands {
        assert_eq!(
            c.get("__t__").and_then(Value::as_str),
            Some("set"),
            "shrink of a tagged set must keep its tag: {c:?}"
        );
        let items = c.get("items").and_then(Value::as_array).expect("items array");
        assert!(items.len() <= 3);
    }
}

#[test]
fn base_vector_comes_first() {
    let sigs = analyze_source("def f(a, b):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 12);
    assert!(!vectors.is_empty());
    // `Shape::Any` is inferred for untyped/unused params; its Base candidate is numeric (`1`) —
    // a string base would poison comparison/arithmetic guards for every sibling parameter held
    // fixed while this one varies.
    assert_eq!(vectors[0].positional, vec![json!(1), json!(1)]);
}

#[test]
fn every_parameter_varies_at_a_small_budget() {
    let sigs = analyze_source("def f(a, b):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 4);
    assert!(!vectors.is_empty());
    let first_values: std::collections::HashSet<_> =
        vectors.iter().map(|v| v.positional[0].to_string()).collect();
    let second_values: std::collections::HashSet<_> =
        vectors.iter().map(|v| v.positional[1].to_string()).collect();
    assert!(
        first_values.len() > 1,
        "the first parameter must vary even at a small budget: {vectors:?}"
    );
    assert!(
        second_values.len() > 1,
        "the second parameter must vary even at a small budget: {vectors:?}"
    );
}

#[test]
fn no_duplicate_vectors() {
    let sigs = analyze_source("def f(a, b, *, c):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 32);
    let mut seen = Vec::new();
    for v in &vectors {
        assert!(!seen.contains(v), "duplicate generated vector: {v:?}");
        seen.push(v.clone());
    }
}

#[test]
fn single_parameter_function_spends_whole_budget_on_it() {
    let sigs = analyze_source("def f(x):\n    return x\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 12);
    let distinct: std::collections::HashSet<_> =
        vectors.iter().map(|v| v.positional[0].to_string()).collect();
    assert_eq!(
        distinct.len(),
        vectors.len(),
        "every emitted vector for a single-param function should differ: {vectors:?}"
    );
    assert!(
        vectors.len() > 1,
        "a single-param function should still spend budget across candidates: {vectors:?}"
    );
}

#[test]
fn keyword_only_params_go_in_kwargs_not_positional() {
    let sigs = analyze_source("def f(a, *, b):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 4);
    assert!(!vectors.is_empty());
    for v in &vectors {
        assert_eq!(v.positional.len(), 1, "`a` is the only positional param: {v:?}");
        assert_eq!(v.kwargs.len(), 1, "`b` should be the only kwarg: {v:?}");
        assert_eq!(v.kwargs[0].0, "b");
    }
}
