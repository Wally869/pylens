//! Branch accounting: the dynamic per-outcome coverage built on top of the static enumeration
//! (sandbox-gated — skips gracefully when the sandbox isn't provisioned, same as
//! `tests/record.rs`). The static enumeration itself is pure and lives in
//! `tests/branches_static.rs`.

use pylens::exec::{Sandbox, probe};
use pylens::model::branch::BranchKind;
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
fn same_line_while_enter_and_skip_are_covered_via_landing_offset_chain() {
    // Item 2 end to end: a same-line `while a and b: a -= 1` — both outcomes resolved from the
    // worker's opcode-level landing-offset chain, not line arcs.
    if !ready("same_line_while_enter_and_skip_are_covered_via_landing_offset_chain") {
        return;
    }
    let src = "def f(a, b):\n    while a and b: a -= 1\n    return a\n";
    let mut replay = ReplayMap::new();
    replay.insert(
        "f".to_string(),
        vec![vec![json!(0), json!(1)], vec![json!(2), json!(1)]],
    );
    let rec = record_file(src, 0, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::While, 2);
    assert_eq!(status_of(bp, "skip"), "covered");
    assert_eq!(status_of(bp, "enter"), "covered");
}

#[test]
fn same_line_while_store_free_body_never_falsely_covers_enter_on_a_skip_only_replay() {
    // Parent-review repro: `while xs: xs.pop()` — a STORE-FREE body (`.pop()` compiles to
    // `LOAD_METHOD`/`CALL_METHOD`/`POP_TOP`, none of which a naive opcode-based "body boundary"
    // heuristic can tell apart from a test operand's own value computation — an earlier version of
    // this scheme merged the pre-loop test and the loop-rotated retest into one run and derived the
    // canonical (true, false) pair from the wrong, polarity-flipped retest, silently INVERTING
    // `enter`/`skip`). Replaying only the empty list never enters the loop at all — `enter` must
    // stay uncovered, not (as the inverted bug reported) `covered`.
    if !ready("same_line_while_store_free_body_never_falsely_covers_enter_on_a_skip_only_replay") {
        return;
    }
    let src = "def f(xs):\n    while xs: xs.pop()\n    return 1\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!([])]]);
    // Forces the always-present floor-of-one generated case (`--inputs 0` still yields 1) to a
    // falsy `None` (never a truthy scalar filler candidate, which `while xs:` would still enter
    // on before failing inside `.pop()`) — so it can't independently prove `enter` and mask what
    // this test is about.
    let domain = pylens::generate::ValueDomain::parse(r#"{"scalars": ["none"]}"#).expect("domain");
    let rec = record_file(
        src,
        0,
        &replay,
        RecordFlags { domain: Some(&domain), ..RecordFlags::default() },
    )
    .expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::While, 2);
    assert_ne!(status_of(bp, "enter"), "covered", "the loop body never ran on an empty list");
    assert_eq!(status_of(bp, "enter"), "uncovered");
    assert_eq!(status_of(bp, "skip"), "covered");
}

#[test]
fn same_line_while_store_free_body_correctly_covers_enter_when_it_actually_enters() {
    // Mirror of the sibling test, the other direction: a one-element list DOES enter the loop
    // (once, then exits after the `.pop()` empties it) — `enter` must show `covered`, not stay
    // stuck `uncovered` the way an inverted label would (proving the fix isn't just "always report
    // uncovered").
    if !ready("same_line_while_store_free_body_correctly_covers_enter_when_it_actually_enters") {
        return;
    }
    let src = "def f(xs):\n    while xs: xs.pop()\n    return 1\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!([1])]]);
    let rec = record_file(src, 0, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::While, 2);
    assert_eq!(status_of(bp, "enter"), "covered");
    assert_eq!(status_of(bp, "skip"), "covered");
}

