//! The module-level pipeline passes, run in order by `analyze::analyze_module`: Imports ->
//! Declarations -> Shapes -> Effects -> Interprocedural -> Purity.

pub(super) mod declarations;
pub(super) mod effects;
pub(super) mod imports;
pub(super) mod interprocedural;
pub(super) mod purity;
pub(super) mod shapes;
