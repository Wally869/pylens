//! Branch accounting: the static enumeration (pure, always runs) and the dynamic per-outcome
//! coverage built on top of it (sandbox-gated — skips gracefully when the sandbox isn't
//! provisioned, same as `tests/record.rs`).

use pylens::analyze_source;
use pylens::exec::{Sandbox, probe};
use pylens::model::branch::{BranchKind, BranchPoint, OutcomeEvidence};
use pylens::model::EffectSignature;
use pylens::record::{BranchReport, FunctionRecord, RecordFlags, ReplayMap, record_file};
use serde_json::json;

fn ready(test: &str) -> bool {
    match probe() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("SKIP {test}: {e}");
            false
        }
    }
}

fn sig<'a>(sigs: &'a [EffectSignature], name: &str) -> &'a EffectSignature {
    sigs.iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no signature named {name}"))
}

fn branch_at(points: &[BranchPoint], kind: BranchKind, line: u32) -> &BranchPoint {
    points
        .iter()
        .find(|b| b.kind == kind && b.line == line)
        .unwrap_or_else(|| panic!("no {kind:?} branch point at line {line}: {points:?}"))
}

fn outcome<'a>(point: &'a BranchPoint, name: &str) -> &'a OutcomeEvidence {
    &point
        .outcomes
        .iter()
        .find(|o| o.outcome == name)
        .unwrap_or_else(|| panic!("no outcome {name:?} on {point:?}"))
        .evidence
}

// ---------------------------------------------------------------------------------------------
// Pure: the static enumeration itself.
// ---------------------------------------------------------------------------------------------

