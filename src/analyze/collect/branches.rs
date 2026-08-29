//! Enumerates a function's branch points from its AST for `record`'s per-branch-outcome
//! accounting: every `if`/`elif`, `while`, `for`, `except` arm, `try`/`else`, `match` arm, plus
//! the same-line constructs line tracing alone can never distinguish (ternaries, `and`/`or`
//! short-circuits, single-line `if x: y` bodies, comprehension `if` guards). Each branch point
//! carries every one of its outcomes, together with the runtime evidence that would prove it
//! happened — a line arc, a bare line, `FineGrained(line, ordinal)` for the opcode-resolvable
//! same-line constructs (see `model::branch::FineTarget`/`FineHit` and `python/worker.py`), or
//! `Unobservable` for the rare case with no evidence at all (an else-less `if` whose false arc
//! has no landing line because the function ends there). Nothing is dropped: an outcome with no
//! distinguishing evidence is still enumerated, just marked unobservable — see
//! `record::branch_report_for`, which turns this into the reported
//! `covered`/`uncovered`/`unobservable_line_granularity` status.
//!
//! Two passes over the body: a hand-written statement walk (mirrors `collect::body_lines`, but
//! also threads the *fall-through line* — where control lands after a body if it runs off the
//! end — needed to compute an else-less `if`'s false-arc target, a `for`'s empty-arc target, and
//! a `while`'s skip-arc target) for the control-flow constructs, and a generic AST
//! [`Visitor`] for the same-line expression constructs (which need no fall-through context, just
//! every expression in the body). Both passes share one [`CollectCtx`] — its `ordinals` counter,
//! keyed per `(line, ProbeCategory)` rather than per line alone, stays aligned with
//! `python/worker.py`'s two separate ordinal-indexed instruction groups (see [`ProbeCategory`]);
//! its `polluted_lines` feeds the final demotion pass (see below). A nested `def`/`class` is not
//! descended into by either pass — its own branches belong to that function, not this one —
//! matching `body_lines`.
//!
//! A test containing a nested conditional (`a and b` directly as a control construct's test, or a
//! ternary/`and`/`or` nested anywhere inside one — see [`contains_conditional`]) compiles to
//! MULTIPLE `POP_JUMP_IF_*` instructions instead of the single one a simple test does (CPython
//! duplicates a nested ternary's own decision across each of the enclosing consumer's branches,
//! and expands a compound boolop into one jump per operand).
//!
//! **Recovering compound tests (the landing-offset scheme).** A test whose ONLY source of
//! complexity is `BoolOp` nesting (`a and b`, `(a and b) or c`, arbitrarily deep `and`/`or`, no
//! ternary anywhere) still compiles to a well-understood shape: CPython backpatches every
//! short-circuiting jump in the chain to land DIRECTLY on one of exactly two offsets — the
//! construct's own true-landing (the fallthrough of the chain's last instruction) or its
//! false-landing (that instruction's own jump target) — never an intermediate offset internal to
//! the test. `python/worker.py`'s `_resolve_landing_chain` verifies this dynamically, against the
//! ACTUAL compiled bytecode, before trusting it: every instruction physically on the line must
//! target one of those two offsets, or the whole line's resolution is abandoned (no `fine_hits`
//! ever recorded for it — safe by construction, since an unresolved `FineGrained` outcome just
//! reports `uncovered`, never `covered`). This same check also naturally REJECTS the shapes that
//! don't flatten (a nested ternary duplicating the outer construct's instructions across its
//! branches; `(a and b) or (c and d)`, whose left AND's failure must jump into the middle of the
//! right OR's own test, an intermediate offset) — the worker's dynamic proof is the actual safety
//! net; this pass's own `contains_unprovable_shape` gate (see below) is only a cheap pre-filter that rules
//! out the ternary case before ever asking the worker to try.
//!
//! `while`'s same-line test additionally gets CPython's loop-rotation treatment: the test is
//! compiled TWICE (once before the loop, once as the bottom-of-loop retest), and the retest's
//! polarity is flipped (whichever operand used to jump on failure now jumps on success) to save an
//! instruction. The retest shares the exact same physical line as the loop body (that is what
//! "same-line" means), so there is no line-table boundary — and no reliable OPCODE-based signal
//! either, a store-free body (`while xs: xs.pop()`) compiles to ordinary `LOAD_METHOD`/
//! `CALL_METHOD`/`POP_TOP` indistinguishable from a test operand's own value computation — to
//! split "pre-loop test" from "retest" instructions. `_resolve_landing_chain` doesn't try:
//! CPython's codegen guarantees the retest's own loop-continuation jump is the ONLY instruction on
//! the line that ever jumps BACKWARD (nothing before the very first instruction of a straight-line
//! test could target an earlier offset), so that one instruction's own jump target IS the
//! true-landing, read directly off it — every OTHER (forward-jumping) instruction on the line,
//! from either the pre-loop test or the retest's own non-final operands, must then share ONE
//! common target, the false-landing. No run grouping is needed at all.


