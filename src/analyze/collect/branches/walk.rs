//! The statement walk: the recursive descent over the body that assigns fall-through-aware
//! branch evidence to `if`/`for`/`while`/`try`/`match` (and their same-line line-index helpers).

use super::*;

pub(super) fn line_at(offset: TextSize, line_index: &LineIndex) -> u32 {
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

pub(super) fn walk(
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
