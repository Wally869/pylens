//! Cross-parameter relation inference: finds pairs of parameters (or one parameter's element and
//! another parameter) that meet as the two operands of one comparison or arithmetic expression
//! somewhere in the body — e.g. `arr[mid] < target` relates `arr`'s element to `target` by
//! [`RelationKind::Order`]. **Generation-only, advisory**: never folds into `Shape`, `Purity`, or
//! `Raises`. A bounded heuristic, not a solver — no aliasing through locals beyond the one
//! direct `for x in p:` loop-binding form, no constraint propagation.

use std::collections::HashMap;

use ruff_python_ast as ast;
use ruff_python_ast::visitor::{self, Visitor};

use crate::model::{ParamRef, ParamRelation, RelationKind};

/// Infer cross-parameter relations from a single walk of `body`. Nested `def`/`class`/`lambda`
/// bodies are not descended into — a separate scope, same exclusion the Effects walk itself
/// applies (see `FunctionFacts::local_defs`'s doc).
pub(in crate::analyze) fn infer_relations(
    body: &[ast::Stmt],
    param_names: &[String],
) -> Vec<ParamRelation> {
    let mut collector = Collector {
        params: param_names,
        loop_binds: HashMap::new(),
        out: Vec::new(),
    };
    for stmt in body {
        collector.visit_stmt(stmt);
    }
    collector.out
}

struct Collector<'a> {
    params: &'a [String],
    /// Name bound by a plain `for x in p:` directly over a bare parameter `p` -> that param's
    /// name. A name reached this way stands for one element of `p`.
    loop_binds: HashMap<String, String>,
    out: Vec<ParamRelation>,
}

impl Collector<'_> {
    fn is_param(&self, name: &str) -> bool {
        self.params.iter().any(|p| p == name)
    }

    /// Resolve an operand expression to the [`ParamRef`] it stands for, or `None` if it isn't one
    /// of the bounded recognized forms.
    fn resolve(&self, expr: &ast::Expr) -> Option<ParamRef> {
        match expr {
            ast::Expr::Name(n) => {
                let name = n.id.as_str();
                if self.is_param(name) {
                    return Some(ParamRef { param: name.to_string(), element: false });
                }
                self.loop_binds.get(name).map(|param| ParamRef { param: param.clone(), element: true })
            }
            ast::Expr::Subscript(s) => {
                if matches!(s.slice.as_ref(), ast::Expr::Slice(_)) {
                    return None;
                }
                match s.value.as_ref() {
                    ast::Expr::Name(n) if self.is_param(n.id.as_str()) => {
                        Some(ParamRef { param: n.id.as_str().to_string(), element: true })
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    fn record(&mut self, left: &ast::Expr, right: &ast::Expr, kind: RelationKind) {
        let (Some(l), Some(r)) = (self.resolve(left), self.resolve(right)) else {
            return;
        };
        if l.param == r.param {
            return;
        }
        let rel = ParamRelation { left: l, right: r, kind };
        if !self.out.contains(&rel) {
            self.out.push(rel);
        }
    }

    fn handle_compare(&mut self, c: &ast::ExprCompare) {
        let mut left = c.left.as_ref();
        for (op, right) in c.ops.iter().zip(c.comparators.iter()) {
            if let Some(kind) = relation_kind_for_cmp(*op) {
                self.record(left, right, kind);
            }
            left = right;
        }
    }

    fn handle_binop(&mut self, b: &ast::ExprBinOp) {
        if let Some(kind) = relation_kind_for_binop(b.op) {
            self.record(&b.left, &b.right, kind);
        }
    }

    fn note_loop_binding(&mut self, for_stmt: &ast::StmtFor) {
        if let (ast::Expr::Name(target), ast::Expr::Name(iter)) =
            (for_stmt.target.as_ref(), for_stmt.iter.as_ref())
            && self.is_param(iter.id.as_str())
        {
            self.loop_binds.insert(target.id.as_str().to_string(), iter.id.as_str().to_string());
        }
    }
}

fn relation_kind_for_cmp(op: ast::CmpOp) -> Option<RelationKind> {
    match op {
        ast::CmpOp::Lt | ast::CmpOp::LtE | ast::CmpOp::Gt | ast::CmpOp::GtE => {
            Some(RelationKind::Order)
        }
        ast::CmpOp::Eq | ast::CmpOp::NotEq => Some(RelationKind::Eq),
        _ => None,
    }
}

fn relation_kind_for_binop(op: ast::Operator) -> Option<RelationKind> {
    match op {
        ast::Operator::Add
        | ast::Operator::Sub
        | ast::Operator::Mult
        | ast::Operator::Div
        | ast::Operator::FloorDiv
        | ast::Operator::Mod
        | ast::Operator::Pow => Some(RelationKind::Arith),
        _ => None,
    }
}

impl<'ast> Visitor<'ast> for Collector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast ast::Stmt) {
        match stmt {
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
            ast::Stmt::For(for_stmt) => {
                self.note_loop_binding(for_stmt);
                visitor::walk_stmt(self, stmt);
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        match expr {
            ast::Expr::Lambda(_) => {}
            ast::Expr::Compare(c) => {
                self.handle_compare(c);
                visitor::walk_expr(self, expr);
            }
            ast::Expr::BinOp(b) => {
                self.handle_binop(b);
                visitor::walk_expr(self, expr);
            }
            _ => visitor::walk_expr(self, expr),
        }
    }
}