//!
//! A `BoolOp` used AS a test (`if a and b:`, `x if (a or b) else y`, a comprehension guard `if a
//! and b`) additionally gets its own `short_circuit`/`full_evaluation` resolution from the exact
//! same per-line instruction plan — see [`suppress_test_boolops`]/`ExprCollector`'s
//! `test_position_boolops`.
//!
//! This recovery only ever applies to a line hosting EXACTLY ONE test position (an `if`/`elif`
//! test, a `while` test, a ternary's test, a comprehension guard): `python/worker.py` groups a
//! line's `POP_JUMP_IF_*` instructions by PHYSICAL LINE, not by which AST node compiled them, so a
//! SECOND test position sharing the line (even a simple one) risks the worker attributing an
//! instruction to the wrong construct — see [`reconcile_fine_grained`]/`CollectCtx::positions`,
//! the generalization of the old `polluted_lines`-demotes-everything rule this pass used before
//! this recovery existed. Same-line `for` (`FOR_ITER`) is out of this pass's scope — its same-line
//! outcome always stays `Unobservable`.

use std::collections::{HashMap, HashSet};

use ruff_python_ast as ast;
use ruff_python_ast::visitor::{self, Visitor};
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextSize};

use crate::model::branch::{BranchKind, BranchPoint, BranchPointOutcome, OutcomeEvidence};

/// Whether `expr` contains, anywhere within it, a `BoolOp` (`a and b`, `a or b`), a ternary
/// (`Expr::If`), or a CHAINED comparison (`a < b < c`, more than one operator in one `Compare`
/// node) — every one of these compiles to MULTIPLE jump instructions when embedded in a test
/// position (a compound boolop expands to a chain; a nested ternary's own decision gets
/// duplicated across the branches of whatever consumes its value; a chained comparison emits one
/// `POP_JUMP_IF_FALSE` per operator plus a `POP_TOP` cleanup on the early-exit path — `dis` on
/// 3.10 confirms `a < b < c`'s first operator's failure jumps to that `POP_TOP`, an offset outside
/// both landing offsets), never the single instruction a simple test compiles to. Does not descend
/// into a nested comprehension's or lambda's own body: those compile to a SEPARATE code object,
/// called once, so whatever conditional logic lives inside one can never duplicate or chain-expand
/// the ENCLOSING test's instructions.
fn contains_conditional(expr: &ast::Expr) -> bool {
    struct Finder {
        found: bool,
    }
    impl<'ast> Visitor<'ast> for Finder {
        fn visit_expr(&mut self, expr: &'ast ast::Expr) {
            if self.found {
                return;
            }
            match expr {
                ast::Expr::BoolOp(_) | ast::Expr::If(_) => {
                    self.found = true;
                }
                ast::Expr::Compare(cmp) if cmp.ops.len() > 1 => {
                    self.found = true;
                }
                ast::Expr::ListComp(_)
                | ast::Expr::SetComp(_)
                | ast::Expr::DictComp(_)
                | ast::Expr::Generator(_)
                | ast::Expr::Lambda(_) => {}
                _ => visitor::walk_expr(self, expr),
            }
        }
    }
    let mut finder = Finder { found: false };
    finder.visit_expr(expr);
    finder.found
}

