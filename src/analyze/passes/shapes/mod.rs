//! Shapes pass: infers a per-function name -> [`Shape`] environment covering BOTH parameters
//! and local variables, by walking the body to a **fixpoint**. Runs after Declarations and
//! before Effects, so the parameter shapes it settles on are already final by the time Effects
//! runs — Effects reads them instead of voting during its own walk.
//!
//! ## Fixpoint
//!
//! One pass is one full walk of the function body, threading a persistent `ShapeState` (an
//! alias map alongside a name-to-shape env). Each statement/expression refines the env in place
//! via [`Shape::join`] (may-evidence accumulation, never a destructive overwrite). Refinement
//! rules that depend on *another* name's settled shape (e.g. "the iterable is at least `Seq` of
//! the loop variable's shape") only see that name's shape as of the *start* of the current pass,
//! so a chain like `matrix -> row -> row[i]` needs a few passes to bottom out (each pass
//! propagates evidence one link further up the chain). We re-walk the whole body, comparing the
//! env snapshot before and after, until a pass produces no change, or [`MAX_ITERATIONS`] passes
//! have run (backstop — guarantees termination even if some construct never quite settles).
//!
//! ## Depth cap
//!
//! A self-referential local (`x = [x]`) would otherwise deepen `x`'s shape by one `Seq` layer
//! every pass, never converging. [`pinning`]'s depth cap clamps any container nesting past its
//! maximum levels to `Shape::Any`, applied every time a shape is committed to the env — so
//! even a pathological input's shape stabilizes (or is truncated) well inside the iteration
//! backstop.
//!
//! ## Refinement rules
//!
//! - **Assignment** `x = E`: a direct `x = y` (name-to-name) aliases `x` to `y`'s current root
//!   (they denote the same object, so evidence gathered through either name must merge); any
//!   other RHS rebinds `x` to its own root and folds in `shape_of(E)`.
//! - **For-loop** `for v in E`: binds `v` to `E`'s element shape, then — the key rule for nested
//!   containers — refines `E`'s root to be *at least* `Seq` of `v`'s (possibly since-refined)
//!   shape. `for i in range(...)` special-cases `i` to `Int` directly.
//! - **Numeric ops**: `/` pins its operands `Float`; `- * // % **` pin `Int` (`*` intentionally
//!   excluded — multiplication doesn't discriminate int vs. sequence-repeat). A pinned operand
//!   that is itself a subscript `B[i]` bumps `B`'s element/value shape instead of `B[i]` itself.
//! - **Subscript read** `B[i]`: its shape is `B`'s element (or map value) shape.
//! - **Iterable-consuming builtins/methods**: `len`/`sum`/`sorted`/`reversed`/`enumerate`/
//!   `min`/`max`'s first argument, and value methods (via [`shape_for_method`]), pin their
//!   receiver to at least a sequence/set/map/string as appropriate.
//! - **Sequence-protocol widening (parameters only)**: the for-loop and `len`/`sum`/... votes
//!   above, and the numeric-pinned-subscript default, only ever observe *protocol* usage — they
//!   never rule out a `str` argument, and a real corpus shows callers pass one about as often as
//!   a `list`. For a parameter (never a local — see [`pinning`]) that evidence votes
//!   `Union(Seq(elem), Str)` instead of committing to `Seq(elem)`; list-specific evidence
//!   (`.append`, `.extend`, `+= [...]`, ...) still votes a bare `Seq`, straight through
//!   [`shape_for_method`]/[`pin_root`]. See `pinning::seq_protocol_shape`.
//! - **Literals**: list/tuple -> `Seq`, set -> `Set`, dict -> `Map`, scalars -> their `Shape`.

use std::collections::HashMap;

use ruff_python_ast as ast;

use crate::model::{ModuleRef, Shape};

use super::super::collect::shapes::{numeric_literal_shape, shape_for_method};
use super::super::context::ModuleAnalysis;
use super::super::pass::Pass;
use super::declarations::ReceiverKind;

mod pinning;
mod shape_of;
mod state;

use pinning::{
    assign_target, numeric_pin_for_op, pin_operand, pin_root, pin_root_seq_evidence, refine_for,
};
use state::ShapeState;

/// Maximum number of full-body refinement passes per function before giving up on reaching a
/// fixpoint naturally. See the module doc's "Fixpoint" section.
const MAX_ITERATIONS: usize = 8;

/// Computes `ModuleAnalysis::shapes`: one name->shape map per function/method, in the same
/// declaration order as `ModuleAnalysis::declarations` (and the traversal `EffectsPass` repeats
/// to consume it).
pub(in crate::analyze) struct ShapesPass;

