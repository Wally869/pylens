//! Purity pass: derives the final [`Purity`] classification from each function's fully
//! assembled effect signature. Runs last, after Effects — the injection point for a future
//! Interprocedural pass sits between them.

use ruff_python_ast as ast;

use crate::model::{Purity, UnresolvedEffect};

use super::super::context::ModuleAnalysis;
use super::super::pass::Pass;

pub(in crate::analyze) struct PurityPass;

impl Pass for PurityPass {
    fn run(&self, _module: &ast::ModModule, ctx: &mut ModuleAnalysis) {
        for sig in &mut ctx.signatures {
            let opaque_decorators: Vec<String> = sig
                .decorators
                .iter()
                .filter(|d| !is_recognized_transparent(d))
                .cloned()
                .collect();
            for decorator in opaque_decorators {
                // A decorator we don't recognize can replace the function entirely (wrap it,
                // memoize it, register a different callable) — we can't see through that.
                sig.unresolved_effects.push(UnresolvedEffect {
                    reason: "decorator".to_string(),
                    callee: Some(decorator),
                    may_affect: Vec::new(),
                });
            }
            sig.purity = if !sig.unresolved_effects.is_empty() {
                Purity::Unknown
            } else if sig.mutations.is_empty()
                && sig.global_writes.is_empty()
                && sig.io.is_empty()
                && !sig.is_generator
            {
                Purity::Pure
            } else {
                Purity::Impure
            };
        }
    }
}

/// Decorators known not to replace the decorated callable — they only affect how it's bound
/// (`staticmethod`/`classmethod`) or how it's read (`property`) or add a runtime check that
/// doesn't change its effects (`abstractmethod`). Matched on the decorator's last dotted
/// component so `abc.abstractmethod` is recognized alongside the bare name.
fn is_recognized_transparent(decorator: &str) -> bool {
    let last = decorator.rsplit('.').next().unwrap_or(decorator);
    matches!(
        last,
        "staticmethod" | "classmethod" | "property" | "abstractmethod"
    )
}
