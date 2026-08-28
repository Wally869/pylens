//! Shared pipeline state: the module-wide [`ModuleAnalysis`] context and the per-function
//! [`FunctionFacts`] accumulator that passes and collectors read and write.

use std::collections::{HashMap, HashSet};

use ruff_python_ast as ast;
use ruff_source_file::LineIndex;

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

/// How a resolved [`CallSite`]'s receiver maps onto the callee's `SelfAttr` mutations.
#[derive(Debug, Clone)]
pub(in crate::analyze) enum CallReceiver {
    /// A plain function call — no receiver.
    None,
    /// `self.m(...)` / `cls.m(...)`: the callee's `SelfAttr` mutations are the caller's own
    /// receiver's mutations too, and propagate unchanged.
    CallerSelf,
    /// `x.m(...)` on a tracked root whose settled shape is exactly `Shape::Instance(C)`: the
    /// callee's `SelfAttr` mutations are mutations of that root, so they're rewritten onto it.
    /// `resolve_target` only ever returns a root here when `x` is a genuine caller-visible
    /// target — a declared parameter, a `self`-attribute, a global, or a nonlocal — never a
    /// fresh local (`x = Foo(...)`), which stays untracked (`aliases.remove`, unchanged since
    /// before this feature) precisely because a freshly constructed object is never the
    /// caller's object. So a resolved call on a fresh local produces `CallReceiver::None`
    /// instead: its raises/io/global-writes/unresolved-effects still propagate (computed
    /// unconditionally in `interprocedural.rs`, independent of the receiver), but its
    /// `SelfAttr` mutations are dropped rather than fabricated onto a name that isn't a
    /// parameter — the same treatment a fresh local list/dict/set already gets. Dropping can
    /// only ever narrow the (already correctly over-approximated) mutation may-set for a target
    /// the dynamic recorder could never observe anyway (it only diffs real arguments and
    /// `self`), so it cannot introduce an under-approximation.
    Root(MutationTarget),
}

/// One resolved intra-module call: a call site, inside some caller function, whose callee the
/// Effects pass determined (via the Declarations symbol table) is a function/method defined in
/// this same module. Consumed by the Interprocedural pass.
///
/// Argument -> parameter mapping covers both positional and keyword call arguments: positional
/// arguments map onto the callee's declared parameters by position (`arg_roots`), keyword
/// arguments map by name (`kwarg_roots`), matched against the callee's `Positional` or
/// `KeywordOnly` parameters. A keyword that names no declared parameter (swallowed by the
/// callee's own `**kwargs`) simply yields no root for that name — sound, since an unmapped
/// root means the caller didn't hand the callee a trackable object there. Unpacking at the
/// call site (`f(*xs)` / `f(**kw)`) is different: it DOES hand the callee trackable objects
/// the mapping can't attribute, so the Effects pass records a `call_unpacked_args`
/// [`UnresolvedEffect`] alongside the site (and `arg_roots` stops at the first `*`-unpack,
/// whose position makes every later positional index unreliable) — the acknowledgment, not
/// the mapping, carries those roots.
#[derive(Debug, Clone)]
pub(in crate::analyze) struct CallSite {
    /// Index into `ModuleAnalysis::declarations` / `ModuleAnalysis::signatures` (the two are
    /// built in the same module-traversal order) identifying the callee.
    pub(in crate::analyze) callee: usize,
    /// How the callee's `SelfAttr` mutations map onto the caller.
    pub(in crate::analyze) receiver: CallReceiver,
    /// The caller-side root each positional call argument resolves to (`None` where it doesn't
    /// root to a tracked target), parallel in order to the call's positional arguments.
    pub(in crate::analyze) arg_roots: Vec<Option<MutationTarget>>,
    /// The caller-side root each keyword call argument resolves to (`None` where it doesn't root
    /// to a tracked target), paired with the keyword's name as written at the call site
    /// (`f(x, key=y)` records `("key", root_of(y))`). `**kwargs`-unpacking keywords (no name)
    /// aren't recorded — they can't be matched to a single callee parameter.
    pub(in crate::analyze) kwarg_roots: Vec<(String, Option<MutationTarget>)>,
}

