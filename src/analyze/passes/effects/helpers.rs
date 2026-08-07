use ruff_python_ast as ast;
use super::super::super::collect::guards::guard_samples;
use super::Walker;

impl Walker < '_ , '_ > {
        /// Extract guard-derived literal samples from a test expression (`if`/`while`/`assert`
        /// tests, ternary conditions) and record them against the parameters they guard — see
        /// `collect::guards`.
        pub fn note_guard_test(&mut self, test: &ast::Expr) {
            for (root, value) in guard_samples(self.facts, test) {
                self.facts.add_guard_sample(root, value);
            }
        }

        /// Whether `expr` is a bare reference to this function's own receiver (`self`/`cls`) —
        /// the shape a call must have to be eligible for local-method resolution (`self.cache
        /// .evict()` doesn't qualify: the receiver there is `self.cache`, not `self`).
        pub fn is_self_receiver(&self, expr: &ast::Expr) -> bool {
            matches!(expr, ast::Expr::Name(n) if Some(n.id.as_str()) == self.facts.self_param.as_deref())
        }

}
