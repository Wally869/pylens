//! TypeCheck pass: flags CONTRADICTIONS between an untrusted declared annotation and the
//! inferred, statically-computed may-set — for both the return type (`declared_return` vs.
//! `returns`) and each parameter (`ParamInfo::declared` vs. `ParamInfo::shape`).
//!
//! Runs after Effects (so `returns`/param `shape`s are final; running after Interprocedural is
//! also fine since interprocedural propagation never touches either). Kept deliberately
//! conservative — the declared annotation is only a hint, and the inferred side is a may-set, not
//! an exact type — so a mismatch is raised **only** when the two sides are fully disjoint. A
//! declared type with `None` plus a permitted kind both present in the inferred side is common
//! (implicit fall-through / an Optional default) and is never flagged. This is advisory only:
//! `type_mismatches` is a report field, never consulted by the may-set or by the Purity pass.

use ruff_python_ast as ast;

use crate::model::{ReturnKind, Shape, TypeMismatch};

use super::super::context::ModuleAnalysis;
use super::super::pass::Pass;

pub(in crate::analyze) struct TypeCheckPass;

impl Pass for TypeCheckPass {
    fn run(&self, _module: &ast::ModModule, ctx: &mut ModuleAnalysis) {
        for sig in &mut ctx.signatures {
            if let Some(declared) = sig.declared_return.clone()
                && let Some(permitted) = permitted_kinds(&declared)
                && !sig.returns.is_empty()
                && !sig.returns.contains(&ReturnKind::Opaque)
                && sig.returns.iter().all(|rk| !permitted.contains(rk))
            {
                sig.type_mismatches.push(TypeMismatch {
                    kind: "return".to_string(),
                    param: None,
                    declared,
                    inferred: sig.returns.clone(),
                    inferred_shape: None,
                });
            }

            for p in &sig.params {
                let Some(declared) = &p.declared else { continue };
                if shape_disjoint_from(declared, &p.shape) {
                    sig.type_mismatches.push(TypeMismatch {
                        kind: "param".to_string(),
                        param: Some(p.name.clone()),
                        declared: declared.clone(),
                        inferred: Vec::new(),
                        inferred_shape: Some(p.shape.clone()),
                    });
                }
            }
        }
    }
}

/// Maps a declared annotation name to the concrete [`Shape`] top-level constructors it permits.
/// `None` means the annotation is a wildcard (`Any`, `Optional`/`Union` without a captured inner
/// type, or anything we don't confidently recognize) — never flagged.
fn permitted_shape_kinds(declared: &str) -> Option<&'static [&'static str]> {
    match declared {
        "int" => Some(&["int"]),
        "float" => Some(&["float", "int"]),
        "str" => Some(&["str"]),
        "bool" => Some(&["bool"]),
        "bytes" => Some(&["bytes"]),
        "list" | "List" | "tuple" | "Tuple" | "Sequence" => Some(&["seq"]),
        "dict" | "Dict" | "Mapping" => Some(&["map"]),
        "set" | "Set" | "frozenset" => Some(&["set"]),
        "None" | "NoneType" => Some(&["none"]),
        _ => None,
    }
}

/// The top-level constructor tag of a single (non-`Any`, non-`Union`) shape, for comparison
/// against [`permitted_shape_kinds`]. `None` for `Any`/`Union` — callers handle those separately.
fn shape_kind_tag(shape: &Shape) -> Option<&'static str> {
    match shape {
        Shape::Int => Some("int"),
        Shape::Float => Some("float"),
        Shape::Bool => Some("bool"),
        Shape::Str => Some("str"),
        Shape::Bytes => Some("bytes"),
        Shape::None => Some("none"),
        Shape::Seq(_) => Some("seq"),
        Shape::Map(..) => Some("map"),
        Shape::Set(_) => Some("set"),
        Shape::Any | Shape::Union(_) => None,
    }
}

/// Whether `shape` (the inferred may-set for a parameter) is fully disjoint from `declared` (the
/// untrusted annotation) — the same "only flag on full disjointness" conservatism the return
/// check uses. `Any` never flags (nothing resolved to compare). A `Union` flags only if **every**
/// member is disjoint from `declared` — one compatible member is enough to withhold the flag,
/// mirroring how a `returns` may-set with any permitted kind present withholds it.
fn shape_disjoint_from(declared: &str, shape: &Shape) -> bool {
    let Some(permitted) = permitted_shape_kinds(declared) else {
        return false;
    };
    match shape {
        Shape::Any => false,
        Shape::Union(members) => members.iter().all(|m| member_disjoint(permitted, m)),
        other => member_disjoint(permitted, other),
    }
}

fn member_disjoint(permitted: &[&str], shape: &Shape) -> bool {
    match shape_kind_tag(shape) {
        Some(tag) => !permitted.contains(&tag),
        None => false,
    }
}

/// Maps a declared return annotation name to the set of [`ReturnKind`]s it permits. `None`
/// means the annotation is a wildcard (`Any`, `Optional`/`Union` without a captured inner type,
/// or anything we don't confidently recognize) — never flagged.
fn permitted_kinds(declared: &str) -> Option<Vec<ReturnKind>> {
    match declared {
        "int" => Some(vec![ReturnKind::Int]),
        "float" => Some(vec![ReturnKind::Float, ReturnKind::Int]),
        "str" => Some(vec![ReturnKind::Str]),
        "bool" => Some(vec![ReturnKind::Bool]),
        "bytes" => Some(vec![ReturnKind::Bytes]),
        "list" | "List" | "tuple" | "Tuple" | "Sequence" => Some(vec![ReturnKind::Sequence]),
        "dict" | "Dict" | "Mapping" => Some(vec![ReturnKind::Mapping]),
        "set" | "Set" | "frozenset" => Some(vec![ReturnKind::Set]),
        "None" | "NoneType" => Some(vec![ReturnKind::None]),
        _ => None,
    }
}