/// One call site, inside some caller function, whose callee is an IMPORTED binding (not a
/// function/method defined in this same module) — the project-facing analogue of [`CallSite`].
/// The Effects pass records one of these alongside every `call_import` [`UnresolvedEffect`] it
/// produces (see `passes::effects::visit_call`), so the project layer (`project::interproc`) can
/// attempt to resolve the import to another file in the same project and propagate that file's
/// effect summary here, instead of leaving the call opaque. Never serialized directly.
#[derive(Debug, Clone)]
pub struct ImportCallSite {
    /// The bound name the call's callee expression starts with (`helper` in `helper(x)`, or
    /// `util` in `util.helper(x)`).
    pub binding: String,
    /// The attribute called on `binding` (`helper` in `util.helper(x)`); `None` for a direct
    /// call of an imported name (`helper(x)` where `helper` itself is the imported binding).
    pub attr: Option<String>,
    /// The caller-side root each positional call argument resolves to (`None` where it doesn't
    /// root to a tracked target), parallel in order to the call's positional arguments — same
    /// shape as [`CallSite::arg_roots`].
    pub arg_roots: Vec<Option<MutationTarget>>,
    /// The caller-side root each keyword call argument resolves to, paired with its name — same
    /// shape as [`CallSite::kwarg_roots`].
    pub kwarg_roots: Vec<(String, Option<MutationTarget>)>,
    /// The call unpacks arguments (`f(*xs)` / `f(**kw)`). Cross-file resolution still propagates
    /// what it can map, but must keep the caller's `call_import` acknowledgment: unpacked
    /// arguments reach callee parameters the mapping can't attribute.
    pub has_unpack: bool,
    /// This call has the unbound-superclass shape (`Base.method(self, ...)`): there's an
    /// attribute (`attr.is_some()`) and the call's own first positional argument is literally
    /// this function's own receiver (`self`/`cls`) — see `Walker::is_self_receiver`. `arg_roots`
    /// still includes that receiver argument at position 0 (recorded the same way as any other
    /// import call site — this flag doesn't change what's stored, only how the project layer's
    /// cross-file resolution (`project::interproc`) is allowed to interpret it): when `Base`
    /// resolves to a project-local class declaring `attr` as a method, the receiver argument is
    /// this call's own caller-visible receiver, mirroring same-module `Base.method(self, ...)`
    /// resolution (`passes::effects::calls`, `CallReceiver::CallerSelf`) — the callee's
    /// `SelfAttr` mutations become the caller's own, and the position -> parameter mapping skips
    /// this first argument. Left unused (always safely ignorable) for a call that doesn't
    /// resolve to a method this way.
    pub unbound_receiver: bool,
}

impl ImportCallSite {
    /// The dotted callee as written at the call site (`"helper"` or `"util.helper"`) — matches
    /// the `callee` string the Effects pass records on the corresponding `call_import`
    /// [`UnresolvedEffect`], so a resolved site can be matched back to remove it.
    pub fn callee_string(&self) -> String {
        match &self.attr {
            Some(attr) => format!("{}.{attr}", self.binding),
            None => self.binding.clone(),
        }
    }
}

