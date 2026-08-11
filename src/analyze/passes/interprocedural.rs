//! Interprocedural pass: propagates effect summaries along the intra-module call graph the
//! Effects pass recorded ([`CallSite`]s under `ModuleAnalysis::call_sites`), so a caller of a
//! function/method defined in this same module inherits that callee's effects instead of
//! treating the call as opaque. Runs between Effects and Purity, so Purity classifies every
//! caller against its *complete*, propagated effect set.
//!
//! ## Mapping rules
//!
//! - A callee mutation on a parameter maps onto the caller-side root passed for that parameter:
//!   by position if the caller passed it positionally (matched against the callee's `Positional`
//!   parameters only), by name if the caller passed it as a keyword (matched against the
//!   callee's `Positional` or `KeywordOnly` parameters by name). Dropped — not attributed to any
//!   caller root — if the caller didn't pass a trackable root there, the parameter isn't
//!   `Positional`/`KeywordOnly`, or the keyword names no declared parameter (swallowed by the
//!   callee's own `**kwargs`); sound, since the may-set only grows and an unmapped root has
//!   nothing to over-approximate onto.
//! - A callee `SelfAttr` mutation propagates unchanged when the call was `self.method(...)` /
//!   `cls.method(...)` on the caller's own receiver — same underlying object
//!   ([`CallReceiver::CallerSelf`]). When the call was `x.method(...)` on a tracked local whose
//!   settled shape resolved to exactly one class ([`CallReceiver::Root`]), the mutation's root is
//!   rewritten onto that local instead — same underlying object, different name. Dropped if the
//!   receiver isn't tracked at all ([`CallReceiver::None`]).
//! - A callee `Global` mutation propagates unchanged (a module-global name is absolute, not
//!   parameterized by the call).
//! - A callee's raises — explicit and implicit alike — fold into the caller's `raises.implicit`:
//!   from the caller's point of view they're induced by the call, not written in its own body.
//! - IO, `is_generator`, and global writes union directly.
//! - A callee's own `unresolved_effects` are inherited (if the callee is opaque about something,
//!   so is the caller, transitively via the call), with `may_affect` remapped by the same
//!   positional/`SelfAttr`/`Global` rule as mutations — falling back to the original target
//!   unchanged when it can't be remapped, since it's still honest information about the callee.
//!
//! ## Fixpoint
//!
//! A call chain `a -> b -> c` needs one sweep of every call site per hop to fully settle, and
//! recursion (direct or mutual) needs the same treatment to avoid missing effects introduced by
//! a later sweep. Each sweep unions the *current* known summary of every callee onto its
//! callers; repeated until a sweep adds nothing (a fixpoint), bounded by `signatures.len() + 1`
//! sweeps — enough for the longest possible acyclic call chain through every function in the
//! module. Because every step only ever adds facts to a bounded universe (never removes), and a
//! cycle can only add the same bounded set of facts each function ever could, this also
//! guarantees termination for recursive call graphs: a fixpoint is reached, not an infinite loop.

use ruff_python_ast as ast;

use crate::model::{Mutation, MutationTarget, ParamKind, UnresolvedEffect};

use super::super::context::{CallReceiver, CallSite, ModuleAnalysis};
use super::super::pass::Pass;

pub(in crate::analyze) struct InterproceduralPass;

impl Pass for InterproceduralPass {
    fn run(&self, _module: &ast::ModModule, ctx: &mut ModuleAnalysis) {
        let max_iterations = ctx.signatures.len().saturating_add(1);
        for _ in 0..max_iterations {
            let mut changed = false;
            for caller in 0..ctx.signatures.len() {
                let sites = ctx.call_sites.get(caller).cloned().unwrap_or_default();
                for site in &sites {
                    changed |= apply_call_site(ctx, caller, site);
                }
            }
            if !changed {
                break;
            }
        }
    }
}

/// Propagate `site`'s callee summary onto `caller`'s signature. Returns whether anything new was
/// added (drives the fixpoint loop).
fn apply_call_site(ctx: &mut ModuleAnalysis, caller: usize, site: &CallSite) -> bool {
    let Some(callee_sig) = ctx.signatures.get(site.callee) else {
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
            let target = remap_target(
                &m.target,
                &callee_positional,
                &site.arg_roots,
                &site.kwarg_roots,
                &site.receiver,
            )?;
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
                    remap_target(t, &callee_positional, &site.arg_roots, &site.kwarg_roots, &site.receiver)
                        .unwrap_or_else(|| t.clone())
                })
                .collect();
            UnresolvedEffect { reason: u.reason.clone(), callee: u.callee.clone(), may_affect }
        })
        .collect();

    let caller_sig = &mut ctx.signatures[caller];
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
    changed
}

/// Map a callee-side mutation/unresolved-effect root onto the caller. `None` if it can't be
/// attributed to any caller root — see the module doc's mapping rules.
fn remap_target(
    target: &MutationTarget,
    callee_positional: &[String],
    arg_roots: &[Option<MutationTarget>],
    kwarg_roots: &[(String, Option<MutationTarget>)],
    receiver: &CallReceiver,
) -> Option<MutationTarget> {
    match target {
        MutationTarget::SelfAttr { .. } => match receiver {
            CallReceiver::CallerSelf => Some(target.clone()),
            CallReceiver::Root(root) => Some(root.clone()),
            CallReceiver::None => None,
        },
        MutationTarget::Param { name } => {
            if let Some((_, root)) = kwarg_roots.iter().find(|(kw, _)| kw == name) {
                return root.clone();
            }
            let idx = callee_positional.iter().position(|p| p == name)?;
            arg_roots.get(idx).cloned().flatten()
        }
        MutationTarget::Global { .. } => Some(target.clone()),
        MutationTarget::Nonlocal { .. } | MutationTarget::Unknown => None,
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
