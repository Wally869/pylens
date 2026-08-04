//! `.pyi`-style type-hint stub rendering, inferred from the static effect analysis. Pure string
//! formatting: no I/O, no jail, unit-testable in isolation — mirrors `report.rs`'s approach but
//! targets Python syntax instead of a terminal summary.
//!
//! Emitted stubs target Python 3.10+: built-in generics (`list[...]`, `dict[...]`) rather than
//! `typing.List`/`typing.Dict`, and `|` union syntax rather than `typing.Union`/`typing.Optional`.

use std::collections::BTreeSet;

use crate::model::{DefKind, EffectSignature, ParamInfo, ParamKind, ReturnKind, Shape};

/// Map an inferred parameter [`Shape`] to a Python type expression.
///
/// `Int`→`int`, `Float`→`float`, `Bool`→`bool`, `Str`→`str`, `Bytes`→`bytes`, `None`→`None`,
/// `Seq(e)`→`list[<e>]`, `Set(e)`→`set[<e>]`, `Map(k,v)`→`dict[<k>, <v>]`, `Any`→`Any`.
pub fn shape_to_pytype(shape: &Shape) -> String {
    match shape {
        Shape::Int => "int".to_string(),
        Shape::Float => "float".to_string(),
        Shape::Bool => "bool".to_string(),
        Shape::Str => "str".to_string(),
        Shape::Bytes => "bytes".to_string(),
        Shape::None => "None".to_string(),
        Shape::Any => "Any".to_string(),
        Shape::Seq(elem) => format!("list[{}]", shape_to_pytype(elem)),
        Shape::Set(elem) => format!("set[{}]", shape_to_pytype(elem)),
        Shape::Map(key, value) => {
            format!("dict[{}, {}]", shape_to_pytype(key), shape_to_pytype(value))
        }
    }
}

/// Map a single inferred [`ReturnKind`] to a Python type name (the building block for the
/// unioned return type; `Opaque` maps to `Any` but callers should special-case its subsuming
/// effect on the whole may-set — see [`returns_to_pytype`]).
fn return_kind_to_pytype(kind: &ReturnKind) -> &'static str {
    match kind {
        ReturnKind::None => "None",
        ReturnKind::Bool => "bool",
        ReturnKind::Int => "int",
        ReturnKind::Float => "float",
        ReturnKind::Str => "str",
        ReturnKind::Bytes => "bytes",
        ReturnKind::Sequence => "list",
        ReturnKind::Mapping => "dict",
        ReturnKind::Set => "set",
        ReturnKind::Opaque => "Any",
    }
}

/// Render the return-type annotation from the `returns` may-set.
///
/// - Empty set → `None`.
/// - Contains `Opaque` → `Any` alone (Opaque subsumes the rest of the union).
/// - Otherwise, the distinct mapped types are joined with ` | ` in `ReturnKind` declaration
///   order (`None` first if present, so `str | int | None` rather than `None | str | int`).
///   `{None}` alone renders as `None`.
pub fn returns_to_pytype(returns: &[ReturnKind]) -> String {
    if returns.is_empty() {
        return "None".to_string();
    }
    if returns.contains(&ReturnKind::Opaque) {
        return "Any".to_string();
    }
    let mut seen = BTreeSet::new();
    let mut parts = Vec::new();
    for kind in returns {
        let py = return_kind_to_pytype(kind);
        if seen.insert(py) {
            parts.push(py);
        }
    }
    parts.join(" | ")
}

/// Whether the rendered type string names `Any` as a whole word (used to decide the `typing`
/// import line), not merely as a substring of another identifier.
fn mentions_any(s: &str) -> bool {
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|word| word == "Any")
}

fn param_to_pytype_str(name: &str, shape: &Shape, has_default: bool) -> String {
    let ty = shape_to_pytype(shape);
    if has_default {
        format!("{name}: {ty} = ...")
    } else {
        format!("{name}: {ty}")
    }
}

