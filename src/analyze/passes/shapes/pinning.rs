//! The refinement (write) side of [`ShapeState`]: pinning an expression's root toward a target
//! shape, the `for`-loop and assignment binding rules, and the depth cap that keeps a
//! self-referential local's shape from growing without bound across fixpoint iterations.

use ruff_python_ast as ast;

use crate::model::Shape;

use super::super::super::collect::aliases::leftmost_name;
use super::shape_of::{element_of, shape_of};
use super::state::ShapeState;

/// Maximum container nesting depth an inferred shape may reach; deeper evidence is truncated to
/// `Shape::Any` at the boundary. See the module doc's "Depth cap" section.
const MAX_DEPTH: usize = 4;

impl ShapeState<'_> {
    /// Merge `shape` into `name`'s root's accumulated evidence via `Shape::join`, clamped by
    /// the depth cap.
    pub(super) fn refine(&mut self, name: &str, shape: Shape) {
        let root = self.root(name);
        let cur = self.env.remove(&root).unwrap_or(Shape::Any);
        self.env.insert(root, cap_depth(Shape::join(cur, shape), MAX_DEPTH));
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
        // A union doesn't consume a nesting level itself; each member is capped at the same
        // remaining depth, then re-normalized (`union_of`) since capping two members to `Any`
        // should collapse the whole union, not leave duplicate `Any` entries.
        Shape::Union(members) => Shape::union_of(members.into_iter().map(|m| cap_depth(m, remaining))),
        other => other,
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
pub(super) fn numeric_pin_for_op(op: ast::Operator) -> Option<Shape> {
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
pub(super) fn pin_operand(state: &mut ShapeState, expr: &ast::Expr, target: Shape) {
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
pub(super) fn pin_root(state: &mut ShapeState, expr: &ast::Expr, shape: Shape) {
    if let Some(name) = leftmost_name(expr) {
        state.refine(name, shape);
    }
}

/// The `for v in E` rule: binds `v` to `E`'s element shape, then refines `E`'s root to be at
/// least a `Seq` of `v`'s shape (as of the start of this pass) — the rule that lifts a nested
/// container's outer shape from its inner loop variable's settled shape.
pub(super) fn refine_for(state: &mut ShapeState, target: &ast::Expr, iter: &ast::Expr) {
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
        ast::Expr::Name(n) => {
            // Each iteration rebinds `n` to a fresh, unrelated element — never the caller's
            // original argument — so a loop target that is a parameter always freezes it.
            state.freeze_if_param(n.id.as_str());
            state.refine(n.id.as_str(), elem);
        }
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

pub(super) fn assign_target(target: &ast::Expr, value: &ast::Expr, state: &mut ShapeState) {
    match target {
        ast::Expr::Name(n) => {
            let x = n.id.as_str();
            if let ast::Expr::Name(y) = value {
                let unrelated = state.root(y.id.as_str()) != state.root(x);
                state.alias(x, y.id.as_str());
                if unrelated {
                    state.freeze_if_param(x);
                }
            } else {
                if !references_name(value, x) {
                    state.freeze_if_param(x);
                }
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

/// True if `expr`'s subtree contains a bare reference to the name `name` — used to tell a
/// self-referential rebind (`x = x.strip()`, `x = x + 1`) from an unrelated one (`x = []`,
/// `x = Box()`, `x = other`). Unmatched variants conservatively return `false` (treated as
/// unrelated), which only errs toward dropping votes, never toward keeping a wrong one.
fn references_name(expr: &ast::Expr, name: &str) -> bool {
    use ast::Expr;
    match expr {
        Expr::Name(n) => n.id.as_str() == name,
        Expr::Yield(y) => y.value.as_deref().is_some_and(|v| references_name(v, name)),
        Expr::YieldFrom(y) => references_name(&y.value, name),
        Expr::Await(a) => references_name(&a.value, name),
        Expr::Call(c) => {
            references_name(&c.func, name)
                || c.arguments.args.iter().any(|a| references_name(a, name))
                || c.arguments.keywords.iter().any(|k| references_name(&k.value, name))
        }
        Expr::Attribute(a) => references_name(&a.value, name),
        Expr::Subscript(s) => references_name(&s.value, name) || references_name(&s.slice, name),
        Expr::BinOp(b) => references_name(&b.left, name) || references_name(&b.right, name),
        Expr::BoolOp(b) => b.values.iter().any(|v| references_name(v, name)),
        Expr::UnaryOp(u) => references_name(&u.operand, name),
        Expr::Compare(c) => {
            references_name(&c.left, name) || c.comparators.iter().any(|v| references_name(v, name))
        }
        Expr::If(i) => {
            references_name(&i.test, name)
                || references_name(&i.body, name)
                || references_name(&i.orelse, name)
        }
        Expr::Named(n) => references_name(&n.value, name),
        Expr::Starred(s) => references_name(&s.value, name),
        Expr::List(l) => l.elts.iter().any(|e| references_name(e, name)),
        Expr::Tuple(t) => t.elts.iter().any(|e| references_name(e, name)),
        Expr::Set(s) => s.elts.iter().any(|e| references_name(e, name)),
        Expr::Dict(d) => d.items.iter().any(|it| {
            it.key.as_ref().is_some_and(|k| references_name(k, name)) || references_name(&it.value, name)
        }),
        Expr::ListComp(c) => references_comprehensions(&c.generators, name) || references_name(&c.elt, name),
        Expr::SetComp(c) => references_comprehensions(&c.generators, name) || references_name(&c.elt, name),
        Expr::DictComp(c) => {
            references_comprehensions(&c.generators, name)
                || c.key.as_ref().is_some_and(|k| references_name(k, name))
                || references_name(&c.value, name)
        }
        Expr::Generator(c) => references_comprehensions(&c.generators, name) || references_name(&c.elt, name),
        Expr::Lambda(l) => references_name(&l.body, name),
        _ => false,
    }
}

fn references_comprehensions(generators: &[ast::Comprehension], name: &str) -> bool {
    generators.iter().any(|g| {
        references_name(&g.iter, name) || g.ifs.iter().any(|cond| references_name(cond, name))
    })
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
                    state.freeze_if_param(n.id.as_str());
                    state.rebind(n.id.as_str());
                }
            }
        }
    }
}
