//! Effects pass: the single per-function AST walk that produces the returns/mutations/
//! exceptions/shapes/unresolved-effects facts, delegating to the collectors under `collect/`.

use std::collections::HashMap;

use ruff_python_ast as ast;

use crate::model::*;

use super::super::collect::aliases::{dotted_attr, leftmost_name};
use super::super::collect::mutations::{is_known_readonly_method, is_mutating_method};
use super::super::collect::returns::{can_fall_through, classify_return};
use super::super::collect::exceptions::{
    binop_implicit_exception, call_implicit_exception, exception_name, is_ordered_compare,
    subscript_read_exceptions,
};
use super::super::context::{FunctionFacts, ModuleAnalysis};
use super::super::pass::Pass;
use super::declarations::ReceiverKind;

mod builtins;
mod dedup;
mod setup;

use builtins::is_known_pure_builtin;
use dedup::{dedup, dedup_mutations};
use setup::{annotation_name, collect_param_defs, collect_param_names, decorator_names};

/// Runs the per-function walk for every module-level function and method, appending the
/// resulting (not-yet-purity-classified) signatures to `ModuleAnalysis::signatures`.
pub(in crate::analyze) struct EffectsPass;

impl Pass for EffectsPass {
    fn run(&self, module: &ast::ModModule, ctx: &mut ModuleAnalysis) {
        // The Declarations pass walked the module in this same order, so its receiver-kind
        // table (and the Shapes pass's per-function shape maps) line up one-to-one with this
        // traversal.
        let mut receivers = ctx.declarations.iter().map(|d| d.receiver);
        let mut shapes_iter = ctx.shapes.iter();
        for stmt in &module.body {
            match stmt {
                ast::Stmt::FunctionDef(def) => {
                    let receiver = receivers.next().unwrap_or(ReceiverKind::None);
                    let shapes = shapes_iter.next().cloned().unwrap_or_default();
                    let sig = analyze_function(
                        def,
                        DefKind::Function,
                        receiver,
                        &ctx.bindings,
                        ctx.has_star,
                        shapes,
                    );
                    ctx.signatures.push(sig);
                }
                ast::Stmt::ClassDef(class) => {
                    for member in &class.body {
                        if let ast::Stmt::FunctionDef(def) = member {
                            let receiver = receivers.next().unwrap_or(ReceiverKind::None);
                            let shapes = shapes_iter.next().cloned().unwrap_or_default();
                            let mut sig = analyze_function(
                                def,
                                DefKind::Method,
                                receiver,
                                &ctx.bindings,
                                ctx.has_star,
                                shapes,
                            );
                            sig.owner = Some(class.name.as_str().to_string());
                            ctx.signatures.push(sig);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Analyze a single function definition. `receiver` is this def's receiver kind from the
/// Declarations pass (`self`/`cls`/none) — the first param is the receiver, not a generatable
/// param, for both instance and class methods. `imports` maps each in-scope import binding to
/// its module; `has_star` flags a `from m import *` in scope.
fn analyze_function(
    def: &ast::StmtFunctionDef,
    kind: DefKind,
    receiver: ReceiverKind,
    imports: &HashMap<String, ModuleRef>,
    has_star: bool,
    shapes: HashMap<String, Shape>,
) -> EffectSignature {
    let params = collect_param_names(&def.parameters);
    let self_param = match receiver {
        ReceiverKind::SelfParam | ReceiverKind::Cls => params.first().cloned(),
        ReceiverKind::None => None,
    };
    let param_defs = collect_param_defs(&def.parameters, self_param.as_deref());
    let mut sig = EffectSignature::new(def.name.as_str(), kind);
    sig.declared_return = annotation_name(def.returns.as_deref());
    sig.decorators = decorator_names(def);

    let mut facts = FunctionFacts::new(self_param, &params, imports, has_star, shapes, sig);
    Walker { facts: &mut facts }.run(&def.body);
    finish(facts, param_defs)
}

fn finish(facts: FunctionFacts, param_defs: Vec<ParamInfo>) -> EffectSignature {
    let mut sig = facts.sig;
    dedup(&mut sig.returns);
    dedup(&mut sig.raises.explicit);
    dedup(&mut sig.raises.implicit);
    dedup(&mut sig.global_writes);
    dedup(&mut sig.io);
    dedup_mutations(&mut sig.mutations);
    sig.params = param_defs
        .into_iter()
        .map(|pi| ParamInfo {
            shape: facts.shapes.get(&pi.name).cloned().unwrap_or(Shape::Any),
            ..pi
        })
        .collect();
    sig.uses = facts
        .used_imports
        .iter()
        .filter_map(|b| facts.imports.get(b).map(|m| ImportUse { binding: b.clone(), module: m.clone() }))
        .collect();
    sig.may_use_star = facts.may_use_star;
    sig
}

/// The per-function AST walker: drives `FunctionFacts` and the `collect/` collectors over one
/// function body.
struct Walker<'f, 'a> {
    facts: &'f mut FunctionFacts<'a>,
}

impl Walker<'_, '_> {
    fn run(&mut self, body: &[ast::Stmt]) {
        // Two notes on the may-set model:
        // - a body that can fall off the end contributes a `None` return.
        // - returns/raises are unioned over all exits.
        self.visit_body(body);
        if can_fall_through(body) {
            self.facts.add_return(ReturnKind::None);
        }
    }

    fn visit_body(&mut self, body: &[ast::Stmt]) {
        for stmt in body {
            self.visit_stmt(stmt);
        }
    }

    fn visit_stmt(&mut self, stmt: &ast::Stmt) {
        use ast::Stmt;
        match stmt {
            Stmt::Return(ret) => match ret.value.as_deref() {
                Some(expr) => {
                    let kind = classify_return(expr);
                    self.facts.add_return(kind);
                    self.visit_expr(expr);
                }
                None => self.facts.add_return(ReturnKind::None),
            },
            Stmt::Raise(raise) => {
                if let Some(exc) = raise.exc.as_deref() {
                    if let Some(name) = exception_name(exc) {
                        self.facts.sig.raises.explicit.push(name);
                    }
                    // The exception *constructor* is not an effect — visit only its arguments
                    // so real effects there are still seen, without flagging the exception
                    // type itself as an unknown mutating callee.
                    match exc {
                        ast::Expr::Call(call) => {
                            for arg in call.arguments.args.iter() {
                                self.visit_expr(arg);
                            }
                            for kw in call.arguments.keywords.iter() {
                                self.visit_expr(&kw.value);
                            }
                        }
                        _ => self.visit_expr(exc),
                    }
                }
            }
            Stmt::Assert(assert_stmt) => {
                // Directly visible in the AST — same high-confidence tier as `raise`.
                self.facts.sig.raises.explicit.push("AssertionError".to_string());
                self.visit_expr(&assert_stmt.test);
                if let Some(msg) = assert_stmt.msg.as_deref() {
                    self.visit_expr(msg);
                }
            }
            Stmt::Global(g) => {
                for name in &g.names {
                    self.facts.globals.insert(name.as_str().to_string());
                }
            }
            Stmt::Nonlocal(n) => {
                for name in &n.names {
                    self.facts.nonlocals.insert(name.as_str().to_string());
                }
            }
            Stmt::Assign(assign) => {
                self.visit_expr(&assign.value);
                for target in &assign.targets {
                    self.handle_assign_target(target, &assign.value);
                }
            }
            Stmt::AugAssign(aug) => {
                self.visit_expr(&aug.value);
                self.handle_aug_target(&aug.target);
            }
            Stmt::AnnAssign(ann) => {
                if let Some(value) = ann.value.as_deref() {
                    self.visit_expr(value);
                    self.handle_assign_target(&ann.target, value);
                }
            }
            Stmt::Delete(del) => {
                for target in &del.targets {
                    self.handle_delete_target(target);
                }
            }
            Stmt::Expr(e) => self.visit_expr(&e.value),
            Stmt::If(if_stmt) => {
                self.visit_expr(&if_stmt.test);
                self.visit_body(&if_stmt.body);
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.visit_expr(test);
                    }
                    self.visit_body(&clause.body);
                }
            }
            Stmt::For(for_stmt) => {
                self.visit_expr(&for_stmt.iter);
                // `for row in matrix:` binds `row` to an element reached through `matrix` — a
                // mutation via `row` (`row[i] = ...`, `row.append(...)`, ...) is a (possibly
                // nested) mutation of `matrix`, so the loop variable aliases the iterable's
                // root for mutation-attribution purposes, same as a direct `q = p` alias.
                if let ast::Expr::Name(target) = for_stmt.target.as_ref()
                    && let Some(root) = self.facts.param_root(&for_stmt.iter)
                {
                    self.facts.aliases.insert(target.id.as_str().to_string(), root);
                }
                self.visit_body(&for_stmt.body);
                self.visit_body(&for_stmt.orelse);
            }
            Stmt::While(while_stmt) => {
                self.visit_expr(&while_stmt.test);
                self.visit_body(&while_stmt.body);
                self.visit_body(&while_stmt.orelse);
            }
            Stmt::With(with_stmt) => {
                for item in &with_stmt.items {
                    self.visit_expr(&item.context_expr);
                }
                self.visit_body(&with_stmt.body);
            }
            Stmt::Try(try_stmt) => {
                self.visit_body(&try_stmt.body);
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    self.visit_body(&h.body);
                }
                self.visit_body(&try_stmt.orelse);
                self.visit_body(&try_stmt.finalbody);
            }
            Stmt::Match(match_stmt) => {
                self.visit_expr(&match_stmt.subject);
                for case in &match_stmt.cases {
                    self.visit_body(&case.body);
                }
            }
            // Nested defs are separate scopes; not descended into yet.
            _ => {}
        }
    }

    fn handle_assign_target(&mut self, target: &ast::Expr, value: &ast::Expr) {
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
                // Rebinding: x no longer aliases its original parameter object.
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

    fn handle_aug_target(&mut self, target: &ast::Expr) {
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

    fn handle_delete_target(&mut self, target: &ast::Expr) {
        match target {
            ast::Expr::Subscript(sub) => {
                if let Some(t) = self.facts.resolve_target(&sub.value, None) {
                    self.facts.add_mutation(t, MutationKind::SubscriptDel, None);
                }
            }
            ast::Expr::Attribute(attr) => {
                if let Some(t) = self.facts.resolve_target(&attr.value, Some(attr.attr.as_str())) {
                    self.facts.add_mutation(t, MutationKind::AttrDel, Some(attr.attr.as_str()));
                }
            }
            _ => {}
        }
    }

    fn visit_expr(&mut self, expr: &ast::Expr) {
        use ast::Expr;
        match expr {
            Expr::Yield(y) => {
                self.facts.sig.is_generator = true;
                if let Some(v) = y.value.as_deref() {
                    self.visit_expr(v);
                }
            }
            Expr::YieldFrom(y) => {
                self.facts.sig.is_generator = true;
                self.visit_expr(&y.value);
            }
            Expr::Await(a) => self.visit_expr(&a.value),
            Expr::Call(call) => self.visit_call(call),
            Expr::Attribute(a) => self.visit_expr(&a.value),
            Expr::Subscript(s) => {
                // A subscript READ (this arm is only reached in value position — assignment
                // and delete targets are handled separately and never call `visit_expr`) may
                // raise, over-approximated by the base's shape known so far in this forward
                // walk: mapping ⇒ `KeyError`, sequence/str ⇒ `IndexError`, else both.
                if let Some(root) = self.facts.param_root(&s.value) {
                    let shape = self.facts.shapes.get(&root).cloned().unwrap_or(Shape::Any);
                    for exc in subscript_read_exceptions(&shape) {
                        self.facts.sig.raises.implicit.push((*exc).to_string());
                    }
                }
                self.visit_expr(&s.value);
                self.visit_expr(&s.slice);
            }
            Expr::BinOp(b) => {
                self.visit_expr(&b.left);
                self.visit_expr(&b.right);
                if let Some(exc) = binop_implicit_exception(b.op) {
                    self.facts.sig.raises.implicit.push(exc.to_string());
                }
                // Arithmetic on an operand whose type the analyzer never pinned may raise
                // `TypeError` — see `collect::exceptions` doc.
                for operand in [b.left.as_ref(), b.right.as_ref()] {
                    if let Some(root) = self.facts.param_root(operand) {
                        self.facts.note_type_error_candidate(&root);
                    }
                }
            }
            Expr::BoolOp(b) => {
                for v in &b.values {
                    self.visit_expr(v);
                }
            }
            Expr::UnaryOp(u) => self.visit_expr(&u.operand),
            Expr::Compare(c) => {
                self.visit_expr(&c.left);
                for v in &c.comparators {
                    self.visit_expr(v);
                }
                // An ordered comparison (`<`/`<=`/`>`/`>=`) on an operand whose type the
                // analyzer never pinned may raise `TypeError` — see `collect::exceptions` doc.
                if c.ops.iter().any(|op| is_ordered_compare(*op)) {
                    let operands =
                        std::iter::once(c.left.as_ref()).chain(c.comparators.iter());
                    for operand in operands {
                        if let Some(root) = self.facts.param_root(operand) {
                            self.facts.note_type_error_candidate(&root);
                        }
                    }
                }
            }
            Expr::If(i) => {
                self.visit_expr(&i.test);
                self.visit_expr(&i.body);
                self.visit_expr(&i.orelse);
            }
            Expr::Name(n) => self.facts.note_name(n.id.as_str()),
            Expr::Named(n) => self.visit_expr(&n.value),
            Expr::Starred(s) => self.visit_expr(&s.value),
            Expr::List(l) => l.elts.iter().for_each(|e| self.visit_expr(e)),
            Expr::Tuple(t) => t.elts.iter().for_each(|e| self.visit_expr(e)),
            Expr::Set(s) => s.elts.iter().for_each(|e| self.visit_expr(e)),
            Expr::Dict(d) => {
                for item in &d.items {
                    if let Some(k) = &item.key {
                        self.visit_expr(k);
                    }
                    self.visit_expr(&item.value);
                }
            }
            Expr::ListComp(c) => {
                self.visit_comprehensions(&c.generators);
                self.visit_expr(&c.elt);
            }
            Expr::SetComp(c) => {
                self.visit_comprehensions(&c.generators);
                self.visit_expr(&c.elt);
            }
            Expr::DictComp(c) => {
                self.visit_comprehensions(&c.generators);
                if let Some(k) = &c.key {
                    self.visit_expr(k);
                }
                self.visit_expr(&c.value);
            }
            Expr::Generator(c) => {
                self.visit_comprehensions(&c.generators);
                self.visit_expr(&c.elt);
            }
            Expr::Lambda(l) => self.visit_expr(&l.body),
            _ => {}
        }
    }

    /// Visit a comprehension's `for`/`if` clauses: the iterable votes shape like a `for` loop's,
    /// and both the iterable and any `if` guards may contain calls/effects that must be seen.
    fn visit_comprehensions(&mut self, generators: &[ast::Comprehension]) {
        for comp in generators {
            self.visit_expr(&comp.iter);
            for cond in &comp.ifs {
                self.visit_expr(cond);
            }
        }
    }

    fn visit_call(&mut self, call: &ast::ExprCall) {
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

    /// Targets among `arguments` that resolve to a tracked root (params passed into a call
    /// may be mutated by it).
    fn args_targets(&self, arguments: &ast::Arguments) -> Vec<MutationTarget> {
        let mut out = Vec::new();
        for arg in arguments.args.iter() {
            if let Some(t) = self.facts.resolve_target(arg, None) {
                out.push(t);
            }
        }
        out
    }
}