#[test]
fn enumerates_if_elif_else_with_true_false_arcs() {
    let sigs = analyze_source(
        "def f(x):\n\
         \x20   if x > 0:\n\
         \x20       return \"pos\"\n\
         \x20   elif x < 0:\n\
         \x20       return \"neg\"\n\
         \x20   else:\n\
         \x20       return \"zero\"\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;

    let top = branch_at(points, BranchKind::If, 2);
    assert_eq!(*outcome(top, "true"), OutcomeEvidence::Arc(2, 3));
    // No parenthetical else on this level — the false path lands on the elif's own test line,
    // which CPython does trace.
    assert_eq!(*outcome(top, "false"), OutcomeEvidence::Arc(2, 4));

    let elif = branch_at(points, BranchKind::If, 4);
    assert_eq!(*outcome(elif, "true"), OutcomeEvidence::Arc(4, 5));
    // A bare `else:` has no condition and traces no line of its own — the false arc lands
    // directly on the else body's first line.
    assert_eq!(*outcome(elif, "false"), OutcomeEvidence::Arc(4, 7));
}

#[test]
fn enumerates_else_less_if_false_arc_to_fallthrough() {
    let sigs = analyze_source(
        "def f(x):\n\
         \x20   if x > 0:\n\
         \x20       y = 1\n\
         \x20   return x\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let bp = branch_at(points, BranchKind::If, 2);
    assert_eq!(*outcome(bp, "true"), OutcomeEvidence::Arc(2, 3));
    assert_eq!(*outcome(bp, "false"), OutcomeEvidence::Arc(2, 4));
}

#[test]
fn enumerates_while_enter_and_skip() {
    let sigs = analyze_source(
        "def f(n):\n\
         \x20   while n > 0:\n\
         \x20       n -= 1\n\
         \x20   return n\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let bp = branch_at(points, BranchKind::While, 2);
    assert_eq!(*outcome(bp, "enter"), OutcomeEvidence::Arc(2, 3));
    assert_eq!(*outcome(bp, "skip"), OutcomeEvidence::Arc(2, 4));
}

#[test]
fn enumerates_for_iterate_and_empty() {
    let sigs = analyze_source(
        "def f(xs):\n\
         \x20   for x in xs:\n\
         \x20       pass\n\
         \x20   return xs\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let bp = branch_at(points, BranchKind::For, 2);
    assert_eq!(*outcome(bp, "iterate"), OutcomeEvidence::Arc(2, 3));
    assert_eq!(*outcome(bp, "empty"), OutcomeEvidence::Arc(2, 4));
}

#[test]
fn enumerates_except_arm_entered_on_its_own_line() {
    let sigs = analyze_source(
        "def f(x):\n\
         \x20   try:\n\
         \x20       return 1 / x\n\
         \x20   except ZeroDivisionError:\n\
         \x20       return 0\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let bp = branch_at(points, BranchKind::Except, 4);
    assert_eq!(*outcome(bp, "entered"), OutcomeEvidence::Line(4));
}

#[test]
fn enumerates_inline_if_and_ternary_and_boolop_as_fine_grained() {
    let sigs = analyze_source(
        "def f(x, y):\n\
         \x20   if x: return 1\n\
         \x20   z = \"a\" if x else \"b\"\n\
         \x20   w = x and y\n\
         \x20   return z, w\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;

    let inline = branch_at(points, BranchKind::InlineIf, 2);
    assert_eq!(*outcome(inline, "true"), OutcomeEvidence::FineGrained(2, 0));
    assert_eq!(*outcome(inline, "false"), OutcomeEvidence::FineGrained(2, 0));

    let ternary = branch_at(points, BranchKind::Ternary, 3);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::FineGrained(3, 0));
    assert_eq!(*outcome(ternary, "false"), OutcomeEvidence::FineGrained(3, 0));

    let boolop = branch_at(points, BranchKind::BoolOp, 4);
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::FineGrained(4, 0));
    assert_eq!(*outcome(boolop, "full_evaluation"), OutcomeEvidence::FineGrained(4, 0));
}

#[test]
fn enumerates_comprehension_if_as_fine_grained() {
    let sigs = analyze_source(
        "def f(xs):\n\
         \x20   return [x for x in xs if x > 0]\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let bp = branch_at(points, BranchKind::ComprehensionIf, 2);
    assert_eq!(*outcome(bp, "true"), OutcomeEvidence::FineGrained(2, 0));
    assert_eq!(*outcome(bp, "false"), OutcomeEvidence::FineGrained(2, 0));
}

#[test]
fn same_line_ordinal_disambiguates_two_ternaries_on_one_line() {
    let sigs = analyze_source(
        "def f(a, b, c, d):\n\
         \x20   return (a if b else c), (c if d else a)\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternaries: Vec<&BranchPoint> = points
        .iter()
        .filter(|b| b.kind == BranchKind::Ternary && b.line == 2)
        .collect();
    assert_eq!(ternaries.len(), 2, "expected two ternaries on line 2: {points:?}");
    assert_eq!(*outcome(ternaries[0], "true"), OutcomeEvidence::FineGrained(2, 0));
    assert_eq!(*outcome(ternaries[1], "true"), OutcomeEvidence::FineGrained(2, 1));
}

#[test]
fn compound_ternary_test_stays_unobservable_not_fine_grained() {
    // `a and b` compiles to a CHAIN of test jumps, not the single instruction a simple test
    // does — attaching the probe to the first jump would only prove `a`'s truthiness, not the
    // whole construct's outcome, so this must stay conservatively `Unobservable`.
    let sigs = analyze_source("def f(a, b):\n    return 1 if a and b else 2\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(ternary, "false"), OutcomeEvidence::Unobservable);
    // The nested `and`'s own boolop branch point is folded into the ternary's jump chain too —
    // also `Unobservable`, never resolved against the wrong (or a nonexistent) instruction.
    let boolop = branch_at(points, BranchKind::BoolOp, 2);
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(boolop, "full_evaluation"), OutcomeEvidence::Unobservable);
}

#[test]
fn compound_or_ternary_test_stays_unobservable_not_fine_grained() {
    // `or` mixes `POP_JUMP_IF_TRUE` (short-circuit success) and `POP_JUMP_IF_FALSE` (failure) in
    // the same chain — still not a single instruction, so still conservatively `Unobservable`.
    let sigs = analyze_source("def f(a, b):\n    return 1 if a or b else 2\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(ternary, "false"), OutcomeEvidence::Unobservable);
}

#[test]
fn ternary_nested_in_ternary_test_and_the_outer_are_both_unobservable() {
    // The `probe_fine2` repro the parent review found: the OUTER ternary's compound test still
    // contributes `POP_JUMP` instructions to this shared line that would otherwise shift the
    // INNER ternary's ordinal in the worker's flat, offset-sorted instruction group — an
    // index-shift risk, not just the outer's own resolvability.
    let sigs = analyze_source("def f(a, b, c):\n    return (1 if a else 2) if b and c else 3\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternaries: Vec<&BranchPoint> =
        points.iter().filter(|b| b.kind == BranchKind::Ternary && b.line == 2).collect();
    assert_eq!(ternaries.len(), 2, "expected inner and outer ternaries on line 2: {points:?}");
    for t in &ternaries {
        assert_eq!(*outcome(t, "true"), OutcomeEvidence::Unobservable);
        assert_eq!(*outcome(t, "false"), OutcomeEvidence::Unobservable);
    }
    let boolop = branch_at(points, BranchKind::BoolOp, 2);
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::Unobservable);
}

#[test]
fn boolop_with_nested_ternary_value_pollutes_the_shared_line() {
    // The mirrored form: `if b and (1 if a else 2): pass` — the boolop test contains a nested
    // ternary (used as a VALUE inside the boolop, not itself compound) — both the `if`'s own
    // same-line outcome and the nested ternary must stay `Unobservable`.
    let sigs = analyze_source("def f(a, b):\n    if b and (1 if a else 2): pass\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let inline = branch_at(points, BranchKind::InlineIf, 2);
    assert_eq!(*outcome(inline, "true"), OutcomeEvidence::Unobservable);
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::Unobservable);
}

#[test]
fn while_test_containing_a_ternary_demotes_the_ternary() {
    // `while (x if a else y):` — CPython's loop rotation duplicates the ternary's own test AND
    // the while's own "is the produced value truthy" decision across each of the ternary's
    // branches, contributing several extra `POP_JUMP_IF_*` instructions to this one line. The
    // while's own enter/skip outcome was never `FineGrained` anyway (multi-line, `Arc`-based —
    // see `two_way`'s `resolvable_same_line` for `while`); this test is about the NESTED ternary,
    // which must not be resolved against one of those duplicated instructions.
    let sigs = analyze_source(
        "def f(a, n, x, y):\n\
         \x20   while (x if a else y):\n\
         \x20       n -= 1\n\
         \x20   return n\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(ternary, "false"), OutcomeEvidence::Unobservable);
    // The while's own multi-line enter/skip outcome is untouched (`Arc`-based, never at risk).
    let while_bp = branch_at(points, BranchKind::While, 2);
    assert!(matches!(*outcome(while_bp, "enter"), OutcomeEvidence::Arc(..)));
}

#[test]
fn if_test_containing_a_ternary_demotes_both_the_inline_if_and_the_ternary() {
    // `if (x if a else y) > 0: return 1` — same-line, so the `if`'s own outcome WOULD have been
    // `InlineIf`/`FineGrained` (its own test is a plain `Compare`, not itself a boolop) if not for
    // the nested ternary inside that test: CPython evaluates the ternary's own decision BEFORE
    // the `if`'s own comparison-based decision, so a naive per-pass ordinal assignment (the
    // statement walk runs before the expression pass) would swap which physical instruction each
    // one's ordinal actually resolves against.
    let sigs =
        analyze_source("def f(a, x, y):\n    if (x if a else y) > 0: return 1\n    return 2\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let inline = branch_at(points, BranchKind::InlineIf, 2);
    assert_eq!(*outcome(inline, "true"), OutcomeEvidence::Unobservable);
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::Unobservable);
}

#[test]
fn test_position_boolop_stays_unobservable_not_fine_grained() {
    // `if a and b:` compiles the boolop into the `if`'s OWN jump chain (plain `POP_JUMP_IF_*`,
    // never `JUMP_IF_*_OR_POP`) — the `if`'s true/false outcome is still a normal multi-line Arc
    // (unaffected), but the boolop's own short_circuit/full_evaluation branch point has no
    // instruction shape this pass knows how to resolve, so it must stay `Unobservable` rather
    // than a permanently `uncovered` outcome the cover loop can never satisfy.
    let sigs = analyze_source(
        "def f(a, b):\n\
         \x20   if a and b:\n\
         \x20       return 1\n\
         \x20   return 2\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let if_bp = branch_at(points, BranchKind::If, 2);
    assert_eq!(*outcome(if_bp, "true"), OutcomeEvidence::Arc(2, 3));
    assert_eq!(*outcome(if_bp, "false"), OutcomeEvidence::Arc(2, 4));
    let boolop = branch_at(points, BranchKind::BoolOp, 2);
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(boolop, "full_evaluation"), OutcomeEvidence::Unobservable);
}

#[test]
fn mixed_ternary_and_boolop_on_one_line_get_independent_per_category_ordinals() {
    // Two DIFFERENT categories on one line (a simple-test ternary and a value-position boolop)
    // must each start counting from ordinal 0 — a shared, line-global counter would give the
    // boolop ordinal 1, which `python/worker.py`'s per-category instruction groups (`test_groups`
    // vs `chain_groups`) would then fail to resolve (only one boolop chain exists on the line).
    let sigs = analyze_source("def f(a, b, c):\n    return (a if b else c), (b and c)\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::FineGrained(2, 0));
    let boolop = branch_at(points, BranchKind::BoolOp, 2);
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::FineGrained(2, 0));
}

#[test]
fn nested_def_branches_are_not_hoisted_into_the_enclosing_function() {
    // pylens only analyzes top-level defs/methods; a nested `def`'s body (including its own
    // `if`) must never be attributed to the enclosing function's branch points.
    let sigs = analyze_source(
        "def outer(x):\n\
         \x20   def inner(y):\n\
         \x20       if y:\n\
         \x20           return 1\n\
         \x20       return 0\n\
         \x20   return inner(x)\n",
    )
    .expect("parse");
    let outer = sig(&sigs, "outer");
    assert!(
        outer.branch_points.is_empty(),
        "the nested function's `if` must not appear on the enclosing function: {:?}",
        outer.branch_points
    );
}

// ---------------------------------------------------------------------------------------------
// Sandbox-gated: the dynamic per-outcome accounting built on top of the enumeration.
// ---------------------------------------------------------------------------------------------

fn find_function<'a>(functions: &'a [FunctionRecord], name: &str) -> &'a FunctionRecord {
    functions
        .iter()
        .find(|f| f.signature.name == name)
        .unwrap_or_else(|| panic!("no function record named {name}"))
}

fn branch_report(branches: &[BranchReport], kind: BranchKind, line: u32) -> &BranchReport {
    branches
        .iter()
        .find(|b| b.kind == kind && b.line == line)
        .unwrap_or_else(|| panic!("no {kind:?} branch report at line {line}: {branches:?}"))
}

fn status_of<'a>(report: &'a BranchReport, outcome: &str) -> &'a str {
    &report
        .outcomes
        .iter()
        .find(|o| o.outcome == outcome)
        .unwrap_or_else(|| panic!("no outcome {outcome:?} on {report:?}"))
        .status
}

fn reason_of<'a>(report: &'a BranchReport, outcome: &str) -> Option<&'a str> {
    report
        .outcomes
        .iter()
        .find(|o| o.outcome == outcome)
        .unwrap_or_else(|| panic!("no outcome {outcome:?} on {report:?}"))
        .reason
        .as_deref()
}

#[test]
fn else_less_if_false_path_is_covered_via_its_arc() {
    if !ready("else_less_if_false_path_is_covered_via_its_arc") {
        return;
    }
    let src = "def f(x):\n    if x > 0:\n        y = 1\n    return x\n";

    // Confirm the raw arc first, directly against the sandbox: calling with a negative x must
    // trace the (2, 4) transition — straight from the test line to the fall-through line,
    // skipping the body's line 3 entirely.
    let sandbox = pylens::exec::Nsjail::new();
    let result = sandbox.call(src, "f", &[json!(-1)], &[], &[]).expect("call");
    assert!(
        result.arcs.contains(&(2, 4)),
        "expected the false-path arc (2, 4): {:?}",
        result.arcs
    );
    assert!(
        !result.arcs.contains(&(2, 3)),
        "the true-path arc (2, 3) must not appear when x is negative: {:?}",
        result.arcs
    );

    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!(-1)]]);
    let rec = record_file(src, 1, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::If, 2);
    assert_eq!(status_of(bp, "false"), "covered");
}

