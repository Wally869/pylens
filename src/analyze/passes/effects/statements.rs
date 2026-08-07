use ruff_python_ast as ast;
use crate::model::*;
use super::super::super::collect::returns::classify_return;
use super::super::super::collect::exceptions::exception_name;
use super::Walker;

impl Walker < '_ , '_ > {
        pub fn visit_body(&mut self, body: &[ast::Stmt]) {
            for stmt in body {
                self.visit_stmt(stmt);
            }
        }

        pub fn visit_stmt(&mut self, stmt: &ast::Stmt) {
            use ast::Stmt;
            match stmt {
                Stmt::Return(ret) => match ret.value.as_deref() {
                    Some(expr) => {
                        let kind = classify_return(expr);
                        self.facts.add_return(kind);
                        self.visit_expr(expr);
                    }
                    None => self.facts.add_return(ReturnKind::None),
                },
                Stmt::Raise(raise) => {
                    if let Some(exc) = raise.exc.as_deref() {
                        if let Some(name) = exception_name(exc) {
                            self.facts.sig.raises.explicit.push(name);
                        }
                        // The exception *constructor* is not an effect — visit only its arguments
                        // so real effects there are still seen, without flagging the exception
                        // type itself as an unknown mutating callee.
                        match exc {
                            ast::Expr::Call(call) => {
                                for arg in call.arguments.args.iter() {
                                    self.visit_expr(arg);
                                }
                                for kw in call.arguments.keywords.iter() {
                                    self.visit_expr(&kw.value);
                                }
                            }
                            _ => self.visit_expr(exc),
                        }
                    }
                }
                Stmt::Assert(assert_stmt) => {
                    // Directly visible in the AST — same high-confidence tier as `raise`.
                    self.facts.sig.raises.explicit.push("AssertionError".to_string());
                    self.note_guard_test(&assert_stmt.test);
                    self.visit_expr(&assert_stmt.test);
                    if let Some(msg) = assert_stmt.msg.as_deref() {
                        self.visit_expr(msg);
                    }
                }
                Stmt::Global(g) => {
                    for name in &g.names {
                        self.facts.globals.insert(name.as_str().to_string());
                    }
                }
                Stmt::Nonlocal(n) => {
                    for name in &n.names {
                        self.facts.nonlocals.insert(name.as_str().to_string());
                    }
                }
                Stmt::Assign(assign) => {
                    self.visit_expr(&assign.value);
                    for target in &assign.targets {
                        self.handle_assign_target(target, &assign.value);
                    }
                }
                Stmt::AugAssign(aug) => {
                    self.visit_expr(&aug.value);
                    self.handle_aug_target(&aug.target);
                }
                Stmt::AnnAssign(ann) => {
                    if let Some(value) = ann.value.as_deref() {
                        self.visit_expr(value);
                        self.handle_assign_target(&ann.target, value);
                    }
                }
                Stmt::Delete(del) => {
                    for target in &del.targets {
                        self.handle_delete_target(target);
                    }
                }
                Stmt::Expr(e) => self.visit_expr(&e.value),
                Stmt::If(if_stmt) => {
                    self.note_guard_test(&if_stmt.test);
                    self.visit_expr(&if_stmt.test);
                    self.visit_body(&if_stmt.body);
                    for clause in &if_stmt.elif_else_clauses {
                        if let Some(test) = &clause.test {
                            self.note_guard_test(test);
                            self.visit_expr(test);
                        }
                        self.visit_body(&clause.body);
                    }
                }
                Stmt::For(for_stmt) => {
                    self.visit_expr(&for_stmt.iter);
                    // `for row in matrix:` binds `row` to an element reached through `matrix` — a
                    // mutation via `row` (`row[i] = ...`, `row.append(...)`, ...) is a (possibly
                    // nested) mutation of `matrix`, so the loop variable aliases the iterable's
                    // root for mutation-attribution purposes, same as a direct `q = p` alias.
                    if let ast::Expr::Name(target) = for_stmt.target.as_ref()
                        && let Some(root) = self.facts.param_root(&for_stmt.iter)
                    {
                        self.facts.aliases.insert(target.id.as_str().to_string(), root);
                    }
                    self.visit_body(&for_stmt.body);
                    self.visit_body(&for_stmt.orelse);
                }
                Stmt::While(while_stmt) => {
                    self.note_guard_test(&while_stmt.test);
                    self.visit_expr(&while_stmt.test);
                    self.visit_body(&while_stmt.body);
                    self.visit_body(&while_stmt.orelse);
                }
                Stmt::With(with_stmt) => {
                    for item in &with_stmt.items {
                        self.visit_expr(&item.context_expr);
                    }
                    self.visit_body(&with_stmt.body);
                }
                Stmt::Try(try_stmt) => {
                    self.visit_body(&try_stmt.body);
                    for handler in &try_stmt.handlers {
                        let ast::ExceptHandler::ExceptHandler(h) = handler;
                        self.visit_body(&h.body);
                    }
                    self.visit_body(&try_stmt.orelse);
                    self.visit_body(&try_stmt.finalbody);
                }
                Stmt::Match(match_stmt) => {
                    self.visit_expr(&match_stmt.subject);
                    for case in &match_stmt.cases {
                        self.visit_body(&case.body);
                    }
                }
                // Nested defs are separate scopes; not descended into yet.
                _ => {}
            }
        }

        /// Visit a comprehension's `for`/`if` clauses: the iterable votes shape like a `for` loop's,
        /// and both the iterable and any `if` guards may contain calls/effects that must be seen.
        pub fn visit_comprehensions(&mut self, generators: &[ast::Comprehension]) {
            for comp in generators {
                self.visit_expr(&comp.iter);
                for cond in &comp.ifs {
                    self.visit_expr(cond);
                }
            }
        }

}
