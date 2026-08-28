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

/// The element list of a `Tuple`/`List` destructuring target pattern, `None` for a plain
/// (non-destructuring) target such as a bare `Name`, `Attribute`, or `Subscript`.
fn destructure_elts(target: &ast::Expr) -> Option<&[ast::Expr]> {
    match target {
        ast::Expr::Tuple(t) => Some(&t.elts),
        ast::Expr::List(l) => Some(&l.elts),
        _ => None,
    }
}

/// Whether an `a, b = value` (or `for a, b in xs:`, treating `value` as one iteration's item)
/// binding may raise `ValueError` for an arity mismatch. `target` not being a destructuring
/// pattern at all is never a raise (a bare-name bind can't mismatch anything). A starred element
/// (`a, *b = value`) is never proven — its accepted arity is a range (`len(value) >= len(target)
/// - 1`), not a single count, and this table doesn't reason about ranges. Otherwise proven only
/// when `value` is itself a literal `Tuple`/`List` display of the exact same length as `target`,
/// recursing pairwise so a proven outer arity match still lets a mismatched NESTED pattern
/// (`(a, b), c = (( 1, 2, 3), 4)`) raise.
pub(in crate::analyze) fn destructure_may_raise(target: &ast::Expr, value: &ast::Expr) -> bool {
    let Some(elts) = destructure_elts(target) else {
        return false;
    };
    if elts.iter().any(|e| matches!(e, ast::Expr::Starred(_))) {
        return true;
    }
    let value_elts = match value {
        ast::Expr::Tuple(t) => &t.elts,
        ast::Expr::List(l) => &l.elts,
        _ => return true,
    };
    if value_elts.len() != elts.len() {
        return true;
    }
    elts.iter().zip(value_elts.iter()).any(|(t, v)| destructure_may_raise(t, v))
}

/// Whether `for target in iter:` (or a comprehension's `for` clause) may raise `ValueError` for
/// an arity mismatch on one of its iterations. `iter` not being a literal `Tuple`/`List` display
/// means each item's own arity is opaque to the analyzer, so — unlike the plain-assignment form
/// — this is never proven safe for a non-literal `iter`, even when `target` isn't itself a
/// destructuring pattern's usual single-value case (a bare `for x in xs:` is still never a raise,
/// since `destructure_may_raise` returns `false` immediately for a non-destructuring `target`).
pub(in crate::analyze) fn for_destructure_may_raise(target: &ast::Expr, iter: &ast::Expr) -> bool {
    if destructure_elts(target).is_none() {
        return false;
    }
    let items: &[ast::Expr] = match iter {
        ast::Expr::Tuple(t) => &t.elts,
        ast::Expr::List(l) => &l.elts,
        _ => return true,
    };
    items.iter().any(|item| destructure_may_raise(target, item))
}

/// Whether `expr` is a non-negative integer literal (`5`, `+5`) — the only form
/// `<<`/`>>`'s right operand is *proven* safe from `ValueError` (Python raises `ValueError` for a
/// negative shift count). A negative literal is a `UnaryOp(USub, ...)` node in the AST, which
/// doesn't match either arm here and so correctly falls through to `false`; anything that isn't a
/// literal at all (a name, a call, an arbitrary expression) is likewise never proven, even if some
/// other part of the analyzer has pinned its shape to `Int` — this rule is deliberately
/// literal-only, no shape lookup.
pub(in crate::analyze) fn is_proven_nonnegative_int_literal(expr: &ast::Expr) -> bool {
    match expr {
        ast::Expr::NumberLiteral(n) => matches!(n.value, ast::Number::Int(_)),
        ast::Expr::UnaryOp(u) if u.op == ast::UnaryOp::UAdd => {
            is_proven_nonnegative_int_literal(&u.operand)
        }
        _ => false,
    }
}
