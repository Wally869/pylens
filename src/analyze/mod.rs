//! Static effect analysis: an ordered pass pipeline over a module AST, producing an
//! [`EffectSignature`] per function/method using may-set (over-approximating) semantics. See
//! DESIGN.md.

mod collect;
mod context;
mod pass;
mod passes;

pub use passes::declarations::{DeclInfo, ReceiverKind};
pub use passes::imports::collect_imports;

use ruff_python_ast as ast;

use crate::model::EffectSignature;
use context::ModuleAnalysis;
use pass::Pass;

/// Analyze every function and method defined at module top level (functions) or directly in
/// a class body (methods). Nested functions are not descended into yet.
///
/// Runs the pipeline **Imports -> Declarations -> Shapes -> Effects -> Purity**. Shapes settles
/// a fixpoint-inferred name->shape environment per function before Effects runs, so Effects
/// reads final parameter shapes instead of voting during its own walk. A future Interprocedural
/// pass — resolving intra-file calls via the Declarations symbol table — would insert between
/// Effects and Purity.
pub fn analyze_module(module: &ast::ModModule) -> Vec<EffectSignature> {
    let mut ctx = ModuleAnalysis::new();
    let pipeline: Vec<Box<dyn Pass>> = vec![
        Box::new(passes::imports::ImportsPass),
        Box::new(passes::declarations::DeclarationsPass),
        Box::new(passes::shapes::ShapesPass),
        Box::new(passes::effects::EffectsPass),
        Box::new(passes::purity::PurityPass),
    ];
    for pass in &pipeline {
        pass.run(module, &mut ctx);
    }
    ctx.signatures
}