/// Module-wide state threaded through the pass pipeline.
pub(in crate::analyze) struct ModuleAnalysis {
    /// Byte-offset -> line-number index over the module source, built once up front so the
    /// Effects pass can compute each function's `body_lines` (the coverage denominator) without
    /// re-scanning the source per function.
    pub(in crate::analyze) line_index: LineIndex,
    /// Full import catalog (all styles, including nested). Built by the Imports pass.
    pub(in crate::analyze) imports: Vec<Import>,
    /// In-scope import binding -> the module it names. Built by the Imports pass.
    pub(in crate::analyze) bindings: HashMap<String, ModuleRef>,
    /// In-scope import binding -> the original name it was imported as, for `from m import x as
    /// y` bindings (`"y" -> "x"`). Built by the Imports pass. Lets a direct call through an
    /// aliased from-import (`j(a, b)` for `from os.path import join as j`) resolve against the
    /// model table under the real symbol name (`os.path.join`) instead of the local alias
    /// (`os.path.j`, which never matches). Only from-import bindings are present; a plain
    /// `import m as n` binds a module, never a directly callable symbol.
    pub(in crate::analyze) import_names: HashMap<String, String>,
    /// A `from m import *` is in scope somewhere in the module. Built by the Imports pass.
    pub(in crate::analyze) has_star: bool,
    /// Function/method symbol table built by the Declarations pass; the enabler for intra-file
    /// call resolution (Effects) and effect propagation (Interprocedural).
    pub(in crate::analyze) declarations: Vec<DeclInfo>,
    /// Names of classes declared at module top level, built by the Declarations pass. The
    /// Shapes pass consults this to infer `Shape::Instance(C)` for a same-module constructor
    /// call `C(...)`; an imported class isn't in this set, so it stays `Any`.
    pub(in crate::analyze) classes: HashSet<String>,
    /// Class name -> the attribute names *every instance is guaranteed to have*: every method
    /// name, every class-level `attr = ...` (bound the moment the class object is defined, before
    /// any instance exists), and every `self.<attr> = ...` in `__init__` specifically — a fresh
    /// instance always runs `__init__` before any other method can observe it, so that's the only
    /// method whose assignments are a guarantee. An assignment in any OTHER method (`def arm
    /// (self): self.v = 1`) proves nothing: an instance can reach a different method without ever
    /// having called `arm()` first (see `temp/probe_selfattr.py`'s `Gadget.read`, a real
    /// `AttributeError` a looser "declared anywhere" rule wrongly suppressed). Built by the
    /// Declarations pass (`passes::declarations::collect_self_attrs`); consulted by
    /// `FunctionFacts::attribute_load_proven` to decide whether an attribute load on a proven
    /// same-module `Shape::Instance(C)` base (or on `self` inside a method of `C`) can skip the
    /// implicit `AttributeError` a load otherwise adds. Only reachable through `__init__`/class
    /// body — an attribute set from outside the class (`obj.extra = 1`) or from a non-`__init__`
    /// method is invisible here, so this table can only ever under-count a class's real
    /// attributes, never over-count: missing an entry just means the may-set stays wider (an
    /// extra predicted `AttributeError`), never narrower.
    pub(in crate::analyze) class_attrs: HashMap<String, HashSet<String>>,
    /// Per-function name->shape environments (params and locals) built by the Shapes pass, one
    /// entry per function/method in the same order as `declarations`. Flow-insensitive: a
    /// parameter unrelatedly rebound in the body (`x = Box()`) still carries its post-rebind
    /// local evidence here (e.g. `Shape::Instance("Box")`) — that evidence is sound for
    /// resolving calls made THROUGH the name after the rebind, exactly like any other local's
    /// shape. It is NOT sound as the parameter's own declared/generated shape (a caller never
    /// sees the rebind), so `ParamInfo::shape` is built from this map filtered through
    /// `frozen_params`, not read from it directly — see `passes::effects::finalization::finish`.
    pub(in crate::analyze) shapes: Vec<HashMap<String, Shape>>,
    /// Per-function set of parameter names unrelatedly rebound somewhere in the body (`x =
    /// Box()`, `x = []`, `x = other`), one entry per function/method in the same order as
    /// `shapes`. Built by the Shapes pass (see `passes::shapes::ShapeState::frozen_params`);
    /// consumed only by `finish` to reset the reported `ParamInfo::shape` to `Any` for these
    /// names — the general `shapes` map above is deliberately left unfrozen.
    pub(in crate::analyze) frozen_params: Vec<HashSet<String>>,
    /// Per-function map, one entry per function/method in the same order as `shapes`, from a
    /// frozen parameter name to the top-level statement index of its dominance-qualifying
    /// rebind — see `passes::shapes::ShapeState::frozen_dominance` for the full soundness
    /// argument. Consumed by `FunctionFacts::env_shape` to gate the Effects pass's use of
    /// `shapes`'s post-rebind evidence to call sites strictly after the rebind.
    pub(in crate::analyze) frozen_dominance: Vec<HashMap<String, usize>>,
    /// Effect signatures produced by the Effects pass, enriched in place by the Interprocedural
    /// pass, and finalized by the Purity pass.
    pub(in crate::analyze) signatures: Vec<EffectSignature>,
    /// Structured intra-module call sites recorded by the Effects pass, one entry per
    /// function/method in the same order as `signatures`/`declarations`. Consumed by the
    /// Interprocedural pass; never serialized.
    pub(in crate::analyze) call_sites: Vec<Vec<CallSite>>,
    /// Structured import-bound call sites recorded by the Effects pass, one entry per
    /// function/method in the same order as `signatures`/`declarations`. Consumed by the
    /// project layer's cross-file propagation (`project::interproc`), not by anything
    /// intra-module; never serialized.
    pub(in crate::analyze) import_call_sites: Vec<Vec<ImportCallSite>>,
}

