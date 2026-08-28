//! Branch accounting: the static enumeration (pure, always runs) and the dynamic per-outcome
//! coverage built on top of it (sandbox-gated — skips gracefully when the sandbox isn't
//! provisioned, same as `tests/record.rs`).

use pylens::analyze_source;
use pylens::exec::{Sandbox, probe};
use pylens::model::branch::{BranchKind, BranchPoint, OutcomeEvidence};
use pylens::model::EffectSignature;
use pylens::record::{BranchReport, FunctionRecord, ReplayMap, record_file_with_options, record_file_with_replay};
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
fn enumerates_inline_if_and_ternary_and_boolop_as_unobservable() {
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
    assert_eq!(*outcome(inline, "true"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(inline, "false"), OutcomeEvidence::Unobservable);

    let ternary = branch_at(points, BranchKind::Ternary, 3);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(ternary, "false"), OutcomeEvidence::Unobservable);

    let boolop = branch_at(points, BranchKind::BoolOp, 4);
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(boolop, "full_evaluation"), OutcomeEvidence::Unobservable);
}

#[test]
fn enumerates_comprehension_if_as_unobservable() {
    let sigs = analyze_source(
        "def f(xs):\n\
         \x20   return [x for x in xs if x > 0]\n",
    )
    .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let bp = branch_at(points, BranchKind::ComprehensionIf, 2);
    assert_eq!(*outcome(bp, "true"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(bp, "false"), OutcomeEvidence::Unobservable);
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
    let result = sandbox.call(src, "f", &[json!(-1)], &[]).expect("call");
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
    let rec = record_file_with_replay(src, 1, &replay).expect("record");
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
    let rec = record_file_with_replay(src, 1, &replay).expect("record");
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
    let rec = record_file_with_replay(src, 1, &replay).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::While, 2);
    assert_eq!(status_of(bp, "skip"), "covered");
    assert_eq!(status_of(bp, "enter"), "covered");
}

#[test]
fn ternary_is_reported_unobservable_regardless_of_input() {
    if !ready("ternary_is_reported_unobservable_regardless_of_input") {
        return;
    }
    let src = "def f(x):\n    return \"a\" if x else \"b\"\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!(true)], vec![json!(false)]]);
    let rec = record_file_with_replay(src, 1, &replay).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::Ternary, 2);
    assert_eq!(status_of(bp, "true"), "unobservable_line_granularity");
    assert_eq!(status_of(bp, "false"), "unobservable_line_granularity");
}

#[test]
fn branch_coverage_rollup_is_a_closed_count() {
    if !ready("branch_coverage_rollup_is_a_closed_count") {
        return;
    }
    let src = "def f(x):\n    if x > 0:\n        y = 1\n    z = \"a\" if x else \"b\"\n    return x, z\n";
    let rec = record_file_with_replay(src, 4, &ReplayMap::new()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let rollup = f.branch_coverage.as_ref().expect("rollup present");
    let total_outcomes: usize = branches.iter().map(|b| b.outcomes.len()).sum();
    assert_eq!(rollup.covered + rollup.uncovered + rollup.unobservable, total_outcomes);
    assert!(rollup.unobservable >= 2, "the ternary's two outcomes must be unobservable");
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
    let rec = record_file_with_options(LEN_EQ_SRC, 16, &ReplayMap::new(), None, false).expect("record");
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
    let rec = record_file_with_options(LEN_EQ_SRC, 16, &ReplayMap::new(), None, true).expect("record");
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
    let rec = record_file_with_options(src, 12, &ReplayMap::new(), None, true).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::If, 2);
    assert_eq!(status_of(bp, "true"), "uncovered");
    assert_eq!(reason_of(bp, "true"), Some("no_synthesizer"));
}
