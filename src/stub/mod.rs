//! `.pyi`-style type-hint stub rendering, inferred from the static effect analysis. Pure string
//! formatting: no I/O, no jail, unit-testable in isolation — mirrors `report.rs`'s approach but
//! targets Python syntax instead of a terminal summary.
//!
//! Emitted stubs target Python 3.10+: built-in generics (`list[...]`, `dict[...]`) rather than
//! `typing.List`/`typing.Dict`, and `|` union syntax rather than `typing.Union`/`typing.Optional`.
//!
//! [`observed`] extends this with an optional, `record`-fed enrichment layer: dynamically
//! observed types folded in wherever the static side stayed `Any`.

use std::collections::{BTreeSet, HashMap};

use crate::model::{DefKind, EffectSignature, ParamInfo, ParamKind, ReturnKind, Shape};

pub mod observed;

/// Dynamically observed types to fold into one function's rendering, keyed by param name (plus
/// the return type). Empty (`ObservedTypes::default()`) for the pure-static rendering path — see
/// `observed::render_record_stub` for how these get populated from recorded cases.
#[derive(Default)]
pub(crate) struct ObservedTypes {
    /// Param name -> observed Python type name. Only consulted for a param whose static `shape`
    /// is `Shape::Any` (a static-known shape always wins — see `param_to_pytype_str`).
    pub(crate) params: HashMap<String, String>,
    /// Observed return type name. Only consulted when the static return rendering is `Any`
    /// (`sig.returns` contains `ReturnKind::Opaque`, or is otherwise unresolved).
    pub(crate) ret: Option<String>,
}

/// Map an inferred parameter [`Shape`] to a Python type expression.
///
/// `Int`→`int`, `Float`→`float`, `Bool`→`bool`, `Str`→`str`, `Bytes`→`bytes`, `None`→`None`,
/// `Seq(e)`→`list[<e>]`, `Set(e)`→`set[<e>]`, `Map(k,v)`→`dict[<k>, <v>]`, `Any`→`Any`,
/// `Union([..])`→PEP 604 `A | B | ...` (an `Optional[X]` is just `Union(X, None)`, so it renders
/// as `X | None` with no separate case needed).
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
        Shape::Union(members) => members
            .iter()
            .map(shape_to_pytype)
            .collect::<Vec<_>>()
            .join(" | "),
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

/// Render one param's `name: type[ = ...]` fragment. A static `Shape::Any` may be overridden by
/// an `observed` type for that param name — recorded (as `"name: type"`) into `notes` so the
/// caller can surface it as a trailing `# observed` comment. A static shape more specific than
/// `Any` always wins and is never overridden.
fn param_to_pytype_str(
    name: &str,
    shape: &Shape,
    has_default: bool,
    observed: &ObservedTypes,
    notes: &mut Vec<String>,
) -> String {
    let ty = match (shape, observed.params.get(name)) {
        (Shape::Any, Some(obs)) => {
            notes.push(format!("{name}: {obs}"));
            obs.clone()
        }
        _ => shape_to_pytype(shape),
    };
    if has_default {
        format!("{name}: {ty} = ...")
    } else {
        format!("{name}: {ty}")
    }
}

