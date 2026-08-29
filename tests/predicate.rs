//! Integration tests for the widened branch-predicate synthesizer (`generate::predicate`):
//! string-method, negation, container-membership, locals-derived, and two-parameter-coordination
//! forms, plus one still-unhandled form to confirm it correctly falls back to no synthesizer.

use pylens::generate::predicate::{self, LinePredicates, Predicate};
use pylens::model::Shape;
use ruff_source_file::LineIndex;

/// Parse `src` (one top-level function), find `if`/`while`'s predicates at `line`, and return
/// them via [`predicate::collect_predicates`] — mirrors `record::cover::run_loop`'s own setup.
fn predicates_at(src: &str, fn_name: &str, params: &[&str], line: u32) -> Vec<Predicate> {
    let parsed = pylens::parse::parse_source(src).expect("parse");
    let body = predicate::find_function_body(parsed.syntax(), fn_name, None).expect("function body");
    let line_index = LineIndex::from_source_text(src);
    let param_names: Vec<String> = params.iter().map(|s| s.to_string()).collect();
    let preds = predicate::collect_predicates(body, &line_index, &param_names);
    match preds.get(&line) {
        Some(LinePredicates::Test(ps)) => ps.clone(),
        Some(LinePredicates::ForIter(p)) => vec![p.clone()],
        None => Vec::new(),
    }
}

#[test]
fn startswith_yields_a_satisfying_and_a_violating_value() {
    let src = "def f(s):\n    if s.startswith(\"ab\"):\n        pass\n";
    let preds = predicates_at(src, "f", &["s"], 2);
    assert_eq!(preds.len(), 1);
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str).expect("violating value");
    assert_eq!(satisfy.as_str().expect("str"), "ab_rest");
    assert!(!violate.as_str().expect("str").starts_with("ab"));
}

#[test]
fn endswith_yields_a_satisfying_and_a_violating_value() {
    let src = "def f(s):\n    if s.endswith(\"xy\"):\n        pass\n";
    let preds = predicates_at(src, "f", &["s"], 2);
    assert_eq!(preds.len(), 1);
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str).expect("violating value");
    assert!(satisfy.as_str().expect("str").ends_with("xy"));
    assert!(!violate.as_str().expect("str").ends_with("xy"));
}

#[test]
fn isdigit_yields_a_digit_string_and_a_non_digit_string() {
    let src = "def f(s):\n    if s.isdigit():\n        pass\n";
    let preds = predicates_at(src, "f", &["s"], 2);
    assert_eq!(preds.len(), 1);
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str).expect("violating value");
    assert!(satisfy.as_str().expect("str").chars().all(|c| c.is_ascii_digit()));
    assert!(!violate.as_str().expect("str").chars().all(|c| c.is_ascii_digit()));
}

#[test]
fn isalpha_yields_an_alpha_string_and_a_non_alpha_string() {
    let src = "def f(s):\n    if s.isalpha():\n        pass\n";
    let preds = predicates_at(src, "f", &["s"], 2);
    assert_eq!(preds.len(), 1);
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str).expect("violating value");
    assert!(satisfy.as_str().expect("str").chars().all(|c| c.is_alphabetic()));
    assert!(!violate.as_str().expect("str").chars().all(|c| c.is_alphabetic()));
}

#[test]
fn not_negates_the_inner_truthiness_predicate() {
    let src = "def f(s):\n    if not s:\n        pass\n";
    let preds = predicates_at(src, "f", &["s"], 2);
    assert_eq!(preds.len(), 1);
    assert!(matches!(preds[0], Predicate::Not(_)));
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str).expect("violating value");
    // `not s` is true (the "true" outcome) exactly when `s` itself is falsy.
    assert_eq!(satisfy.as_str().expect("str"), "");
    assert_ne!(violate.as_str().expect("str"), "");
}

#[test]
fn container_membership_yields_a_containing_and_an_excluding_value() {
    let src = "def f(xs):\n    if \"x\" in xs:\n        pass\n";
    let preds = predicates_at(src, "f", &["xs"], 2);
    assert_eq!(preds.len(), 1);
    assert!(matches!(preds[0], Predicate::ContainerMembership { .. }));
    let shape = Shape::any_seq();
    let satisfy = predicate::synthesize(&preds[0], true, &shape).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &shape).expect("violating value");
    let satisfy_items = satisfy.as_array().expect("array");
    let violate_items = violate.as_array().expect("array");
    assert!(satisfy_items.iter().any(|v| v == "x"));
    assert!(!violate_items.iter().any(|v| v == "x"));
}

#[test]
fn local_len_alias_resolves_back_to_the_parameter() {
    let src = "def f(s):\n    n = len(s)\n    if n > 3:\n        pass\n";
    let preds = predicates_at(src, "f", &["s"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "s");
            assert_eq!(*deriv, predicate::Derivation::Len);
        }
        other => panic!("expected a Compare over `len(s)`, got {other:?}"),
    }
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str).expect("violating value");
    assert!(satisfy.as_str().expect("str").len() > 3);
    assert!(violate.as_str().expect("str").len() <= 3);
}

#[test]
fn param_compare_synthesizes_a_coordinated_pair() {
    let src = "def f(a, b):\n    if a < b:\n        pass\n";
    let preds = predicates_at(src, "f", &["a", "b"], 2);
    assert_eq!(preds.len(), 1);
    let Predicate::ParamCompare { deriv_a, op, deriv_b, .. } = &preds[0] else {
        panic!("expected a ParamCompare, got {:?}", preds[0]);
    };
    let (satisfy_a, satisfy_b) =
        predicate::synthesize_pair(deriv_a, *op, deriv_b, &Shape::Int, &Shape::Int, true)
            .expect("satisfying pair");
    let a = satisfy_a.as_i64().expect("int");
    let b = satisfy_b.as_i64().expect("int");
    assert!(a < b, "expected a < b, got a={a} b={b}");

    let (violate_a, violate_b) =
        predicate::synthesize_pair(deriv_a, *op, deriv_b, &Shape::Int, &Shape::Int, false)
            .expect("violating pair");
    let a = violate_a.as_i64().expect("int");
    let b = violate_b.as_i64().expect("int");
    assert!(a >= b, "expected a >= b, got a={a} b={b}");
}

#[test]
fn attribute_truthiness_is_still_unhandled() {
    let src = "def f(x):\n    if x.flag:\n        pass\n";
    let preds = predicates_at(src, "f", &["x"], 2);
    assert!(preds.is_empty(), "attribute predicates are explicitly out of scope: {preds:?}");
}
