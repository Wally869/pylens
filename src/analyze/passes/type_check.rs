//! TypeCheck pass: flags CONTRADICTIONS between the untrusted declared return annotation
//! (`declared_return`) and the inferred, statically-computed return may-set (`returns`).
//!
//! Runs after Effects (so `returns` is final; running after Interprocedural is also fine since
//! interprocedural propagation never touches `returns`). Kept deliberately conservative — the
//! declared annotation is only a hint, and `returns` is a may-set, not an exact type — so a
//! mismatch is raised **only** when the two sets are fully disjoint. A declared type with `None`
//! plus a permitted kind both present in `returns` is common (implicit fall-through) and is never
//! flagged.
//!
//! Scope: return-type mismatches only. Param-annotation mismatch is future work — param
//! annotations aren't captured by the analyzer yet.

use ruff_python_ast as ast;

use crate::model::{ReturnKind, TypeMismatch};

use super::super::context::ModuleAnalysis;
use super::super::pass::Pass;

pub(in crate::analyze) struct TypeCheckPass;

impl Pass for TypeCheckPass {
    fn run(&self, _module: &ast::ModModule, ctx: &mut ModuleAnalysis) {
        for sig in &mut ctx.signatures {
            let Some(declared) = &sig.declared_return else {
                continue;
            };
            let Some(permitted) = permitted_kinds(declared) else {
                // Wildcard (Any / Optional / unrecognized annotation) — never flag.
                continue;
            };
            if sig.returns.is_empty() || sig.returns.contains(&ReturnKind::Opaque) {
                // Empty: nothing to compare against. Opaque: some returned value's kind is
                // unknown, so we can't be sure it contradicts the declaration.
                continue;
            }
            let disjoint = sig.returns.iter().all(|rk| !permitted.contains(rk));
            if disjoint {
                sig.type_mismatches.push(TypeMismatch {
                    kind: "return".to_string(),
                    declared: declared.clone(),
                    inferred: sig.returns.clone(),
                });
            }
        }
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