impl ModuleAnalysis {
    pub(in crate::analyze) fn new(src: &str) -> Self {
        Self {
            line_index: LineIndex::from_source_text(src),
            imports: Vec::new(),
            bindings: HashMap::new(),
            import_names: HashMap::new(),
            has_star: false,
            declarations: Vec::new(),
            classes: HashSet::new(),
            class_attrs: HashMap::new(),
            shapes: Vec::new(),
            frozen_params: Vec::new(),
            frozen_dominance: Vec::new(),
            signatures: Vec::new(),
            call_sites: Vec::new(),
            import_call_sites: Vec::new(),
        }
    }
}

/// The Shapes pass's three per-function outputs, bundled into one value so `FunctionFacts::new`/
/// `passes::effects::analyze_function` don't take an overlong parameter list. See
/// `ModuleAnalysis::shapes`/`frozen_params`/`frozen_dominance` for what each field means.
pub(in crate::analyze) struct ShapeFacts {
    pub(in crate::analyze) shapes: HashMap<String, Shape>,
    pub(in crate::analyze) frozen_params: HashSet<String>,
    pub(in crate::analyze) frozen_dominance: HashMap<String, usize>,
}

/// Read-only module-wide inputs every function's Effects walk needs but none of them mutate —
/// bundled into one value so per-function setup doesn't take a long parameter list.
#[derive(Clone, Copy)]
pub(in crate::analyze) struct ModuleCtx<'a> {
    /// In-scope import binding -> the module it names.
    pub(in crate::analyze) imports: &'a HashMap<String, ModuleRef>,
    /// In-scope import binding -> the original name it was imported as, for aliased from-import
    /// bindings. See `ModuleAnalysis::import_names`.
    pub(in crate::analyze) import_names: &'a HashMap<String, String>,
    /// A `from m import *` is in scope.
    pub(in crate::analyze) has_star: bool,
    /// The module-wide function/method symbol table, for resolving local calls.
    pub(in crate::analyze) declarations: &'a [DeclInfo],
    /// Names of classes declared at module top level, for resolving a same-module constructor
    /// call `C(...)` against `C.__init__`.
    pub(in crate::analyze) classes: &'a HashSet<String>,
    /// Class name -> its declared attribute names. See `ModuleAnalysis::class_attrs`.
    pub(in crate::analyze) class_attrs: &'a HashMap<String, HashSet<String>>,
}

