//! Exception inference: the explicit-raise name extractor, plus the implicit (may-set)
//! operator-induced exceptions — division, subscript reads, `int()`/`float()` conversions, and
//! type-confused comparisons/arithmetic.

use ruff_python_ast as ast;

use crate::model::Shape;

pub(in crate::analyze) fn exception_name(exc: &ast::Expr) -> Option<String> {
    match exc {
        ast::Expr::Call(call) => exception_name(&call.func),
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Attribute(a) => Some(a.attr.as_str().to_string()),
        _ => None,
    }
}

/// The exception a binary operator may induce on a zero/invalid right-hand operand, if any.
pub(in crate::analyze) fn binop_implicit_exception(op: ast::Operator) -> Option<&'static str> {
    match op {
        ast::Operator::Div | ast::Operator::FloorDiv | ast::Operator::Mod => {
            Some("ZeroDivisionError")
        }
        _ => None,
    }
}

/// The exception(s) a subscript read on a value of `shape` may induce — a may-set: known
/// mapping shapes raise `KeyError`, known sequence/string shapes raise `IndexError`, and an
/// undetermined shape must over-approximate with both. `is_param_root` forces the full
/// over-approximation even for a pinned mapping/sequence shape: a parameter's shape is inferred
/// from usage inside the function, not enforced on the caller, so a caller can pass a mapping
/// where the function's own code only ever indexes with ints — narrowing on that shape would be
/// unsound. A local's pinned shape (no caller can substitute a different value into it) still
/// narrows normally.
pub(in crate::analyze) fn subscript_read_exceptions(
    shape: &Shape,
    is_param_root: bool,
) -> &'static [&'static str] {
    if is_param_root {
        return &["KeyError", "IndexError"];
    }
    match shape {
        Shape::Map(..) => &["KeyError"],
        Shape::Seq(..) | Shape::Str => &["IndexError"],
        _ => &["KeyError", "IndexError"],
    }
}

/// The exception a builtin conversion call may induce on an unparsable argument, if any.
pub(in crate::analyze) fn call_implicit_exception(callee: &str) -> Option<&'static str> {
    matches!(callee, "int" | "float").then_some("ValueError")
}

/// Whether `op` is an ordered comparison (`<`, `<=`, `>`, `>=`) — the comparison kind whose
/// mismatched operand types raise `TypeError` on *both* operands. Equality (`==`/`!=`) and
/// identity (`is`/`is not`) never raise `TypeError` for a type mismatch, so they're excluded.
/// Membership (`in`/`not in`) is handled separately (see the Effects walk's `Compare` arm): it
/// can raise `TypeError` too, but only via its LEFT operand (an unhashable/unsupported value
/// probed against the right-hand container), so it doesn't fit this all-operands rule.
///
/// Used, together with `binop_implicit_exception`'s arithmetic operators, to seed implicit
/// `TypeError` candidates: `TypeError` is a may-raise when an operand's type is unknown to the
/// analyzer (a name that never got a shape vote), since ordered comparisons and arithmetic on
/// such a value can genuinely mistype at runtime — *or* when the operand roots to a parameter,
/// regardless of what shape the analyzer settled on for it. A parameter's shape is a hypothesis
/// inferred from how *this* function happens to use it (e.g. `x < 0` is itself the only evidence
/// that pins `x` to `int`); Python guarantees a caller nothing, so that inferred shape cannot be
/// used to prove the very comparison that produced it safe — doing so is circular. Only a
/// *local's* pinned shape (never substitutable by a caller) can still narrow the may-set. See
/// `FunctionFacts::note_type_error_candidate`, which checks both the candidate's rooted *final*
/// settled shape (Shapes runs before Effects, and its env covers params **and** locals — see
/// `FunctionFacts::env_shape`) and whether it roots to a parameter (`FunctionFacts::param_root`).
pub(in crate::analyze) fn is_ordered_compare(op: ast::CmpOp) -> bool {
    matches!(
        op,
        ast::CmpOp::Lt | ast::CmpOp::LtE | ast::CmpOp::Gt | ast::CmpOp::GtE
    )
}
