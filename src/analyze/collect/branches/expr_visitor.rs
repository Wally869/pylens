//! The `ExprCollector` visitor: same-line constructs (ternary, boolop, comprehension guard)
//! reached anywhere in the body, plus the suppress/test-position bookkeeping that keeps a
//! chain-folded `BoolOp` from double-reporting.

use super::*;
use super::walk::line_at;

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

impl< 'ast > Visitor < 'ast > for ExprCollector < '_ > {
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
}
