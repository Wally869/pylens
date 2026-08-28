//! The three "is this expression *proven* safe" checks the Effects pass consults before adding
//! an implicit exception to a call/attribute-load/for-loop candidate: [`FunctionFacts::
//! attribute_load_proven`], [`FunctionFacts::iterable_proven`], and [`FunctionFacts::
//! builtin_shadowed`]. Kept apart from [`super::context`]'s shared-state structs since these are
//! reasoning, not state.

use ruff_python_ast as ast;

use crate::model::Shape;

use super::collect::aliases::leftmost_name;
use super::context::FunctionFacts;

impl<'a> FunctionFacts<'a> {
    /// Whether an attribute load's `base.attr` is *proven* safe from `AttributeError` — see
    /// `passes::effects::expressions`'s `Expr::Attribute` arm, the sole caller. Two forms of
    /// proof, both requiring the base to resolve to a same-module class whose declared members
    /// (`ModuleAnalysis::class_attrs`) include `attr`:
    /// - `self` inside one of that class's own methods (`self.owner`/`self_param`), or
    /// - a *local*'s settled shape (`env_shape`) is exactly `Shape::Instance(C)` — never a
    ///   `Union`, which could still mean some other class entirely.
    ///
    /// A **parameter** base is never proven, even when its inferred shape happens to look like a
    /// `Shape::Instance` — that shape is a hypothesis built from how this function's own body
    /// happens to use the parameter, not a contract a caller is bound by (see the module doc's
    /// soundness rule). Checked via `param_names` (fixed for the whole walk) rather than
    /// `param_root`/`aliases`, which drop a rebound parameter's identity — `p = Box(); p.n` must
    /// still count `p` as a parameter, not fall through to `env_shape`'s post-rebind `Instance`
    /// evidence.
    pub(in crate::analyze) fn attribute_load_proven(&self, base: &ast::Expr, attr: &str) -> bool {
        let self_receiver = matches!(base, ast::Expr::Name(n) if Some(n.id.as_str()) == self.self_param.as_deref());
        if self_receiver {
            return self
                .owner
                .as_deref()
                .and_then(|c| self.class_attrs.get(c))
                .is_some_and(|attrs| attrs.contains(attr));
        }
        if leftmost_name(base).is_some_and(|n| self.param_names.contains(n)) {
            return false;
        }
        match self.env_shape(base) {
            Some(Shape::Instance(class)) => self
                .class_attrs
                .get(&class)
                .is_some_and(|attrs| attrs.contains(attr)),
            _ => false,
        }
    }

    /// Whether `expr` is *proven* iterable — the condition under which a `for`/comprehension
    /// clause's TypeError candidate (see `passes::effects::statements`) is suppressed. Two proof
    /// forms:
    /// - a literal container display (`[...]`, `(...)`, `{...}`, a dict/set/list/generator
    ///   comprehension, a string/bytes literal) or a bare `range(...)` call — always iterable by
    ///   construction, no shape lookup needed;
    /// - a *local*'s settled shape (`env_shape`) is a known-iterable shape (`Seq`, `Str`, `Map`,
    ///   `Set`).
    ///
    /// A **parameter** is never proven, for the same reason `attribute_load_proven` excludes one:
    /// its shape is a hypothesis this function's own body produced, not a caller-enforced
    /// contract. Anything else (a call's return value, an attribute chain, ...) is conservatively
    /// unproven too — this table doesn't attempt to model arbitrary call return shapes.
    pub(in crate::analyze) fn iterable_proven(&self, expr: &ast::Expr) -> bool {
        match expr {
            ast::Expr::List(_)
            | ast::Expr::Tuple(_)
            | ast::Expr::Set(_)
            | ast::Expr::Dict(_)
            | ast::Expr::ListComp(_)
            | ast::Expr::SetComp(_)
            | ast::Expr::DictComp(_)
            | ast::Expr::Generator(_)
            | ast::Expr::StringLiteral(_)
            | ast::Expr::BytesLiteral(_)
            | ast::Expr::FString(_) => true,
            ast::Expr::Call(c) => {
                matches!(c.func.as_ref(), ast::Expr::Name(n)
                    if n.id.as_str() == "range" && !self.builtin_shadowed("range"))
            }
            _ => {
                if leftmost_name(expr).is_some_and(|n| self.param_names.contains(n)) {
                    return false;
                }
                matches!(
                    self.env_shape(expr),
                    Some(Shape::Seq(_)) | Some(Shape::Str) | Some(Shape::Map(..)) | Some(Shape::Set(_))
                )
            }
        }
    }

    /// Whether `name` — a bare callee that resolved to none of imports/local declarations/
    /// classes — is shadowed within this function by a parameter, a local rebind, or a nested
    /// `def`/`class` of the same name, so the builtin implicit-raise table (`models::builtins`)
    /// must not apply to it here. `shapes` covers params and every rebound local (see
    /// `ModuleAnalysis::shapes`); `local_defs` covers a nested def/class the Shapes/Effects walks
    /// don't otherwise track (see `local_defs`'s doc).
    pub(in crate::analyze) fn builtin_shadowed(&self, name: &str) -> bool {
        self.shapes.contains_key(name) || self.local_defs.contains(name)
    }
}
