//! Static effect analysis: an ordered pass pipeline over a module AST, producing an
//! [`EffectSignature`] per function/method using may-set (over-approximating) semantics. See
//! docs/DESIGN.md.

mod collect;
mod context;
mod models;
mod pass;
mod passes;

pub use context::ImportCallSite;
pub use passes::declarations::{DeclInfo, ReceiverKind};
pub use passes::imports::collect_imports;

use ruff_python_ast as ast;

use crate::model::EffectSignature;
use context::ModuleAnalysis;
use pass::Pass;

/// A module's effect signatures plus, per function (same order/index as `signatures`), the
/// structured call sites the Effects pass recorded for calls to IMPORTED bindings. Additive to
/// [`analyze_module`] — used only by the project layer's cross-file propagation
/// (`project::interproc`), which needs the raw call sites `analyze_module` discards. Single-file
/// analysis (`analyze_module`/`analyze_source`) is unaffected.
pub struct ModuleAnalysisResult {
    pub signatures: Vec<EffectSignature>,
    pub import_call_sites: Vec<Vec<ImportCallSite>>,
}

/// Analyze every function and method defined at module top level (functions) or directly in
/// a class body (methods). Nested functions are not descended into yet.
///
/// Runs the pipeline **Imports -> Declarations -> Shapes -> Effects -> Interprocedural ->
/// TypeCheck -> Purity**. Shapes settles a fixpoint-inferred name->shape environment per
/// function before Effects runs, so Effects reads final parameter shapes instead of voting
/// during its own walk. Interprocedural resolves calls to functions/methods defined in this
/// same module (recorded by Effects as structured call sites) and propagates the callee's
/// effects onto the caller to a fixpoint. TypeCheck then compares each function's final
/// `returns` may-set against its untrusted `declared_return` annotation, flagging only full
/// disjointness. Purity runs last, classifying callers against their complete, propagated
/// effect set rather than treating every local call as opaque.
pub fn analyze_module(module: &ast::ModModule, src: &str) -> Vec<EffectSignature> {
    analyze_module_with_call_sites(module, src).signatures
}

/// Same pipeline as [`analyze_module`], additionally returning each function's imported call
/// sites — see [`ModuleAnalysisResult`]. `src` is the module's source text, needed to resolve
/// each function's `body_lines` (byte offsets -> line numbers).
pub fn analyze_module_with_call_sites(module: &ast::ModModule, src: &str) -> ModuleAnalysisResult {
    let mut ctx = ModuleAnalysis::new(src);
    let pipeline: Vec<Box<dyn Pass>> = vec![
        Box::new(passes::imports::ImportsPass),
        Box::new(passes::declarations::DeclarationsPass),
        Box::new(passes::shapes::ShapesPass),
        Box::new(passes::effects::EffectsPass),
        Box::new(passes::interprocedural::InterproceduralPass),
        Box::new(passes::type_check::TypeCheckPass),
        Box::new(passes::purity::PurityPass),
    ];
    for pass in &pipeline {
        pass.run(module, &mut ctx);
    }
    ModuleAnalysisResult {
        signatures: ctx.signatures,
        import_call_sites: ctx.import_call_sites,
    }
}
