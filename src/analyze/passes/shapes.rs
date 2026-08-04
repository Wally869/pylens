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
//! every pass, never converging. [`cap_depth`] clamps any container nesting past
//! [`MAX_DEPTH`] levels to `Shape::Any`, applied every time a shape is committed to the env — so
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
//! - **Literals**: list/tuple -> `Seq`, set -> `Set`, dict -> `Map`, scalars -> their `Shape`.

use std::collections::HashMap;

use ruff_python_ast as ast;

use crate::model::Shape;

use super::super::collect::aliases::leftmost_name;
use super::super::collect::shapes::{numeric_literal_shape, shape_for_method};
use super::super::context::ModuleAnalysis;
use super::super::pass::Pass;
use super::declarations::ReceiverKind;

/// Maximum container nesting depth an inferred shape may reach; deeper evidence is truncated to
/// `Shape::Any` at the boundary. See the module doc's "Depth cap" section.
const MAX_DEPTH: usize = 4;

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
                    ctx.shapes.push(infer_function(def, receiver));
                }
                ast::Stmt::ClassDef(class) => {
                    for member in &class.body {
                        if let ast::Stmt::FunctionDef(def) = member {
                            let receiver = receivers.next().unwrap_or(ReceiverKind::None);
                            ctx.shapes.push(infer_function(def, receiver));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Alias tracking + the name -> shape environment for one function's fixpoint walk. Every read
/// or write of a name's shape goes through its **root** — the representative name reached by
/// following `aliases` — so `q = p; q.append(1)` and later uses of `p` see the same evidence.
struct ShapeState {
    aliases: HashMap<String, String>,
    env: HashMap<String, Shape>,
}

impl ShapeState {
    fn new(params: &[String]) -> Self {
        let mut aliases = HashMap::new();
        let mut env = HashMap::new();
        for p in params {
            aliases.insert(p.clone(), p.clone());
            env.insert(p.clone(), Shape::Any);
        }
        Self { aliases, env }
    }

    /// The representative name `name` currently denotes the same object as (identity, not
    /// alias to a param specifically — every local is tracked, not just param aliases).
    fn root(&self, name: &str) -> String {
        self.aliases
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    fn shape_of_name(&self, name: &str) -> Shape {
        self.env.get(&self.root(name)).cloned().unwrap_or(Shape::Any)
    }

    /// Merge `shape` into `name`'s root's accumulated evidence via `Shape::join`, clamped by
    /// the depth cap.
    fn refine(&mut self, name: &str, shape: Shape) {
        let root = self.root(name);
        let cur = self.env.remove(&root).unwrap_or(Shape::Any);
        self.env.insert(root, cap_depth(Shape::join(cur, shape), MAX_DEPTH));
    }

    /// `x = y` (direct name-to-name assignment): `x` now denotes the same object as `y`.
    fn alias(&mut self, x: &str, y: &str) {
        let root = self.root(y);
        self.aliases.insert(x.to_string(), root);
    }

    /// `x = <non-name expr>`: `x` is rebound to a new object, severing any prior alias.
    fn rebind(&mut self, x: &str) {
        self.aliases.insert(x.to_string(), x.to_string());
    }
}

/// Clamp `shape`'s container nesting to `remaining` more levels, replacing anything deeper with
/// `Shape::Any`. See [`MAX_DEPTH`].
fn cap_depth(shape: Shape, remaining: usize) -> Shape {
    match shape {
        Shape::Seq(e) => {
            if remaining == 0 {
                Shape::Any
            } else {
                Shape::Seq(Box::new(cap_depth(*e, remaining - 1)))
            }
        }
        Shape::Set(e) => {
            if remaining == 0 {
                Shape::Any
            } else {
                Shape::Set(Box::new(cap_depth(*e, remaining - 1)))
            }
        }
        Shape::Map(k, v) => {
            if remaining == 0 {
                Shape::Any
            } else {
                Shape::Map(
                    Box::new(cap_depth(*k, remaining - 1)),
                    Box::new(cap_depth(*v, remaining - 1)),
                )
            }
        }
        other => other,
    }
}

/// Run the fixpoint walk for one function, returning the final name -> shape env (params and
/// locals alike; the caller/`EffectsPass` picks out only the parameter names it needs).
fn infer_function(def: &ast::StmtFunctionDef, receiver: ReceiverKind) -> HashMap<String, Shape> {
    let params = param_names(&def.parameters);
    let self_param = match receiver {
        ReceiverKind::SelfParam | ReceiverKind::Cls => params.first().cloned(),
        ReceiverKind::None => None,
    };
    let tracked: Vec<String> = params
        .into_iter()
        .filter(|p| Some(p.as_str()) != self_param.as_deref())
        .collect();

    let mut state = ShapeState::new(&tracked);
    for _ in 0..MAX_ITERATIONS {
        let before = state.env.clone();
        visit_body(&def.body, &mut state);
        if state.env == before {
            break;
        }
    }
    state.env
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

/// `element_of(Seq(e)) = e`, `element_of(Set(e)) = e`, `element_of(Map(k,_)) = k` (iterating a
/// dict yields keys), `element_of(Any) = Any`.
fn element_of(shape: &Shape) -> Shape {
    match shape {
        Shape::Seq(e) => (**e).clone(),
        Shape::Set(e) => (**e).clone(),
        Shape::Map(k, _) => (**k).clone(),
        _ => Shape::Any,
    }
}

fn is_range_call(expr: &ast::Expr) -> bool {
    matches!(
        expr,
        ast::Expr::Call(c) if matches!(c.func.as_ref(), ast::Expr::Name(n) if n.id.as_str() == "range")
    )
}

/// The numeric shape a `/`, `-`, `%`, `**`, or `//` operand is pinned toward, if any. `*` is
/// intentionally excluded — it doesn't discriminate a numeric type from a sequence-repeat.
fn numeric_pin_for_op(op: ast::Operator) -> Option<Shape> {
    match op {
        ast::Operator::Div => Some(Shape::Float),
        ast::Operator::Sub | ast::Operator::Mod | ast::Operator::Pow | ast::Operator::FloorDiv => {
            Some(Shape::Int)
        }
        _ => None,
    }
}

/// Pin `expr`'s shape toward `target`: a plain name is refined directly; a subscript `B[i]`
/// bumps `B`'s element (or map value) shape instead, since `B[i]`'s own shape isn't a name we
/// can store evidence against.
fn pin_operand(state: &mut ShapeState, expr: &ast::Expr, target: Shape) {
    match expr {
        ast::Expr::Name(n) => state.refine(n.id.as_str(), target),
        ast::Expr::Subscript(sub) => {
            if let Some(name) = leftmost_name(&sub.value) {
                let cur = state.shape_of_name(name);
                let bumped = match cur {
                    Shape::Map(k, v) => Shape::Map(k, Box::new(Shape::join(*v, target))),
                    Shape::Set(e) => Shape::Set(Box::new(Shape::join(*e, target))),
                    Shape::Seq(e) => Shape::Seq(Box::new(Shape::join(*e, target))),
                    _ => Shape::Seq(Box::new(target)),
                };
                state.refine(name, bumped);
            }
        }
        _ => {}
    }
}

/// Refine `expr`'s root name toward `shape` (a container-element/method-receiver pin). No-op if
/// `expr` doesn't root to a plain name chain.
fn pin_root(state: &mut ShapeState, expr: &ast::Expr, shape: Shape) {
    if let Some(name) = leftmost_name(expr) {
        state.refine(name, shape);
    }
}

/// The `for v in E` rule: binds `v` to `E`'s element shape, then refines `E`'s root to be at
/// least a `Seq` of `v`'s shape (as of the start of this pass) — the rule that lifts a nested
/// container's outer shape from its inner loop variable's settled shape.
fn refine_for(state: &mut ShapeState, target: &ast::Expr, iter: &ast::Expr) {
    if is_range_call(iter) {
        bind_for_target(target, Shape::Int, state);
        return;
    }
    let elem = element_of(&shape_of(iter, state));
    bind_for_target(target, elem, state);
    if let Some(root) = leftmost_name(iter) {
        let v_shape = shape_of(target, state);
        state.refine(root, Shape::Seq(Box::new(v_shape)));
    }
}

fn bind_for_target(target: &ast::Expr, elem: Shape, state: &mut ShapeState) {
    match target {
        ast::Expr::Name(n) => state.refine(n.id.as_str(), elem),
        ast::Expr::Tuple(t) => {
            for el in &t.elts {
                bind_for_target(el, Shape::Any, state);
            }
        }
        ast::Expr::List(l) => {
            for el in &l.elts {
                bind_for_target(el, Shape::Any, state);
            }
        }
        _ => {}
    }
}

/// The shape of a value expression, read-only (no env mutation) — used for assignment RHS,
/// for-loop iterables/targets, and subscript bases.
fn shape_of(expr: &ast::Expr, state: &ShapeState) -> Shape {
    use ast::Expr;
    match expr {
        Expr::Name(n) => state.shape_of_name(n.id.as_str()),
        Expr::NumberLiteral(n) => match n.value {
            ast::Number::Int(_) => Shape::Int,
            ast::Number::Float(_) => Shape::Float,
            ast::Number::Complex { .. } => Shape::Any,
        },
        Expr::BooleanLiteral(_) => Shape::Bool,
        Expr::StringLiteral(_) | Expr::FString(_) => Shape::Str,
        Expr::BytesLiteral(_) => Shape::Bytes,
        Expr::NoneLiteral(_) => Shape::None,
        Expr::List(l) => Shape::Seq(Box::new(join_all(l.elts.iter().map(|e| shape_of(e, state))))),
        Expr::Tuple(t) => Shape::Seq(Box::new(join_all(t.elts.iter().map(|e| shape_of(e, state))))),
        Expr::Set(s) => Shape::Set(Box::new(join_all(s.elts.iter().map(|e| shape_of(e, state))))),
        Expr::Dict(d) => {
            let keys = d.items.iter().filter_map(|it| it.key.as_ref()).map(|k| shape_of(k, state));
            let values = d.items.iter().map(|it| shape_of(&it.value, state));
            Shape::Map(Box::new(join_all(keys)), Box::new(join_all(values)))
        }
        Expr::Subscript(s) => element_of(&shape_of(&s.value, state)),
        Expr::BinOp(b) => numeric_pin_for_op(b.op).unwrap_or(Shape::Any),
        Expr::UnaryOp(u) => match u.op {
            ast::UnaryOp::Not => Shape::Bool,
            _ => shape_of(&u.operand, state),
        },
        Expr::Compare(_) => Shape::Bool,
        Expr::BoolOp(b) => join_all(b.values.iter().map(|v| shape_of(v, state))),
        Expr::If(i) => Shape::join(shape_of(&i.body, state), shape_of(&i.orelse, state)),
        Expr::Named(n) => shape_of(&n.value, state),
        Expr::Starred(s) => shape_of(&s.value, state),
        Expr::Call(c) => shape_of_call(c),
        Expr::ListComp(_) => Shape::any_seq(),
        Expr::SetComp(_) => Shape::any_set(),
        Expr::DictComp(_) => Shape::any_map(),
        Expr::Generator(_) => Shape::any_seq(),
        _ => Shape::Any,
    }
}

fn join_all(iter: impl Iterator<Item = Shape>) -> Shape {
    iter.fold(Shape::Any, Shape::join)
}

fn shape_of_call(call: &ast::ExprCall) -> Shape {
    let ast::Expr::Name(n) = call.func.as_ref() else {
        return Shape::Any;
    };
    match n.id.as_str() {
        "int" => Shape::Int,
        "float" => Shape::Float,
        "bool" => Shape::Bool,
        "str" | "repr" | "chr" => Shape::Str,
        "bytes" => Shape::Bytes,
        "list" | "tuple" | "sorted" | "reversed" => Shape::any_seq(),
        "dict" => Shape::any_map(),
        "set" | "frozenset" => Shape::any_set(),
        _ => Shape::Any,
    }
}

fn visit_body(body: &[ast::Stmt], state: &mut ShapeState) {
    for stmt in body {
        visit_stmt(stmt, state);
    }
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

fn assign_target(target: &ast::Expr, value: &ast::Expr, state: &mut ShapeState) {
    match target {
        ast::Expr::Name(n) => {
            let x = n.id.as_str();
            if let ast::Expr::Name(y) = value {
                state.alias(x, y.id.as_str());
            } else {
                state.rebind(x);
                let sh = shape_of(value, state);
                state.refine(x, sh);
            }
        }
        ast::Expr::Tuple(t) => destructure(&t.elts, value, state),
        ast::Expr::List(l) => destructure(&l.elts, value, state),
        _ => {}
    }
}

/// Tuple/list-unpacking assignment (`a, b = ...`): pairs elementwise when the RHS is itself a
/// literal of the same arity; otherwise each target is rebound with no shape evidence (`Any`) —
/// the analyzer can't decompose an opaque iterable's per-position element shapes.
fn destructure(targets: &[ast::Expr], value: &ast::Expr, state: &mut ShapeState) {
    match value {
        ast::Expr::Tuple(vt) if vt.elts.len() == targets.len() => {
            for (t, v) in targets.iter().zip(vt.elts.iter()) {
                assign_target(t, v, state);
            }
        }
        ast::Expr::List(vl) if vl.elts.len() == targets.len() => {
            for (t, v) in targets.iter().zip(vl.elts.iter()) {
                assign_target(t, v, state);
            }
        }
        _ => {
            for t in targets {
                if let ast::Expr::Name(n) = t {
                    state.rebind(n.id.as_str());
                }
            }
        }
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
                pin_root(state, arg, Shape::any_seq());
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
