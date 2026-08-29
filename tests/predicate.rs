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

#[test]
fn plain_loop_element_synthesizes_a_one_element_list() {
    let src = "def f(commands):\n    for direction in commands:\n        if direction == \"N\":\n            pass\n";
    let preds = predicates_at(src, "f", &["commands"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "commands");
            assert_eq!(*deriv, predicate::Derivation::Element { field: None, arity: 1, leading: 0 });
        }
        other => panic!("expected a Compare over the loop element, got {other:?}"),
    }
    let shape = Shape::any_seq();
    let satisfy = predicate::synthesize(&preds[0], true, &shape).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &shape).expect("violating value");
    let satisfy_items = satisfy.as_array().expect("array");
    let violate_items = violate.as_array().expect("array");
    assert_eq!(satisfy_items.len(), 1, "must be non-empty so the for body executes: {satisfy_items:?}");
    assert_eq!(satisfy_items[0], "N");
    assert_eq!(violate_items.len(), 1, "must be non-empty so the for body executes: {violate_items:?}");
    assert_ne!(violate_items[0], "N");
}

#[test]
fn tuple_unpacked_for_target_synthesizes_a_matching_tagged_tuple() {
    let src = "def f(pairs):\n    for action, amount in pairs:\n        if action == \"withdraw\":\n            pass\n";
    let preds = predicates_at(src, "f", &["pairs"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "pairs");
            assert_eq!(*deriv, predicate::Derivation::Element { field: Some(0), arity: 2, leading: 0 });
        }
        other => panic!("expected a Compare over field 0 of the tuple element, got {other:?}"),
    }
    let shape = Shape::any_seq();
    let satisfy = predicate::synthesize(&preds[0], true, &shape).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &shape).expect("violating value");
    let satisfy_items = satisfy.as_array().expect("array");
    assert_eq!(satisfy_items.len(), 1);
    let tuple = satisfy_items[0].as_object().expect("tagged tuple");
    assert_eq!(tuple["__t__"], "tuple");
    let fields = tuple["items"].as_array().expect("items");
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0], "withdraw");

    let violate_items = violate.as_array().expect("array");
    assert_eq!(violate_items.len(), 1, "must be non-empty so the for body executes: {violate_items:?}");
    let violate_tuple = violate_items[0].as_object().expect("tagged tuple");
    let violate_fields = violate_tuple["items"].as_array().expect("items");
    assert_ne!(violate_fields[0], "withdraw");
}

#[test]
fn assign_unpack_of_a_whole_loop_element_binds_each_field() {
    let src = "def f(transactions):\n    for transaction in transactions:\n        action, amount = transaction\n        if action == \"withdraw\":\n            pass\n";
    let preds = predicates_at(src, "f", &["transactions"], 4);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "transactions");
            assert_eq!(*deriv, predicate::Derivation::Element { field: Some(0), arity: 2, leading: 0 });
        }
        other => panic!("expected a Compare over field 0 of the unpacked element, got {other:?}"),
    }
    let shape = Shape::any_seq();
    let satisfy = predicate::synthesize(&preds[0], true, &shape).expect("satisfying value");
    let items = satisfy.as_array().expect("array");
    assert_eq!(items.len(), 1);
    let tuple = items[0].as_object().expect("tagged tuple");
    assert_eq!(tuple["items"].as_array().expect("items")[0], "withdraw");
}

#[test]
fn loop_element_vs_another_parameter_stays_unhandled() {
    let src = "def f(pairs, other):\n    for a, b in pairs:\n        if a == other:\n            pass\n";
    let preds = predicates_at(src, "f", &["pairs", "other"], 3);
    assert_eq!(preds.len(), 1);
    let Predicate::ParamCompare { deriv_a, op, deriv_b, .. } = &preds[0] else {
        panic!("expected a ParamCompare, got {:?}", preds[0]);
    };
    let pair = predicate::synthesize_pair(deriv_a, *op, deriv_b, &Shape::any_seq(), &Shape::Str, true);
    assert!(pair.is_none(), "pairing a loop element is pinned unhandled, got {pair:?}");
}

