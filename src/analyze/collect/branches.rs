//! Enumerates a function's branch points from its AST for `record`'s per-branch-outcome
//! accounting: every `if`/`elif`, `while`, `for`, `except` arm, `try`/`else`, `match` arm, plus
//! the same-line constructs line tracing can never distinguish (ternaries, `and`/`or`
//! short-circuits, single-line `if x: y` bodies, comprehension `if` guards). Each branch point
//! carries every one of its outcomes, together with the runtime evidence that would prove it
//! happened — a line arc, a bare line, or `Unobservable` for the same-line constructs. Nothing is
//! dropped: an outcome with no distinguishing evidence is still enumerated, just marked
//! unobservable — see `record::branch_report_for`, which turns this into the reported
//! `covered`/`uncovered`/`unobservable_line_granularity` status.
//!
//! Two passes over the body: a hand-written statement walk (mirrors `collect::body_lines`, but
//! also threads the *fall-through line* — where control lands after a body if it runs off the
//! end — needed to compute an else-less `if`'s false-arc target, a `for`'s empty-arc target, and
//! a `while`'s skip-arc target) for the control-flow constructs, and a generic AST
//! [`Visitor`] for the same-line expression constructs (which need no fall-through context, just
//! every expression in the body). A nested `def`/`class` is not descended into by either pass —
//! its own branches belong to that function, not this one — matching `body_lines`.

use ruff_python_ast as ast;
use ruff_python_ast::visitor::{self, Visitor};
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextSize};

use crate::model::branch::{BranchKind, BranchPoint, BranchPointOutcome, OutcomeEvidence};