#[test]
fn nested_compound_boolop_test_is_never_falsely_covered_by_either_replay() {
    // `(a and b) or (c and d)` — no ternary anywhere, so `analyze::collect::branches`'s AST-level
    // gate can't rule it out (see the static test pinning that), but the shape doesn't flatten:
    // the left AND's failure jumps into the middle of the right OR's own test (an intermediate
    // offset neither the true nor the false landing) — only the worker's dynamic bytecode
    // verification catches this, at `_build_fine_plan` time, and records no `fine_hits` for it
    // at all. The hard property this pins: replaying EITHER a semantically-true and a
    // semantically-false input must never report either outcome `covered`.
    if !ready("nested_compound_boolop_test_is_never_falsely_covered_by_either_replay") {
        return;
    }
    let src = "def f(a, b, c, d):\n    return 1 if (a and b) or (c and d) else 2\n";
    let mut replay = ReplayMap::new();
    replay.insert(
        "f".to_string(),
        vec![
            vec![json!(1), json!(1), json!(0), json!(0)], // (a and b) true -> the ternary's true arm
            vec![json!(0), json!(0), json!(0), json!(0)], // both false -> the ternary's false arm
        ],
    );
    let rec = record_file(src, 0, &replay, RecordFlags::default()).expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::Ternary, 2);
    assert_ne!(status_of(bp, "true"), "covered", "the landing-offset scheme must refuse this shape, not guess");
    assert_ne!(status_of(bp, "false"), "covered", "the landing-offset scheme must refuse this shape, not guess");
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
    // The hard property, pinned by the landing-offset scheme's own design: `1 if a and b else 2`
    // replayed with (1, 0) takes the ELSE branch (`a and b` is falsy, short-circuiting on `b`). A
    // naive scheme attaching the probe to the FIRST test jump (`a`'s own `POP_JUMP_IF_FALSE`,
    // which falls through when `a` alone is truthy) would prove nothing about the whole construct
    // and wrongly report "true" covered — the landing-offset scheme instead watches which of the
    // construct's two PROVEN landing offsets the frame actually reaches, so "true" stays
    // uncovered no matter which single instruction along the way jumped or fell through.
    if !ready("compound_ternary_test_replaying_only_the_else_branch_never_falsely_covers_true") {
        return;
    }
    let src = "def f(a, b):\n    return 1 if a and b else 2\n";
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!(1), json!(0)]]);
    // A `--value-domain` admitting only `none` for scalars keeps the always-present floor-of-one
    // generated case (`gen_inputs` never drops below 1 even at `--inputs 0`) at `(None, None)` —
    // falsy, so it can't independently prove "true" and mask what THIS test is actually about:
    // whether the (1, 0) replay alone gets misattributed.
    let domain = pylens::generate::ValueDomain::parse(r#"{"scalars": ["none"]}"#).expect("domain");
    let rec = record_file(
        src,
        0,
        &replay,
        RecordFlags { domain: Some(&domain), ..RecordFlags::default() },
    )
    .expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let bp = branch_report(branches, BranchKind::Ternary, 2);
    assert_ne!(status_of(bp, "true"), "covered", "no observation proves the true branch ran");
    assert_eq!(status_of(bp, "true"), "uncovered");
    assert_eq!(status_of(bp, "false"), "covered");
}

#[test]
fn compound_or_ternary_test_is_never_falsely_covered() {
    // Positive companion to the sibling `and` test: two replays exercising BOTH outcomes must
    // both resolve `covered` — the landing-offset scheme's proof holds either direction.
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
    assert_eq!(status_of(bp, "true"), "covered");
    assert_eq!(status_of(bp, "false"), "covered");
}