#[test]
fn post_loop_reference_to_the_target_name_is_not_an_element() {
    let src = "def f(xs):\n    for x in xs:\n        pass\n    if x == \"N\":\n        pass\n";
    let preds = predicates_at(src, "f", &["xs"], 4);
    assert!(preds.is_empty(), "the loop target must not resolve past the loop body: {preds:?}");
}

#[test]
fn split_len_compare_synthesizes_a_joined_string_with_the_right_part_count() {
    let src = "def f(ip):\n    parts = ip.split('.')\n    if len(parts) != 4:\n        pass\n";
    let preds = predicates_at(src, "f", &["ip"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "ip");
            assert_eq!(*deriv, predicate::Derivation::SplitLen(Some(".".to_string())));
        }
        other => panic!("expected a Compare over len(parts), got {other:?}"),
    }
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str).expect("violating value");
    let satisfy_count = satisfy.as_str().expect("str").split('.').count();
    let violate_count = violate.as_str().expect("str").split('.').count();
    assert_ne!(satisfy_count, 4, "true outcome means len(parts) != 4");
    assert_eq!(violate_count, 4, "false outcome means len(parts) == 4");
}

#[test]
fn for_iter_over_a_split_local_synthesizes_a_joined_string() {
    let src = "def f(ip):\n    parts = ip.split('.')\n    for part in parts:\n        pass\n";
    let preds = predicates_at(src, "f", &["ip"], 3);
    assert_eq!(preds.len(), 1);
    let Predicate::ForIter { param, deriv } = &preds[0] else {
        panic!("expected a ForIter, got {:?}", preds[0]);
    };
    assert_eq!(param, "ip");
    assert_eq!(*deriv, predicate::Derivation::Split(Some(".".to_string())));
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying (nonempty) value");
    assert!(satisfy.as_str().expect("str").split('.').count() >= 1);
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str);
    assert!(violate.is_none(), "an explicit separator's split() never yields zero parts");
}

#[test]
fn strmethod_on_a_split_element_synthesizes_a_single_joined_part() {
    let src = "def f(ip):\n    parts = ip.split('.')\n    for part in parts:\n        if part.isdigit():\n            pass\n";
    let preds = predicates_at(src, "f", &["ip"], 4);
    assert_eq!(preds.len(), 1);
    let Predicate::StrMethod { param, deriv, .. } = &preds[0] else {
        panic!("expected a StrMethod, got {:?}", preds[0]);
    };
    assert_eq!(param, "ip");
    assert_eq!(*deriv, predicate::Derivation::SplitElement(Some(".".to_string())));
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str).expect("violating value");
    let satisfy_parts: Vec<&str> = satisfy.as_str().expect("str").split('.').collect();
    let violate_parts: Vec<&str> = violate.as_str().expect("str").split('.').collect();
    assert_eq!(satisfy_parts.len(), 1, "must split back into one part carrying the digit string");
    assert!(satisfy_parts[0].chars().all(|c| c.is_ascii_digit()));
    assert_eq!(violate_parts.len(), 1, "must split back into one part carrying the non-digit string");
    assert!(!violate_parts[0].chars().all(|c| c.is_ascii_digit()));
}

#[test]
fn split_element_len_compare_synthesizes_a_single_part_of_the_right_length() {
    let src = "def f(ip):\n    parts = ip.split('.')\n    for part in parts:\n        if len(part) > 1:\n            pass\n";
    let preds = predicates_at(src, "f", &["ip"], 4);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "ip");
            assert_eq!(*deriv, predicate::Derivation::SplitElementLen(Some(".".to_string())));
        }
        other => panic!("expected a Compare over len(part), got {other:?}"),
    }
    let satisfy = predicate::synthesize(&preds[0], true, &Shape::Str).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &Shape::Str).expect("violating value");
    let satisfy_parts: Vec<&str> = satisfy.as_str().expect("str").split('.').collect();
    let violate_parts: Vec<&str> = violate.as_str().expect("str").split('.').collect();
    assert_eq!(satisfy_parts.len(), 1);
    assert!(satisfy_parts[0].len() > 1, "true outcome means len(part) > 1");
    assert_eq!(violate_parts.len(), 1);
    assert!(violate_parts[0].len() <= 1, "false outcome means len(part) <= 1");
}