#[test]
fn for_loop_iterate_and_empty_are_both_covered() {
    if !ready("for_loop_iterate_and_empty_are_both_covered") {
        return;
    }
    let src = "def f(xs):\n    for x in xs:\n        pass\n    return xs\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!([])], vec![json!([1])]]);
    let rec = record_file(src, 1, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::For, 2);
    assert_eq!(status_of(bp, "iterate"), "covered");
    assert_eq!(status_of(bp, "empty"), "covered");
}

#[test]
fn while_loop_skip_is_covered() {
    if !ready("while_loop_skip_is_covered") {
        return;
    }
    let src = "def f(n):\n    while n > 0:\n        n -= 1\n    return n\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!(0)], vec![json!(2)]]);
    let rec = record_file(src, 1, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::While, 2);
    assert_eq!(status_of(bp, "skip"), "covered");
    assert_eq!(status_of(bp, "enter"), "covered");
}

#[test]
fn ternary_both_outcomes_are_observable_via_opcode_tracing() {
    if !ready("ternary_both_outcomes_are_observable_via_opcode_tracing") {
        return;
    }
    let src = "def f(x):\n    return \"a\" if x else \"b\"\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!(true)], vec![json!(false)]]);
    let rec = record_file(src, 1, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::Ternary, 2);
    assert_eq!(status_of(bp, "true"), "covered");
    assert_eq!(status_of(bp, "false"), "covered");
}

