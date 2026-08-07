//! Cross-file effect propagation: the project-wide analogue of the intra-file `Interprocedural`
//! pass (`analyze::passes::interprocedural`), which this module's fixpoint mirrors exactly at
//! the mapping level. Resolves calls to project-local IMPORTED functions (recorded per-file by
//! the Effects pass as [`ImportCallSite`]s — see `analyze::context`) and propagates the callee's
//! effect summary onto the caller across file boundaries, so a caller of a helper defined in
//! another project file shows that helper's effects instead of an opaque `call_import`
//! unresolved effect.
//!
//! ## Project symbol table
//!
//! Maps `(file path, function name)` -> that function's flat position in the project (its file
//! index and its index within that file's signature list). Only free functions (`DefKind::
//! Function`, no `owner`) are indexed — methods need an instantiated receiver to call through,
//! which cross-file resolution doesn't attempt; see the module doc for the intra-file pass for
//! why `self`/`cls` calls are handled separately there.
//!
//! ## Binding resolution
//!
//! Each file's own import list resolves its bindings against the project's [`ModuleIndex`]
//! (reusing `project::resolve`) into either:
//! - a **function target** (`from .util import helper` / `from pkg.util import helper`): the
//!   binding names one specific function in one specific file.
//! - a **module target** (`import util [as u]`): the binding names a module; the actual function
//!   is whichever attribute the call site accesses (`util.helper(...)` / `u.helper(...)`).
//!
//! An [`ImportCallSite`] combines a binding with an optional attribute the same way — a function
//! target only resolves when the call site has no attribute (`helper(x)`), a module target only
//! resolves when it does (`util.helper(x)`). Anything else (external imports, `import *`,
//! unresolved relative imports, multi-level attribute chains the Effects pass didn't record a
//! site for) is left unresolved — see the doc on [`ImportCallSite`] for the attribute-chain
//! restriction.
//!
//! ## Mapping and fixpoint
//!
//! Mapping a resolved callee's summary onto its caller follows the exact same rules as the
//! intra-file pass (positional/keyword param -> caller arg root, `Global` unchanged, raises fold
//! into `implicit`, io/global_writes/is_generator union, unresolved effects inherited with
//! `may_affect` remapped) with one simplification: a project-symbol-table callee is always a free
//! function, so it never has a `SelfAttr` mutation to propagate. The corresponding `call_import`
//! unresolved effect is removed from the caller once its call site resolves. Iterated to a
//! fixpoint (bounded by the total function count across the project, +1) for the same
//! soundness/termination reasons as the intra-file pass: cross-file (mutual) recursion only ever
//! adds facts from a bounded
//! universe, so a fixpoint is guaranteed, not an infinite loop.
//!
//! Purity is recomputed after propagation settles: an inherited unresolved effect still leaves
//! the caller `Unknown`, while a fully-resolved mutating callee now makes the caller `Impure`
//! rather than `Unknown` (the `call_import` that used to force `Unknown` is gone).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::analyze::ImportCallSite;
use crate::model::{
    DefKind, EffectSignature, Import, Mutation, MutationTarget, ParamKind, Purity, UnresolvedEffect,
};

use super::resolve::{ModuleIndex, resolve_import};

/// One file's contribution to project-wide propagation: its effect signatures (mutated in place
/// by [`propagate`]), the imported call sites recorded per function (parallel, by index, to
/// `signatures`), and the file's own import catalog (for binding resolution).
pub struct FileUnit {
    pub path: String,
    pub signatures: Vec<EffectSignature>,
    pub import_call_sites: Vec<Vec<ImportCallSite>>,
    pub imports: Vec<Import>,
}

/// What a project-local import binding resolves to.
#[derive(Clone)]
enum ImportTarget {
    /// `from <module> import <function>` — the binding names one specific function.
    Function { file: String, function: String },
    /// `import <module>` — the binding names the module; the function is whatever attribute the
    /// call site accesses.
    Module { file: String },
}