/// Whether `expr` contains an `Expr::If` (ternary) or a chained comparison anywhere within it — a
/// strict subset of [`contains_conditional`] that ignores pure `BoolOp` nesting. A test flagged
/// by this is never eligible for the landing-offset chain recovery (see the module doc's
/// "Recovering compound tests" section): a nested ternary duplicates the enclosing construct's
/// own decision across each of its branches, and a chained comparison's early-exit path routes
/// through a `POP_TOP` cleanup instruction — both produce an intermediate jump target the chain
/// can't attribute safely; a pure `and`/`or` tree never does (`python/worker.py`'s
/// `_resolve_landing_chain` would eventually reject either dynamically too, since neither one's
/// instructions all land on a single proven pair — this is only a cheap pre-filter that skips the
/// attempt and reports the honest `unobservable_line_granularity` instead of a permanent, wasted
/// `uncovered`). Same traversal shape as `contains_conditional` (does not descend into a nested
/// comprehension's or lambda's own body).
fn contains_unprovable_shape(expr: &ast::Expr) -> bool {
    struct Finder {
        found: bool,
    }
    impl<'ast> Visitor<'ast> for Finder {
        fn visit_expr(&mut self, expr: &'ast ast::Expr) {
            if self.found {
                return;
            }
            match expr {
                ast::Expr::If(_) => {
                    self.found = true;
                }
                ast::Expr::Compare(cmp) if cmp.ops.len() > 1 => {
                    self.found = true;
                }
                ast::Expr::ListComp(_)
                | ast::Expr::SetComp(_)
                | ast::Expr::DictComp(_)
                | ast::Expr::Generator(_)
                | ast::Expr::Lambda(_) => {}
                _ => visitor::walk_expr(self, expr),
            }
        }
    }
    let mut finder = Finder { found: false };
    finder.visit_expr(expr);
    finder.found
}

/// Recursively marks `expr` and every `BoolOp` nested inside it (through further `BoolOp`
/// operands only — a non-boolop operand, e.g. `(a and b) == c`, needs its own value and so
/// compiles normally) as "compiled for control flow, not as a value" — see
/// [`contains_conditional`]. [`ExprCollector`] consults `suppress` before turning a `BoolOp` node
/// into a `FineGrained` outcome, so a boolop CPython folded into an enclosing test's jump chain is
/// reported `Unobservable` instead of resolved against the wrong (or a nonexistent) instruction.
fn mark_boolop_chain(expr: &ast::Expr, suppress: &mut HashSet<TextSize>) {
    if let ast::Expr::BoolOp(bool_op) = expr {
        suppress.insert(bool_op.range().start());
        for value in &bool_op.values {
            mark_boolop_chain(value, suppress);
        }
    }
}

/// Suppresses every `BoolOp` nested inside `bool_op`'s own operands (as [`mark_boolop_chain`]),
/// but NOT `bool_op` itself. Used at a test position whose test is *directly* a `BoolOp` (`if a
/// and b:`, `a if (x or y) else b`, a comprehension guard `if a and b`) — that outer node is the
/// landing-offset chain recovery's other target (see [`ExprCollector`]'s `test_position_boolops`),
/// so it must stay eligible for its own `FineGrained` outcome instead of being unconditionally
/// suppressed like a value-position boolop.
fn mark_boolop_children(bool_op: &ast::ExprBoolOp, suppress: &mut HashSet<TextSize>) {
    for value in &bool_op.values {
        mark_boolop_chain(value, suppress);
    }
}

/// Suppresses `test`'s boolop content for [`ExprCollector`]'s `suppress` set, and returns whether
/// `test` is itself directly a `BoolOp` (the landing-offset recovery's other target — see
/// [`mark_boolop_children`]).
fn suppress_test_boolops(test: &ast::Expr, suppress: &mut HashSet<TextSize>) -> bool {
    if let ast::Expr::BoolOp(bool_op) = test {
        mark_boolop_children(bool_op, suppress);
        true
    } else {
        mark_boolop_chain(test, suppress);
        false
    }
}

/// Which bytecode shape a [`OutcomeEvidence::FineGrained`] point resolves against — must match
/// `python/worker.py`'s `_build_fine_plan`, which keeps a SEPARATE ordinal-indexed group per
/// category (`test_groups` vs `chain_groups`), never one shared list. `ordinal` is therefore
/// counted per `(line, ProbeCategory)`, not per line alone: a ternary at ordinal 0 and a boolop
/// at ordinal 1 on the same line would otherwise send the worker looking for a *second* boolop
/// chain that doesn't exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ProbeCategory {
    /// A single `POP_JUMP_IF_*` settles the whole outcome — ternary, inline-if, comprehension-if.
    Test,
    /// A chain of `JUMP_IF_*_OR_POP` sharing one target — boolop in value position.
    Chain,
}