impl Pass for ShapesPass {
    fn run(&self, module: &ast::ModModule, ctx: &mut ModuleAnalysis) {
        let mut receivers = ctx.declarations.iter().map(|d| d.receiver);
        for stmt in &module.body {
            match stmt {
                ast::Stmt::FunctionDef(def) => {
                    let receiver = receivers.next().unwrap_or(ReceiverKind::None);
                    let (shapes, frozen, dominance) = infer_function(
                        def,
                        receiver,
                        &ctx.classes,
                        &ctx.bindings,
                        &ctx.import_names,
                    );
                    ctx.shapes.push(shapes);
                    ctx.frozen_params.push(frozen);
                    ctx.frozen_dominance.push(dominance);
                }
                ast::Stmt::ClassDef(class) => {
                    for member in &class.body {
                        if let ast::Stmt::FunctionDef(def) = member {
                            let receiver = receivers.next().unwrap_or(ReceiverKind::None);
                            let (shapes, frozen, dominance) = infer_function(
                                def,
                                receiver,
                                &ctx.classes,
                                &ctx.bindings,
                                &ctx.import_names,
                            );
                            ctx.shapes.push(shapes);
                            ctx.frozen_params.push(frozen);
                            ctx.frozen_dominance.push(dominance);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Run the fixpoint walk for one function, returning the final name -> shape env (params and
/// locals alike; the caller/`EffectsPass` picks out only the parameter names it needs).
type InferResult = (HashMap<String, Shape>, std::collections::HashSet<String>, HashMap<String, usize>);

fn infer_function(
    def: &ast::StmtFunctionDef,
    receiver: ReceiverKind,
    classes: &std::collections::HashSet<String>,
    bindings: &HashMap<String, ModuleRef>,
    import_names: &HashMap<String, String>,
) -> InferResult {
    let params = param_names(&def.parameters);
    let self_param = match receiver {
        ReceiverKind::SelfParam | ReceiverKind::Cls => params.first().cloned(),
        ReceiverKind::None => None,
    };
    let tracked: Vec<String> = params
        .into_iter()
        .filter(|p| Some(p.as_str()) != self_param.as_deref())
        .collect();

    let mut state = ShapeState::new(&tracked, classes.clone(), bindings, import_names);
    for _ in 0..MAX_ITERATIONS {
        let before = state.env.clone();
        visit_top_level(&def.body, &mut state);
        if state.env == before {
            break;
        }
    }
    // A parameter rebound to an unrelated value anywhere in the body severs the shape from the
    // caller's argument for `ParamInfo::shape` purposes — that override lives in
    // `passes::effects::finalization::finish`, driven by `frozen_params` returned here. The env
    // itself is returned unfrozen: after the rebind, the name genuinely denotes whatever the
    // rebind's evidence says (`x = Box()` really is a `Box` from that point on), which is sound
    // to resolve calls made through the name — same as any other local, GATED by
    // `frozen_dominance` (see its doc) so a call site that isn't textually dominated by the
    // rebind never sees the post-rebind evidence. See `ModuleAnalysis::shapes`'s doc.
    (state.env, state.frozen_params, state.frozen_dominance)
}

/// The top-level (function-body-direct) statement walk: sets `depth = 0` and the current
/// top-level index for each statement before visiting it — the anchor `frozen_dominance`'s
/// indices are measured against. See `visit_body`, which every NESTED recursion goes through
/// instead (incrementing `depth`, leaving `top_level_index` inherited from the enclosing
/// top-level statement).
fn visit_top_level(body: &[ast::Stmt], state: &mut ShapeState) {
    for (i, stmt) in body.iter().enumerate() {
        state.top_level_index = i;
        state.depth = 0;
        visit_stmt(stmt, state);
    }
}

fn param_names(params: &ast::Parameters) -> Vec<String> {
    let mut out = Vec::new();
    for p in &params.posonlyargs {
        out.push(p.parameter.name.as_str().to_string());
    }
    for p in &params.args {
        out.push(p.parameter.name.as_str().to_string());
    }
    if let Some(v) = &params.vararg {
        out.push(v.name.as_str().to_string());
    }
    for p in &params.kwonlyargs {
        out.push(p.parameter.name.as_str().to_string());
    }
    if let Some(k) = &params.kwarg {
        out.push(k.name.as_str().to_string());
    }
    out
}

/// The NESTED statement walk: every call site is inside some compound statement's body, so
/// `depth` is incremented for the duration — see `frozen_dominance`'s doc on why a rebind seen
/// at `depth > 0` never qualifies for the dominance gate. `top_level_index` is left untouched,
/// inherited from the enclosing top-level statement (`visit_top_level`).
fn visit_body(body: &[ast::Stmt], state: &mut ShapeState) {
    state.depth += 1;
    for stmt in body {
        visit_stmt(stmt, state);
    }
    state.depth -= 1;
}

fn visit_stmt(stmt: &ast::Stmt, state: &mut ShapeState) {
    use ast::Stmt;
    match stmt {
        Stmt::Return(ret) => {
            if let Some(v) = ret.value.as_deref() {
                visit_expr(v, state);
            }
        }
        Stmt::Raise(raise) => {
            if let Some(exc) = raise.exc.as_deref() {
                match exc {
                    ast::Expr::Call(call) => {
                        for arg in call.arguments.args.iter() {
                            visit_expr(arg, state);
                        }
                        for kw in call.arguments.keywords.iter() {
                            visit_expr(&kw.value, state);
                        }
                    }
                    _ => visit_expr(exc, state),
                }
            }
        }
        Stmt::Assert(a) => {
            visit_expr(&a.test, state);
            if let Some(msg) = a.msg.as_deref() {
                visit_expr(msg, state);
            }
        }
        Stmt::Assign(assign) => {
            visit_expr(&assign.value, state);
            for target in &assign.targets {
                assign_target(target, &assign.value, state);
            }
        }
        Stmt::AugAssign(aug) => {
            visit_expr(&aug.value, state);
            visit_expr(&aug.target, state);
            if let Some(pin) = numeric_pin_for_op(aug.op) {
                pin_operand(state, &aug.target, pin.clone());
                pin_operand(state, &aug.value, pin);
            }
        }
        Stmt::AnnAssign(ann) => {
            if let Some(value) = ann.value.as_deref() {
                visit_expr(value, state);
                assign_target(&ann.target, value, state);
            }
        }
        Stmt::Expr(e) => visit_expr(&e.value, state),
        Stmt::If(if_stmt) => {
            visit_expr(&if_stmt.test, state);
            visit_body(&if_stmt.body, state);
            for clause in &if_stmt.elif_else_clauses {
                if let Some(test) = &clause.test {
                    visit_expr(test, state);
                }
                visit_body(&clause.body, state);
            }
        }
        Stmt::For(for_stmt) => {
            visit_expr(&for_stmt.iter, state);
            refine_for(state, &for_stmt.target, &for_stmt.iter);
            visit_body(&for_stmt.body, state);
            visit_body(&for_stmt.orelse, state);
        }
        Stmt::While(while_stmt) => {
            visit_expr(&while_stmt.test, state);
            visit_body(&while_stmt.body, state);
            visit_body(&while_stmt.orelse, state);
        }
        Stmt::With(with_stmt) => {
            for item in &with_stmt.items {
                visit_expr(&item.context_expr, state);
            }
            visit_body(&with_stmt.body, state);
        }
        Stmt::Try(try_stmt) => {
            visit_body(&try_stmt.body, state);
            for handler in &try_stmt.handlers {
                let ast::ExceptHandler::ExceptHandler(h) = handler;
                visit_body(&h.body, state);
            }
            visit_body(&try_stmt.orelse, state);
            visit_body(&try_stmt.finalbody, state);
        }
        Stmt::Match(match_stmt) => {
            visit_expr(&match_stmt.subject, state);
            for case in &match_stmt.cases {
                visit_body(&case.body, state);
            }
        }
        _ => {}
    }
}

fn visit_expr(expr: &ast::Expr, state: &mut ShapeState) {
    use ast::Expr;
    match expr {
        Expr::Yield(y) => {
            if let Some(v) = y.value.as_deref() {
                visit_expr(v, state);
            }
        }
        Expr::YieldFrom(y) => visit_expr(&y.value, state),
        Expr::Await(a) => visit_expr(&a.value, state),
        Expr::Call(call) => visit_call(call, state),
        Expr::Attribute(a) => visit_expr(&a.value, state),
        Expr::Subscript(s) => {
            visit_expr(&s.value, state);
            visit_expr(&s.slice, state);
        }
        Expr::BinOp(b) => {
            visit_expr(&b.left, state);
            visit_expr(&b.right, state);
            if let Some(pin) = numeric_pin_for_op(b.op) {
                pin_operand(state, &b.left, pin.clone());
                pin_operand(state, &b.right, pin);
            }
        }
        Expr::BoolOp(b) => {
            for v in &b.values {
                visit_expr(v, state);
            }
        }
        Expr::UnaryOp(u) => visit_expr(&u.operand, state),
        Expr::Compare(c) => {
            visit_expr(&c.left, state);
            for v in &c.comparators {
                visit_expr(v, state);
            }
            // Comparing against a numeric literal ⇒ the other operand(s) are numeric.
            let operands: Vec<&ast::Expr> =
                std::iter::once(c.left.as_ref()).chain(c.comparators.iter()).collect();
            for (i, op) in operands.iter().enumerate() {
                if let Some(sh) = numeric_literal_shape(op) {
                    for (j, other) in operands.iter().enumerate() {
                        if j != i {
                            pin_operand(state, other, sh.clone());
                        }
                    }
                }
            }
        }
        Expr::If(i) => {
            visit_expr(&i.test, state);
            visit_expr(&i.body, state);
            visit_expr(&i.orelse, state);
        }
        Expr::Named(n) => visit_expr(&n.value, state),
        Expr::Starred(s) => visit_expr(&s.value, state),
        Expr::List(l) => l.elts.iter().for_each(|e| visit_expr(e, state)),
        Expr::Tuple(t) => t.elts.iter().for_each(|e| visit_expr(e, state)),
        Expr::Set(s) => s.elts.iter().for_each(|e| visit_expr(e, state)),
        Expr::Dict(d) => {
            for item in &d.items {
                if let Some(k) = &item.key {
                    visit_expr(k, state);
                }
                visit_expr(&item.value, state);
            }
        }
        Expr::ListComp(c) => {
            visit_comprehensions(&c.generators, state);
            visit_expr(&c.elt, state);
        }
        Expr::SetComp(c) => {
            visit_comprehensions(&c.generators, state);
            visit_expr(&c.elt, state);
        }
        Expr::DictComp(c) => {
            visit_comprehensions(&c.generators, state);
            if let Some(k) = &c.key {
                visit_expr(k, state);
            }
            visit_expr(&c.value, state);
        }
        Expr::Generator(c) => {
            visit_comprehensions(&c.generators, state);
            visit_expr(&c.elt, state);
        }
        Expr::Lambda(l) => visit_expr(&l.body, state),
        _ => {}
    }
}

/// A comprehension's `for`/`if` clauses follow the same element/iterable refinement as a `for`
/// statement's.
fn visit_comprehensions(generators: &[ast::Comprehension], state: &mut ShapeState) {
    for comp in generators {
        visit_expr(&comp.iter, state);
        refine_for(state, &comp.target, &comp.iter);
        for cond in &comp.ifs {
            visit_expr(cond, state);
        }
    }
}

fn visit_call(call: &ast::ExprCall, state: &mut ShapeState) {
    match call.func.as_ref() {
        ast::Expr::Attribute(attr) => {
            if let Some(sh) = shape_for_method(attr.attr.as_str()) {
                pin_root(state, &attr.value, sh);
            }
        }
        ast::Expr::Name(name) => {
            if matches!(
                name.id.as_str(),
                "len" | "sum" | "sorted" | "reversed" | "enumerate" | "min" | "max"
            ) && let Some(arg) = call.arguments.args.first()
            {
                pin_root_seq_evidence(state, arg);
            }
        }
        _ => {}
    }
    visit_expr(&call.func, state);
    for arg in call.arguments.args.iter() {
        visit_expr(arg, state);
    }
    for kw in call.arguments.keywords.iter() {
        visit_expr(&kw.value, state);
    }
}

#[cfg(test)]
mod tests {
    use super::super::declarations::DeclarationsPass;
    use super::super::imports::ImportsPass;
    use super::*;

    /// Run just Imports -> Declarations -> Shapes and return the name->shape env for the
    /// module's first (and only) function.
    fn shapes_of(src: &str) -> HashMap<String, Shape> {
        let parsed = crate::parse::parse_source(src).expect("parse");
        let module = parsed.syntax();
        let mut ctx = ModuleAnalysis::new(src);
        ImportsPass.run(module, &mut ctx);
        DeclarationsPass.run(module, &mut ctx);
        ShapesPass.run(module, &mut ctx);
        ctx.shapes.into_iter().next().expect("one function")
    }

    #[test]
    fn stdlib_call_via_import_gives_local_the_modelled_return_shape() {
        let env = shapes_of("import os.path\ndef f(a, b):\n    p = os.path.join(a, b)\n    return p\n");
        assert_eq!(env.get("p"), Some(&Shape::Str));
    }

    #[test]
    fn stdlib_call_via_aliased_import_gives_local_the_modelled_return_shape() {
        let env =
            shapes_of("import os.path as p\ndef f(a, b):\n    r = p.join(a, b)\n    return r\n");
        assert_eq!(env.get("r"), Some(&Shape::Str));
    }

    #[test]
    fn stdlib_call_via_from_import_gives_local_the_modelled_return_shape() {
        let env = shapes_of(
            "from os.path import join\ndef f(a, b):\n    p = join(a, b)\n    return p\n",
        );
        assert_eq!(env.get("p"), Some(&Shape::Str));
    }

    #[test]
    fn stdlib_call_absent_from_the_return_kind_table_stays_any() {
        let env = shapes_of(
            "import os.path\ndef f(a, b):\n    p = os.path.samestat(a, b)\n    return p\n",
        );
        assert_eq!(env.get("p"), Some(&Shape::Any));
    }
}
