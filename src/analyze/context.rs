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

/// One resolved intra-module call: a call site, inside some caller function, whose callee the
/// Effects pass determined (via the Declarations symbol table) is a function/method defined in
/// this same module. Consumed by the Interprocedural pass.
///
/// Argument -> parameter mapping is **positional only** (v1); keyword-argument mapping is a
/// future refinement. A call the analyzer can't line up 1:1 with the callee's declared
/// positional parameters (keywords, `*args`/`**kwargs` at the call site) simply yields `None`
/// roots for the unmapped positions — sound, since an unmapped root means the caller didn't hand
/// the callee a trackable object there, so nothing to (mis)attribute.
#[derive(Debug, Clone)]
pub(in crate::analyze) struct CallSite {
    /// Index into `ModuleAnalysis::declarations` / `ModuleAnalysis::signatures` (the two are
    /// built in the same module-traversal order) identifying the callee.
    pub(in crate::analyze) callee: usize,
    /// The call was `self.method(...)` / `cls.method(...)` on the caller's own receiver — so
    /// the callee's `SelfAttr` mutations are mutations of the caller's own receiver too, and
    /// propagate unchanged rather than through `arg_roots`.
    pub(in crate::analyze) via_self: bool,
    /// The caller-side root each positional call argument resolves to (`None` where it doesn't
    /// root to a tracked target), parallel in order to the call's positional arguments.
    pub(in crate::analyze) arg_roots: Vec<Option<MutationTarget>>,
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
    /// Function/method symbol table built by the Declarations pass; the enabler for intra-file
    /// call resolution (Effects) and effect propagation (Interprocedural).
    pub(in crate::analyze) declarations: Vec<DeclInfo>,
    /// Per-function name->shape environments (params and locals) built by the Shapes pass, one
    /// entry per function/method in the same order as `declarations`. The Effects pass reads
    /// the parameter shapes out of these; never serialized directly.
    pub(in crate::analyze) shapes: Vec<HashMap<String, Shape>>,
    /// Effect signatures produced by the Effects pass, enriched in place by the Interprocedural
    /// pass, and finalized by the Purity pass.
    pub(in crate::analyze) signatures: Vec<EffectSignature>,
    /// Structured intra-module call sites recorded by the Effects pass, one entry per
    /// function/method in the same order as `signatures`/`declarations`. Consumed by the
    /// Interprocedural pass; never serialized.
    pub(in crate::analyze) call_sites: Vec<Vec<CallSite>>,
}

impl ModuleAnalysis {
    pub(in crate::analyze) fn new() -> Self {
        Self::default()
    }
}

/// Read-only module-wide inputs every function's Effects walk needs but none of them mutate —
/// bundled into one value so per-function setup doesn't take a long parameter list.
#[derive(Clone, Copy)]
pub(in crate::analyze) struct ModuleCtx<'a> {
    /// In-scope import binding -> the module it names.
    pub(in crate::analyze) imports: &'a HashMap<String, ModuleRef>,
    /// A `from m import *` is in scope.
    pub(in crate::analyze) has_star: bool,
    /// The module-wide function/method symbol table, for resolving local calls.
    pub(in crate::analyze) declarations: &'a [DeclInfo],
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
    /// The module-wide symbol table (read-only here), for resolving a call to a locally-defined
    /// function/method — see `passes::declarations::resolve_unique`.
    pub(in crate::analyze) declarations: &'a [DeclInfo],
    /// The class this function is a method of, `None` for a free function — the `owner` half of
    /// resolving `self.method(...)` / `cls.method(...)` against `declarations`.
    pub(in crate::analyze) owner: Option<String>,
    /// Structured call sites recorded when a call resolves to a local function/method, consumed
    /// by the Interprocedural pass.
    pub(in crate::analyze) call_sites: Vec<CallSite>,
    /// Parameter root -> guard-derived literal samples, collected from `if`/`while`/`assert`
    /// tests and ternary conditions as they're visited — see `collect::guards`. Merged into
    /// `ParamInfo::guard_samples` in `finish`.
    pub(in crate::analyze) guard_samples: HashMap<String, Vec<serde_json::Value>>,
    pub(in crate::analyze) sig: EffectSignature,
}

impl<'a> FunctionFacts<'a> {
    pub(in crate::analyze) fn new(
        self_param: Option<String>,
        params: &[String],
        module: ModuleCtx<'a>,
        shapes: HashMap<String, Shape>,
        sig: EffectSignature,
        owner: Option<String>,
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
            imports: module.imports,
            has_star: module.has_star,
            used_imports: Vec::new(),
            may_use_star: false,
            declarations: module.declarations,
            owner,
            call_sites: Vec::new(),
            guard_samples: HashMap::new(),
            sig,
        }
    }

    /// Record a guard-derived sample value for a parameter root, deduplicating against samples
    /// already recorded for that root.
    pub(in crate::analyze) fn add_guard_sample(&mut self, root: String, value: serde_json::Value) {
        let samples = self.guard_samples.entry(root).or_default();
        if !samples.contains(&value) {
            samples.push(value);
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

    /// `expr`'s settled shape per the Shapes pass's full env (params **and** locals), resolved
    /// through the same alias map `param_root` uses — but, unlike `param_root`, covering *any*
    /// tracked local, not just names that alias a parameter: a name with no alias entry (e.g. a
    /// rebound local like `node = stack.pop()`) roots to itself, mirroring the Shapes pass's own
    /// alias-collapse on rebind. `None` for the method receiver (`self`/`cls`, not shape-tracked)
    /// or an expression with no leftmost name — the same exclusions `param_root` applies.
    pub(in crate::analyze) fn env_shape(&self, expr: &ast::Expr) -> Option<Shape> {
        let name = leftmost_name(expr)?;
        if Some(name) == self.self_param.as_deref() {
            return None;
        }
        let root = self.aliases.get(name).cloned().unwrap_or_else(|| name.to_string());
        Some(self.shapes.get(&root).cloned().unwrap_or(Shape::Any))
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

    /// `expr` sits in a position that mistypes at runtime when its type is unknown (an ordered
    /// comparison/arithmetic operand, a subscript key or base, a membership test's left
    /// operand, ...): if `expr`'s rooted [`env_shape`](Self::env_shape) is still `Shape::Any`,
    /// its type genuinely is unknown to the analyzer and the operation may raise `TypeError` at
    /// runtime. An operand pinned to a concrete shape is confident enough to omit it — input
    /// generation respects that shape, so the runtime call site won't mistype it.
    pub(in crate::analyze) fn note_type_error_candidate(&mut self, expr: &ast::Expr) {
        if self.env_shape(expr) == Some(Shape::Any) {
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