#[test]
fn boolop_both_outcomes_are_observable_via_opcode_tracing() {
    if !ready("boolop_both_outcomes_are_observable_via_opcode_tracing") {
        return;
    }
    let src = "def f(x, y):\n    return x and y\n";
    let mut replay = ReplayMap::new();
    replay.insert(
        "f".to_string(),
        vec![vec![json!(false), json!(true)], vec![json!(true), json!(true)]],
    );
    let rec = record_file(src, 1, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::BoolOp, 2);
    assert_eq!(status_of(bp, "short_circuit"), "covered");
    assert_eq!(status_of(bp, "full_evaluation"), "covered");
}

#[test]
fn inline_if_both_outcomes_are_observable_via_opcode_tracing() {
    if !ready("inline_if_both_outcomes_are_observable_via_opcode_tracing") {
        return;
    }
    let src = "def f(x):\n    if x: return 1\n    return 0\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!(true)], vec![json!(false)]]);
    let rec = record_file(src, 1, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::InlineIf, 2);
    assert_eq!(status_of(bp, "true"), "covered");
    assert_eq!(status_of(bp, "false"), "covered");
}

#[test]
fn compound_ternary_test_replaying_only_the_else_branch_never_falsely_covers_true() {
    // Regression for a defect the parent review caught: `1 if a and b else 2` replayed with
    // (1, 0) takes the ELSE branch (`a and b` is falsy). The old design attached the probe to
    // the FIRST test jump (`a`'s own `POP_JUMP_IF_FALSE`), which falls through when `a` alone is
    // truthy — proving nothing about the whole construct — and wrongly reported "true" covered.
    if !ready("compound_ternary_test_replaying_only_the_else_branch_never_falsely_covers_true") {
        return;
    }
    let src = "def f(a, b):\n    return 1 if a and b else 2\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!(1), json!(0)]]);
    let rec = record_file(src, 0, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::Ternary, 2);
    assert_ne!(status_of(bp, "true"), "covered", "no observation proves the true branch ran");
    assert_eq!(status_of(bp, "true"), "unobservable_line_granularity");
    assert_eq!(status_of(bp, "false"), "unobservable_line_granularity");
}

