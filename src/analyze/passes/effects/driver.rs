use ruff_python_ast as ast;
use crate::model::*;
use super::super::super::collect::body_lines::collect_body_lines;
use super::super::super::collect::returns::can_fall_through;
use super::super::super::context::{ModuleAnalysis, ModuleCtx};
use super::super::super::pass::Pass;
use super::super::declarations::ReceiverKind;
use super::{Walker, EffectsPass};
use super::function_analysis::analyze_function;

impl Walker < '_ , '_ > {
        pub fn run(&mut self, body: &[ast::Stmt]) {
            // Two notes on the may-set model:
            // - a body that can fall off the end contributes a `None` return.
            // - returns/raises are unioned over all exits.
            self.visit_body(body);
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
            let module_ctx = ModuleCtx {
                imports: &ctx.bindings,
                has_star: ctx.has_star,
                declarations: &ctx.declarations,
                classes: &ctx.classes,
            };
            for stmt in &module.body {
                match stmt {
                    ast::Stmt::FunctionDef(def) => {
                        let receiver = receivers.next().unwrap_or(ReceiverKind::None);
                        let shapes = shapes_iter.next().cloned().unwrap_or_default();
                        let (mut sig, call_sites, import_call_sites) =
                            analyze_function(def, DefKind::Function, receiver, module_ctx, shapes, None);
                        sig.body_lines = collect_body_lines(&def.body, &ctx.line_index);
                        ctx.signatures.push(sig);
                        ctx.call_sites.push(call_sites);
                        ctx.import_call_sites.push(import_call_sites);
                    }
                    ast::Stmt::ClassDef(class) => {
                        for member in &class.body {
                            if let ast::Stmt::FunctionDef(def) = member {
                                let receiver = receivers.next().unwrap_or(ReceiverKind::None);
                                let shapes = shapes_iter.next().cloned().unwrap_or_default();
                                let (mut sig, call_sites, import_call_sites) = analyze_function(
                                    def,
                                    DefKind::Method,
                                    receiver,
                                    module_ctx,
                                    shapes,
                                    Some(class.name.as_str()),
                                );
                                sig.owner = Some(class.name.as_str().to_string());
                                sig.body_lines = collect_body_lines(&def.body, &ctx.line_index);
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
