//! Branch accounting: the static enumeration (pure, always runs). The dynamic per-outcome
//! coverage built on top of it is sandbox-gated — see `tests/branches_sandbox.rs`.

use pylens::analyze_source;
use pylens::model::branch::{BranchKind, BranchPoint, OutcomeEvidence};
use pylens::model::EffectSignature;

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
    assert_eq!(*outcome(inline, "true"), OutcomeEvidence::FineGrained(2, 0, false));
    assert_eq!(*outcome(inline, "false"), OutcomeEvidence::FineGrained(2, 0, false));

    let ternary = branch_at(points, BranchKind::Ternary, 3);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::FineGrained(3, 0, false));
    assert_eq!(*outcome(ternary, "false"), OutcomeEvidence::FineGrained(3, 0, false));

    let boolop = branch_at(points, BranchKind::BoolOp, 4);
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::FineGrained(4, 0, false));
    assert_eq!(*outcome(boolop, "full_evaluation"), OutcomeEvidence::FineGrained(4, 0, false));
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
    assert_eq!(*outcome(bp, "true"), OutcomeEvidence::FineGrained(2, 0, false));
    assert_eq!(*outcome(bp, "false"), OutcomeEvidence::FineGrained(2, 0, false));
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
    assert_eq!(*outcome(ternaries[0], "true"), OutcomeEvidence::FineGrained(2, 0, false));
    assert_eq!(*outcome(ternaries[1], "true"), OutcomeEvidence::FineGrained(2, 1, false));
}

#[test]
fn compound_ternary_test_resolves_via_landing_offset_chain() {
    // `a and b` compiles to a CHAIN of test jumps, not the single instruction a simple test
    // does — but every jump in that chain lands on exactly one of two proven offsets (the
    // ternary's own true/false landing), so the landing-offset scheme (see
    // `analyze::collect::branches`'s module doc) resolves it: `compound: true`, sole test
    // position on the line, no nested ternary.
    let sigs = analyze_source("def f(a, b):\n    return 1 if a and b else 2\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::FineGrained(2, 0, true));
    assert_eq!(*outcome(ternary, "false"), OutcomeEvidence::FineGrained(2, 0, true));
    // The nested `and` IS the ternary's whole test (a test-position `BoolOp`) — resolved from the
    // exact same instruction plan, not suppressed.
    let boolop = branch_at(points, BranchKind::BoolOp, 2);
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::FineGrained(2, 0, true));
    assert_eq!(*outcome(boolop, "full_evaluation"), OutcomeEvidence::FineGrained(2, 0, true));
}