/// Per-`(physical line, ProbeCategory)` counter for [`OutcomeEvidence::FineGrained`] ordinals —
/// shared across both passes (the statement walk and the [`ExprCollector`]) so a line mixing
/// same-category kinds (e.g. an inline-if whose body is itself a ternary) still gets distinct
/// ordinals within that category.
type Ordinals = HashMap<(u32, ProbeCategory), u32>;

/// Shared mutable state threaded through both branch-enumeration passes — see the module doc.
#[derive(Default)]
struct CollectCtx {
    ordinals: Ordinals,
    /// Lines carrying a test whose [`contains_conditional`] is true, from ANY test position
    /// (`if`/`elif`, `while`, a ternary's test, a comprehension guard) — regardless of whether
    /// that particular test's own construct ends up `FineGrained` or not.
    polluted_lines: HashSet<u32>,
    /// Per-line count of test positions anchored there (an `if`/`elif` test, a `while` test, a
    /// ternary's test, a comprehension guard) — same-line or not, resolvable or not. The
    /// landing-offset recovery (see [`reconcile_fine_grained`]) only ever trusts a `polluted_lines`
    /// line whose count here is exactly 1: with two or more test positions sharing one physical
    /// line, the worker's bytecode scan for one of them could observe instructions that actually
    /// belong to the other.
    positions: HashMap<u32, u32>,
    /// Lines carrying a test position whose own test [`contains_unprovable_shape`] — never recoverable
    /// (see [`contains_unprovable_shape`]'s doc), regardless of `positions`' count.
    unprovable_lines: HashSet<u32>,
}

fn next_ordinal(ctx: &mut CollectCtx, line: u32, category: ProbeCategory) -> u32 {
    let slot = ctx.ordinals.entry((line, category)).or_insert(0);
    let ordinal = *slot;
    *slot += 1;
    ordinal
}

/// Records one test-position anchor at `line` (an `if`/`elif`/`while` test, a ternary's test, a
/// comprehension guard) into [`CollectCtx`]'s `positions`/`unprovable_lines`/`polluted_lines` —
/// called at EVERY test position (if/elif/while tests in the statement walk; ternary tests and
/// comprehension guards in [`ExprCollector`]), regardless of that test's own resolvability. See
/// [`reconcile_fine_grained`] for how these three facts gate the landing-offset recovery.
fn record_test_position(ctx: &mut CollectCtx, test: &ast::Expr, line: u32) {
    *ctx.positions.entry(line).or_insert(0) += 1;
    if contains_unprovable_shape(test) {
        ctx.unprovable_lines.insert(line);
    }
    if contains_conditional(test) {
        ctx.polluted_lines.insert(line);
    }
}

/// Reconciles every `FineGrained` outcome assigned optimistically during the two enumeration
/// passes against the now-complete `positions`/`unprovable_lines`/`polluted_lines` facts — the final
/// step of `collect_branches`. A same-line Test-category outcome
/// (`Ternary`/`InlineIf`/`ComprehensionIf`/`While`) or a test-position `BoolOp` (one whose
/// evidence already carries `compound: true` — see [`suppress_test_boolops`]; a value-position
/// `BoolOp` never does and is left untouched here) demotes to `Unobservable` UNLESS its line is
/// either not `polluted_lines` at all (nothing on it was ever compound — the legacy
/// single-instruction resolution, still `compound: false`, is exactly as reliable as before this
/// recovery pass existed) or hosts EXACTLY ONE test position with no ternary anywhere in that
/// position's own test (the landing-offset scheme's precondition — see the module doc). Anything
/// else — two or more test positions sharing a line, or a ternary anywhere in the sole one's own
/// test — stays conservatively demoted, exactly as `demote_polluted_lines` did before this pass
/// existed.
fn reconcile_fine_grained(out: &mut [BranchPoint], ctx: &CollectCtx) {
    for bp in out.iter_mut() {
        let is_boolop = bp.kind == BranchKind::BoolOp;
        let is_test_category = matches!(
            bp.kind,
            BranchKind::Ternary | BranchKind::InlineIf | BranchKind::ComprehensionIf | BranchKind::While
        );
        if !is_test_category && !is_boolop {
            continue;
        }
        if is_boolop
            && !bp.outcomes.iter().any(|o| matches!(o.evidence, OutcomeEvidence::FineGrained(_, _, true)))
        {
            continue;
        }
        if !ctx.polluted_lines.contains(&bp.line) {
            continue;
        }
        let recoverable = ctx.positions.get(&bp.line) == Some(&1) && !ctx.unprovable_lines.contains(&bp.line);
        if !recoverable {
            for outcome in &mut bp.outcomes {
                if matches!(outcome.evidence, OutcomeEvidence::FineGrained(..)) {
                    outcome.evidence = OutcomeEvidence::Unobservable;
                }
            }
        }
    }
}

