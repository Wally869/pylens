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
//! and expands a compound boolop into one jump per operand). This pass does not attempt to
//! attribute an outcome across that — the construct's own outcome, and any `BoolOp` folded into
//! its jump chain (see [`mark_boolop_chain`]), stay `Unobservable`.
//!
//! Demoting only the complex construct itself isn't enough, though: `python/worker.py` indexes a
//! flat, offset-sorted `test_groups[line]` built from EVERY `POP_JUMP_IF_*` instruction physically
//! on a line, regardless of which AST node it came from — so a demoted-but-still-compiled
//! instruction still shifts the index any OTHER, otherwise-simple, `FineGrained` Test-category
//! sibling on that SAME line would land on. `collect_branches`'s final pass therefore demotes
//! EVERY Test-category outcome on a line carrying any such test to `Unobservable`, not just the
//! complex construct's own — see `polluted_lines`.

use std::collections::{HashMap, HashSet};

use ruff_python_ast as ast;
use ruff_python_ast::visitor::{self, Visitor};
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextSize};

use crate::model::branch::{BranchKind, BranchPoint, BranchPointOutcome, OutcomeEvidence};

/// Whether `expr` contains, anywhere within it, a `BoolOp` (`a and b`, `a or b`) or a ternary
/// (`Expr::If`) — either compiles to MULTIPLE jump instructions when embedded in a test position
/// (a compound boolop expands to a chain; a nested ternary's own decision gets duplicated across
/// the branches of whatever consumes its value), never the single instruction a simple test
/// compiles to. Does not descend into a nested comprehension's or lambda's own body: those compile
/// to a SEPARATE code object, called once, so whatever conditional logic lives inside one can
/// never duplicate or chain-expand the ENCLOSING test's instructions.
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
    /// that particular test's own construct ends up `FineGrained` or not. See the module doc's
    /// second paragraph for why every Test-category outcome on such a line is demoted, not just
    /// the complex test's own.
    polluted_lines: HashSet<u32>,
}

fn next_ordinal(ctx: &mut CollectCtx, line: u32, category: ProbeCategory) -> u32 {
    let slot = ctx.ordinals.entry((line, category)).or_insert(0);
    let ordinal = *slot;
    *slot += 1;
    ordinal
}

/// Records `line` as polluted when `test`'s [`contains_conditional`] — called at EVERY test
/// position (if/elif/while tests in the statement walk; ternary tests and comprehension guards in
/// [`ExprCollector`]), regardless of that test's own resolvability.
fn mark_if_polluted(ctx: &mut CollectCtx, test: &ast::Expr, line: u32) {
    if contains_conditional(test) {
        ctx.polluted_lines.insert(line);
    }
}