#[test]
fn indexing_a_split_element_stays_unhandled() {
    let src = "def f(ip):\n    parts = ip.split('.')\n    for part in parts:\n        if part[0] == '0':\n            pass\n";
    let preds = predicates_at(src, "f", &["ip"], 4);
    assert!(preds.is_empty(), "part[0] is a third-level derivation (param -> split -> element -> index), out of scope: {preds:?}");
}

#[test]
fn loop_state_flag_true_outcome_synthesizes_a_singleton_that_never_flips_it() {
    let src = "def f(xs):\n    is_asc = True\n    for x in xs:\n        if x < 0:\n            is_asc = False\n    if is_asc:\n        pass\n";
    let preds = predicates_at(src, "f", &["xs"], 6);
    assert_eq!(preds.len(), 1);
    assert!(matches!(preds[0], Predicate::Not(_)));
    let shape = Shape::any_seq();
    let satisfy = predicate::synthesize(&preds[0], true, &shape).expect("satisfying value");
    let items = satisfy.as_array().expect("array");
    assert_eq!(items.len(), 1, "must be non-empty so the loop actually runs: {items:?}");
    assert!(items[0].as_i64().expect("int") >= 0, "the element must not satisfy x < 0: {items:?}");
}

#[test]
fn loop_state_flag_false_outcome_synthesizes_a_singleton_that_flips_it() {
    let src = "def f(xs):\n    is_asc = True\n    for x in xs:\n        if x < 0:\n            is_asc = False\n    if is_asc:\n        pass\n";
    let preds = predicates_at(src, "f", &["xs"], 6);
    assert_eq!(preds.len(), 1);
    let shape = Shape::any_seq();
    let violate = predicate::synthesize(&preds[0], false, &shape).expect("violating value");
    let items = violate.as_array().expect("array");
    assert_eq!(items.len(), 1, "must be non-empty so the loop actually runs: {items:?}");
    assert!(items[0].as_i64().expect("int") < 0, "the element must satisfy x < 0: {items:?}");
}

#[test]
fn loop_state_flag_with_a_second_reassignment_stays_unhandled() {
    let src = "def f(xs):\n    is_asc = True\n    for x in xs:\n        if x < 0:\n            is_asc = False\n        if x == 0:\n            is_asc = False\n    if is_asc:\n        pass\n";
    let preds = predicates_at(src, "f", &["xs"], 8);
    assert!(preds.is_empty(), "more than one reassignment of the flag is out of scope: {preds:?}");
}

#[test]
fn rebind_with_an_unrecognized_rhs_clears_the_prior_alias() {
    let src = "def f(s):\n    n = len(s)\n    n = hash(s)\n    if n > 3:\n        pass\n";
    let preds = predicates_at(src, "f", &["s"], 4);
    assert!(preds.is_empty(), "the rebind to hash(s) must clear the stale `n = len(s)` alias: {preds:?}");
}

#[test]
fn slice_lower_bound_puts_the_element_inside_the_sliced_region() {
    let src = "def f(commands):\n    for command in commands[1:]:\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(src, "f", &["commands"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "commands");
            assert_eq!(*deriv, predicate::Derivation::Element { field: None, arity: 1, leading: 1 });
        }
        other => panic!("expected a Compare over the sliced loop element, got {other:?}"),
    }
    let shape = Shape::any_seq();
    let satisfy = predicate::synthesize(&preds[0], true, &shape).expect("satisfying value");
    let violate = predicate::synthesize(&preds[0], false, &shape).expect("violating value");
    let satisfy_items = satisfy.as_array().expect("array");
    assert_eq!(satisfy_items.len(), 2, "one filler element ahead of the target: {satisfy_items:?}");
    assert_eq!(satisfy_items[1], "N");
    let violate_items = violate.as_array().expect("array");
    assert_eq!(violate_items.len(), 2);
    assert_ne!(violate_items[1], "N");
}

