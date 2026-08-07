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

impl ShapeState {
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

pub(super) fn assign_target(target: &ast::Expr, value: &ast::Expr, state: &mut ShapeState) {
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
