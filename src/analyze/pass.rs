//! The pipeline injection point: a [`Pass`] reads and writes the shared [`ModuleAnalysis`]
//! context. Passes run in the fixed order the driver lists them today; the trait itself doesn't
//! preclude reordering, inserting new passes (e.g. a future Interprocedural pass between Effects
//! and Purity), or iterating a pass to a fixpoint later.

use ruff_python_ast as ast;

use super::context::ModuleAnalysis;

/// One stage of the module analysis pipeline.
pub(in crate::analyze) trait Pass {
    /// Read/update `ctx` from `module`. Each pass sees the accumulated state of everything that
    /// ran before it.
    fn run(&self, module: &ast::ModModule, ctx: &mut ModuleAnalysis);
}
