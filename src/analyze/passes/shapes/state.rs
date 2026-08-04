//! [`ShapeState`]: the alias map + name-to-shape environment threaded through one function's
//! fixpoint walk, and its basic construction/lookup operations. See [`super::pinning`] for the
//! refinement (write) side of the env, and [`super::shape_of`] for read-only shape derivation
//! from an expression.

use std::collections::HashMap;

use crate::model::Shape;

/// Alias tracking + the name -> shape environment for one function's fixpoint walk. Every read
/// or write of a name's shape goes through its **root** — the representative name reached by
/// following `aliases` — so `q = p; q.append(1)` and later uses of `p` see the same evidence.
pub(super) struct ShapeState {
    pub(super) aliases: HashMap<String, String>,
    pub(super) env: HashMap<String, Shape>,
}

impl ShapeState {
    pub(super) fn new(params: &[String]) -> Self {
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
