use ruff_python_ast as ast;
use crate::model::*;
use super::Walker;

impl Walker < '_ , '_ > {
        pub fn handle_assign_target(&mut self, target: &ast::Expr, value: &ast::Expr) {
            match target {
                ast::Expr::Name(name) => {
                    let n = name.id.as_str();
                    // Alias tracking: `x = y` where y aliases a param.
                    if let ast::Expr::Name(rhs) = value
                        && let Some(root) = self.facts.aliases.get(rhs.id.as_str()).cloned()
                    {
                        self.facts.aliases.insert(n.to_string(), root);
                        return;
                    }
                    // Rebinding: x no longer aliases its original parameter object. This holds
                    // even for `x = Foo(...)` where `Foo` is a same-module class: the freshly
                    // constructed instance is never the caller's object, so a later
                    // `x.method(...)`'s `SelfAttr` mutations can't be attributed to any
                    // caller-visible root either (`CallReceiver::Root` requires `x` to already be
                    // a tracked root — a parameter, `self`-attribute, global, or nonlocal — which
                    // a fresh local, by construction, is not). Mirrors the treatment of a fresh
                    // local list/dict/set: the mutation is real but invisible to the caller, so it
                    // isn't recorded as an effect at all, never fabricated onto a name that isn't
                    // actually a parameter.
                    self.facts.aliases.remove(n);
                    // Writing a module-level global declared in this scope.
                    if self.facts.globals.contains(n) {
                        self.facts.sig.global_writes.push(n.to_string());
                    }
                }
                ast::Expr::Subscript(sub) => {
                    if let Some(t) = self.facts.resolve_target(&sub.value, None) {
                        self.facts.add_mutation(t, MutationKind::SubscriptSet, None);
                    }
                    // The key may raise `TypeError` on assignment too (`d[k] = v` with an
                    // unhashable `k`) — see the read-position comment in `visit_expr`.
                    self.facts.note_type_error_candidate(&sub.slice);
                    // The BASE itself may not be subscriptable at all (`alias[k] = v` where
                    // `alias` aliases an `Any`-typed param) — see the read-position comment.
                    self.facts.note_type_error_candidate(&sub.value);
                }
                ast::Expr::Attribute(attr) => {
                    if let Some(t) = self.facts.resolve_target(&attr.value, Some(attr.attr.as_str())) {
                        self.facts.add_mutation(t, MutationKind::AttrSet, Some(attr.attr.as_str()));
                    }
                }
                ast::Expr::Tuple(tuple) => {
                    for el in &tuple.elts {
                        self.handle_assign_target(el, value);
                    }
                }
                ast::Expr::List(list) => {
                    for el in &list.elts {
                        self.handle_assign_target(el, value);
                    }
                }
                _ => {}
            }
        }

        pub fn handle_aug_target(&mut self, target: &ast::Expr) {
            match target {
                ast::Expr::Subscript(sub) => {
                    if let Some(t) = self.facts.resolve_target(&sub.value, None) {
                        self.facts.add_mutation(t, MutationKind::AugSubscript, None);
                    }
                }
                ast::Expr::Attribute(attr) => {
                    if let Some(t) = self.facts.resolve_target(&attr.value, Some(attr.attr.as_str())) {
                        self.facts.add_mutation(t, MutationKind::AugAttr, Some(attr.attr.as_str()));
                    }
                }
                ast::Expr::Name(name) => {
                    let n = name.id.as_str();
                    if self.facts.globals.contains(n) {
                        self.facts.sig.global_writes.push(n.to_string());
                    } else if let Some(t) = self.facts.resolve_target(target, None) {
                        // `p += [1]` may mutate `p` in place (list/set `+=`); the caller owns `p`,
                        // so this is a may-mutation even though `+=` on an int would not mutate.
                        self.facts.add_mutation(t, MutationKind::AugName, None);
                    }
                }
                _ => {}
            }
        }

        pub fn handle_delete_target(&mut self, target: &ast::Expr) {
            match target {
                ast::Expr::Subscript(sub) => {
                    if let Some(t) = self.facts.resolve_target(&sub.value, None) {
                        self.facts.add_mutation(t, MutationKind::SubscriptDel, None);
                    }
                    // The key may raise `TypeError` on deletion too (`del d[k]` with an
                    // unhashable `k`) — see the read-position comment in `visit_expr`.
                    self.facts.note_type_error_candidate(&sub.slice);
                    // The BASE itself may not be subscriptable at all — see the read-position
                    // comment.
                    self.facts.note_type_error_candidate(&sub.value);
                }
                ast::Expr::Attribute(attr) => {
                    if let Some(t) = self.facts.resolve_target(&attr.value, Some(attr.attr.as_str())) {
                        self.facts.add_mutation(t, MutationKind::AttrDel, Some(attr.attr.as_str()));
                    }
                }
                _ => {}
            }
        }

}