#[test]
fn compound_or_ternary_test_is_never_falsely_covered() {
    if !ready("compound_or_ternary_test_is_never_falsely_covered") {
        return;
    }
    let src = "def f(a, b):\n    return 1 if a or b else 2\n";
    let mut replay = ReplayMap::new();
    replay.insert(
        "f".to_string(),
        vec![vec![json!(true), json!(false)], vec![json!(false), json!(false)]],
    );
    let rec = record_file(src, 0, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::Ternary, 2);
    assert_eq!(status_of(bp, "true"), "unobservable_line_granularity");
    assert_eq!(status_of(bp, "false"), "unobservable_line_granularity");
}

#[test]
fn test_position_boolop_is_never_permanently_uncovered() {
    // Before the fix this was reported "uncovered" forever (the worker's `JUMP_IF_*_OR_POP`
    // chain lookup found nothing, since `if a and b:` compiles plain `POP_JUMP_IF_*`s), which
    // would make `--cover-branches` burn its whole budget chasing an outcome it can never prove.
    if !ready("test_position_boolop_is_never_permanently_uncovered") {
        return;
    }
    let src = "def f(a, b):\n    if a and b:\n        return 1\n    return 2\n";
    let rec = record_file(
        src,
        8,
        &ReplayMap::new(),
        RecordFlags { cover_branches: true, ..RecordFlags::default() },
    )
    .expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::BoolOp, 2);
    assert_ne!(status_of(bp, "short_circuit"), "uncovered");
    assert_ne!(status_of(bp, "full_evaluation"), "uncovered");
    assert_eq!(status_of(bp, "short_circuit"), "unobservable_line_granularity");
    assert_eq!(status_of(bp, "full_evaluation"), "unobservable_line_granularity");
}

