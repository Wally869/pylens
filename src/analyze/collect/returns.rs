//! Return classification and fall-through analysis: what coarse kind of value each `return`
//! exit produces, plus whether a body can fall off the end (which contributes an implicit
//! `None` return).

use ruff_python_ast as ast;

use crate::model::ReturnKind;

pub(in crate::analyze) fn classify_return(expr: &ast::Expr) -> ReturnKind {
    use ast::Expr;
    match expr {
        Expr::NoneLiteral(_) => ReturnKind::None,
        Expr::BooleanLiteral(_) => ReturnKind::Bool,
        Expr::NumberLiteral(n) => match n.value {
            ast::Number::Int(_) => ReturnKind::Int,
            ast::Number::Float(_) => ReturnKind::Float,
            ast::Number::Complex { .. } => ReturnKind::Opaque,
        },
        Expr::StringLiteral(_) | Expr::FString(_) => ReturnKind::Str,
        Expr::BytesLiteral(_) => ReturnKind::Bytes,
        Expr::List(_) | Expr::ListComp(_) | Expr::Tuple(_) => ReturnKind::Sequence,
        Expr::Dict(_) | Expr::DictComp(_) => ReturnKind::Mapping,
        Expr::Set(_) | Expr::SetComp(_) => ReturnKind::Set,
        Expr::Compare(_) => ReturnKind::Bool,
        // `-1`, `+x`, `~n` parse as a unary op over the literal; `not x` is a bool.
        Expr::UnaryOp(u) => match u.op {
            ast::UnaryOp::Not => ReturnKind::Bool,
            ast::UnaryOp::USub | ast::UnaryOp::UAdd | ast::UnaryOp::Invert => {
                classify_return(&u.operand)
            }
        },
        Expr::Call(call) => match &*call.func {
            Expr::Name(n) => match n.id.as_str() {
                "bool" => ReturnKind::Bool,
                "int" | "len" | "ord" | "hash" => ReturnKind::Int,
                "float" => ReturnKind::Float,
                "str" | "repr" | "chr" => ReturnKind::Str,
                "bytes" => ReturnKind::Bytes,
                "list" | "tuple" | "sorted" => ReturnKind::Sequence,
                "dict" => ReturnKind::Mapping,
                "set" | "frozenset" => ReturnKind::Set,
                _ => ReturnKind::Opaque,
            },
            _ => ReturnKind::Opaque,
        },
        _ => ReturnKind::Opaque,
    }
}

/// Whether control can reach the end of `body` without an explicit return/raise — in which
/// case the function contributes an implicit `None` return.
pub(in crate::analyze) fn can_fall_through(body: &[ast::Stmt]) -> bool {
    !body_terminates(body)
}

/// Whether a block always exits via return/raise (its last statement terminates).
fn body_terminates(body: &[ast::Stmt]) -> bool {
    body.last().is_some_and(stmt_terminates)
}

/// Whether `stmt` exits the enclosing block on every path. Conservative: constructs we can't
/// prove exhaustive (`match`, `try`, loops) are treated as able to fall through.
fn stmt_terminates(stmt: &ast::Stmt) -> bool {
    match stmt {
        ast::Stmt::Return(_) | ast::Stmt::Raise(_) => true,
        // An `if` terminates only with an `else` where every branch terminates.
        ast::Stmt::If(s) => {
            let mut has_else = false;
            let mut all = body_terminates(&s.body);
            for clause in &s.elif_else_clauses {
                if clause.test.is_none() {
                    has_else = true;
                }
                all = all && body_terminates(&clause.body);
            }
            has_else && all
        }
        // A `with` terminates iff its body does.
        ast::Stmt::With(s) => body_terminates(&s.body),
        _ => false,
    }
}
