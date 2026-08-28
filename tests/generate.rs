//! Pure-Rust tests for input generation (no jail).

use pylens::analyze_source;
use pylens::generate::{ValueDomain, gen_inputs, shrink_candidates};
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
    let vectors = gen_inputs(&f, 4, None);
    assert!(!vectors.is_empty());
    for v in &vectors {
        assert_eq!(v.positional.len(), 1, "only `a` should get a positional slot: {v:?}");
    }
}

#[test]
fn ordinary_params_unaffected() {
    let sigs = analyze_source("def f(a, b):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 4, None);
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
    let vectors = gen_inputs(&f, 12, None);
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
    let vectors = gen_inputs(&f, 32, None);
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
    let vectors = gen_inputs(&f, 12, None);
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
    assert!(shrink_candidates(&Value::Null, None).is_empty());
    assert!(shrink_candidates(&json!(true), None).is_empty());
    assert!(shrink_candidates(&json!(false), None).is_empty());
}

#[test]
fn shrink_candidates_int_are_strictly_smaller_in_magnitude() {
    for original in [json!(7), json!(-3), json!(1), json!(0)] {
        let cands = shrink_candidates(&original, None);
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
    let cands = shrink_candidates(&original, None);
    assert!(!cands.is_empty());
    for c in &cands {
        assert!(c.is_number());
        assert!(abs_num(c) < abs_num(&original));
    }
}

#[test]
fn shrink_candidates_string_are_strictly_shorter() {
    let original = json!("hello world");
    let cands = shrink_candidates(&original, None);
    assert!(!cands.is_empty());
    for c in &cands {
        let s = c.as_str().expect("shrink of a string must stay a string");
        assert!(s.len() < "hello world".len(), "candidate {s:?} is not shorter");
    }
}

#[test]
fn shrink_candidates_empty_string_has_no_variant() {
    assert!(shrink_candidates(&json!(""), None).is_empty());
}

#[test]
fn shrink_candidates_array_stays_array_and_shrinks() {
    let original = json!([1, 2, 3]);
    let cands = shrink_candidates(&original, None);
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
    let cands = shrink_candidates(&original, None);
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
    assert!(shrink_candidates(&json!([]), None).is_empty());
}

#[test]
fn shrink_candidates_plain_dict_stays_object_and_shrinks() {
    let mut original = serde_json::Map::new();
    original.insert("a".to_string(), json!(1));
    original.insert("b".to_string(), json!(2));
    let original = Value::Object(original);
    let cands = shrink_candidates(&original, None);
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
    let cands = shrink_candidates(&original, None);
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
    let vectors = gen_inputs(&f, 12, None);
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
    let vectors = gen_inputs(&f, 4, None);
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
    let vectors = gen_inputs(&f, 32, None);
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
    let vectors = gen_inputs(&f, 12, None);
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
fn value_domain_parses_a_full_profile() {
    let text = r#"{
        "scalars": ["int", "float", "bool", "str", "none"],
        "list_elements": ["int", "float", "bool", "str", "none", "list"],
        "max_list_len": 6,
        "max_str_len": 64,
        "max_list_depth": 2
    }"#;
    ValueDomain::parse(text).expect("a well-formed profile must parse");
}

#[test]
fn value_domain_rejects_unknown_kind() {
    let err = ValueDomain::parse(r#"{"scalars": ["int", "bogus"]}"#)
        .expect_err("an unknown kind string must be rejected");
    assert!(err.contains("bogus"), "unexpected message: {err}");
}

#[test]
fn value_domain_rejects_unknown_field() {
    let err = ValueDomain::parse(r#"{"typo_field": []}"#)
        .expect_err("an unknown field must be rejected");
    assert!(err.contains("typo_field"), "unexpected message: {err}");
}

#[test]
fn value_domain_scalars_only_excludes_containers_and_admits_declared_scalars() {
    let domain = ValueDomain::parse(r#"{"scalars": ["int"]}"#).expect("parse");
    assert!(domain.allows(&json!(1)));
    assert!(!domain.allows(&json!(1.5)), "float not in scalars");
    assert!(!domain.allows(&json!("x")), "str not in scalars");
    assert!(!domain.allows(&Value::Null), "none not in scalars");
    assert!(!domain.allows(&json!([1])), "no list_elements given, so lists are excluded");
    assert!(!domain.allows(&json!({"a": 1})), "dicts have no kind string, always excluded");
    assert!(
        !domain.allows(&json!({"__t__": "set", "items": [1]})),
        "sets have no kind string, always excluded"
    );
}

#[test]
fn value_domain_list_elements_present_permits_lists_at_top_level() {
    let domain =
        ValueDomain::parse(r#"{"scalars": ["int"], "list_elements": ["int"]}"#).expect("parse");
    assert!(domain.allows(&json!([1, 2])));
    assert!(!domain.allows(&json!([1, "x"])), "str element not in list_elements");
}

#[test]
fn value_domain_enforces_max_list_len_and_max_str_len_and_max_list_depth() {
    let domain = ValueDomain::parse(
        r#"{"scalars": ["int", "str"], "list_elements": ["int", "list"], "max_list_len": 2, "max_str_len": 3, "max_list_depth": 2}"#,
    )
    .expect("parse");
    assert!(domain.allows(&json!([1, 2])));
    assert!(!domain.allows(&json!([1, 2, 3])), "exceeds max_list_len");
    assert!(domain.allows(&json!("abc")));
    assert!(!domain.allows(&json!("abcd")), "exceeds max_str_len");
    assert!(domain.allows(&json!([[1]])), "nesting to depth 2 is within max_list_depth");
    assert!(!domain.allows(&json!([[[1]]])), "nesting to depth 3 exceeds max_list_depth");
}

#[test]
fn gen_inputs_under_a_restrictive_domain_produces_only_in_domain_values() {
    // `Shape::Any` (no evidence for `x`) spreads across ints, a float, a string, a list, a
    // dict and a set (see `seeds::candidates`'s `Shape::Any` arm) — exactly the corpora a
    // `scalars: ["int"]` domain must strip down to its surviving int candidates.
    let sigs = analyze_source("def f(x):\n    return x\n").expect("parse");
    let f = sig(&sigs, "f");
    let domain = ValueDomain::parse(r#"{"scalars": ["int"]}"#).expect("parse");
    let vectors = gen_inputs(&f, 12, Some(&domain));
    assert!(!vectors.is_empty());
    assert!(
        vectors.len() > 1,
        "budget should still fill from the surviving int candidates: {vectors:?}"
    );
    for v in &vectors {
        let value = &v.positional[0];
        assert!(
            value.as_i64().is_some() || value.as_u64().is_some(),
            "expected an int under a scalars: [int] domain, got {value:?}"
        );
    }
}

#[test]
fn gen_inputs_domain_filter_covers_guard_and_hint_candidates_too() {
    // `url`'s candidates include a guard-derived string literal (`"exact"`) and, via the
    // `hints` collector, the well-formed/malformed URL corpus — both string-shaped, so a
    // `scalars: ["int"]` domain must strip every one of them, leaving only `count`'s int
    // candidates to fill the budget.
    let sigs = analyze_source(
        "from urllib.parse import urlparse\n\
         def f(url, count):\n    if url == \"exact\":\n        return 1\n    return urlparse(url) if count else 0\n",
    )
    .expect("parse");
    let f = sig(&sigs, "f");
    let domain = ValueDomain::parse(r#"{"scalars": ["int"]}"#).expect("parse");
    let vectors = gen_inputs(&f, 32, Some(&domain));
    assert!(!vectors.is_empty());
    assert!(vectors.len() > 1, "budget should still fill via `count`'s int candidates: {vectors:?}");
    for v in &vectors {
        for value in &v.positional {
            assert!(
                value.as_i64().is_some() || value.as_u64().is_some(),
                "no string (guard literal or hint corpus) may survive a scalars: [int] domain: {value:?}"
            );
        }
    }
}

#[test]
fn keyword_only_params_go_in_kwargs_not_positional() {
    let sigs = analyze_source("def f(a, *, b):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 4, None);
    assert!(!vectors.is_empty());
    for v in &vectors {
        assert_eq!(v.positional.len(), 1, "`a` is the only positional param: {v:?}");
        assert_eq!(v.kwargs.len(), 1, "`b` should be the only kwarg: {v:?}");
        assert_eq!(v.kwargs[0].0, "b");
    }
}