#[test]
fn test_position_boolop_is_never_permanently_uncovered() {
    // Before the landing-offset scheme this was reported `unobservable_line_granularity` forever
    // (the worker's `JUMP_IF_*_OR_POP` chain lookup found nothing, since `if a and b:` compiles
    // plain `POP_JUMP_IF_*`s) — now it resolves from the `if`'s own chain, and a small
    // `--cover-branches` budget covers both outcomes.
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
    assert_ne!(status_of(bp, "short_circuit"), "unobservable_line_granularity");
    assert_ne!(status_of(bp, "full_evaluation"), "unobservable_line_granularity");
    assert_eq!(status_of(bp, "short_circuit"), "covered");
    assert_eq!(status_of(bp, "full_evaluation"), "covered");
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

// ---------------------------------------------------------------------------------------------
// `--cover-branches`'s merged-override synthesis for a `BoolOp` conjunction/disjunction: an
// outcome that needs more than one operand's own predicate satisfied together in ONE input
// (an `and`'s `true`, an `or`'s `false`) — see `record::cover::merge_group`/`want_for_outcome`.
// `startswith`/`endswith` targets (not plain integer comparisons): a numeric threshold gets
// GUARD-SAMPLED (`analyze::collect::guards` seeds the literal and its ±1 neighbors per parameter
// independently), and the plain ranked initial batch's own per-parameter candidate cross-product
// was empirically found to stumble onto an arbitrary numeric conjunction on its own once the
// budget is large enough — which would make the test pass whether or not the merge fix is
// present. An arbitrary literal string prefix/suffix has no such shortcut: nothing in the seed
// corpus or guard sampling ever manufactures `"zqx..."`, so `true`/`false` can only be covered by
// an override that deliberately builds it — confirmed by reverting this change locally and
// re-running these exact sources: `true`/`false` stayed `uncovered` (`candidates_exhausted`) even
// at a budget of 300, while `full_evaluation` (which only needs the FIRST operand, not a merge —
// see `want_for_outcome`'s doc) was already `covered` before the fix, as expected.
// ---------------------------------------------------------------------------------------------

#[test]
fn and_conjunction_true_needs_the_merged_pair() {
    // 130 (not a smaller budget): the initial ranked batch off two `Str` parameters' combined
    // seed corpus alone can run past 100 cases before the cover loop gets a turn — see
    // `LEN_EQ_SRC`'s doc for the same headroom concern with one parameter; two multiplies it.
    if !ready("and_conjunction_true_needs_the_merged_pair") {
        return;
    }
    let src = "def f(a, b):\n    if a.startswith(\"zqx\") and b.endswith(\"vwq\"):\n        return 1\n    return 0\n";
    let rec = record_file(
        src,
        130,
        &ReplayMap::new(),
        RecordFlags { cover_branches: true, ..RecordFlags::default() },
    )
    .expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let if_bp = branch_report(branches, BranchKind::If, 2);
    assert_eq!(status_of(if_bp, "true"), "covered");
    let boolop_bp = branch_report(branches, BranchKind::BoolOp, 2);
    // `full_evaluation` only needs the FIRST operand true (`a.startswith("zqx")`) — reachable off
    // the existing single-predicate path alone, not a discriminator for this change on its own,
    // but it must still end up `covered`.
    assert_eq!(status_of(boolop_bp, "full_evaluation"), "covered");
    // `short_circuit` needs only the FIRST operand at the opposite polarity — same existing path.
    assert_eq!(status_of(boolop_bp, "short_circuit"), "covered");
}

#[test]
fn or_disjunction_false_needs_the_merged_pair() {
    // Each operand's usual (default-generated) value already makes it TRUE (`not
    // x.startswith("zqx")` holds for virtually every generated string) — so `true` is trivially
    // reachable without any synthesis. The discriminating outcome is `false`, which needs BOTH
    // operands false at once: `a.startswith("zqx")` AND `b.startswith("zqx")` — the exact mirror
    // of the `and` case above, reached through `or`'s own merge branch (`want == group.and` with
    // `group.and == false`).
    if !ready("or_disjunction_false_needs_the_merged_pair") {
        return;
    }
    let src =
        "def f(a, b):\n    if not a.startswith(\"zqx\") or not b.startswith(\"zqx\"):\n        return 1\n    return 0\n";
    let rec = record_file(
        src,
        130,
        &ReplayMap::new(),
        RecordFlags { cover_branches: true, ..RecordFlags::default() },
    )
    .expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let if_bp = branch_report(branches, BranchKind::If, 2);
    assert_eq!(status_of(if_bp, "true"), "covered");
    assert_eq!(status_of(if_bp, "false"), "covered");
}

#[test]
fn conjunction_over_the_same_parameter_never_falsely_covers_via_merge() {
    // Both operands name the SAME parameter (`a`) — `merge_group` refuses to combine them (no
    // constraint solving over one parameter's value), so `true` must never be reported `covered`
    // by a merged guess. The interval is also empty (`> 1_000_010` and `< 1_000_000` can never
    // both hold), so this also pins that an unsatisfiable conjunction is never falsely covered.
    if !ready("conjunction_over_the_same_parameter_never_falsely_covers_via_merge") {
        return;
    }
    let src = "def f(a):\n    if a > 1000010 and a < 1000000:\n        return 1\n    return 0\n";
    let rec = record_file(
        src,
        60,
        &ReplayMap::new(),
        RecordFlags { cover_branches: true, ..RecordFlags::default() },
    )
    .expect("record");
    let f = find_function(&rec.functions, "f");
    let branches = f.branches.as_ref().expect("branches present");
    let if_bp = branch_report(branches, BranchKind::If, 2);
    assert_ne!(status_of(if_bp, "true"), "covered");
    // Each single-predicate override IS individually synthesizable (`a > 1_000_010` alone, or
    // `a < 1_000_000` alone) — the loop tries and exhausts them, it just never gets a candidate
    // that satisfies both at once, since the merge that would build one refuses same-parameter
    // operands outright.
    assert_eq!(reason_of(if_bp, "true"), Some("candidates_exhausted"));
}
