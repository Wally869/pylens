//! Usage-based shape lookups shared by the Shapes pass (fixpoint inference) and the mutation
//! collector: which value methods discriminate a container kind, and which literal expressions
//! pin a numeric shape.

use ruff_python_ast as ast;

use crate::model::Shape;

pub(in crate::analyze) fn shape_for_method(m: &str) -> Option<Shape> {
    match m {
        "split" | "rsplit" | "strip" | "lstrip" | "rstrip" | "upper" | "lower" | "title"
        | "capitalize" | "replace" | "startswith" | "endswith" | "join" | "encode"
        | "splitlines" | "format" | "isdigit" | "isalpha" | "zfill" => Some(Shape::Str),
        "keys" | "values" | "items" | "get" | "setdefault" | "popitem" => Some(Shape::any_map()),
        "add" | "discard" | "union" | "intersection" | "difference" | "issubset"
        | "issuperset" | "symmetric_difference" => Some(Shape::any_set()),
        "append" | "extend" | "insert" | "sort" | "reverse" => Some(Shape::any_seq()),
        _ => None,
    }
}

/// If `expr` is a numeric literal (optionally unary-signed), the corresponding param shape.
pub(in crate::analyze) fn numeric_literal_shape(expr: &ast::Expr) -> Option<Shape> {
    match expr {
        ast::Expr::NumberLiteral(n) => match n.value {
            ast::Number::Int(_) => Some(Shape::Int),
            ast::Number::Float(_) => Some(Shape::Float),
            ast::Number::Complex { .. } => None,
        },
        ast::Expr::UnaryOp(u) => match u.op {
            ast::UnaryOp::USub | ast::UnaryOp::UAdd => numeric_literal_shape(&u.operand),
            _ => None,
        },
        _ => None,
    }
}