#[test]
fn slice_upper_bound_synthesizes_a_single_leading_element() {
    let src = "def f(commands):\n    for command in commands[:3]:\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(src, "f", &["commands"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "commands");
            assert_eq!(*deriv, predicate::Derivation::Element { field: None, arity: 1, leading: 0 });
        }
        other => panic!("expected a Compare over the sliced loop element, got {other:?}"),
    }
}

#[test]
fn slice_both_bounds_synthesizes_the_start_offset_element() {
    let src = "def f(commands):\n    for command in commands[1:3]:\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(src, "f", &["commands"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "commands");
            assert_eq!(*deriv, predicate::Derivation::Element { field: None, arity: 1, leading: 1 });
        }
        other => panic!("expected a Compare over the sliced loop element, got {other:?}"),
    }
}

#[test]
fn slice_with_negative_or_empty_bounds_stays_unhandled() {
    let negative = "def f(commands):\n    for command in commands[-1:]:\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(negative, "f", &["commands"], 3);
    assert!(preds.is_empty(), "a negative slice bound is refused: {preds:?}");

    let empty = "def f(commands):\n    for command in commands[3:1]:\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(empty, "f", &["commands"], 3);
    assert!(preds.is_empty(), "start >= end can never iterate, so the target is refused: {preds:?}");

    let stepped = "def f(commands):\n    for command in commands[1::2]:\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(stepped, "f", &["commands"], 3);
    assert!(preds.is_empty(), "a step is refused: {preds:?}");
}

#[test]
fn enumerate_binds_the_element_name_not_the_index() {
    let src = "def f(commands):\n    for i, command in enumerate(commands):\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(src, "f", &["commands"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "commands");
            assert_eq!(*deriv, predicate::Derivation::Element { field: None, arity: 1, leading: 0 });
        }
        other => panic!("expected a Compare over the enumerate element, got {other:?}"),
    }
}

#[test]
fn enumerate_with_a_literal_start_still_binds_the_element() {
    let src = "def f(commands):\n    for i, command in enumerate(commands, 1):\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(src, "f", &["commands"], 3);
    assert_eq!(preds.len(), 1);
    assert!(matches!(&preds[0], Predicate::Compare { deriv: predicate::Derivation::Element { .. }, .. }));
}

#[test]
fn reversed_binds_the_target_as_a_plain_element() {
    let src = "def f(commands):\n    for command in reversed(commands):\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(src, "f", &["commands"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "commands");
            assert_eq!(*deriv, predicate::Derivation::Element { field: None, arity: 1, leading: 0 });
        }
        other => panic!("expected a Compare over the reversed element, got {other:?}"),
    }
}

#[test]
fn sorted_binds_the_target_as_a_plain_element() {
    let src = "def f(commands):\n    for command in sorted(commands):\n        if command == 'N':\n            pass\n";
    let preds = predicates_at(src, "f", &["commands"], 3);
    assert_eq!(preds.len(), 1);
    match &preds[0] {
        Predicate::Compare { param, deriv, .. } => {
            assert_eq!(param, "commands");
            assert_eq!(*deriv, predicate::Derivation::Element { field: None, arity: 1, leading: 0 });
        }
        other => panic!("expected a Compare over the sorted element, got {other:?}"),
    }
}

#[test]
fn zip_of_two_parameters_stays_unhandled() {
    let src = "def f(xs, ys):\n    for x, y in zip(xs, ys):\n        if x == 'N':\n            pass\n";
    let preds = predicates_at(src, "f", &["xs", "ys"], 3);
    assert!(preds.is_empty(), "zip is explicitly out of scope: {preds:?}");
}
