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
    /// Names of this function's tracked parameters (excludes `self`/`cls`). Needed to tell a
    /// rebind of a parameter apart from a rebind of a plain local.
    params: HashSet<String>,
    /// Parameters rebound to an unrelated value somewhere in the body (`x = []`, `x = Box()`,
    /// `x = other`). A parameter in this set has its shape reset to `Shape::Any` once the
    /// fixpoint settles, dropping every vote — a rebound name says nothing about what the
    /// caller supplied. A rebind whose RHS still reads the parameter (`x = x.strip()`) is not
    /// unrelated and never lands here; see `references_name` in `pinning`.
    pub(super) frozen_params: HashSet<String>,
}

impl<'a> ShapeState<'a> {
    pub(super) fn new(
        params: &[String],
        classes: HashSet<String>,
        bindings: &'a HashMap<String, ModuleRef>,
    ) -> Self {
        let mut aliases = HashMap::new();
        let mut env = HashMap::new();
        for p in params {
            aliases.insert(p.clone(), p.clone());
            env.insert(p.clone(), Shape::Any);
        }
        let params = params.iter().cloned().collect();
        Self { aliases, env, classes, bindings, params, frozen_params: HashSet::new() }
    }

    /// Marks `x` as unrelately rebound if it names one of this function's parameters; a no-op
    /// for a local. See `frozen_params`.
    pub(super) fn freeze_if_param(&mut self, x: &str) {
        if self.params.contains(x) {
            self.frozen_params.insert(x.to_string());
        }
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