/// Per-function accumulator: the mutable state one function's Effects walk reads and writes,
/// plus the alias-resolution helpers every collector needs to root an expression at a
/// parameter, `self` attribute, global, or nonlocal.
pub(in crate::analyze) struct FunctionFacts<'a> {
    /// The method receiver's parameter name (`self`), if any.
    pub(in crate::analyze) self_param: Option<String>,
    /// This function's declared parameter names, fixed for the whole walk — unlike `aliases`
    /// (which drops an entry the moment a param is rebound to anything but a simple name-alias,
    /// see `passes::effects::targets::handle_assign_target`), this never loses a name. Consulted
    /// by `attribute_load_proven` so a rebound parameter (`p = Box(); p.n`) still counts as a
    /// parameter base — `param_root`/`aliases` alone would miss it post-rebind, which would let a
    /// merely-hypothesized post-rebind `Shape::Instance` prove the very load it produced safe,
    /// the same unsoundness the module doc's soundness rule warns about for `TypeError`.
    pub(in crate::analyze) param_names: HashSet<String>,
    /// Local name -> the parameter name it currently aliases (params seed this with identity).
    pub(in crate::analyze) aliases: HashMap<String, String>,
    pub(in crate::analyze) globals: HashSet<String>,
    pub(in crate::analyze) nonlocals: HashSet<String>,
    /// Final name -> shape environment settled by the Shapes pass (params and locals), read-only
    /// here — the Effects walk looks shapes up but never mutates this map. Flow-insensitive and
    /// deliberately unfrozen for a rebound parameter — see `ModuleAnalysis::shapes`.
    pub(in crate::analyze) shapes: HashMap<String, Shape>,
    /// Parameter names unrelatedly rebound somewhere in this function's body. See
    /// `ModuleAnalysis::frozen_params`; consumed only by `finish` to reset `ParamInfo::shape`.
    pub(in crate::analyze) frozen_params: HashSet<String>,
    /// Frozen parameter name -> the top-level statement index of its dominance-qualifying
    /// rebind. See `ModuleAnalysis::frozen_dominance`; consumed by `env_shape` to gate use of
    /// `shapes`'s post-rebind evidence to call sites the Effects walk has determined are
    /// strictly past the rebind — see `top_level_index`/`depth`.
    pub(in crate::analyze) frozen_dominance: HashMap<String, usize>,
    /// Current nesting depth relative to the function body, maintained by the Effects walk the
    /// same way `passes::shapes::ShapeState` maintains its own copy: 0 while visiting a
    /// top-level statement directly, incremented while inside any compound statement's nested
    /// block(s). See `frozen_dominance`.
    pub(in crate::analyze) depth: usize,
    /// The top-level index of the top-level statement currently being visited (or being visited
    /// by an ancestor, for nested traversal) — the value `frozen_dominance`'s indices are
    /// compared against. See `frozen_dominance`.
    pub(in crate::analyze) top_level_index: usize,
    /// In-scope import binding -> the module it names (e.g. `np` -> numpy).
    pub(in crate::analyze) imports: &'a HashMap<String, ModuleRef>,
    /// In-scope import binding -> the original name it was imported as, for aliased from-import
    /// bindings. See `ModuleAnalysis::import_names`.
    pub(in crate::analyze) import_names: &'a HashMap<String, String>,
    /// A `from m import *` is in scope.
    pub(in crate::analyze) has_star: bool,
    /// Import bindings referenced in the body, in first-seen order.
    pub(in crate::analyze) used_imports: Vec<String>,
    /// Set when an unresolved free callee is seen while a star import is in scope.
    pub(in crate::analyze) may_use_star: bool,
    /// The module-wide symbol table (read-only here), for resolving a call to a locally-defined
    /// function/method — see `passes::declarations::resolve_unique`.
    pub(in crate::analyze) declarations: &'a [DeclInfo],
    /// Names of classes declared at module top level, for resolving a same-module constructor
    /// call `C(...)` against `C.__init__` the same way a method call resolves against a class.
    pub(in crate::analyze) classes: &'a HashSet<String>,
    /// Class name -> its declared attribute names. See `ModuleAnalysis::class_attrs`.
    pub(in crate::analyze) class_attrs: &'a HashMap<String, HashSet<String>>,
    /// The class this function is a method of, `None` for a free function — the `owner` half of
    /// resolving `self.method(...)` / `cls.method(...)` against `declarations`.
    pub(in crate::analyze) owner: Option<String>,
    /// Structured call sites recorded when a call resolves to a local function/method, consumed
    /// by the Interprocedural pass.
    pub(in crate::analyze) call_sites: Vec<CallSite>,
    /// Structured call sites recorded when a call resolves to an imported binding, consumed by
    /// the project layer's cross-file propagation.
    pub(in crate::analyze) import_call_sites: Vec<ImportCallSite>,
    /// Parameter root -> guard-derived literal samples, collected from `if`/`while`/`assert`
    /// tests and ternary conditions as they're visited — see `collect::guards`. Merged into
    /// `ParamInfo::guard_samples` in `finish`.
    pub(in crate::analyze) guard_samples: HashMap<String, Vec<serde_json::Value>>,
    /// Parameter root -> inferred content-domain tags (`"url"`, `"json"`, ...), computed once
    /// up front from the whole function body — see `collect::hints`. Merged into
    /// `ParamInfo::hints` in `finish`. Unlike `guard_samples`, this isn't accumulated
    /// incrementally during the walk.
    pub(in crate::analyze) hints: HashMap<String, Vec<String>>,
    /// Names bound to a nested `def`/`class` statement anywhere in this function's body (any
    /// depth, not crossing into a further-nested def/class's own body) — computed once up front
    /// by `passes::effects::setup::local_def_names`. Consulted by `builtin_shadowed` so a local
    /// redefinition of a builtin name (`def len(x): ...`) suppresses the builtin implicit-raise
    /// table for calls to that name within this function, the same way a parameter/local
    /// rebind does via `shapes`.
    pub(in crate::analyze) local_defs: HashSet<String>,
    pub(in crate::analyze) sig: EffectSignature,
}

