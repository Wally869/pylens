use std::collections::{HashMap, HashSet};

use ruff_python_ast as ast;

use ruff_python_ast::visitor::{self, Visitor};

use ruff_source_file::LineIndex;

use ruff_text_size::{Ranged, TextSize};

use crate::model::branch::{BranchKind, BranchPoint, BranchPointOutcome, OutcomeEvidence};

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



mod walk;
mod expr_visitor;

use walk::walk;

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

fn next_ordinal(ctx: &mut CollectCtx, line: u32, category: ProbeCategory) -> u32 {
    let slot = ctx.ordinals.entry((line, category)).or_insert(0);
    let ordinal = *slot;
    *slot += 1;
    ordinal
}