/// Render one function's parameter list, in `.pyi` syntax: positional params first, a bare `*`
/// separator before the first keyword-only param, `*args`/`**kwargs` rendered without a default.
fn render_params(
    params: &[ParamInfo],
    receiver: Option<&str>,
    observed: &ObservedTypes,
    notes: &mut Vec<String>,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(recv) = receiver {
        parts.push(recv.to_string());
    }
    let mut emitted_star = false;
    for p in params {
        match p.kind {
            ParamKind::Positional => {
                parts.push(param_to_pytype_str(&p.name, &p.shape, p.has_default, observed, notes));
            }
            ParamKind::KeywordOnly => {
                if !emitted_star {
                    parts.push("*".to_string());
                    emitted_star = true;
                }
                parts.push(param_to_pytype_str(&p.name, &p.shape, p.has_default, observed, notes));
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

/// Render the return-type annotation for one function, folding in the generator special case and
/// (when the static side is `Any`) the observed return type — recorded into `notes` as
/// `"-> type"` so the caller can surface it as a trailing `# observed` comment.
fn render_return(sig: &EffectSignature, observed: &ObservedTypes, notes: &mut Vec<String>) -> String {
    if sig.is_generator {
        return "Iterator[Any]".to_string();
    }
    let base = returns_to_pytype(&sig.returns);
    match (&base[..], &observed.ret) {
        ("Any", Some(obs)) => {
            notes.push(format!("-> {obs}"));
            obs.clone()
        }
        _ => base,
    }
}

/// A short trailing comment noting a declared-vs-inferred mismatch, if the analyzer flagged one
/// on the return type or a parameter. The stub's emitted type always reflects the INFERRED
/// may-set, never the (untrusted) declaration — this comment is informational only.
fn mismatch_comment(sig: &EffectSignature) -> Option<String> {
    if sig.type_mismatches.is_empty() {
        return None;
    }
    let parts: Vec<String> = sig
        .type_mismatches
        .iter()
        .map(|m| match m.kind.as_str() {
            "param" => {
                let name = m.param.as_deref().unwrap_or("?");
                let inferred = m
                    .inferred_shape
                    .as_ref()
                    .map(shape_to_pytype)
                    .unwrap_or_else(|| "?".to_string());
                format!("{name}: declared {}, inferred {inferred}", m.declared)
            }
            _ => format!(
                "return: declared {}, inferred {}",
                m.declared,
                returns_to_pytype(&m.inferred)
            ),
        })
        .collect();
    Some(format!("  # note: {}", parts.join("; ")))
}

/// Render one `def` line (no trailing newline), at the given indent.
fn render_def(sig: &EffectSignature, receiver: Option<&str>, indent: &str, observed: &ObservedTypes) -> String {
    let mut notes = Vec::new();
    let params = render_params(&sig.params, receiver, observed, &mut notes);
    let ret = render_return(sig, observed, &mut notes);
    let mut line = format!("{indent}def {}({params}) -> {ret}: ...", sig.name);
    if !notes.is_empty() {
        line.push_str(&format!("  # observed: {}", notes.join(", ")));
    }
    if let Some(comment) = mismatch_comment(sig) {
        line.push_str(&comment);
    }
    line
}

/// Render a full `.pyi` module from its effect signatures: free functions at top level, then
/// one `class <owner>:` block per method-owning class, in stable first-seen order. Emits the
/// `from typing import ...` header only for names actually used (`Any`, `Iterator`).
///
/// `observed_for` supplies per-signature [`ObservedTypes`] (empty for the pure-static path —
/// see [`render_stub`] — or fed from recorded cases — see [`observed::render_record_stub`]).
pub(crate) fn render_stub_generic(
    sigs: &[&EffectSignature],
    observed_for: impl Fn(&EffectSignature) -> ObservedTypes,
) -> String {
    let functions: Vec<&&EffectSignature> = sigs.iter().filter(|s| s.kind == DefKind::Function).collect();

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
        body.push_str(&render_def(f, None, "", &observed_for(f)));
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
            body.push_str(&render_def(s, Some("self"), "    ", &observed_for(s)));
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

/// Render a full `.pyi` module purely from static effect signatures (no observed enrichment).
pub fn render_stub(sigs: &[EffectSignature]) -> String {
    let refs: Vec<&EffectSignature> = sigs.iter().collect();
    render_stub_generic(&refs, |_| ObservedTypes::default())
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
            declared: None,
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
            declared: None,
            guard_samples: Vec::new(),
        });
        sig.params.push(ParamInfo {
            name: "flag".to_string(),
            shape: Shape::Bool,
            has_default: true,
            kind: ParamKind::KeywordOnly,
            declared: None,
            guard_samples: Vec::new(),
        });
        sig.params.push(ParamInfo {
            name: "kwargs".to_string(),
            shape: Shape::Any,
            has_default: false,
            kind: ParamKind::VarKeyword,
            declared: None,
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

    #[test]
    fn union_shape_param_renders_pep604() {
        let mut sig = EffectSignature::new("maybe_len", DefKind::Function);
        sig.params.push(positional(
            "x",
            Shape::Union(vec![Shape::Int, Shape::None]),
            false,
        ));
        sig.returns = vec![ReturnKind::Int, ReturnKind::None];
        let out = render_stub(std::slice::from_ref(&sig));
        assert!(out.contains("def maybe_len(x: int | None) -> int | None: ..."));
    }
}