pub(in crate::analyze) fn collect_branches(
    body: &[ast::Stmt],
    line_index: &LineIndex,
) -> Vec<BranchPoint> {
    let mut out = Vec::new();
    walk(body, line_index, None, &mut out);
    let mut expr_collector = ExprCollector {
        line_index,
        out: &mut out,
    };
    for stmt in body {
        expr_collector.visit_stmt(stmt);
    }
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

fn two_way(
    kind: BranchKind,
    same_line_kind: BranchKind,
    line: u32,
    true_outcome: &str,
    false_outcome: &str,
    body_line: Option<u32>,
    false_target: Option<u32>,
) -> Option<BranchPoint> {
    let body_line = body_line?;
    if is_same_line(line, body_line) {
        return Some(BranchPoint {
            kind: same_line_kind,
            line,
            outcomes: vec![
                BranchPointOutcome {
                    outcome: true_outcome.to_string(),
                    evidence: OutcomeEvidence::Unobservable,
                },
                BranchPointOutcome {
                    outcome: false_outcome.to_string(),
                    evidence: OutcomeEvidence::Unobservable,
                },
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

fn walk(body: &[ast::Stmt], line_index: &LineIndex, fallthrough: Option<u32>, out: &mut Vec<BranchPoint>) {
    for (i, stmt) in body.iter().enumerate() {
        let next = if i + 1 < body.len() {
            Some(line_at(body[i + 1].range().start(), line_index))
        } else {
            fallthrough
        };
        walk_stmt(stmt, line_index, next, out);
    }
}

fn walk_stmt(stmt: &ast::Stmt, line_index: &LineIndex, next: Option<u32>, out: &mut Vec<BranchPoint>) {
    match stmt {
        ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
        ast::Stmt::If(if_stmt) => walk_if(if_stmt, line_index, next, out),
        ast::Stmt::For(for_stmt) => walk_for(for_stmt, line_index, next, out),
        ast::Stmt::While(while_stmt) => walk_while(while_stmt, line_index, next, out),
        ast::Stmt::With(with_stmt) => walk(&with_stmt.body, line_index, next, out),
        ast::Stmt::Try(try_stmt) => walk_try(try_stmt, line_index, next, out),
        ast::Stmt::Match(match_stmt) => walk_match(match_stmt, line_index, next, out),
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

fn walk_if(if_stmt: &ast::StmtIf, line_index: &LineIndex, next: Option<u32>, out: &mut Vec<BranchPoint>) {
    let line = line_at(if_stmt.range().start(), line_index);
    let body_line = first_line(&if_stmt.body, line_index);
    let false_target = false_target_from(&if_stmt.elif_else_clauses, 0, next, line_index);
    if let Some(bp) = two_way(
        BranchKind::If,
        BranchKind::InlineIf,
        line,
        "true",
        "false",
        body_line,
        false_target,
    ) {
        out.push(bp);
    }
    walk(&if_stmt.body, line_index, next, out);

    for (i, clause) in if_stmt.elif_else_clauses.iter().enumerate() {
        if clause.test.is_some() {
            let clause_line = line_at(clause.range().start(), line_index);
            let clause_body_line = first_line(&clause.body, line_index);
            let false_target = false_target_from(&if_stmt.elif_else_clauses, i + 1, next, line_index);
            if let Some(bp) = two_way(
                BranchKind::If,
                BranchKind::InlineIf,
                clause_line,
                "true",
                "false",
                clause_body_line,
                false_target,
            ) {
                out.push(bp);
            }
        }
        walk(&clause.body, line_index, next, out);
    }
}

fn walk_for(for_stmt: &ast::StmtFor, line_index: &LineIndex, next: Option<u32>, out: &mut Vec<BranchPoint>) {
    let line = line_at(for_stmt.range().start(), line_index);
    let body_line = first_line(&for_stmt.body, line_index);
    let empty_target = if for_stmt.orelse.is_empty() {
        next
    } else {
        first_line(&for_stmt.orelse, line_index)
    };
    if let Some(bp) = two_way(
        BranchKind::For,
        BranchKind::For,
        line,
        "iterate",
        "empty",
        body_line,
        empty_target,
    ) {
        out.push(bp);
    }
    walk(&for_stmt.body, line_index, Some(line), out);
    walk(&for_stmt.orelse, line_index, next, out);
}

fn walk_while(while_stmt: &ast::StmtWhile, line_index: &LineIndex, next: Option<u32>, out: &mut Vec<BranchPoint>) {
    let line = line_at(while_stmt.range().start(), line_index);
    let body_line = first_line(&while_stmt.body, line_index);
    let skip_target = if while_stmt.orelse.is_empty() {
        next
    } else {
        first_line(&while_stmt.orelse, line_index)
    };
    if let Some(bp) = two_way(
        BranchKind::While,
        BranchKind::While,
        line,
        "enter",
        "skip",
        body_line,
        skip_target,
    ) {
        out.push(bp);
    }
    walk(&while_stmt.body, line_index, Some(line), out);
    walk(&while_stmt.orelse, line_index, next, out);
}

fn walk_try(try_stmt: &ast::StmtTry, line_index: &LineIndex, next: Option<u32>, out: &mut Vec<BranchPoint>) {
    walk(&try_stmt.body, line_index, next, out);
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
        walk(&h.body, line_index, next, out);
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
    walk(&try_stmt.orelse, line_index, next, out);
    walk(&try_stmt.finalbody, line_index, next, out);
}

fn walk_match(match_stmt: &ast::StmtMatch, line_index: &LineIndex, next: Option<u32>, out: &mut Vec<BranchPoint>) {
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
        walk(&case.body, line_index, next, out);
    }
}

/// Finds every same-line construct (ternary, boolop, comprehension guard) anywhere in the body —
/// these need no fall-through context, just every expression reached.
struct ExprCollector<'a> {
    line_index: &'a LineIndex,
    out: &'a mut Vec<BranchPoint>,
}

impl<'ast> Visitor<'ast> for ExprCollector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        if matches!(stmt, ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_)) {
            return;
        }
        visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        match expr {
            ast::Expr::If(if_expr) => {
                let line = line_at(if_expr.range().start(), self.line_index);
                self.out.push(BranchPoint {
                    kind: BranchKind::Ternary,
                    line,
                    outcomes: vec![
                        BranchPointOutcome {
                            outcome: "true".to_string(),
                            evidence: OutcomeEvidence::Unobservable,
                        },
                        BranchPointOutcome {
                            outcome: "false".to_string(),
                            evidence: OutcomeEvidence::Unobservable,
                        },
                    ],
                });
            }
            ast::Expr::BoolOp(bool_op) => {
                let line = line_at(bool_op.range().start(), self.line_index);
                self.out.push(BranchPoint {
                    kind: BranchKind::BoolOp,
                    line,
                    outcomes: vec![
                        BranchPointOutcome {
                            outcome: "short_circuit".to_string(),
                            evidence: OutcomeEvidence::Unobservable,
                        },
                        BranchPointOutcome {
                            outcome: "full_evaluation".to_string(),
                            evidence: OutcomeEvidence::Unobservable,
                        },
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
            self.out.push(BranchPoint {
                kind: BranchKind::ComprehensionIf,
                line,
                outcomes: vec![
                    BranchPointOutcome {
                        outcome: "true".to_string(),
                        evidence: OutcomeEvidence::Unobservable,
                    },
                    BranchPointOutcome {
                        outcome: "false".to_string(),
                        evidence: OutcomeEvidence::Unobservable,
                    },
                ],
            });
        }
        visitor::walk_comprehension(self, comprehension);
    }
}