impl<'a> FunctionFacts<'a> {
    pub(in crate::analyze) fn new(
        self_param: Option<String>,
        params: &[String],
        module: ModuleCtx<'a>,
        shape_facts: ShapeFacts,
        sig: EffectSignature,
        owner: Option<String>,
    ) -> Self {
        let mut aliases = HashMap::new();
        for p in params {
            aliases.insert(p.clone(), p.clone());
        }
        let param_names = params.iter().cloned().collect();
        Self {
            self_param,
            param_names,
            aliases,
            globals: HashSet::new(),
            nonlocals: HashSet::new(),
            shapes: shape_facts.shapes,
            frozen_params: shape_facts.frozen_params,
            frozen_dominance: shape_facts.frozen_dominance,
            depth: 0,
            top_level_index: 0,
            imports: module.imports,
            import_names: module.import_names,
            has_star: module.has_star,
            used_imports: Vec::new(),
            may_use_star: false,
            declarations: module.declarations,
            classes: module.classes,
            class_attrs: module.class_attrs,
            owner,
            call_sites: Vec::new(),
            import_call_sites: Vec::new(),
            guard_samples: HashMap::new(),
            hints: HashMap::new(),
            local_defs: HashSet::new(),
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
    ///
    /// **Dominance gate**: `shapes` is flow-insensitive — a rebound parameter's entry there is
    /// evidence merged over the WHOLE function, including any post-rebind evidence (`x = Box()`
    /// really is a `Box` from that statement on, but the SAME merged entry also reads at a call
    /// site textually BEFORE the rebind, where `x` could still be the caller's original value of
    /// any type). So for a root in `frozen_params`, this only returns the accumulated `shapes`
    /// entry when the CURRENT call site (`self.top_level_index`) is strictly past the root's
    /// `frozen_dominance` index — i.e. sits in a later top-level statement than a rebind that
    /// itself sat directly in the top-level sequence (never inside a loop/branch/etc., since one
    /// iteration/branch could execute the "later" use before the rebind runs). Anywhere that
    /// gate isn't satisfied (no qualifying rebind at all, or the call site isn't past it), this
    /// returns `Shape::Any` — full width, same as the parameter's own reported shape — instead of
    /// the merged evidence. See `passes::shapes::state::ShapeState::frozen_dominance` for the
    /// full argument.
    pub(in crate::analyze) fn env_shape(&self, expr: &ast::Expr) -> Option<Shape> {
        let name = leftmost_name(expr)?;
        if Some(name) == self.self_param.as_deref() {
            return None;
        }
        let root = self.aliases.get(name).cloned().unwrap_or_else(|| name.to_string());
        if self.frozen_params.contains(&root) {
            let dominated = self
                .frozen_dominance
                .get(&root)
                .is_some_and(|&idx| self.top_level_index > idx);
            if !dominated {
                return Some(Shape::Any);
            }
        }
        Some(self.shapes.get(&root).cloned().unwrap_or(Shape::Any))
    }

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
    /// operand, ...): the operation may raise `TypeError` if either (a) `expr`'s rooted
    /// [`env_shape`](Self::env_shape) is still `Shape::Any`/`Shape::Union` — its type genuinely
    /// is unknown to the analyzer — or (b) `expr` roots to a *parameter* ([`param_root`]). A
    /// parameter's shape is only a hypothesis inferred from how the function uses it internally;
    /// Python enforces no such contract on callers, so a pinned parameter shape constrains no
    /// caller and cannot narrow this may-set. A *local* built from a literal (`xs = []`) has no
    /// such caller, so its pinned shape can still narrow — hence the two-part check below rather
    /// than folding this into `env_shape` itself.
    pub(in crate::analyze) fn note_type_error_candidate(&mut self, expr: &ast::Expr) {
        // A `Union` operand is just as "not confidently a single concrete type" as `Any` is —
        // treating it as safe here would narrow the may-set below what `Any` gave before `Union`
        // existed, which is unsound (see `Shape::Union`'s doc).
        let unresolved_shape = matches!(self.env_shape(expr), Some(Shape::Any) | Some(Shape::Union(_)));
        let roots_at_param = self.param_root(expr).is_some();
        if unresolved_shape || roots_at_param {
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
