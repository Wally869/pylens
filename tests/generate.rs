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
fn declared_int_annotation_ranks_an_int_candidate_first_without_dropping_other_types() {
    let sigs = analyze_source("def f(x: int):\n    return x\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 12, None);
    assert!(!vectors.is_empty());
    assert!(
        vectors[0].positional.first().is_some_and(Value::is_i64),
        "expected the first generated vector to use an int candidate: {vectors:?}"
    );
    assert!(
        vectors
            .iter()
            .any(|v| v.positional.first().is_some_and(|val| !val.is_i64())),
        "expected later vectors to still cover non-int candidates: {vectors:?}"
    );
}

#[test]
fn unparseable_declared_annotation_leaves_generation_unchanged() {
    let with_annotation = analyze_source("def f(x: SomeClass):\n    return x\n").expect("parse");
    let without_annotation = analyze_source("def f(x):\n    return x\n").expect("parse");
    let f_with = sig(&with_annotation, "f");
    let f_without = sig(&without_annotation, "f");
    assert_eq!(gen_inputs(&f_with, 12, None), gen_inputs(&f_without, 12, None));
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
fn value_domain_allows_a_tagged_tuple_when_named() {
    let domain = ValueDomain::parse(r#"{"scalars": ["tuple", "str", "int"]}"#).expect("parse");
    assert!(domain.allows(&json!({"__t__": "tuple", "items": ["withdraw", 5]})));
}

#[test]
fn value_domain_rejects_a_tagged_tuple_when_not_named() {
    let domain = ValueDomain::parse(r#"{"scalars": ["str", "int"]}"#).expect("parse");
    assert!(
        !domain.allows(&json!({"__t__": "tuple", "items": ["withdraw", 5]})),
        "tuple not named in scalars, so it must be rejected"
    );
}

#[test]
fn value_domain_checks_tuple_items_against_list_elements() {
    let domain = ValueDomain::parse(
        r#"{"scalars": ["tuple"], "list_elements": ["str", "int"], "max_list_len": 2}"#,
    )
    .expect("parse");
    assert!(domain.allows(&json!({"__t__": "tuple", "items": ["withdraw", 5]})));
    assert!(
        !domain.allows(&json!({"__t__": "tuple", "items": [1.5, 5]})),
        "float item not in list_elements"
    );
    assert!(
        !domain.allows(&json!({"__t__": "tuple", "items": ["a", "b", "c"]})),
        "exceeds max_list_len"
    );
}

#[test]
fn value_domain_enforces_max_list_depth_on_nested_tuples() {
    let domain = ValueDomain::parse(
        r#"{"scalars": ["tuple"], "list_elements": ["tuple", "int"], "max_list_depth": 1}"#,
    )
    .expect("parse");
    assert!(domain.allows(&json!({"__t__": "tuple", "items": [1, 2]})));
    assert!(
        !domain.allows(&json!({
            "__t__": "tuple",
            "items": [{"__t__": "tuple", "items": [1]}]
        })),
        "nested tuple exceeds max_list_depth"
    );
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
fn related_string_and_number_params_stay_kind_paired() {
    let src = "\
def find_closest_element(arr, target):
    left = 0
    right = len(arr) - 1
    while left <= right:
        mid = (left + right) // 2
        if arr[mid] == target:
            return arr[mid]
        elif arr[mid] < target:
            left = mid + 1
        else:
            right = mid - 1
    if left >= len(arr):
        return arr[-1]
    if right < 0:
        return arr[0]
    if abs(arr[left] - target) < abs(arr[right] - target):
        return arr[left]
    else:
        return arr[right]
";
    let sigs = analyze_source(src).expect("parse");
    let f = sig(&sigs, "find_closest_element");
    assert!(!f.param_relations.is_empty(), "expected arr/target relations to be inferred");
    let vectors = gen_inputs(&f, 24, None);
    assert!(!vectors.is_empty());

    let mut saw_string_pair = false;
    let mut saw_array_number_pair = false;
    for v in &vectors {
        let (Some(arr), Some(target)) = (v.positional.first(), v.positional.get(1)) else {
            continue;
        };
        if arr.is_string() {
            assert!(
                target.is_string(),
                "a string `arr` must be paired with a string `target`: {v:?}"
            );
            saw_string_pair = true;
        }
        if let Some(items) = arr.as_array()
            && !items.is_empty()
            && items[0].is_number()
        {
            assert!(
                target.is_number(),
                "a numeric-array `arr` must be paired with a numeric `target`: {v:?}"
            );
            saw_array_number_pair = true;
        }
    }
    assert!(saw_string_pair, "expected a string/string pairing among generated vectors: {vectors:?}");
    assert!(
        saw_array_number_pair,
        "expected an array-of-numbers/number pairing among generated vectors: {vectors:?}"
    );
}

#[test]
fn unrelated_params_generate_identically_to_no_relations() {
    let sigs = analyze_source("def f(a, b):\n    return len(a) + b\n").expect("parse");
    let f = sig(&sigs, "f");
    assert!(
        f.param_relations.is_empty(),
        "a and b never meet as operands, so no relation should be inferred: {:?}",
        f.param_relations
    );
    let mut cleared = f.clone();
    cleared.param_relations.clear();
    assert_eq!(gen_inputs(&f, 24, None), gen_inputs(&cleared, 24, None));
}

#[test]
fn related_scalars_always_share_kind_or_have_a_none_side() {
    let sigs = analyze_source("def g(x, y):\n    return x < y\n").expect("parse");
    let f = sig(&sigs, "g");
    let vectors = gen_inputs(&f, 24, None);
    assert!(!vectors.is_empty());
    for v in &vectors {
        let (Some(x), Some(y)) = (v.positional.first(), v.positional.get(1)) else {
            continue;
        };
        let x_is_scalar_kind = x.is_number() || x.is_string();
        let y_is_scalar_kind = y.is_number() || y.is_string();
        if x_is_scalar_kind && y_is_scalar_kind {
            assert_eq!(
                x.is_number(),
                y.is_number(),
                "related scalars x and y must share the same kind: {v:?}"
            );
        }
    }
}

#[test]
fn value_domain_still_holds_under_relation_repair() {
    let src = "\
def find_closest_element(arr, target):
    left = 0
    right = len(arr) - 1
    while left <= right:
        mid = (left + right) // 2
        if arr[mid] == target:
            return arr[mid]
        elif arr[mid] < target:
            left = mid + 1
        else:
            right = mid - 1
    if left >= len(arr):
        return arr[-1]
    if right < 0:
        return arr[0]
    if abs(arr[left] - target) < abs(arr[right] - target):
        return arr[left]
    else:
        return arr[right]
";
    let sigs = analyze_source(src).expect("parse");
    let f = sig(&sigs, "find_closest_element");
    let domain = ValueDomain::parse(
        r#"{"scalars": ["int"], "list_elements": ["int"], "max_list_len": 6}"#,
    )
    .expect("parse");
    let vectors = gen_inputs(&f, 24, Some(&domain));
    assert!(!vectors.is_empty());
    for v in &vectors {
        for value in &v.positional {
            assert!(domain.allows(value), "value {value:?} escaped the domain: {v:?}");
        }
    }
}

const FIND_CLOSEST_ELEMENT_SRC: &str = "\
def find_closest_element(arr, target):
    left = 0
    right = len(arr) - 1
    while left <= right:
        mid = (left + right) // 2
        if arr[mid] == target:
            return arr[mid]
        elif arr[mid] < target:
            left = mid + 1
        else:
            right = mid - 1
    if left >= len(arr):
        return arr[-1]
    if right < 0:
        return arr[0]
    if abs(arr[left] - target) < abs(arr[right] - target):
        return arr[left]
    else:
        return arr[right]
";

/// The `Int` outlier property seed is `[1, 2, 3, 4, 1000]` (`seeds::seq_property_candidates`,
/// `outlier_example`) — min 1, max 1000, middle element (index 2 of the sorted-dedup 5-element
/// list) 3, and the widest adjacent gap 4..1000 (gap 996) giving the midpoint 502 (equidistant)
/// and the off-centre 503 (strictly closer to 1000 than to 4).
#[test]
fn relative_vectors_place_target_around_the_outlier_array() {
    let sigs = analyze_source(FIND_CLOSEST_ELEMENT_SRC).expect("parse");
    let f = sig(&sigs, "find_closest_element");
    assert!(!f.param_relations.is_empty(), "expected arr/target relations to be inferred");
    let vectors = gen_inputs(&f, 24, None);
    assert!(!vectors.is_empty());

    let arr = json!([1, 2, 3, 4, 1000]);
    let targets_with_arr: std::collections::HashSet<i64> = vectors
        .iter()
        .filter(|v| v.positional.first() == Some(&arr))
        .filter_map(|v| v.positional.get(1).and_then(Value::as_i64))
        .collect();

    for expected in [0, 1001, 3, 502, 503] {
        assert!(
            targets_with_arr.contains(&expected),
            "expected target {expected} paired with arr {arr:?} among generated vectors: {vectors:?}"
        );
    }
}

#[test]
fn relative_vectors_stay_in_domain() {
    let sigs = analyze_source(FIND_CLOSEST_ELEMENT_SRC).expect("parse");
    let f = sig(&sigs, "find_closest_element");
    let domain = ValueDomain::parse(
        r#"{"scalars": ["int"], "list_elements": ["int"], "max_list_len": 8}"#,
    )
    .expect("parse");
    let vectors = gen_inputs(&f, 24, Some(&domain));
    assert!(!vectors.is_empty());
    for v in &vectors {
        for value in &v.positional {
            assert!(domain.allows(value), "value {value:?} escaped the domain: {v:?}");
        }
    }
}

#[test]
fn relative_vectors_cover_a_string_container() {
    let sigs = analyze_source(
        "def g(s, ch):\n    return s.index(ch) if ch < s[0] else -1\n",
    )
    .expect("parse");
    let f = sig(&sigs, "g");
    assert!(!f.param_relations.is_empty(), "expected s/ch relations to be inferred");
    let vectors = gen_inputs(&f, 24, None);
    assert!(!vectors.is_empty());

    let sorted_dedup_chars = |s: &str| -> Vec<char> {
        let mut chars: Vec<char> = s.chars().collect();
        chars.sort_unstable();
        chars.dedup();
        chars
    };

    let saw_empty_ch = vectors.iter().any(|v| {
        let (Some(s), Some(ch)) = (v.positional.first().and_then(Value::as_str), v.positional.get(1).and_then(Value::as_str)) else {
            return false;
        };
        sorted_dedup_chars(s).len() >= 2 && ch.is_empty()
    });
    assert!(saw_empty_ch, "expected a vector with a string s and ch == \"\": {vectors:?}");

    let saw_above_max_ch = vectors.iter().any(|v| {
        let (Some(s), Some(ch)) = (v.positional.first().and_then(Value::as_str), v.positional.get(1).and_then(Value::as_str)) else {
            return false;
        };
        let chars = sorted_dedup_chars(s);
        let Some(max_char) = chars.last() else { return false };
        chars.len() >= 2 && ch == format!("{max_char}z")
    });
    assert!(
        saw_above_max_ch,
        "expected a vector with ch == s's max char followed by 'z': {vectors:?}"
    );
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
