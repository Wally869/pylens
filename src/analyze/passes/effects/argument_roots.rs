use ruff_python_ast as ast;
use crate::model::*;
use super::Walker;

impl Walker < '_ , '_ > {
        /// The caller-side root each positional argument resolves to, `None` where it doesn't root
        /// to a tracked target — parallel in order to `arguments.args`, for a [`CallSite`].
        pub fn positional_arg_roots(&self, arguments: &ast::Arguments) -> Vec<Option<MutationTarget>> {
            arguments.args.iter().map(|arg| self.facts.resolve_target(arg, None)).collect()
        }

        /// The caller-side root each keyword argument resolves to, paired with its name — for a
        /// [`CallSite`]/[`ImportCallSite`]. A `**kwargs`-unpacking keyword (no name) is skipped: it
        /// can't be matched to a single callee parameter.
        pub fn keyword_arg_roots(&self, arguments: &ast::Arguments) -> Vec<(String, Option<MutationTarget>)> {
            arguments
                .keywords
                .iter()
                .filter_map(|kw| {
                    let name = kw.arg.as_ref()?.id.to_string();
                    Some((name, self.facts.resolve_target(&kw.value, None)))
                })
                .collect()
        }

}
