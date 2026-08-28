//! Mutation detection: the fixed list of value methods that mutate their receiver in place
//! (list/dict/set builtins), and the complementary list of value methods known to only read
//! their receiver. Turning a resolved root + write kind into a `Mutation` fact is
//! `FunctionFacts::add_mutation` in `context.rs`.

use ruff_python_ast as ast;

use super::shapes::shape_for_method;

/// Value methods that mutate their receiver in place.
pub(in crate::analyze) fn is_mutating_method(name: &str) -> bool {
    matches!(
        name,
        // list
        "append" | "extend" | "insert" | "remove" | "pop" | "clear" | "sort" | "reverse"
        // dict
        | "update" | "setdefault" | "popitem"
        // set
        | "add" | "discard" | "intersection_update" | "difference_update"
        | "symmetric_difference_update"
    )
}

/// Value methods known to only read their receiver — never mutate it. Anything not on this
/// list and not on [`is_mutating_method`] is an unrecognized method we can't see through, so a
/// call to it on a tracked root must be recorded as unresolved (see the Effects pass).
pub(in crate::analyze) fn is_known_readonly_method(name: &str) -> bool {
    if is_mutating_method(name) {
        return false;
    }
    if shape_for_method(name).is_some() {
        return true;
    }
    matches!(
        name,
        "count" | "index" | "copy" | "find" | "rfind" | "partition" | "rpartition"
            | "casefold" | "isupper" | "islower" | "isspace" | "isnumeric" | "istitle"
            | "isalnum" | "isidentifier" | "isprintable" | "isascii" | "expandtabs"
            | "center" | "ljust" | "rjust" | "translate" | "maketrans" | "swapcase"
            | "isdisjoint" | "fromkeys"
    )
}

/// Whether a call to a receiver-safe readonly method (`is_known_readonly_method`) may still
/// raise `TypeError` on a mistyped ARGUMENT — the receiver being safe (`s.count(...)` never
/// raises `AttributeError` on a proven `str`) says nothing about the argument's own contract
/// (`str.count` needs a `str` needle, `str.join` needs an iterable of `str`, ...). Deliberately
/// coarse and over-approximating, the same trade `builtin_raises` makes: only a bare string
/// literal (or, for `join`, a literal `list`/`tuple` of string literals) counts as *proven* — a
/// parameter, a local, or any other expression is unproven and predicts `TypeError` even on a
/// call that happens to be well-typed at runtime. A zero-argument call (`s.lower()`) never
/// triggers this — no argument, no contract to violate.
///
/// `index` is deliberately excluded even though it's on the readonly-method list: it's shared
/// between `str.index` (a `str` contract) and `list.index`/`tuple.index` (which accepts anything,
/// and instead raises `ValueError` when absent — a different exception class this rule doesn't
/// model), and this flat method-name table has no way to tell which receiver a given call is
/// actually against.
pub(in crate::analyze) fn readonly_arg_contract_violated(method: &str, args: &ast::Arguments) -> bool {
    let Some(first) = args.args.first() else {
        return false;
    };
    match method {
        "count" | "find" | "rfind" | "startswith" | "endswith" | "split" | "rsplit" => {
            !is_str_literal(first)
        }
        "replace" => args.args.iter().take(2).any(|a| !is_str_literal(a)),
        "join" => !is_str_literal_list(first),
        _ => false,
    }
}

fn is_str_literal(expr: &ast::Expr) -> bool {
    matches!(expr, ast::Expr::StringLiteral(_))
}

fn is_str_literal_list(expr: &ast::Expr) -> bool {
    let elts = match expr {
        ast::Expr::List(l) => &l.elts,
        ast::Expr::Tuple(t) => &t.elts,
        _ => return false,
    };
    elts.iter().all(is_str_literal)
}