/// Render one function's parameter list, in `.pyi` syntax: positional params first, a bare `*`
/// separator before the first keyword-only param, `*args`/`**kwargs` rendered without a default.
fn render_params(params: &[ParamInfo], receiver: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(recv) = receiver {
        parts.push(recv.to_string());
    }
    let mut emitted_star = false;
    for p in params {
        match p.kind {
            ParamKind::Positional => {
                parts.push(param_to_pytype_str(&p.name, &p.shape, p.has_default));
            }
            ParamKind::KeywordOnly => {
                if !emitted_star {
                    parts.push("*".to_string());
                    emitted_star = true;
                }
                parts.push(param_to_pytype_str(&p.name, &p.shape, p.has_default));
            }
            ParamKind::VarPositional => {
                emitted_star = true; // *args also satisfies the keyword-only separator.
                match &p.shape {
                    Shape::Any => parts.push(format!("*{}", p.name)),
                    other => parts.push(format!("*{}: {}", p.name, shape_to_pytype(other))),
                }
            }
            ParamKind::VarKeyword => match &p.shape {
                Shape::Any => parts.push(format!("**{}", p.name)),
                other => parts.push(format!("**{}: {}", p.name, shape_to_pytype(other))),
            },
        }
    }
    parts.join(", ")
}

/// Render the return-type annotation for one function, folding in the generator special case.
fn render_return(sig: &EffectSignature) -> String {
    if sig.is_generator {
        "Iterator[Any]".to_string()
    } else {
        returns_to_pytype(&sig.returns)
    }
}

/// A short trailing comment noting a declared-vs-inferred mismatch, if the analyzer flagged one
/// on the return type. The stub's emitted type always reflects the INFERRED may-set, never the
/// (untrusted) declaration — this comment is informational only.
fn mismatch_comment(sig: &EffectSignature) -> Option<String> {
    let mismatch = sig
        .type_mismatches
        .iter()
        .find(|m| m.kind == "return")?;
    let declared = &sig.declared_return;
    let declared = declared.as_deref().unwrap_or(mismatch.declared.as_str());
    Some(format!(
        "  # note: declared {declared}, inferred {}",
        returns_to_pytype(&mismatch.inferred)
    ))
}

/// Render one `def` line (no trailing newline), at the given indent.
fn render_def(sig: &EffectSignature, receiver: Option<&str>, indent: &str) -> String {
    let params = render_params(&sig.params, receiver);
    let ret = render_return(sig);
    let mut line = format!("{indent}def {}({params}) -> {ret}: ...", sig.name);
    if let Some(comment) = mismatch_comment(sig) {
        line.push_str(&comment);
    }
    line
}