/// Demotes every Test-category (`Ternary`/`InlineIf`/`ComprehensionIf`) outcome on a
/// `polluted_lines` line to `Unobservable` — the final step of `collect_branches`, applied after
/// both passes (and thus every `polluted_lines` entry) are complete. `Chain`-category (`BoolOp`)
/// outcomes are untouched: a compound or nested-conditional test never contributes the
/// `JUMP_IF_*_OR_POP` instructions a value-position boolop chain resolves against, so `BoolOp`'s
/// own ordinal-indexed group on the worker side is never polluted by this.
fn demote_polluted_lines(out: &mut [BranchPoint], polluted_lines: &HashSet<u32>) {
    for bp in out {
        if matches!(bp.kind, BranchKind::Ternary | BranchKind::InlineIf | BranchKind::ComprehensionIf)
            && polluted_lines.contains(&bp.line)
        {
            for outcome in &mut bp.outcomes {
                outcome.evidence = OutcomeEvidence::Unobservable;
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
        out: &mut out,
    };
    for stmt in body {
        expr_collector.visit_stmt(stmt);
    }
    demote_polluted_lines(&mut out, &ctx.polluted_lines);
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

/// `same_line`'s second element (`resolvable_same_line`): whether a same-line occurrence of this
/// construct can be resolved via opcode-level tracing (`true` for `if`/`elif` — a single
/// `POP_JUMP_IF_*` the worker can match; `false` for `for`/`while`, whose same-line iteration
/// test uses `FOR_ITER`/its own opcode and isn't decoded here — out of this pass's scope, so it
/// stays `Unobservable`).
fn two_way(
    kind: BranchKind,
    line: u32,
    outcome_names: (&str, &str),
    body_line: Option<u32>,
    false_target: Option<u32>,
    same_line: (BranchKind, bool),
    ctx: &mut CollectCtx,
) -> Option<BranchPoint> {
    let (true_outcome, false_outcome) = outcome_names;
    let (same_line_kind, resolvable_same_line) = same_line;
    let body_line = body_line?;
    if is_same_line(line, body_line) {
        let evidence = if resolvable_same_line {
            OutcomeEvidence::FineGrained(line, next_ordinal(ctx, line, ProbeCategory::Test))
        } else {
            OutcomeEvidence::Unobservable
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
        (BranchKind::InlineIf, !contains_conditional(&if_stmt.test)),
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
                (BranchKind::InlineIf, !contains_conditional(test)),
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
        (BranchKind::For, false),
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
        (BranchKind::While, false),
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
/// `suppress` holds the range-start of every `BoolOp` CPython compiles into an enclosing
/// construct's own control-flow jump chain rather than as a value (a control construct's — `if`,
/// `while`, a ternary's test, a comprehension guard — compound test, and any `BoolOp` nested
/// inside one through further `BoolOp` operands) — see [`mark_boolop_chain`]. A node in
/// `suppress` is reported `Unobservable`, never `FineGrained`: its instructions aren't the
/// single test or uniform-target chain this pass knows how to attribute an outcome to.
struct ExprCollector<'a> {
    line_index: &'a LineIndex,
    ctx: &'a mut CollectCtx,
    suppress: HashSet<TextSize>,
    out: &'a mut Vec<BranchPoint>,
}

/// `Unobservable` when `unresolvable`, else `FineGrained(line, next ordinal)` — no ordinal is
/// spent for an `Unobservable` outcome.
fn fine_or_unobservable(
    unresolvable: bool,
    line: u32,
    category: ProbeCategory,
    ctx: &mut CollectCtx,
) -> OutcomeEvidence {
    if unresolvable {
        OutcomeEvidence::Unobservable
    } else {
        OutcomeEvidence::FineGrained(line, next_ordinal(ctx, line, category))
    }
}

impl<'ast> Visitor<'ast> for ExprCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => return,
            ast::Stmt::If(if_stmt) => {
                let line = line_at(if_stmt.range().start(), self.line_index);
                mark_if_polluted(self.ctx, &if_stmt.test, line);
                mark_boolop_chain(&if_stmt.test, &mut self.suppress);
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        let clause_line = line_at(clause.range().start(), self.line_index);
                        mark_if_polluted(self.ctx, test, clause_line);
                        mark_boolop_chain(test, &mut self.suppress);
                    }
                }
            }
            ast::Stmt::While(while_stmt) => {
                let line = line_at(while_stmt.range().start(), self.line_index);
                mark_if_polluted(self.ctx, &while_stmt.test, line);
                mark_boolop_chain(&while_stmt.test, &mut self.suppress);
            }
            _ => {}
        }
        visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        match expr {
            ast::Expr::If(if_expr) => {
                let line = line_at(if_expr.range().start(), self.line_index);
                // The ternary's OWN start differs from its `test`'s start, so its resolvability
                // is a direct `contains_conditional` check, not a `suppress` lookup (that's only
                // for the nested `BoolOp` node itself, marked below and consulted when the walk
                // reaches it in the `Expr::BoolOp` arm).
                let evidence =
                    fine_or_unobservable(contains_conditional(&if_expr.test), line, ProbeCategory::Test, self.ctx);
                mark_if_polluted(self.ctx, &if_expr.test, line);
                mark_boolop_chain(&if_expr.test, &mut self.suppress);
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
                let unresolvable = self.suppress.contains(&bool_op.range().start());
                let evidence = fine_or_unobservable(unresolvable, line, ProbeCategory::Chain, self.ctx);
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
            // A comprehension guard's own range IS its test's range, so `contains_conditional(cond)`
            // and a `suppress` lookup on `cond`'s own start agree — using the direct check keeps
            // this symmetric with the ternary arm above.
            let evidence = fine_or_unobservable(contains_conditional(cond), line, ProbeCategory::Test, self.ctx);
            mark_if_polluted(self.ctx, cond, line);
            mark_boolop_chain(cond, &mut self.suppress);
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
