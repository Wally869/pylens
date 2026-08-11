use std::collections::HashMap;
use ruff_python_ast as ast;
use crate::model::*;
use super::super::super::collect::hints::infer_hints;
use super::super::super::context::{CallSite, FunctionFacts, ImportCallSite, ModuleCtx};
use super::super::declarations::ReceiverKind;
use super::setup::{annotation_name, collect_param_defs, collect_param_names, decorator_names};
use super::{Walker};
use super::finalization::finish;

/// Analyze a single function definition. `receiver` is this def's receiver kind from the
/// Declarations pass (`self`/`cls`/none) — the first param is the receiver, not a generatable
/// param, for both instance and class methods. `imports` maps each in-scope import binding to
/// its module; `has_star` flags a `from m import *` in scope.
pub(super) fn analyze_function(
    def: &ast::StmtFunctionDef,
    kind: DefKind,
    receiver: ReceiverKind,
    module: ModuleCtx,
    shapes: HashMap<String, Shape>,
    owner: Option<&str>,
) -> (EffectSignature, Vec<CallSite>, Vec<ImportCallSite>) {
    let params = collect_param_names(&def.parameters);
    let self_param = match receiver {
        ReceiverKind::SelfParam | ReceiverKind::Cls => params.first().cloned(),
        ReceiverKind::None => None,
    };
    let param_defs = collect_param_defs(&def.parameters, self_param.as_deref());
    let mut sig = EffectSignature::new(def.name.as_str(), kind);
    sig.declared_return = annotation_name(def.returns.as_deref());
    sig.decorators = decorator_names(def);

    let param_names: Vec<String> = param_defs.iter().map(|p| p.name.clone()).collect();
    let mut facts =
        FunctionFacts::new(self_param, &params, module, shapes, sig, owner.map(str::to_string));
    facts.hints = infer_hints(&def.body, &param_names);
    Walker { facts: &mut facts }.run(&def.body);
    let call_sites = std::mem::take(&mut facts.call_sites);
    let import_call_sites = std::mem::take(&mut facts.import_call_sites);
    (finish(facts, param_defs), call_sites, import_call_sites)
}