/// Render a full `.pyi` module from its effect signatures: free functions at top level, then
/// one `class <owner>:` block per method-owning class, in stable first-seen order. Emits the
/// `from typing import ...` header only for names actually used (`Any`, `Iterator`).
pub fn render_stub(sigs: &[EffectSignature]) -> String {
    let functions: Vec<&EffectSignature> = sigs
        .iter()
        .filter(|s| s.kind == DefKind::Function)
        .collect();

    let mut owners: Vec<&str> = Vec::new();
    for s in sigs {
        if s.kind == DefKind::Method
            && let Some(owner) = &s.owner
            && !owners.contains(&owner.as_str())
        {
            owners.push(owner.as_str());
        }
    }

    let mut body = String::new();
    for f in &functions {
        body.push_str(&render_def(f, None, ""));
        body.push('\n');
    }
    if !functions.is_empty() && !owners.is_empty() {
        body.push('\n');
    }
    for (i, owner) in owners.iter().enumerate() {
        if i > 0 {
            body.push('\n');
        }
        body.push_str(&format!("class {owner}:\n"));
        for s in sigs
            .iter()
            .filter(|s| s.kind == DefKind::Method && s.owner.as_deref() == Some(*owner))
        {
            body.push_str(&render_def(s, Some("self"), "    "));
            body.push('\n');
        }
    }

    let uses_any = mentions_any(&body);
    let uses_iterator = body.contains("Iterator[");
    let mut header = String::new();
    if uses_any || uses_iterator {
        let mut names = Vec::new();
        if uses_any {
            names.push("Any");
        }
        if uses_iterator {
            names.push("Iterator");
        }
        header.push_str(&format!("from typing import {}\n\n", names.join(", ")));
    }

    format!("{header}{body}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ParamInfo;

    fn positional(name: &str, shape: Shape, has_default: bool) -> ParamInfo {
        ParamInfo {
            name: name.to_string(),
            shape,
            has_default,
            kind: ParamKind::Positional,
            guard_samples: Vec::new(),
        }
    }

    #[test]
    fn nested_seq_param_and_sequence_return_render() {
        let mut sig = EffectSignature::new("normalize_rows", DefKind::Function);
        sig.params.push(positional(
            "matrix",
            Shape::Seq(Box::new(Shape::Seq(Box::new(Shape::Float)))),
            false,
        ));
        sig.returns = vec![ReturnKind::Sequence];
        let out = render_stub(std::slice::from_ref(&sig));
        assert!(out.contains("def normalize_rows(matrix: list[list[float]]) -> list: ..."));
    }

    #[test]
    fn union_return_and_any_default_render() {
        let mut sig = EffectSignature::new("classify", DefKind::Function);
        sig.params.push(positional("score", Shape::Int, false));
        sig.params.push(positional("threshold", Shape::Any, true));
        sig.returns = vec![ReturnKind::Str, ReturnKind::Int, ReturnKind::None];
        let out = render_stub(std::slice::from_ref(&sig));
        assert!(out.contains("from typing import Any"));
        assert!(out.contains(
            "def classify(score: int, threshold: Any = ...) -> str | int | None: ..."
        ));
    }

    #[test]
    fn opaque_subsumes_return_union() {
        let mut sig = EffectSignature::new("f", DefKind::Function);
        sig.returns = vec![ReturnKind::Int, ReturnKind::Opaque];
        assert_eq!(returns_to_pytype(&sig.returns), "Any");
    }

    #[test]
    fn empty_returns_is_none() {
        assert_eq!(returns_to_pytype(&[]), "None");
    }

    #[test]
    fn method_renders_under_class_with_self() {
        let mut sig = EffectSignature::new("add", DefKind::Method);
        sig.owner = Some("Inventory".to_string());
        sig.params.push(positional("name", Shape::Str, false));
        sig.params.push(positional("qty", Shape::Int, false));
        sig.returns = vec![ReturnKind::Int];
        let out = render_stub(std::slice::from_ref(&sig));
        assert!(out.contains("class Inventory:\n"));
        assert!(out.contains("    def add(self, name: str, qty: int) -> int: ..."));
    }

    #[test]
    fn varargs_and_keyword_only_render_with_correct_syntax() {
        let mut sig = EffectSignature::new("f", DefKind::Function);
        sig.params.push(positional("a", Shape::Int, false));
        sig.params.push(ParamInfo {
            name: "args".to_string(),
            shape: Shape::Any,
            has_default: false,
            kind: ParamKind::VarPositional,
            guard_samples: Vec::new(),
        });
        sig.params.push(ParamInfo {
            name: "flag".to_string(),
            shape: Shape::Bool,
            has_default: true,
            kind: ParamKind::KeywordOnly,
            guard_samples: Vec::new(),
        });
        sig.params.push(ParamInfo {
            name: "kwargs".to_string(),
            shape: Shape::Any,
            has_default: false,
            kind: ParamKind::VarKeyword,
            guard_samples: Vec::new(),
        });
        let out = render_stub(std::slice::from_ref(&sig));
        assert!(out.contains(
            "def f(a: int, *args, flag: bool = ..., **kwargs) -> None: ..."
        ));
    }

    #[test]
    fn generator_renders_iterator_any_and_pulls_in_header() {
        let mut sig = EffectSignature::new("gen", DefKind::Function);
        sig.is_generator = true;
        let out = render_stub(std::slice::from_ref(&sig));
        assert!(out.contains("from typing import Any, Iterator"));
        assert!(out.contains("def gen() -> Iterator[Any]: ..."));
    }

    #[test]
    fn no_any_or_iterator_omits_header() {
        let mut sig = EffectSignature::new("f", DefKind::Function);
        sig.params.push(positional("x", Shape::Int, false));
        sig.returns = vec![ReturnKind::Int];
        let out = render_stub(std::slice::from_ref(&sig));
        assert!(!out.contains("from typing import"));
    }
}