pub(in crate::analyze) fn collect_branches(
    body: &[ast::Stmt],
    line_index: &LineIndex,
) -> Vec<BranchPoint> {
    let mut out = Vec::new();
    let mut ctx = CollectCtx::default();
    walk(body, line_index, None, &mut ctx, &mut out);
    let mut expr_collector = ExprCollector {
        line_index,
        ctx: &mut ctx,
        suppress: HashSet::new(),
        test_position_boolops: HashSet::new(),
        out: &mut out,
    };
    for stmt in body {
        expr_collector.visit_stmt(stmt);
    }
    reconcile_fine_grained(&mut out, &ctx);
    out
}

fn line_at(offset: TextSize, line_index: &LineIndex) -> u32 {
    line_index.line_index(offset).get() as u32
}

fn first_line(stmts: &[ast::Stmt], line_index: &LineIndex) -> Option<u32> {
    stmts.first().map(|s| line_at(s.range().start(), line_index))
}

/// A same-line construct: the test/header and the body's first statement share one traced line,
/// so line tracing can't tell the outcomes apart — see `BranchKind::InlineIf`.
fn is_same_line(test_line: u32, body_line: u32) -> bool {
    body_line == test_line
}

/// `same_line`'s second element: `None` when a same-line occurrence of this construct has no
/// opcode shape this pass knows how to resolve at all (`for`'s same-line iteration test uses
/// `FOR_ITER`, out of this pass's scope — stays `Unobservable`); `Some((test, force_compound))`
/// when it does (`if`/`elif`, whose `test` decides `compound` itself — see
/// [`OutcomeEvidence::FineGrained`] — and `while`, which always needs the landing-offset chain
/// resolution regardless of its own test's complexity, since CPython's loop-rotation duplicates
/// even a simple test into two differently-polarized copies — see the module doc). The
/// optimistic `FineGrained` assigned here is reconciled against the line's final pollution facts
/// by [`reconcile_fine_grained`], once both enumeration passes are complete.
fn two_way(
    kind: BranchKind,
    line: u32,
    outcome_names: (&str, &str),
    body_line: Option<u32>,
    false_target: Option<u32>,
    same_line: (BranchKind, Option<(&ast::Expr, bool)>),
    ctx: &mut CollectCtx,
) -> Option<BranchPoint> {
    let (true_outcome, false_outcome) = outcome_names;
    let (same_line_kind, same_line_test) = same_line;
    let body_line = body_line?;
    if is_same_line(line, body_line) {
        let evidence = match same_line_test {
            Some((test, force_compound)) => {
                let compound = force_compound || contains_conditional(test);
                OutcomeEvidence::FineGrained(line, next_ordinal(ctx, line, ProbeCategory::Test), compound)
            }
            None => OutcomeEvidence::Unobservable,
        };
        return Some(BranchPoint {
            kind: same_line_kind,
            line,
            outcomes: vec![
                BranchPointOutcome { outcome: true_outcome.to_string(), evidence },
                BranchPointOutcome { outcome: false_outcome.to_string(), evidence },
            ],
        });
    }
    let false_evidence = match false_target {
        Some(t) => OutcomeEvidence::Arc(line, t),
        None => OutcomeEvidence::Unobservable,
    };
    Some(BranchPoint {
        kind,
        line,
        outcomes: vec![
            BranchPointOutcome {
                outcome: true_outcome.to_string(),
                evidence: OutcomeEvidence::Arc(line, body_line),
            },
            BranchPointOutcome {
                outcome: false_outcome.to_string(),
                evidence: false_evidence,
            },
        ],
    })
}

