use ruff_python_ast as ast;
use crate::model::*;
use super::Walker;

impl Walker < '_ , '_ > {
        /// The caller-side root each positional argument resolves to, `None` where it doesn't root
        /// to a tracked target — parallel in order to `arguments.args`, for a [`CallSite`].
        ///
        /// Stops at the first `*`-unpacking argument: a splat of statically-unknown length makes
        /// every later positional index unreliable, so only the prefix before it maps by position.
        /// The unpacked roots themselves are carried by the `call_unpacked_args` acknowledgment
        /// (see [`Self::unpacked_arg_roots`]), not by positional mapping.
        pub fn positional_arg_roots(&self, arguments: &ast::Arguments) -> Vec<Option<MutationTarget>> {
            arguments
                .args
                .iter()
                .take_while(|arg| !matches!(arg, ast::Expr::Starred(_)))
                .map(|arg| self.facts.resolve_target(arg, None))
                .collect()
        }

        /// Same as [`Self::positional_arg_roots`], but for the unbound-superclass call form
        /// (`Base.method(self, ...)`), whose first positional argument is the receiver written
        /// explicitly rather than implicit in the attribute access. The callee's own parameter
        /// list is already receiver-less (`self` never appears in `EffectSignature::params`), so
        /// the position->parameter mapping must skip that first argument too.
        pub fn positional_arg_roots_skip_first(&self, arguments: &ast::Arguments) -> Vec<Option<MutationTarget>> {
            arguments
                .args
                .iter()
                .skip(1)
                .take_while(|arg| !matches!(arg, ast::Expr::Starred(_)))
                .map(|arg| self.facts.resolve_target(arg, None))
                .collect()
        }

        /// The caller-side root each keyword argument resolves to, paired with its name — for a
        /// [`CallSite`]/[`ImportCallSite`]. A `**kwargs`-unpacking keyword (no name) is skipped: it
        /// can't be matched to a single callee parameter (its roots travel via
        /// [`Self::unpacked_arg_roots`] instead).
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

        /// Whether the call unpacks arguments (`f(*xs)` / `f(**kw)`) — argument->parameter
        /// mapping is then incomplete and the call needs a `call_unpacked_args` acknowledgment.
        pub fn call_has_unpack(arguments: &ast::Arguments) -> bool {
            arguments.args.iter().any(|a| matches!(a, ast::Expr::Starred(_)))
                || arguments.keywords.iter().any(|kw| kw.arg.is_none())
        }

        /// Tracked roots the argument->parameter mapping cannot attribute because of unpacking:
        /// every `*`-unpacked iterable, every positional argument at or after the first
        /// `*`-unpack (its binding position depends on the splat's runtime length), and every
        /// `**`-unpacked mapping. These are the objects a resolved callee may reach that
        /// [`Self::positional_arg_roots`]/[`Self::keyword_arg_roots`] make no claim about.
        pub fn unpacked_arg_roots(&self, arguments: &ast::Arguments) -> Vec<MutationTarget> {
            let mut out = Vec::new();
            let mut past_star = false;
            for arg in arguments.args.iter() {
                if let ast::Expr::Starred(s) = arg {
                    past_star = true;
                    if let Some(t) = self.facts.resolve_target(&s.value, None) {
                        out.push(t);
                    }
                } else if past_star
                    && let Some(t) = self.facts.resolve_target(arg, None)
                {
                    out.push(t);
                }
            }
            for kw in arguments.keywords.iter() {
                if kw.arg.is_none()
                    && let Some(t) = self.facts.resolve_target(&kw.value, None)
                {
                    out.push(t);
                }
            }
            out
        }

        /// Targets among `arguments` that resolve to a tracked root (anything passed into an
        /// opaque call may be mutated by it): positional arguments (`*`-unpacks resolve to the
        /// unpacked iterable's root), then keyword arguments (named values and `**`-unpacked
        /// mappings alike).
        pub fn args_targets(&self, arguments: &ast::Arguments) -> Vec<MutationTarget> {
            let mut out = Vec::new();
            for arg in arguments.args.iter() {
                let expr = match arg {
                    ast::Expr::Starred(s) => s.value.as_ref(),
                    other => other,
                };
                if let Some(t) = self.facts.resolve_target(expr, None) {
                    out.push(t);
                }
            }
            for kw in arguments.keywords.iter() {
                if let Some(t) = self.facts.resolve_target(&kw.value, None) {
                    out.push(t);
                }
            }
            out
        }

}
