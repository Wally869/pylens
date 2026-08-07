use ruff_python_ast as ast;
use crate::model::*;
use super::super::super::collect::aliases::{dotted_attr, leftmost_name};
use super::super::super::collect::mutations::{is_known_readonly_method, is_mutating_method};
use super::super::super::collect::exceptions::call_implicit_exception;
use super::super::super::context::{CallSite, ImportCallSite};
use super::super::declarations::resolve_unique;
use super::builtins::is_known_pure_builtin;
use super::Walker;

impl Walker < '_ , '_ > {
        pub fn visit_call(&mut self, call: &ast::ExprCall) {
            // Argument unpacking (`f(*xs)` / `f(**kw)`) can itself raise `TypeError` (non-iterable
            // splat, non-mapping `**`, arity/duplicate-keyword mismatch) before any callee code
            // runs — a call-operation effect, independent of what the callee is.
            let has_unpack = Self::call_has_unpack(&call.arguments);
            if has_unpack {
                self.facts.sig.raises.implicit.push("TypeError".to_string());
            }
            match call.func.as_ref() {
                // Method call: `base.method(...)`.
                ast::Expr::Attribute(attr) => {
                    // A call through an imported name (`np.mean(...)`, `os.path.join(...)`) is a
                    // foreign effect we can't see through — record it (so the function isn't
                    // mistaken for pure) rather than treating it as a value method.
                    if let Some(base) = leftmost_name(&attr.value)
                        && self.facts.imports.contains_key(base)
                    {
                        self.facts.sig.unresolved_effects.push(UnresolvedEffect {
                            reason: "call_import".to_string(),
                            callee: dotted_attr(&call.func),
                            may_affect: self.args_targets(&call.arguments),
                        });
                        // Only a direct `binding.attr(...)` (not a deeper chain like
                        // `binding.sub.attr(...)`) lines up unambiguously with a project-local
                        // `binding.attr` symbol — see `ImportCallSite` doc.
                        if matches!(attr.value.as_ref(), ast::Expr::Name(_)) {
                            let arg_roots = self.positional_arg_roots(&call.arguments);
                            let kwarg_roots = self.keyword_arg_roots(&call.arguments);
                            self.facts.import_call_sites.push(ImportCallSite {
                                binding: base.to_string(),
                                attr: Some(attr.attr.to_string()),
                                arg_roots,
                                kwarg_roots,
                                has_unpack,
                            });
                        }
                    } else if self.is_self_receiver(&attr.value)
                        && let Some(callee) = resolve_unique(
                            self.facts.declarations,
                            self.facts.owner.as_deref(),
                            attr.attr.as_str(),
                        )
                    {
                        // `self.method(...)` / `cls.method(...)` resolving to a method defined in
                        // this same class — a structured call site, not an opaque one.
                        let arg_roots = self.positional_arg_roots(&call.arguments);
                        let kwarg_roots = self.keyword_arg_roots(&call.arguments);
                        self.facts.call_sites.push(CallSite {
                            callee,
                            via_self: true,
                            arg_roots,
                            kwarg_roots,
                        });
                        if has_unpack {
                            self.acknowledge_unpacked_call(attr.attr.as_str(), &call.arguments);
                        }
                    } else {
                        let method = attr.attr.as_str();
                        if is_mutating_method(method) {
                            if let Some(t) = self.facts.resolve_target(&attr.value, None) {
                                self.facts.add_mutation(t, MutationKind::Method, Some(method));
                            }
                        } else if !is_known_readonly_method(method)
                            && let Some(t) = self.facts.resolve_target(&attr.value, None)
                        {
                            // An unrecognized method on a tracked root could mutate its receiver —
                            // we can't see through it, so record it rather than assume purity.
                            self.facts.sig.unresolved_effects.push(UnresolvedEffect {
                                reason: "call_method_unknown".to_string(),
                                callee: Some(method.to_string()),
                                may_affect: vec![t],
                            });
                        }
                    }
                }
                // Plain function call: `name(...)`.
                ast::Expr::Name(name) => {
                    let n = name.id.as_str();
                    if self.facts.imports.contains_key(n) {
                        // A directly-imported callable (`from json import dumps; dumps(x)`).
                        self.facts.sig.unresolved_effects.push(UnresolvedEffect {
                            reason: "call_import".to_string(),
                            callee: Some(n.to_string()),
                            may_affect: self.args_targets(&call.arguments),
                        });
                        let arg_roots = self.positional_arg_roots(&call.arguments);
                        let kwarg_roots = self.keyword_arg_roots(&call.arguments);
                        self.facts.import_call_sites.push(ImportCallSite {
                            binding: n.to_string(),
                            attr: None,
                            arg_roots,
                            kwarg_roots,
                            has_unpack,
                        });
                    } else if let Some(callee) = resolve_unique(self.facts.declarations, None, n) {
                        // A call to a module-level function defined in this same module — a
                        // structured call site, not an opaque one.
                        let arg_roots = self.positional_arg_roots(&call.arguments);
                        let kwarg_roots = self.keyword_arg_roots(&call.arguments);
                        self.facts.call_sites.push(CallSite {
                            callee,
                            via_self: false,
                            arg_roots,
                            kwarg_roots,
                        });
                        if has_unpack {
                            self.acknowledge_unpacked_call(n, &call.arguments);
                        }
                    } else {
                        if let Some(exc) = call_implicit_exception(n) {
                            self.facts.sig.raises.implicit.push(exc.to_string());
                        }
                        match n {
                            "print" => self.facts.sig.io.push("stdout".to_string()),
                            "open" => self.facts.sig.io.push("filesystem".to_string()),
                            "input" => self.facts.sig.io.push("stdin".to_string()),
                            "setattr" | "delattr" | "exec" | "eval" => {
                                self.facts.sig.unresolved_effects.push(UnresolvedEffect {
                                    reason: format!("dynamic_{n}"),
                                    callee: Some(n.to_string()),
                                    may_affect: self.args_targets(&call.arguments),
                                });
                            }
                            _ if !is_known_pure_builtin(n) => {
                                // The callee isn't a recognized builtin or import — record it even
                                // when none of its arguments root to a tracked target, since the
                                // call itself may raise, do I/O, or touch module state we can't see.
                                self.facts.sig.unresolved_effects.push(UnresolvedEffect {
                                    reason: "call_unknown_callee".to_string(),
                                    callee: Some(n.to_string()),
                                    may_affect: self.args_targets(&call.arguments),
                                });
                                // The callee isn't a builtin or a known import; if a star import is
                                // in scope, it may have come from there.
                                if self.facts.has_star {
                                    self.facts.may_use_star = true;
                                }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
            self.visit_expr(&call.func);
            for arg in call.arguments.args.iter() {
                self.visit_expr(arg);
            }
            for kw in call.arguments.keywords.iter() {
                self.visit_expr(&kw.value);
            }
        }

        /// A resolved (local/self-method) call that unpacks arguments hands the callee tracked
        /// objects the argument->parameter mapping can't attribute — acknowledge that blind spot
        /// so the may-set never silently claims completeness over them.
        fn acknowledge_unpacked_call(&mut self, callee: &str, arguments: &ast::Arguments) {
            let may_affect = self.unpacked_arg_roots(arguments);
            self.facts.sig.unresolved_effects.push(UnresolvedEffect {
                reason: "call_unpacked_args".to_string(),
                callee: Some(callee.to_string()),
                may_affect,
            });
        }

}