/// Run cross-file effect propagation over every file's signatures in place, to a fixpoint.
pub fn propagate(files: &mut [FileUnit], root: &Path) {
    let file_paths: Vec<PathBuf> = files.iter().map(|f| root.join(&f.path)).collect();
    let index = ModuleIndex::build(root, &file_paths);
    let binding_maps: Vec<HashMap<String, ImportTarget>> =
        files.iter().map(|f| build_binding_map(&f.imports, &f.path, &index)).collect();
    let symbol_table = build_symbol_table(files);

    let total_functions: usize = files.iter().map(|f| f.signatures.len()).sum();
    let max_iterations = total_functions.saturating_add(1);
    for _ in 0..max_iterations {
        let mut changed = false;
        for file_i in 0..files.len() {
            let n = files[file_i].signatures.len();
            for func_i in 0..n {
                let sites = files[file_i].import_call_sites[func_i].clone();
                for site in &sites {
                    if let Some(&(callee_file, callee_func)) =
                        resolve_site(&binding_maps[file_i], site, &symbol_table)
                    {
                        changed |=
                            apply_call_site(files, file_i, func_i, callee_file, callee_func, site);
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    for file in files.iter_mut() {
        for sig in &mut file.signatures {
            recompute_purity(sig);
        }
    }
}

/// Build `(file path, function name) -> (file index, signature index)` for every project-local
/// free function.
fn build_symbol_table(files: &[FileUnit]) -> HashMap<(String, String), (usize, usize)> {
    let mut table = HashMap::new();
    for (file_i, file) in files.iter().enumerate() {
        for (func_i, sig) in file.signatures.iter().enumerate() {
            if sig.kind == DefKind::Function && sig.owner.is_none() {
                table.entry((file.path.clone(), sig.name.clone())).or_insert((file_i, func_i));
            }
        }
    }
    table
}

/// Build this file's import-binding -> target map from its own import catalog, resolving each
/// import against the project's module index. Star imports and imports that don't resolve
/// project-local are skipped (left for `call_import` to carry, unresolved).
fn build_binding_map(
    imports: &[Import],
    file_path: &str,
    index: &ModuleIndex,
) -> HashMap<String, ImportTarget> {
    let mut map = HashMap::new();
    for imp in imports {
        if imp.star {
            continue;
        }
        let resolution = resolve_import(index, file_path, imp);
        let Some(target_file) = resolution.project_target else {
            continue;
        };
        if imp.from {
            for name in &imp.names {
                let binding = name.alias.clone().unwrap_or_else(|| name.name.clone());
                map.insert(
                    binding,
                    ImportTarget::Function { file: target_file.clone(), function: name.name.clone() },
                );
            }
        } else {
            let binding = imp.alias.clone().unwrap_or_else(|| imp.module.package.clone());
            map.insert(binding, ImportTarget::Module { file: target_file });
        }
    }
    map
}

/// Resolve one call site's binding (+ optional attribute) to a project symbol table entry, when
/// the binding's import target and the call site's shape (attribute or not) line up — see the
/// module doc's "Binding resolution" section.
fn resolve_site<'t>(
    binding_map: &HashMap<String, ImportTarget>,
    site: &ImportCallSite,
    symbol_table: &'t HashMap<(String, String), (usize, usize)>,
) -> Option<&'t (usize, usize)> {
    let target = binding_map.get(&site.binding)?;
    let key = match (target, &site.attr) {
        (ImportTarget::Function { file, function }, None) => (file.clone(), function.clone()),
        (ImportTarget::Module { file }, Some(attr)) => (file.clone(), attr.clone()),
        _ => return None,
    };
    symbol_table.get(&key)
}

/// Propagate `callee`'s current summary onto `caller`'s signature, and remove the caller's
/// `call_import` unresolved effect for this now-resolved call site. Returns whether anything
/// changed (drives the fixpoint loop) — mirrors `passes::interprocedural::apply_call_site`.
fn apply_call_site(
    files: &mut [FileUnit],
    caller_file: usize,
    caller_func: usize,
    callee_file: usize,
    callee_func: usize,
    site: &ImportCallSite,
) -> bool {
    let Some(callee_sig) = files[callee_file].signatures.get(callee_func) else {
        return false;
    };
    let callee_positional: Vec<String> = callee_sig
        .params
        .iter()
        .filter(|p| p.kind == ParamKind::Positional)
        .map(|p| p.name.clone())
        .collect();

    let new_mutations: Vec<Mutation> = callee_sig
        .mutations
        .iter()
        .filter_map(|m| {
            let target =
                remap_target(&m.target, &callee_positional, &site.arg_roots, &site.kwarg_roots)?;
            Some(Mutation { target, via: m.via, name: m.name.clone() })
        })
        .collect();
    let new_implicit_raises: Vec<String> = callee_sig
        .raises
        .explicit
        .iter()
        .chain(callee_sig.raises.implicit.iter())
        .cloned()
        .collect();
    let new_io = callee_sig.io.clone();
    let new_globals = callee_sig.global_writes.clone();
    let callee_is_generator = callee_sig.is_generator;
    let new_unresolved: Vec<UnresolvedEffect> = callee_sig
        .unresolved_effects
        .iter()
        .map(|u| {
            let may_affect = u
                .may_affect
                .iter()
                .map(|t| {
                    remap_target(t, &callee_positional, &site.arg_roots, &site.kwarg_roots)
                        .unwrap_or_else(|| t.clone())
                })
                .collect();
            UnresolvedEffect { reason: u.reason.clone(), callee: u.callee.clone(), may_affect }
        })
        .collect();

    let expected_callee = site.callee_string();
    // Mirrors how the Effects walk built the `call_import` acknowledgment's `may_affect`
    // (`args_targets`: positional roots, then keyword-argument roots) so a fully-mapped site
    // matches its acknowledgment exactly.
    let expected_may_affect: Vec<MutationTarget> = site
        .arg_roots
        .iter()
        .flatten()
        .chain(site.kwarg_roots.iter().filter_map(|(_, root)| root.as_ref()))
        .cloned()
        .collect();

    let caller_sig = &mut files[caller_file].signatures[caller_func];
    let mut changed = false;
    for m in new_mutations {
        changed |= push_unique(&mut caller_sig.mutations, m);
    }
    for r in new_implicit_raises {
        changed |= push_unique(&mut caller_sig.raises.implicit, r);
    }
    for io in new_io {
        changed |= push_unique(&mut caller_sig.io, io);
    }
    for g in new_globals {
        changed |= push_unique(&mut caller_sig.global_writes, g);
    }
    if callee_is_generator && !caller_sig.is_generator {
        caller_sig.is_generator = true;
        changed = true;
    }
    for u in new_unresolved {
        changed |= push_unique(&mut caller_sig.unresolved_effects, u);
    }

    // An unpacked call site keeps its `call_import` acknowledgment even after resolution: the
    // unpacked arguments reach callee parameters the argument->parameter mapping can't
    // attribute, so the caller's may-set must not claim completeness over them.
    if !site.has_unpack {
        let before = caller_sig.unresolved_effects.len();
        caller_sig.unresolved_effects.retain(|u| {
            !(u.reason == "call_import"
                && u.callee.as_deref() == Some(expected_callee.as_str())
                && u.may_affect == expected_may_affect)
        });
        changed |= caller_sig.unresolved_effects.len() != before;
    }

    changed
}

/// Map a callee-side mutation/unresolved-effect root onto the caller. `None` if it can't be
/// attributed to any caller root. A project-symbol-table callee is always a free function, so
/// unlike the intra-file version there is no `SelfAttr`/`via_self` case to handle.
fn remap_target(
    target: &MutationTarget,
    callee_positional: &[String],
    arg_roots: &[Option<MutationTarget>],
    kwarg_roots: &[(String, Option<MutationTarget>)],
) -> Option<MutationTarget> {
    match target {
        MutationTarget::Param { name } => {
            if let Some((_, root)) = kwarg_roots.iter().find(|(kw, _)| kw == name) {
                return root.clone();
            }
            let idx = callee_positional.iter().position(|p| p == name)?;
            arg_roots.get(idx).cloned().flatten()
        }
        MutationTarget::Global { .. } => Some(target.clone()),
        MutationTarget::SelfAttr { .. } | MutationTarget::Nonlocal { .. } | MutationTarget::Unknown => None,
    }
}

fn push_unique<T: PartialEq>(v: &mut Vec<T>, item: T) -> bool {
    if v.contains(&item) {
        false
    } else {
        v.push(item);
        true
    }
}

/// Re-derive `sig.purity` from its (now possibly cross-file-enriched) effect set. Mirrors
/// `passes::purity::PurityPass`'s classification, minus the decorator handling — decorators were
/// already folded into `unresolved_effects` by the per-file pass before propagation ran.
fn recompute_purity(sig: &mut EffectSignature) {
    sig.purity = if !sig.unresolved_effects.is_empty() {
        Purity::Unknown
    } else if sig.mutations.is_empty() && sig.global_writes.is_empty() && sig.io.is_empty() && !sig.is_generator
    {
        Purity::Pure
    } else {
        Purity::Impure
    };
}
