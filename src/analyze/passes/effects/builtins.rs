//! Classification of builtin callees the Effects walk already knows are pure, so a call to one
//! of them isn't recorded as an unresolved effect.

pub(super) fn is_known_pure_builtin(name: &str) -> bool {
    matches!(
        name,
        "len" | "range" | "enumerate" | "zip" | "map" | "filter" | "sorted" | "reversed"
            | "int" | "float" | "str" | "bool" | "bytes" | "list" | "dict" | "set"
            | "tuple" | "frozenset" | "abs" | "min" | "max" | "sum" | "round" | "ord"
            | "chr" | "repr" | "hash" | "isinstance" | "issubclass" | "type" | "all"
            | "any" | "divmod" | "pow" | "hex" | "oct" | "bin" | "format"
    )
}