#[test]
fn compound_or_ternary_test_resolves_via_landing_offset_chain() {
    // `or` mixes `POP_JUMP_IF_TRUE` (short-circuit success) and `POP_JUMP_IF_FALSE` (failure) in
    // the same chain — still not a single instruction, but every jump still lands on one of the
    // two proven landing offsets, so this also resolves.
    let sigs = analyze_source("def f(a, b):\n    return 1 if a or b else 2\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::FineGrained(2, 0, true));
    assert_eq!(*outcome(ternary, "false"), OutcomeEvidence::FineGrained(2, 0, true));
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
fn same_line_while_resolves_enter_and_skip_via_landing_offset_chain() {
    // `while a and b: a -= 1` — same-line, so line-tracing alone can't tell `enter` from `skip`
    // (item 2: `while`'s loop-rotation-duplicated test, resolved via the SAME landing-offset
    // scheme as a compound ternary — see `_resolve_landing_chain`'s module doc).
    let sigs = analyze_source("def f(a, b):\n    while a and b: a -= 1\n    return a\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let bp = branch_at(points, BranchKind::While, 2);
    assert_eq!(*outcome(bp, "enter"), OutcomeEvidence::FineGrained(2, 0, true));
    assert_eq!(*outcome(bp, "skip"), OutcomeEvidence::FineGrained(2, 0, true));
}

#[test]
fn same_line_while_with_simple_test_also_resolves() {
    // A same-line `while` needs the landing-offset scheme's multi-run handling regardless of its
    // own test's complexity — CPython's loop rotation duplicates even a plain test.
    let sigs = analyze_source("def f(n): \n    while n: n -= 1\n    return n\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let bp = branch_at(points, BranchKind::While, 2);
    assert_eq!(*outcome(bp, "enter"), OutcomeEvidence::FineGrained(2, 0, true));
    assert_eq!(*outcome(bp, "skip"), OutcomeEvidence::FineGrained(2, 0, true));
}

#[test]
fn nested_compound_boolop_test_has_no_ternary_but_still_stays_unobservable_pre_worker() {
    // `(a and b) or (c and d)` has no ternary anywhere, so `analyze::collect::branches`'s cheap
    // AST-level gate optimistically marks it `compound: true` — the deliberately-ambiguous shape
    // this pass CANNOT prove safe from the AST alone: the left AND's failure jumps into the
    // middle of the right OR's own test (an intermediate offset), which only the WORKER's dynamic
    // bytecode verification (`_resolve_landing_chain`) can detect and reject — see the sandbox
    // test `nested_compound_boolop_test_is_never_falsely_covered_by_either_replay` for the actual
    // safety proof. Statically, this is still `FineGrained` (the AST gate alone can't tell).
    let sigs = analyze_source("def f(a, b, c, d):\n    return 1 if (a and b) or (c and d) else 2\n")
        .expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::FineGrained(2, 0, true));
}

#[test]
fn chained_comparison_ternary_test_demotes_to_unobservable() {
    // `1 if a < b < c else 2` — `dis` on 3.10 shows two `POP_JUMP_IF_FALSE` instructions, but the
    // FIRST operator's failure jumps to a `POP_TOP` cleanup (discarding the dangling comparison
    // value) rather than either landing offset — an intermediate offset the landing-offset scheme
    // can never attribute. `contains_unprovable_shape` catches this at the AST level (a `Compare`
    // with more than one operator), demoting it up front rather than letting the worker discover
    // the same thing dynamically and report a permanent, wasted `uncovered`.
    let sigs = analyze_source("def f(a, b, c):\n    return 1 if a < b < c else 2\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let ternary = branch_at(points, BranchKind::Ternary, 2);
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::Unobservable);
    assert_eq!(*outcome(ternary, "false"), OutcomeEvidence::Unobservable);
}

#[test]
fn comprehension_guard_with_a_method_call_resolves_via_landing_offset_chain() {
    // `[x for x in xs if x.check()]` — `dis` on 3.10 shows the guard compiles to a single
    // `LOAD_METHOD`/`CALL_METHOD`/`POP_JUMP_IF_FALSE`, exactly the simple one-instruction shape
    // (no chain, no intermediate offset) — a call in a guard doesn't itself introduce the
    // multi-jump complexity a `BoolOp`/ternary/chained-comparison would. Sole test position on its
    // line, no ternary — resolves.
    let sigs = analyze_source("def f(xs):\n    return [x for x in xs if x.check()]\n").expect("parse");
    let points = &sig(&sigs, "f").branch_points;
    let bp = branch_at(points, BranchKind::ComprehensionIf, 2);
    assert_eq!(*outcome(bp, "true"), OutcomeEvidence::FineGrained(2, 0, false));
    assert_eq!(*outcome(bp, "false"), OutcomeEvidence::FineGrained(2, 0, false));
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
fn test_position_boolop_resolves_via_landing_offset_chain() {
    // `if a and b:` compiles the boolop into the `if`'s OWN jump chain (plain `POP_JUMP_IF_*`,
    // never `JUMP_IF_*_OR_POP`) — the `if`'s own true/false outcome is still a normal multi-line
    // Arc (unaffected, non-same-line), but the boolop's own short_circuit/full_evaluation branch
    // point now resolves from the SAME chain via the landing-offset scheme (the `if` is the sole
    // test position on this line, its test is a pure `and`, no ternary).
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
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::FineGrained(2, 0, true));
    assert_eq!(*outcome(boolop, "full_evaluation"), OutcomeEvidence::FineGrained(2, 0, true));
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
    assert_eq!(*outcome(ternary, "true"), OutcomeEvidence::FineGrained(2, 0, false));
    let boolop = branch_at(points, BranchKind::BoolOp, 2);
    assert_eq!(*outcome(boolop, "short_circuit"), OutcomeEvidence::FineGrained(2, 0, false));
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
