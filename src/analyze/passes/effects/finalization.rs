use crate::model::*;
use super::super::super::context::FunctionFacts;
use super::dedup::{dedup, dedup_mutations};

pub(super) fn finish(facts: FunctionFacts, param_defs: Vec<ParamInfo>) -> EffectSignature {
    let mut sig = facts.sig;
    dedup(&mut sig.returns);
    dedup(&mut sig.raises.explicit);
    dedup(&mut sig.raises.implicit);
    dedup(&mut sig.global_writes);
    dedup(&mut sig.io);
    dedup_mutations(&mut sig.mutations);
    sig.params = param_defs
        .into_iter()
        .map(|pi| {
            // A parameter unrelatedly rebound in the body (`x = Box()`) has its shape reset to
            // `Any` here — `facts.shapes` itself stays unfrozen (see `ModuleAnalysis::shapes`)
            // because the post-rebind local fact is sound for resolving calls made through the
            // name, just not for the parameter's own declared/generated shape, which no rebind
            // inside the callee can narrow for the caller.
            let shape = if facts.frozen_params.contains(&pi.name) {
                Shape::Any
            } else {
                facts.shapes.get(&pi.name).cloned().unwrap_or(Shape::Any)
            };
            ParamInfo {
                shape,
                guard_samples: facts.guard_samples.get(&pi.name).cloned().unwrap_or_default(),
                hints: facts.hints.get(&pi.name).cloned().unwrap_or_default(),
                ..pi
            }
        })
        .collect();
    sig.uses = facts
        .used_imports
        .iter()
        .filter_map(|b| facts.imports.get(b).map(|m| ImportUse { binding: b.clone(), module: m.clone() }))
        .collect();
    sig.may_use_star = facts.may_use_star;
    sig
}