fn walk(
    body: &[ast::Stmt],
    line_index: &LineIndex,
    fallthrough: Option<u32>,
    ctx: &mut CollectCtx,
    out: &mut Vec<BranchPoint>,
) {
    for (i, stmt) in body.iter().enumerate() {
        let next = if i + 1 < body.len() {
            Some(line_at(body[i + 1].range().start(), line_index))
        } else {
            fallthrough
        };
        walk_stmt(stmt, line_index, next, ctx, out);
    }
}

fn walk_stmt(
    stmt: &ast::Stmt,
    line_index: &LineIndex,
    next: Option<u32>,
    ctx: &mut CollectCtx,
    out: &mut Vec<BranchPoint>,
) {
    match stmt {
        ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
        ast::Stmt::If(if_stmt) => walk_if(if_stmt, line_index, next, ctx, out),
        ast::Stmt::For(for_stmt) => walk_for(for_stmt, line_index, next, ctx, out),
        ast::Stmt::While(while_stmt) => walk_while(while_stmt, line_index, next, ctx, out),
        ast::Stmt::With(with_stmt) => walk(&with_stmt.body, line_index, next, ctx, out),
        ast::Stmt::Try(try_stmt) => walk_try(try_stmt, line_index, next, ctx, out),
        ast::Stmt::Match(match_stmt) => walk_match(match_stmt, line_index, next, ctx, out),
        _ => {}
    }
}

/// The line control lands on if this `elif`/`else` clause (at `clauses[idx]`) is entered: an
/// `elif`'s own test line (CPython traces it), or an `else`'s first body line.
fn clause_entry_line(clause: &ast::ElifElseClause, line_index: &LineIndex) -> Option<u32> {
    if clause.test.is_some() {
        Some(line_at(clause.range().start(), line_index))
    } else {
        first_line(&clause.body, line_index)
    }
}

/// Where control lands when every test up to and including `clauses[idx]` is false: the entry
/// line of `clauses[idx]`, or `next` (the fall-through) if there's no such clause.
fn false_target_from(
    clauses: &[ast::ElifElseClause],
    idx: usize,
    next: Option<u32>,
    line_index: &LineIndex,
) -> Option<u32> {
    match clauses.get(idx) {
        Some(clause) => clause_entry_line(clause, line_index),
        None => next,
    }
}

fn walk_if(
    if_stmt: &ast::StmtIf,
    line_index: &LineIndex,
    next: Option<u32>,
    ctx: &mut CollectCtx,
    out: &mut Vec<BranchPoint>,
) {
    let line = line_at(if_stmt.range().start(), line_index);
    let body_line = first_line(&if_stmt.body, line_index);
    let false_target = false_target_from(&if_stmt.elif_else_clauses, 0, next, line_index);
    if let Some(bp) = two_way(
        BranchKind::If,
        line,
        ("true", "false"),
        body_line,
        false_target,
        (BranchKind::InlineIf, Some((&if_stmt.test, false))),
        ctx,
    ) {
        out.push(bp);
    }
    walk(&if_stmt.body, line_index, next, ctx, out);

    for (i, clause) in if_stmt.elif_else_clauses.iter().enumerate() {
        if let Some(test) = &clause.test {
            let clause_line = line_at(clause.range().start(), line_index);
            let clause_body_line = first_line(&clause.body, line_index);
            let false_target = false_target_from(&if_stmt.elif_else_clauses, i + 1, next, line_index);
            if let Some(bp) = two_way(
                BranchKind::If,
                clause_line,
                ("true", "false"),
                clause_body_line,
                false_target,
                (BranchKind::InlineIf, Some((test, false))),
                ctx,
            ) {
                out.push(bp);
            }
        }
        walk(&clause.body, line_index, next, ctx, out);
    }
}

fn walk_for(
    for_stmt: &ast::StmtFor,
    line_index: &LineIndex,
    next: Option<u32>,
    ctx: &mut CollectCtx,
    out: &mut Vec<BranchPoint>,
) {
    let line = line_at(for_stmt.range().start(), line_index);
    let body_line = first_line(&for_stmt.body, line_index);
    let empty_target = if for_stmt.orelse.is_empty() {
        next
    } else {
        first_line(&for_stmt.orelse, line_index)
    };
    if let Some(bp) = two_way(
        BranchKind::For,
        line,
        ("iterate", "empty"),
        body_line,
        empty_target,
        (BranchKind::For, None),
        ctx,
    ) {
        out.push(bp);
    }
    walk(&for_stmt.body, line_index, Some(line), ctx, out);
    walk(&for_stmt.orelse, line_index, next, ctx, out);
}

