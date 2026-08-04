//! Shared pipeline state: the module-wide [`ModuleAnalysis`] context and the per-function
//! [`FunctionFacts`] accumulator that passes and collectors read and write.

use std::collections::{HashMap, HashSet};

use ruff_python_ast as ast;

use crate::model::*;

use super::collect::aliases::leftmost_name;
use super::passes::declarations::DeclInfo;

/// Walks an attribute/subscript chain down to the first attribute directly owned by
/// `self_param`, e.g. `self.cache.evict()` -> `"cache"`, `cls.registry[k]` -> `"registry"`.
/// `None` if `expr` isn't (transitively) an attribute access rooted at the receiver.
fn attr_off_receiver<'e>(expr: &'e ast::Expr, self_param: &str) -> Option<&'e str> {
    match expr {
        ast::Expr::Attribute(a) => match a.value.as_ref() {
            ast::Expr::Name(n) if n.id.as_str() == self_param => Some(a.attr.as_str()),
            other => attr_off_receiver(other, self_param),
        },
        ast::Expr::Subscript(s) => attr_off_receiver(&s.value, self_param),
        _ => None,
    }
}

/// Module-wide state threaded through the pass pipeline.
#[derive(Default)]
pub(in crate::analyze) struct ModuleAnalysis {
    /// Full import catalog (all styles, including nested). Built by the Imports pass.
    pub(in crate::analyze) imports: Vec<Import>,
    /// In-scope import binding -> the module it names. Built by the Imports pass.
    pub(in crate::analyze) bindings: HashMap<String, ModuleRef>,
    /// A `from m import *` is in scope somewhere in the module. Built by the Imports pass.
    pub(in crate::analyze) has_star: bool,
    /// Function/method symbol table built by the Declarations pass. Groundwork for a future
    /// Interprocedural pass; not yet consumed, never serialized.
    pub(in crate::analyze) declarations: Vec<DeclInfo>,
    /// Per-function name->shape environments (params and locals) built by the Shapes pass, one
    /// entry per function/method in the same order as `declarations`. The Effects pass reads
    /// the parameter shapes out of these; never serialized directly.
    pub(in crate::analyze) shapes: Vec<HashMap<String, Shape>>,
    /// Effect signatures produced by the Effects pass and finalized by the Purity pass.
    pub(in crate::analyze) signatures: Vec<EffectSignature>,
}

impl ModuleAnalysis {
    pub(in crate::analyze) fn new() -> Self {
        Self::default()
    }
}

/// Per-function accumulator: the mutable state one function's Effects walk reads and writes,
/// plus the alias-resolution helpers every collector needs to root an expression at a
/// parameter, `self` attribute, global, or nonlocal.
pub(in crate::analyze) struct FunctionFacts<'a> {
    /// The method receiver's parameter name (`self`), if any.
    pub(in crate::analyze) self_param: Option<String>,
    /// Local name -> the parameter name it currently aliases (params seed this with identity).
    pub(in crate::analyze) aliases: HashMap<String, String>,
    pub(in crate::analyze) globals: HashSet<String>,
    pub(in crate::analyze) nonlocals: HashSet<String>,
    /// Final name -> shape environment settled by the Shapes pass (params and locals), read-only
    /// here — the Effects walk looks shapes up but never mutates this map.
    pub(in crate::analyze) shapes: HashMap<String, Shape>,
    /// In-scope import binding -> the module it names (e.g. `np` -> numpy).
    pub(in crate::analyze) imports: &'a HashMap<String, ModuleRef>,
    /// A `from m import *` is in scope.
    pub(in crate::analyze) has_star: bool,
    /// Import bindings referenced in the body, in first-seen order.
    pub(in crate::analyze) used_imports: Vec<String>,
    /// Set when an unresolved free callee is seen while a star import is in scope.
    pub(in crate::analyze) may_use_star: bool,
    pub(in crate::analyze) sig: EffectSignature,
}

