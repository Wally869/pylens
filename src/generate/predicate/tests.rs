//! Extraction/synthesis round-trip checks: for each handled predicate form, extract it from a
//! parsed test expression and confirm synthesize produces a value that satisfies it and one
//! that violates it.

use ruff_python_ast as ast;
use serde_json::{Value, json};

use crate::model::Shape;

use super::{Aliases, FlagPreds, Predicate};
use super::extraction::extract;
use super::synthesis::synthesize;

fn test_expr_of(src: &str) -> ast::Expr {
    let parsed = crate::parse::parse_source(src).expect("parse");
    let module = parsed.syntax();
    match &module.body[0] {
        ast::Stmt::If(if_stmt) => *if_stmt.test.clone(),
        ast::Stmt::While(w) => *w.test.clone(),
        other => panic!("expected an If/While statement, got {other:?}"),
    }
}

fn params(names: &[&str]) -> Vec<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn no_aliases() -> Aliases {
    Aliases::new()
}

fn no_flags() -> FlagPreds {
    FlagPreds::new()
}

#[test]
fn eq_int_yields_the_literal_and_a_violator() {
    let test = test_expr_of("if p == 42:\n    pass\n");
    let preds = extract(&test, &params(&["p"]), &no_aliases(), &no_flags());
    assert_eq!(preds.len(), 1);
    let satisfy = synthesize(&preds[0], true, &Shape::Int).expect("satisfying value");
    let violate = synthesize(&preds[0], false, &Shape::Int).expect("violating value");
    assert_eq!(satisfy, json!(42));
    assert_ne!(violate, json!(42));
}

#[test]
fn len_gt_yields_a_long_and_a_short_list() {
    let test = test_expr_of("if len(p) > 3:\n    pass\n");
    let preds = extract(&test, &params(&["p"]), &no_aliases(), &no_flags());
    assert_eq!(preds.len(), 1);
    let shape = Shape::any_seq();
    let satisfy = synthesize(&preds[0], true, &shape).expect("satisfying value");
    let violate = synthesize(&preds[0], false, &shape).expect("violating value");
    let Value::Array(long) = satisfy else { panic!("expected an array") };
    let Value::Array(short) = violate else { panic!("expected an array") };
    assert!(long.len() > 3, "expected more than 3 elements, got {}", long.len());
    assert!(short.len() <= 3, "expected at most 3 elements, got {}", short.len());
}

#[test]
fn mod_eq_yields_even_and_odd() {
    let test = test_expr_of("if p % 2 == 0:\n    pass\n");
    let preds = extract(&test, &params(&["p"]), &no_aliases(), &no_flags());
    assert_eq!(preds.len(), 1);
    let satisfy = synthesize(&preds[0], true, &Shape::Int).expect("satisfying value");
    let violate = synthesize(&preds[0], false, &Shape::Int).expect("violating value");
    assert_eq!(satisfy.as_i64().expect("int") % 2, 0);
    assert_ne!(violate.as_i64().expect("int") % 2, 0);
}

#[test]
fn bare_name_test_yields_truthiness() {
    let test = test_expr_of("if p:\n    pass\n");
    let preds = extract(&test, &params(&["p"]), &no_aliases(), &no_flags());
    assert_eq!(preds, vec![Predicate::Truthy { param: "p".to_string() }]);
    let truthy = synthesize(&preds[0], true, &Shape::Int).expect("truthy value");
    let falsy = synthesize(&preds[0], false, &Shape::Int).expect("falsy value");
    assert_ne!(truthy, json!(0));
    assert_eq!(falsy, json!(0));
}

#[test]
fn and_decomposes_into_its_operands() {
    let test = test_expr_of("if p == 1 and q == 2:\n    pass\n");
    let preds = extract(&test, &params(&["p", "q"]), &no_aliases(), &no_flags());
    assert_eq!(preds.len(), 2);
    assert!(preds.iter().any(|p| p.param() == "p"));
    assert!(preds.iter().any(|p| p.param() == "q"));
}

#[test]
fn unhandled_predicate_yields_nothing() {
    let test = test_expr_of("if hash(p) == 0:\n    pass\n");
    let preds = extract(&test, &params(&["p"]), &no_aliases(), &no_flags());
    assert!(preds.is_empty(), "hash(p) is not a handled derivation");
}