fn walk_while(
    while_stmt: &ast::StmtWhile,
    line_index: &LineIndex,
    next: Option<u32>,
    ctx: &mut CollectCtx,
    out: &mut Vec<BranchPoint>,
) {
    let line = line_at(while_stmt.range().start(), line_index);
    let body_line = first_line(&while_stmt.body, line_index);
    let skip_target = if while_stmt.orelse.is_empty() {
        next
    } else {
        first_line(&while_stmt.orelse, line_index)
    };
    if let Some(bp) = two_way(
        BranchKind::While,
        line,
        ("enter", "skip"),
        body_line,
        skip_target,
        (BranchKind::While, Some((&while_stmt.test, true))),
        ctx,
    ) {
        out.push(bp);
    }
    walk(&while_stmt.body, line_index, Some(line), ctx, out);
    walk(&while_stmt.orelse, line_index, next, ctx, out);
}

fn walk_try(
    try_stmt: &ast::StmtTry,
    line_index: &LineIndex,
    next: Option<u32>,
    ctx: &mut CollectCtx,
    out: &mut Vec<BranchPoint>,
) {
    walk(&try_stmt.body, line_index, next, ctx, out);
    for handler in &try_stmt.handlers {
        let ast::ExceptHandler::ExceptHandler(h) = handler;
        let h_line = line_at(h.range().start(), line_index);
        out.push(BranchPoint {
            kind: BranchKind::Except,
            line: h_line,
            outcomes: vec![BranchPointOutcome {
                outcome: "entered".to_string(),
                evidence: OutcomeEvidence::Line(h_line),
            }],
        });
        walk(&h.body, line_index, next, ctx, out);
    }
    if let Some(else_line) = first_line(&try_stmt.orelse, line_index) {
        out.push(BranchPoint {
            kind: BranchKind::TryElse,
            line: else_line,
            outcomes: vec![BranchPointOutcome {
                outcome: "entered".to_string(),
                evidence: OutcomeEvidence::Line(else_line),
            }],
        });
    }
    walk(&try_stmt.orelse, line_index, next, ctx, out);
    walk(&try_stmt.finalbody, line_index, next, ctx, out);
}

fn walk_match(
    match_stmt: &ast::StmtMatch,
    line_index: &LineIndex,
    next: Option<u32>,
    ctx: &mut CollectCtx,
    out: &mut Vec<BranchPoint>,
) {
    for case in &match_stmt.cases {
        let case_line = line_at(case.range().start(), line_index);
        out.push(BranchPoint {
            kind: BranchKind::Match,
            line: case_line,
            outcomes: vec![BranchPointOutcome {
                outcome: "entered".to_string(),
                evidence: OutcomeEvidence::Line(case_line),
            }],
        });
        walk(&case.body, line_index, next, ctx, out);
    }
}

/// Finds every same-line construct (ternary, boolop, comprehension guard) anywhere in the body —
/// these need no fall-through context, just every expression reached.
///
/// `suppress` holds the range-start of every `BoolOp` CPython folds into an enclosing construct's
/// own control-flow jump chain rather than compiling as a value — every `BoolOp` reached only
/// through further `BoolOp` operands (`a and (b or c)`'s inner `b or c`), which never gets its own
/// attributable instruction group — see [`mark_boolop_chain`]/[`mark_boolop_children`]. A node in
/// `suppress` is reported `Unobservable`, never `FineGrained`.
///
/// `test_position_boolops` holds the range-start of every `BoolOp` that IS directly a test's own
/// expression (`if a and b:`, `x if (a or b) else y`, a comprehension guard `if a and b`) — see
/// [`suppress_test_boolops`]. Never suppressed, but its evidence's `compound` bit is set from
/// membership here rather than always `true`: unlike Test-category outcomes, this is the ONLY way
/// to tell a test-position `BoolOp` (needs the landing-offset chain resolution — see the module
/// doc) from an ordinary value-position one (needs the older `JUMP_IF_*_OR_POP` chain lookup,
/// `compound: false`) once both have become plain `Expr::BoolOp` nodes in the walk.
struct ExprCollector<'a> {
    line_index: &'a LineIndex,
    ctx: &'a mut CollectCtx,
    suppress: HashSet<TextSize>,
    test_position_boolops: HashSet<TextSize>,
    out: &'a mut Vec<BranchPoint>,
}

