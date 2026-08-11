use ruff_python_ast as ast;
use crate::model::*;
use super::super::super::collect::aliases::{dotted_attr, leftmost_name};
use super::super::super::collect::mutations::{is_known_readonly_method, is_mutating_method};
use super::super::super::collect::exceptions::call_implicit_exception;
use super::super::super::context::{CallReceiver, CallSite, ImportCallSite};
use super::super::declarations::resolve_unique;
use super::builtins::is_known_pure_builtin;
use super::Walker;

/// Where a `print(...)` call's output goes, resolved from its `file` keyword argument.
enum PrintTarget {
    Stdout,
    Stderr,
    /// A `file=` keyword is present but its target isn't statically resolvable; the may-set
    /// must cover both channels rather than guess.
    Unknown,
}

/// Classify a `print(...)` call's destination: no `file=` keyword ⇒ [`PrintTarget::Stdout`];
/// `file=sys.stderr` ⇒ [`PrintTarget::Stderr`]; any other `file=` expression ⇒
/// [`PrintTarget::Unknown`] (record both channels).
fn print_file_target(args: &ast::Arguments) -> PrintTarget {
    let Some(kw) = args.keywords.iter().find(|kw| kw.arg.as_ref().is_some_and(|a| a.as_str() == "file")) else {
        return PrintTarget::Stdout;
    };
    if dotted_attr(&kw.value).as_deref() == Some("sys.stderr") {
        PrintTarget::Stderr
    } else {
        PrintTarget::Unknown
    }
}

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
                            receiver: CallReceiver::CallerSelf,
                            arg_roots,
                            kwarg_roots,
                        });
                        if has_unpack {
                            self.acknowledge_unpacked_call(attr.attr.as_str(), &call.arguments);
                        }
                    } else if let Some(Shape::Instance(class)) = self.facts.env_shape(&attr.value)
                        && let Some(callee) = resolve_unique(
                            self.facts.declarations,
                            Some(class.as_str()),
                            attr.attr.as_str(),
                        )
                    {
                        // `x.method(...)` where `x`'s settled shape is EXACTLY one
                        // `Shape::Instance(C)` (a `Union` containing instances never matches this
                        // arm — resolving to just one member would under-approximate the others)
                        // and `method` resolves uniquely on `C` itself (an inherited method,
                        // defined on a base class, isn't owned by `C` in the declarations table
                        // and so deliberately stays unresolved below — resolving it would risk
                        // misattributing effects to the wrong class if `C` overrides it). The call
                        // resolves — and thus the callee's raises/io/unresolved effects propagate
                        // — regardless of whether `x` itself is a trackable root; the receiver
                        // maps its `SelfAttr` mutations onto `x` only when it is.
                        let receiver = match self.facts.resolve_target(&attr.value, None) {
                            Some(t) => CallReceiver::Root(t),
                            None => CallReceiver::None,
                        };
                        let arg_roots = self.positional_arg_roots(&call.arguments);
                        let kwarg_roots = self.keyword_arg_roots(&call.arguments);
                        self.facts.call_sites.push(CallSite {
                            callee,
                            receiver,
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
                        } else if !is_known_readonly_method(method) {
                            // An unrecognized method could mutate its receiver or raise — we can't
                            // see through it, so record it rather than assume purity. This holds
                            // whether or not the receiver is itself a trackable root (e.g. `x`
                            // bound to two different classes on two branches, so its shape is a
                            // `Union` rather than one `Instance` — see the arm above); `may_affect`
                            // is simply narrower (possibly empty) when it isn't.
                            let may_affect = self.facts.resolve_target(&attr.value, None).into_iter().collect();
                            self.facts.sig.unresolved_effects.push(UnresolvedEffect {
                                reason: "call_method_unknown".to_string(),
                                callee: Some(method.to_string()),
                                may_affect,
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
                            receiver: CallReceiver::None,
                            arg_roots,
                            kwarg_roots,
                        });
                        if has_unpack {
                            self.acknowledge_unpacked_call(n, &call.arguments);
                        }
                    } else if self.facts.classes.contains(n)
                        && let Some(callee) =
                            resolve_unique(self.facts.declarations, Some(n), "__init__")
                    {
                        // `Foo(...)` where `Foo` is a class declared in this module and declares
                        // its own `__init__` — resolves the same way `x.m(...)` resolves against a
                        // class (a class with no declared `__init__`, an inherited one, has no
                        // entry under its own name in `declarations` and so `resolve_unique`
                        // leaves it unresolved, deliberately). The receiver is always fresh — the
                        // call expression itself is the constructor, there's no caller-visible
                        // object yet to attribute `SelfAttr` mutations to — so raises/io/global
                        // writes/unresolved effects propagate, but the receiver never does.
                        let arg_roots = self.positional_arg_roots(&call.arguments);
                        let kwarg_roots = self.keyword_arg_roots(&call.arguments);
                        self.facts.call_sites.push(CallSite {
                            callee,
                            receiver: CallReceiver::None,
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
                            "print" => {
                                match print_file_target(&call.arguments) {
                                    PrintTarget::Stdout => {
                                        self.facts.sig.io.push("stdout".to_string())
                                    }
                                    PrintTarget::Stderr => {
                                        self.facts.sig.io.push("stderr".to_string())
                                    }
                                    PrintTarget::Unknown => {
                                        self.facts.sig.io.push("stdout".to_string());
                                        self.facts.sig.io.push("stderr".to_string());
                                    }
                                }
                            }
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
