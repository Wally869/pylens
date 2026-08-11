//! Pure, read-only shape derivation from an expression: [`shape_of`] and its literal/call/
//! subscript helpers. Used both by the fixpoint driver (for assignment RHS values) and by
//! [`super::pinning`] (for-loop iterables/targets).

use ruff_python_ast as ast;

use crate::model::Shape;

use super::state::ShapeState;

/// `element_of(Seq(e)) = e`, `element_of(Set(e)) = e`, `element_of(Map(k,_)) = k` (iterating a
/// dict yields keys), `element_of(Any) = Any`.
pub(super) fn element_of(shape: &Shape) -> Shape {
    match shape {
        Shape::Seq(e) => (**e).clone(),
        Shape::Set(e) => (**e).clone(),
        Shape::Map(k, _) => (**k).clone(),
        // Conservative: the element shape of a union is the join of each member's element
        // shape (`Any` for non-iterable members), never narrower than treating the whole thing
        // as unresolved.
        Shape::Union(members) => members
            .iter()
            .map(element_of)
            .fold(Shape::Any, Shape::join),
        _ => Shape::Any,
    }
}

/// The shape of a value expression, read-only (no env mutation) — used for assignment RHS,
/// for-loop iterables/targets, and subscript bases.
pub(super) fn shape_of(expr: &ast::Expr, state: &ShapeState) -> Shape {
    use ast::Expr;
    match expr {
        Expr::Name(n) => state.shape_of_name(n.id.as_str()),
        Expr::NumberLiteral(n) => match n.value {
            ast::Number::Int(_) => Shape::Int,
            ast::Number::Float(_) => Shape::Float,
            ast::Number::Complex { .. } => Shape::Any,
        },
        Expr::BooleanLiteral(_) => Shape::Bool,
        Expr::StringLiteral(_) | Expr::FString(_) => Shape::Str,
        Expr::BytesLiteral(_) => Shape::Bytes,
        Expr::NoneLiteral(_) => Shape::None,
        Expr::List(l) => Shape::Seq(Box::new(join_all(l.elts.iter().map(|e| shape_of(e, state))))),
        Expr::Tuple(t) => Shape::Seq(Box::new(join_all(t.elts.iter().map(|e| shape_of(e, state))))),
        Expr::Set(s) => Shape::Set(Box::new(join_all(s.elts.iter().map(|e| shape_of(e, state))))),
        Expr::Dict(d) => {
            let keys = d.items.iter().filter_map(|it| it.key.as_ref()).map(|k| shape_of(k, state));
            let values = d.items.iter().map(|it| shape_of(&it.value, state));
            Shape::Map(Box::new(join_all(keys)), Box::new(join_all(values)))
        }
        Expr::Subscript(s) => element_of(&shape_of(&s.value, state)),
        Expr::BinOp(b) => super::pinning::numeric_pin_for_op(b.op).unwrap_or(Shape::Any),
        Expr::UnaryOp(u) => match u.op {
            ast::UnaryOp::Not => Shape::Bool,
            _ => shape_of(&u.operand, state),
        },
        Expr::Compare(_) => Shape::Bool,
        Expr::BoolOp(b) => join_all(b.values.iter().map(|v| shape_of(v, state))),
        Expr::If(i) => Shape::join(shape_of(&i.body, state), shape_of(&i.orelse, state)),
        Expr::Named(n) => shape_of(&n.value, state),
        Expr::Starred(s) => shape_of(&s.value, state),
        Expr::Call(c) => shape_of_call(c, state),
        Expr::ListComp(_) => Shape::any_seq(),
        Expr::SetComp(_) => Shape::any_set(),
        Expr::DictComp(_) => Shape::any_map(),
        Expr::Generator(_) => Shape::any_seq(),
        _ => Shape::Any,
    }
}

fn join_all(iter: impl Iterator<Item = Shape>) -> Shape {
    iter.fold(Shape::Any, Shape::join)
}

fn shape_of_call(call: &ast::ExprCall, state: &ShapeState) -> Shape {
    let ast::Expr::Name(n) = call.func.as_ref() else {
        return Shape::Any;
    };
    match n.id.as_str() {
        "int" => Shape::Int,
        "float" => Shape::Float,
        "bool" => Shape::Bool,
        "str" | "repr" | "chr" => Shape::Str,
        "bytes" => Shape::Bytes,
        "list" | "tuple" | "sorted" | "reversed" => Shape::any_seq(),
        "dict" => Shape::any_map(),
        "set" | "frozenset" => Shape::any_set(),
        name if state.classes.contains(name) => Shape::Instance(name.to_string()),
        _ => Shape::Any,
    }
}
