//! Shared name-chain helpers used to root a mutation/argument expression: peeling `.attr` and
//! `[...]` off an expression down to its leftmost name, and reconstructing a dotted access
//! (`os.path.join`) for unresolved-effect reporting.

use ruff_python_ast as ast;

/// The leftmost `Name` reached by peeling `.attr` and `[...]` off an expression.
pub(in crate::analyze) fn leftmost_name(expr: &ast::Expr) -> Option<&str> {
    match expr {
        ast::Expr::Name(n) => Some(n.id.as_str()),
        ast::Expr::Attribute(a) => leftmost_name(&a.value),
        ast::Expr::Subscript(s) => leftmost_name(&s.value),
        _ => None,
    }
}

/// Reconstruct a dotted attribute/name access as a string, e.g. `os.path.join`. `None` if the
/// base isn't a plain name chain (e.g. a subscript or call sits in the way).
pub(in crate::analyze) fn dotted_attr(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Attribute(a) => Some(format!("{}.{}", dotted_attr(&a.value)?, a.attr)),
        _ => None,
    }
}
