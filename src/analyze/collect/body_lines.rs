//! The line-number denominator for `record`'s executed-line coverage. Walks a function's body
//! recursively through every control-flow construct (`if`/`elif`/`else`, `for`/`orelse`,
//! `while`/`orelse`, `with`, `try`/`except`/`orelse`/`finally`, `match`/`case`), recording the
//! 1-based start line of every statement. A nested `def`/`class` statement's own line is kept
//! (it runs when the enclosing function runs) but its body is not descended into (that only runs
//! when the nested function/class is itself called/used). A bare string-literal expression
//! statement (a docstring, or any other stray string constant used as a statement) is excluded
//! entirely: CPython compiles it to nothing and never emits a line event for it, so it can never
//! be covered — counting it in the denominator would make every documented function look
//! permanently short. An `elif`'s own line IS counted (CPython does trace the condition check),
//! even though ruff models it as an [`ast::ElifElseClause`] on the `If` statement rather than a
//! separate `Stmt`; a bare `else:` has no condition and emits no line event, so it's left out.
//!
//! CPython's tracer reports the *first* line of a multi-line statement, so a statement spread
//! over several source lines can read as partially missed even when it ran — this under-reports
//! coverage, which is the honest direction for a may-set-style tool.

use ruff_python_ast as ast;
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextSize};

/// The sorted, deduplicated 1-based line numbers of every statement in `body`, nested statements
/// included per the module doc comment. Excludes the enclosing `def` line itself — callers pass
/// only the function's body, not the `def` statement.
pub(in crate::analyze) fn collect_body_lines(body: &[ast::Stmt], line_index: &LineIndex) -> Vec<u32> {
    let mut lines = Vec::new();
    walk(body, line_index, &mut lines);
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn line_at(offset: TextSize, line_index: &LineIndex) -> u32 {
    line_index.line_index(offset).get() as u32
}

/// A bare string-literal expression statement (docstring or stray constant) — never traced, see
/// the module doc comment.
fn is_bare_string_literal(stmt: &ast::Stmt) -> bool {
    matches!(stmt, ast::Stmt::Expr(e) if matches!(e.value.as_ref(), ast::Expr::StringLiteral(_)))
}

fn walk(body: &[ast::Stmt], line_index: &LineIndex, out: &mut Vec<u32>) {
    for stmt in body {
        if is_bare_string_literal(stmt) {
            continue;
        }
        out.push(line_at(stmt.range().start(), line_index));
        match stmt {
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {
                // The statement line itself runs; the nested body only runs when called/used.
            }
            ast::Stmt::If(if_stmt) => {
                walk(&if_stmt.body, line_index, out);
                for clause in &if_stmt.elif_else_clauses {
                    if clause.test.is_some() {
                        out.push(line_at(clause.range().start(), line_index));
                    }
                    walk(&clause.body, line_index, out);
                }
            }
            ast::Stmt::For(for_stmt) => {
                walk(&for_stmt.body, line_index, out);
                walk(&for_stmt.orelse, line_index, out);
            }
            ast::Stmt::While(while_stmt) => {
                walk(&while_stmt.body, line_index, out);
                walk(&while_stmt.orelse, line_index, out);
            }
            ast::Stmt::With(with_stmt) => {
                walk(&with_stmt.body, line_index, out);
            }
            ast::Stmt::Try(try_stmt) => {
                walk(&try_stmt.body, line_index, out);
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk(&h.body, line_index, out);
                }
                walk(&try_stmt.orelse, line_index, out);
                walk(&try_stmt.finalbody, line_index, out);
            }
            ast::Stmt::Match(match_stmt) => {
                for case in &match_stmt.cases {
                    walk(&case.body, line_index, out);
                }
            }
            _ => {}
        }
    }
}