#[test]
fn mixed_ternary_and_boolop_on_one_line_both_resolve_via_correct_per_category_ordinal() {
    if !ready("mixed_ternary_and_boolop_on_one_line_both_resolve_via_correct_per_category_ordinal") {
        return;
    }
    let src = "def f(a, b, c):\n    return (a if b else c), (b and c)\n";
    let mut replay = ReplayMap::new();
    replay.insert(
        "f".to_string(),
        vec![
            // b truthy: ternary picks `a` (true), and `b and c` evaluates `c` too (full_evaluation).
            vec![json!(1), json!(true), json!(2)],
            // b falsy: ternary picks `c` (false), and `b and c` short-circuits on `b` alone.
            vec![json!(1), json!(false), json!(2)],
        ],
    );
    let rec = record_file(src, 0, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let ternary = branch_report(branches, BranchKind::Ternary, 2);
    assert_eq!(status_of(ternary, "true"), "covered");
    assert_eq!(status_of(ternary, "false"), "covered");
    let boolop = branch_report(branches, BranchKind::BoolOp, 2);
    assert_eq!(status_of(boolop, "full_evaluation"), "covered");
    assert_eq!(status_of(boolop, "short_circuit"), "covered");
}

#[test]
fn nested_ternary_in_a_compound_ternary_test_is_never_falsely_covered() {
    // The exact `probe_fine2` repro the parent review found: `(1 if a else 2) if b and c else 3`
    // replayed with `(1, 1, 0)` takes the outer's ELSE branch (`b and c` is falsy) — the inner
    // ternary never executes. The old ordinal scheme resolved the inner ternary's probe against
    // the outer's (demoted) first test jump instead, wrongly reporting "true" covered.
    if !ready("nested_ternary_in_a_compound_ternary_test_is_never_falsely_covered") {
        return;
    }
    let src = "def f(a, b, c):\n    return (1 if a else 2) if b and c else 3\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!(1), json!(1), json!(0)]]);
    let rec = record_file(src, 0, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let ternaries: Vec<&BranchReport> = branches.iter().filter(|b| b.kind == BranchKind::Ternary).collect();
    assert_eq!(ternaries.len(), 2, "expected inner and outer ternary reports: {branches:?}");
    for t in &ternaries {
        assert_ne!(status_of(t, "true"), "covered", "no observation proves either ternary's true branch ran");
        assert_eq!(status_of(t, "true"), "unobservable_line_granularity");
    }
}

#[test]
fn branch_coverage_rollup_is_a_closed_count() {
    if !ready("branch_coverage_rollup_is_a_closed_count") {
        return;
    }
    // The second `if`'s false outcome has no line to land on (the function ends there) — the one
    // genuinely `Unobservable` outcome left after ternaries/boolops/inline-ifs became fine-grained.
    let src = "def f(x):\n    if x > 0:\n        return 1\n    if x < 0:\n        y = 2\n";
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let rollup = f.branch_coverage.as_ref().expect("rollup present");
    let total_outcomes: usize = branches.iter().map(|b| b.outcomes.len()).sum();
    assert_eq!(rollup.covered + rollup.uncovered + rollup.unobservable, total_outcomes);
    assert!(
        rollup.unobservable >= 1,
        "the else-less if's false outcome (no fall-through line) must stay unobservable"
    );
}

// ---------------------------------------------------------------------------------------------
// Dynamic: `--cover-branches`'s predicate-targeted loop.
// ---------------------------------------------------------------------------------------------

/// `len(x) == 5` isn't a form `analyze::collect::guards`'s guard-sample extraction handles (it
/// only roots a name/attribute/subscript chain, and a `Call` like `len(x)` has none) — so, unlike
/// a bare `x == 42`, plain generation has no pre-existing heuristic nudging it toward a
/// length-5 sequence, and the branch stays uncovered at a small budget without
/// `--cover-branches`. `5` (not `4`) is deliberate: `len(x)`'s sole evidence widens `x`'s shape
/// to admit a `str` alongside the `Seq` (see `analyze::passes::shapes`'s sequence-protocol
/// widening), and the `str` seed corpus's one length-4 filler (`"Word"`) would otherwise satisfy
/// the branch by coincidence at this budget — no length in the corpus happens to be `5`.
const LEN_EQ_SRC: &str = "def f(x):\n    if len(x) == 5:\n        return 1\n    return 0\n";

#[test]
fn cover_branches_off_leaves_the_equality_branch_uncovered_with_loop_not_run() {
    if !ready("cover_branches_off_leaves_the_equality_branch_uncovered_with_loop_not_run") {
        return;
    }
    // 16 (not 12): `x`'s shape is a `Union(Seq, Str)` (sequence-protocol widening — see
    // `LEN_EQ_SRC`'s doc), whose combined seed corpus alone is 13 candidates; a budget of exactly
    // 12 would let the initial ranked batch consume the whole budget before the (here, disabled)
    // cover-branches loop ever gets a turn, which isn't what this test is about.
    let rec = record_file(LEN_EQ_SRC, 16, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::If, 2);
    assert_eq!(status_of(bp, "true"), "uncovered");
    assert_eq!(reason_of(bp, "true"), Some("loop_not_run"));
}

#[test]
fn cover_branches_on_covers_the_equality_branch_and_terminates() {
    if !ready("cover_branches_on_covers_the_equality_branch_and_terminates") {
        return;
    }
    // See the sibling `off` test for why the budget is 16, not 12: the initial ranked batch off
    // `x`'s widened `Union(Seq, Str)` shape alone can consume up to 13 cases, so the
    // cover-branches loop needs headroom past that to add its targeted case.
    let rec = record_file(
        LEN_EQ_SRC,
        16,
        &ReplayMap::new(),
        RecordFlags { cover_branches: true, ..RecordFlags::default() },
    )
    .expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::If, 2);
    assert_eq!(status_of(bp, "true"), "covered");
    assert!(
        f.cases.len() <= 16,
        "the loop must respect the total per-function case budget: got {} cases",
        f.cases.len()
    );
}

#[test]
fn cover_branches_on_an_opaque_predicate_stays_uncovered_with_no_synthesizer() {
    if !ready("cover_branches_on_an_opaque_predicate_stays_uncovered_with_no_synthesizer") {
        return;
    }
    // `id(x)` (a memory address) is both unhandled by `extract_deriv` (not a recognized
    // derivation) and, practically, never equal to a fixed literal by chance — so this branch's
    // `true` outcome stays uncovered whether or not `--cover-branches` runs, for two independent
    // reasons that both point at `no_synthesizer`.
    let src = "def f(x):\n    if id(x) == 999999999999:\n        return 1\n    return 0\n";
    let rec = record_file(
        src,
        12,
        &ReplayMap::new(),
        RecordFlags { cover_branches: true, ..RecordFlags::default() },
    )
    .expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::If, 2);
    assert_eq!(status_of(bp, "true"), "uncovered");
    assert_eq!(reason_of(bp, "true"), Some("no_synthesizer"));
}
