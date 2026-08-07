use super::super::context::FunctionFacts;

mod builtins;

mod dedup;

mod setup;

/// Runs the per-function walk for every module-level function and method, appending the
/// resulting (not-yet-purity-classified) signatures to `ModuleAnalysis::signatures`.
pub(in crate::analyze) struct EffectsPass;

/// The per-function AST walker: drives `FunctionFacts` and the `collect/` collectors over one
/// function body.
pub(super) struct Walker<'f, 'a> {
    facts: &'f mut FunctionFacts<'a>,
}


/// Entry points for the effects walker: `run` for single and module functions.
mod driver;

/// Main function-level analysis logic.
mod function_analysis;

/// Finalization and effect signature completion.
mod finalization;

/// Statement-level walking (body, if/for/while/try, comprehensions).
mod statements;

/// Target handling for assignments, augmented assignments, and deletions.
mod targets;

/// Expression visiting and recursive expression walks.
mod expressions;

/// Call site analysis and argument resolution.
mod calls;

/// Argument root extraction (positional and keyword).
mod argument_roots;

/// Call target resolution for arguments.
mod argument_targets;

/// Guard test tracking and receiver context helpers.
mod helpers;

