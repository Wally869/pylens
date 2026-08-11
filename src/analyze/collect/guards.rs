//! Guard-derived literal sampling: extracts "interesting" values from parameter-guarding test
//! expressions (`if`/`elif`/`while`/`assert` tests, ternary conditions) so guided input
//! generation can target both sides of a guard instead of spreading blindly across a shape's
//! generic candidates. A bounded heuristic — direct per-parameter literal extraction from a
//! single comparison/membership/identity test only; no constraint solving, no fixpoint, no
//! cross-parameter reasoning. An extra recorded candidate is harmless (generation caps the
//! vector count), so extraction stays permissive rather than trying to be exact.

use ruff_python_ast as ast;
use serde_json::{Value, json};

use super::super::context::FunctionFacts;

/// Extract `(parameter root, sample value)` pairs from a single guard test expression.
/// Descends into boolean combinators (`and`/`or`/`not`) since compound guards are common;
/// otherwise looks only at direct comparisons within the test.
pub(in crate::analyze) fn guard_samples(
    facts: &FunctionFacts,
    test: &ast::Expr,
) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    walk(facts, test, &mut out);
    out
}

fn walk(facts: &FunctionFacts, expr: &ast::Expr, out: &mut Vec<(String, Value)>) {
    match expr {
        ast::Expr::BoolOp(b) => {
            for v in &b.values {
                walk(facts, v, out);
            }
        }
        ast::Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::Not) => walk(facts, &u.operand, out),
        ast::Expr::Compare(c) => walk_compare(facts, c, out),
        _ => {}
    }
}

fn walk_compare(facts: &FunctionFacts, c: &ast::ExprCompare, out: &mut Vec<(String, Value)>) {
    let mut left = c.left.as_ref();
    for (op, right) in c.ops.iter().zip(c.comparators.iter()) {
        handle_pair(facts, left, *op, right, out);
        left = right;
    }
}

fn handle_pair(
    facts: &FunctionFacts,
    left: &ast::Expr,
    op: ast::CmpOp,
    right: &ast::Expr,
    out: &mut Vec<(String, Value)>,
) {
    match op {
        ast::CmpOp::Is | ast::CmpOp::IsNot => record_none_identity(facts, left, right, out),
        ast::CmpOp::In | ast::CmpOp::NotIn => record_membership(facts, left, right, out),
        ast::CmpOp::Eq | ast::CmpOp::NotEq => record_literal(facts, left, right, out, false),
        ast::CmpOp::Lt | ast::CmpOp::LtE | ast::CmpOp::Gt | ast::CmpOp::GtE => {
            record_literal(facts, left, right, out, true)
        }
    }
}

/// `x == L` / `L == x` (and the ordered forms, either side): record the literal, plus its
/// integer boundary neighbors (`L-1`, `L+1`) when `ordered` is set — an ordered comparison's
/// decision boundary sits either side of the literal. Floats only record `L` itself.
fn record_literal(
    facts: &FunctionFacts,
    left: &ast::Expr,
    right: &ast::Expr,
    out: &mut Vec<(String, Value)>,
    ordered: bool,
) {
    if let Some(root) = facts.param_root(left)
        && let Some(v) = literal_value(right)
    {
        push_with_boundary(out, root, v, ordered);
    }
    if let Some(root) = facts.param_root(right)
        && let Some(v) = literal_value(left)
    {
        push_with_boundary(out, root, v, ordered);
    }
}

fn push_with_boundary(out: &mut Vec<(String, Value)>, root: String, v: Value, ordered: bool) {
    if ordered
        && let Some(i) = v.as_i64()
    {
        out.push((root.clone(), json!(i - 1)));
        out.push((root.clone(), json!(i + 1)));
    }
    out.push((root, v));
}

/// `x is None` / `x is not None` (either side).
fn record_none_identity(
    facts: &FunctionFacts,
    left: &ast::Expr,
    right: &ast::Expr,
    out: &mut Vec<(String, Value)>,
) {
    if matches!(right, ast::Expr::NoneLiteral(_))
        && let Some(root) = facts.param_root(left)
    {
        out.push((root, Value::Null));
    }
    if matches!(left, ast::Expr::NoneLiteral(_))
        && let Some(root) = facts.param_root(right)
    {
        out.push((root, Value::Null));
    }
}

/// `x in [a, b, c]` (a literal list/tuple/set container) — records each element as a sample for
/// `x`.
fn record_membership(
    facts: &FunctionFacts,
    left: &ast::Expr,
    right: &ast::Expr,
    out: &mut Vec<(String, Value)>,
) {
    if let Some(root) = facts.param_root(left)
        && let Some(items) = literal_container(right)
    {
        for v in items {
            out.push((root.clone(), v));
        }
    }
}

fn literal_container(expr: &ast::Expr) -> Option<Vec<Value>> {
    let elts: &[ast::Expr] = match expr {
        ast::Expr::List(l) => &l.elts,
        ast::Expr::Tuple(t) => &t.elts,
        ast::Expr::Set(s) => &s.elts,
        _ => return None,
    };
    let vals: Vec<Value> = elts.iter().filter_map(literal_value).collect();
    if vals.is_empty() { None } else { Some(vals) }
}

/// A directly-written literal's value: number, string, bool, `None`, or a negated number
/// (`-1`). Shared with `effects::setup`'s default-value extraction — a parameter's own default
/// is the same kind of "written literal" evidence a guard comparison is.
pub(in crate::analyze) fn literal_value(expr: &ast::Expr) -> Option<Value> {
    match expr {
        ast::Expr::NumberLiteral(n) => match &n.value {
            ast::Number::Int(i) => i.as_i64().map(|v| json!(v)),
            ast::Number::Float(f) => Some(json!(*f)),
            ast::Number::Complex { .. } => None,
        },
        ast::Expr::StringLiteral(s) => Some(json!(s.value.to_str())),
        ast::Expr::BooleanLiteral(b) => Some(json!(b.value)),
        ast::Expr::NoneLiteral(_) => Some(Value::Null),
        ast::Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::USub) => {
            negate(literal_value(&u.operand)?)
        }
        _ => None,
    }
}

fn negate(v: Value) -> Option<Value> {
    match v.as_i64() {
        Some(i) => Some(json!(-i)),
        None => v.as_f64().map(|f| json!(-f)),
    }
}