impl<'ast> Visitor<'ast> for ExprCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => return,
            ast::Stmt::If(if_stmt) => {
                let line = line_at(if_stmt.range().start(), self.line_index);
                record_test_position(self.ctx, &if_stmt.test, line);
                if suppress_test_boolops(&if_stmt.test, &mut self.suppress) {
                    self.test_position_boolops.insert(if_stmt.test.range().start());
                }
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        let clause_line = line_at(clause.range().start(), self.line_index);
                        record_test_position(self.ctx, test, clause_line);
                        if suppress_test_boolops(test, &mut self.suppress) {
                            self.test_position_boolops.insert(test.range().start());
                        }
                    }
                }
            }
            ast::Stmt::While(while_stmt) => {
                let line = line_at(while_stmt.range().start(), self.line_index);
                record_test_position(self.ctx, &while_stmt.test, line);
                if suppress_test_boolops(&while_stmt.test, &mut self.suppress) {
                    self.test_position_boolops.insert(while_stmt.test.range().start());
                }
            }
            _ => {}
        }
        visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        match expr {
            ast::Expr::If(if_expr) => {
                let line = line_at(if_expr.range().start(), self.line_index);
                // Optimistically assigned regardless of the test's own complexity — a compound
                // test is still resolvable via the landing-offset chain (`compound: true`) — and
                // reconciled against the line's final pollution facts by `reconcile_fine_grained`.
                let compound = contains_conditional(&if_expr.test);
                let evidence =
                    OutcomeEvidence::FineGrained(line, next_ordinal(self.ctx, line, ProbeCategory::Test), compound);
                record_test_position(self.ctx, &if_expr.test, line);
                if suppress_test_boolops(&if_expr.test, &mut self.suppress) {
                    self.test_position_boolops.insert(if_expr.test.range().start());
                }
                self.out.push(BranchPoint {
                    kind: BranchKind::Ternary,
                    line,
                    outcomes: vec![
                        BranchPointOutcome { outcome: "true".to_string(), evidence },
                        BranchPointOutcome { outcome: "false".to_string(), evidence },
                    ],
                });
            }
            ast::Expr::BoolOp(bool_op) => {
                let line = line_at(bool_op.range().start(), self.line_index);
                let start = bool_op.range().start();
                let evidence = if self.suppress.contains(&start) {
                    OutcomeEvidence::Unobservable
                } else {
                    let compound = self.test_position_boolops.contains(&start);
                    OutcomeEvidence::FineGrained(line, next_ordinal(self.ctx, line, ProbeCategory::Chain), compound)
                };
                self.out.push(BranchPoint {
                    kind: BranchKind::BoolOp,
                    line,
                    outcomes: vec![
                        BranchPointOutcome { outcome: "short_circuit".to_string(), evidence },
                        BranchPointOutcome { outcome: "full_evaluation".to_string(), evidence },
                    ],
                });
            }
            _ => {}
        }
        visitor::walk_expr(self, expr);
    }

    fn visit_comprehension(&mut self, comprehension: &'ast ast::Comprehension) {
        for cond in &comprehension.ifs {
            let line = line_at(cond.range().start(), self.line_index);
            // A comprehension guard's own range IS its test's range, so `contains_conditional`
            // and `suppress_test_boolops` here agree with the direct check used above.
            let compound = contains_conditional(cond);
            let evidence =
                OutcomeEvidence::FineGrained(line, next_ordinal(self.ctx, line, ProbeCategory::Test), compound);
            record_test_position(self.ctx, cond, line);
            if suppress_test_boolops(cond, &mut self.suppress) {
                self.test_position_boolops.insert(cond.range().start());
            }
            self.out.push(BranchPoint {
                kind: BranchKind::ComprehensionIf,
                line,
                outcomes: vec![
                    BranchPointOutcome { outcome: "true".to_string(), evidence },
                    BranchPointOutcome { outcome: "false".to_string(), evidence },
                ],
            });
        }
        visitor::walk_comprehension(self, comprehension);
    }
}
