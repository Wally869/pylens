use ruff_python_ast as ast;
use crate::model::*;
use super::Walker;

impl Walker < '_ , '_ > {
        /// Targets among `arguments` that resolve to a tracked root (params passed into a call
        /// may be mutated by it).
        pub fn args_targets(&self, arguments: &ast::Arguments) -> Vec<MutationTarget> {
            let mut out = Vec::new();
            for arg in arguments.args.iter() {
                if let Some(t) = self.facts.resolve_target(arg, None) {
                    out.push(t);
                }
            }
            out
        }

}
