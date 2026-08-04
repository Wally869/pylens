//! Mutation detection: the fixed list of value methods that mutate their receiver in place
//! (list/dict/set builtins), and the complementary list of value methods known to only read
//! their receiver. Turning a resolved root + write kind into a `Mutation` fact is
//! `FunctionFacts::add_mutation` in `context.rs`.

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
