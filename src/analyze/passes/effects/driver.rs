use ruff_python_ast as ast;
use crate::model::*;
use super::super::super::collect::body_lines::collect_body_lines;
use super::super::super::collect::branches::collect_branches;
use super::super::super::collect::returns::{can_fall_through, collect_return_lines};
use super::super::super::context::{ModuleAnalysis, ModuleCtx, ShapeFacts};
use super::super::super::pass::Pass;
use super::super::declarations::ReceiverKind;
use super::{Walker, EffectsPass};
use super::function_analysis::analyze_function;

impl Walker < '_ , '_ > {
        pub fn run(&mut self, body: &[ast::Stmt]) {
            // The top-level (function-body-direct) walk: sets `depth = 0` and the current
            // top-level index for each statement before visiting it, mirroring
            // `passes::shapes::visit_top_level` exactly (same soundness argument) — the anchor
            // `FunctionFacts::env_shape`'s dominance gate measures against. Every NESTED
            // recursion instead goes through `visit_body`, which increments `depth`.
            for (i, stmt) in body.iter().enumerate() {
                self.facts.top_level_index = i;
                self.facts.depth = 0;
                self.visit_stmt(stmt);
            }
            // Two notes on the may-set model:
            // - a body that can fall off the end contributes a `None` return.
            // - returns/raises are unioned over all exits.
            if can_fall_through(body) {
                self.facts.add_return(ReturnKind::None);
            }
        }

}

impl Pass for EffectsPass {
        fn run(&self, module: &ast::ModModule, ctx: &mut ModuleAnalysis) {
            // The Declarations pass walked the module in this same order, so its receiver-kind
            // table (and the Shapes pass's per-function shape maps) line up one-to-one with this
            // traversal.
            let mut receivers = ctx.declarations.iter().map(|d| d.receiver);
            let mut shapes_iter = ctx.shapes.iter();
            let mut frozen_iter = ctx.frozen_params.iter();
            let mut dominance_iter = ctx.frozen_dominance.iter();
            let module_ctx = ModuleCtx {
                imports: &ctx.bindings,
                import_names: &ctx.import_names,
                has_star: ctx.has_star,
                declarations: &ctx.declarations,
                classes: &ctx.classes,
            };
            for stmt in &module.body {
                match stmt {
                    ast::Stmt::FunctionDef(def) => {
                        let receiver = receivers.next().unwrap_or(ReceiverKind::None);
                        let shape_facts = ShapeFacts {
                            shapes: shapes_iter.next().cloned().unwrap_or_default(),
                            frozen_params: frozen_iter.next().cloned().unwrap_or_default(),
                            frozen_dominance: dominance_iter.next().cloned().unwrap_or_default(),
                        };
                        let (mut sig, call_sites, import_call_sites) = analyze_function(
                            def,
                            DefKind::Function,
                            receiver,
                            module_ctx,
                            shape_facts,
                            None,
                        );
                        sig.body_lines = collect_body_lines(&def.body, &ctx.line_index);
                        sig.branch_points = collect_branches(&def.body, &ctx.line_index);
                        sig.return_lines = collect_return_lines(&def.body, &ctx.line_index);
                        ctx.signatures.push(sig);
                        ctx.call_sites.push(call_sites);
                        ctx.import_call_sites.push(import_call_sites);
                    }
                    ast::Stmt::ClassDef(class) => {
                        for member in &class.body {
                            if let ast::Stmt::FunctionDef(def) = member {
                                let receiver = receivers.next().unwrap_or(ReceiverKind::None);
                                let shape_facts = ShapeFacts {
                                    shapes: shapes_iter.next().cloned().unwrap_or_default(),
                                    frozen_params: frozen_iter.next().cloned().unwrap_or_default(),
                                    frozen_dominance: dominance_iter
                                        .next()
                                        .cloned()
                                        .unwrap_or_default(),
                                };
                                let (mut sig, call_sites, import_call_sites) = analyze_function(
                                    def,
                                    DefKind::Method,
                                    receiver,
                                    module_ctx,
                                    shape_facts,
                                    Some(class.name.as_str()),
                                );
                                sig.owner = Some(class.name.as_str().to_string());
                                sig.body_lines = collect_body_lines(&def.body, &ctx.line_index);
                                sig.branch_points = collect_branches(&def.body, &ctx.line_index);
                                sig.return_lines = collect_return_lines(&def.body, &ctx.line_index);
                                ctx.signatures.push(sig);
                                ctx.call_sites.push(call_sites);
                                ctx.import_call_sites.push(import_call_sites);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

}
