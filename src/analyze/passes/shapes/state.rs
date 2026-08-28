//! [`ShapeState`]: the alias map + name-to-shape environment threaded through one function's
//! fixpoint walk, and its basic construction/lookup operations. See [`super::pinning`] for the
//! refinement (write) side of the env, and [`super::shape_of`] for read-only shape derivation
//! from an expression.

use std::collections::{HashMap, HashSet};

use crate::model::{ModuleRef, Shape};

/// Alias tracking + the name -> shape environment for one function's fixpoint walk. Every read
/// or write of a name's shape goes through its **root** — the representative name reached by
/// following `aliases` — so `q = p; q.append(1)` and later uses of `p` see the same evidence.
pub(super) struct ShapeState<'a> {
    pub(super) aliases: HashMap<String, String>,
    pub(super) env: HashMap<String, Shape>,
    /// Names of classes declared in this module — a constructor call `C(...)` for `C` in this
    /// set infers `Shape::Instance(C)`; anything else (including an imported class) doesn't.
    pub(super) classes: HashSet<String>,
    /// In-scope import binding -> the module it names, built by the Imports pass (which runs
    /// before Shapes). Lets `shape_of` resolve a stdlib call (`os.path.join(...)`) against
    /// `models::return_kind` the same way the Effects pass resolves it against `models::lookup`.
    pub(super) bindings: &'a HashMap<String, ModuleRef>,
    /// In-scope import binding -> the original name it was imported as, for aliased from-import
    /// bindings. See `context::ModuleAnalysis::import_names`; lets a direct call through an
    /// aliased from-import resolve `models::return_kind` under the real symbol name.
    pub(super) import_names: &'a HashMap<String, String>,
    /// Names of this function's tracked parameters (excludes `self`/`cls`). Needed to tell a
    /// rebind of a parameter apart from a rebind of a plain local.
    params: HashSet<String>,
    /// Parameters rebound to an unrelated value somewhere in the body (`x = []`, `x = Box()`,
    /// `x = other`). A parameter in this set has its shape reset to `Shape::Any` once the
    /// fixpoint settles, dropping every vote — a rebound name says nothing about what the
    /// caller supplied. A rebind whose RHS still reads the parameter (`x = x.strip()`) is not
    /// unrelated and never lands here; see `references_name` in `pinning`.
    pub(super) frozen_params: HashSet<String>,
    /// For a name in `frozen_params` whose FIRST unrelated rebind sits directly in the function
    /// body's top-level statement sequence (nesting depth 0) — the top-level index of that
    /// statement. A name is present here only when its first rebind qualifies; a rebind nested
    /// inside ANY compound statement (`if`/`for`/`while`/`try`/`with`/`match`) never qualifies,
    /// even if the same name is rebound again later at depth 0 — the name then stays fully
    /// frozen for the whole function, exactly as before dominance gating existed. This is the
    /// SOUND half of the flow-sensitive rebind fix: a later top-level statement can never
    /// execute before an earlier one completes (no CFG needed to know that), so a call
    /// (resolved by the Effects pass) sitting in a top-level statement whose index is strictly
    /// greater than the rebind's index is safely past it; a call at or before that index sees a
    /// parameter that could still be the caller's original value, so it must not be resolved
    /// through the post-rebind shape. See `context::FunctionFacts::env_shape`, which applies
    /// this gate using its own (equivalently computed) top-level index during the Effects walk.
    pub(super) frozen_dominance: HashMap<String, usize>,
    /// Current nesting depth relative to the function body: 0 while processing a top-level
    /// statement directly, incremented while inside any compound statement's nested block(s).
    pub(super) depth: usize,
    /// The top-level index (0-based, into the function body's direct statement list) of the
    /// top-level statement currently being processed (or being processed by an ancestor, for
    /// nested traversal) — valid at any depth.
    pub(super) top_level_index: usize,
}

impl<'a> ShapeState<'a> {
    pub(super) fn new(
        params: &[String],
        classes: HashSet<String>,
        bindings: &'a HashMap<String, ModuleRef>,
        import_names: &'a HashMap<String, String>,
    ) -> Self {
        let mut aliases = HashMap::new();
        let mut env = HashMap::new();
        for p in params {
            aliases.insert(p.clone(), p.clone());
            env.insert(p.clone(), Shape::Any);
        }
        let params = params.iter().cloned().collect();
        Self {
            aliases,
            env,
            classes,
            bindings,
            import_names,
            params,
            frozen_params: HashSet::new(),
            frozen_dominance: HashMap::new(),
            depth: 0,
            top_level_index: 0,
        }
    }

    /// Marks `x` as unrelately rebound if it names one of this function's parameters; a no-op
    /// for a local. See `frozen_params`/`frozen_dominance`. Idempotent across fixpoint
    /// iterations: only the FIRST time `x` is frozen does its depth/index get recorded (or
    /// permanently withheld, if that first rebind was nested).
    pub(super) fn freeze_if_param(&mut self, x: &str) {
        if !self.params.contains(x) {
            return;
        }
        let already_frozen = self.frozen_params.contains(x);
        self.frozen_params.insert(x.to_string());
        if !already_frozen && self.depth == 0 {
            self.frozen_dominance.insert(x.to_string(), self.top_level_index);
        }
    }

    /// True if `name` (after alias resolution) denotes one of this function's tracked
    /// parameters — the hypothesis side of shape inference, where sequence-protocol evidence
    /// (iteration, `len`/`sum`/... first argument) widens to admit a `str` argument too, since
    /// real callers pass one just as often. See `pinning::seq_protocol_shape`. A local's shape
    /// (`xs = [...]`) is a value fact, never gated by this.
    pub(super) fn is_param(&self, name: &str) -> bool {
        self.params.contains(&self.root(name))
    }

    /// The representative name `name` currently denotes the same object as (identity, not
    /// alias to a param specifically — every local is tracked, not just param aliases).
    pub(super) fn root(&self, name: &str) -> String {
        self.aliases
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.to_string())
    }

    pub(super) fn shape_of_name(&self, name: &str) -> Shape {
        self.env.get(&self.root(name)).cloned().unwrap_or(Shape::Any)
    }

    /// `x = y` (direct name-to-name assignment): `x` now denotes the same object as `y`.
    pub(super) fn alias(&mut self, x: &str, y: &str) {
        let root = self.root(y);
        self.aliases.insert(x.to_string(), root);
    }

    /// `x = <non-name expr>`: `x` is rebound to a new object, severing any prior alias.
    pub(super) fn rebind(&mut self, x: &str) {
        self.aliases.insert(x.to_string(), x.to_string());
    }
}