impl<'a> FunctionFacts<'a> {
    pub(in crate::analyze) fn new(
        self_param: Option<String>,
        params: &[String],
        imports: &'a HashMap<String, ModuleRef>,
        has_star: bool,
        shapes: HashMap<String, Shape>,
        sig: EffectSignature,
    ) -> Self {
        let mut aliases = HashMap::new();
        for p in params {
            aliases.insert(p.clone(), p.clone());
        }
        Self {
            self_param,
            aliases,
            globals: HashSet::new(),
            nonlocals: HashSet::new(),
            shapes,
            imports,
            has_star,
            used_imports: Vec::new(),
            may_use_star: false,
            sig,
        }
    }

    /// Note that `name` was referenced; if it's an import binding, record it as used.
    pub(in crate::analyze) fn note_name(&mut self, name: &str) {
        if self.imports.contains_key(name) && !self.used_imports.iter().any(|u| u == name) {
            self.used_imports.push(name.to_string());
        }
    }

    /// Resolve an expression's leftmost name to a generatable parameter root (excludes the
    /// receiver and non-parameters).
    pub(in crate::analyze) fn param_root(&self, expr: &ast::Expr) -> Option<String> {
        let name = leftmost_name(expr)?;
        if Some(name) == self.self_param.as_deref() {
            return None;
        }
        self.aliases.get(name).cloned()
    }

    /// Resolve the base of a mutation/argument expression to a [`MutationTarget`] root.
    /// `attr` is the attribute name when the mutation is an attribute write on this base.
    pub(in crate::analyze) fn resolve_target(
        &self,
        base: &ast::Expr,
        attr: Option<&str>,
    ) -> Option<MutationTarget> {
        let name = leftmost_name(base)?;
        if let Some(self_p) = &self.self_param
            && name == self_p
        {
            if let Some(attr) = attr {
                // A direct `self.attr = ...` / `cls.attr = ...`.
                return Some(MutationTarget::SelfAttr {
                    name: attr.to_string(),
                });
            }
            // A method call or subscript reached through an attribute chain on the receiver
            // (`self.cache.evict()`, `cls.registry[k] = v`) — still a receiver mutation, rooted
            // at the first attribute owned by the receiver.
            if let Some(owned) = attr_off_receiver(base, self_p) {
                return Some(MutationTarget::SelfAttr {
                    name: owned.to_string(),
                });
            }
        }
        if let Some(root) = self.aliases.get(name) {
            if self.globals.contains(root) {
                return Some(MutationTarget::Global { name: root.clone() });
            }
            return Some(MutationTarget::Param { name: root.clone() });
        }
        if self.globals.contains(name) {
            return Some(MutationTarget::Global {
                name: name.to_string(),
            });
        }
        if self.nonlocals.contains(name) {
            return Some(MutationTarget::Nonlocal {
                name: name.to_string(),
            });
        }
        None
    }

    pub(in crate::analyze) fn add_return(&mut self, kind: ReturnKind) {
        self.sig.returns.push(kind);
    }

    /// An operand rooting to `root` sits in an ordered comparison or arithmetic binop; if
    /// `root`'s final settled shape is still `Shape::Any`, its type genuinely is unknown to the
    /// analyzer and the operation may raise `TypeError` at runtime. A root pinned to a concrete
    /// shape is confident enough to omit it — input generation respects that shape, so the
    /// runtime call site won't mistype it.
    pub(in crate::analyze) fn note_type_error_candidate(&mut self, root: &str) {
        if self.shapes.get(root).cloned().unwrap_or(Shape::Any) == Shape::Any {
            self.sig.raises.implicit.push("TypeError".to_string());
        }
    }

    pub(in crate::analyze) fn add_mutation(&mut self, target: MutationTarget, via: MutationKind, name: Option<&str>) {
        self.sig.mutations.push(Mutation {
            target,
            via,
            name: name.map(str::to_string),
        });
    }
}
