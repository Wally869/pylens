//! Declarations pass: builds a function/method symbol table — name, owner class, params (with
//! receiver kind), and decorator names — for every top-level function and method. Groundwork
//! for a future Interprocedural pass that resolves intra-file calls; the table is collected here
//! but not yet consumed by anything downstream, and it is never serialized.

use std::collections::HashSet;

use ruff_python_ast as ast;

use super::super::context::ModuleAnalysis;
use super::super::pass::Pass;

/// How a declaration's first parameter binds, if at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverKind {
    None,
    SelfParam,
    Cls,
}

/// One function or method declaration. Part of the pipeline's public surface so a future
/// Interprocedural pass (or an external consumer inspecting the symbol table) can name it; not
/// read anywhere in v1.
#[derive(Debug, Clone)]
pub struct DeclInfo {
    pub name: String,
    /// The class this is a method of; `None` for a free function.
    pub owner: Option<String>,
    /// Parameter names in declaration order (including the receiver, if any).
    pub params: Vec<String>,
    pub receiver: ReceiverKind,
    pub decorators: Vec<String>,
}

/// Builds `ModuleAnalysis::declarations`.
pub(in crate::analyze) struct DeclarationsPass;

impl Pass for DeclarationsPass {
    fn run(&self, module: &ast::ModModule, ctx: &mut ModuleAnalysis) {
        for stmt in &module.body {
            match stmt {
                ast::Stmt::FunctionDef(def) => {
                    ctx.declarations.push(decl_info(def, None));
                }
                ast::Stmt::ClassDef(class) => {
                    let name = class.name.as_str().to_string();
                    ctx.classes.insert(name.clone());
                    let attrs = ctx.class_attrs.entry(name).or_default();
                    for member in &class.body {
                        match member {
                            ast::Stmt::FunctionDef(def) => {
                                attrs.insert(def.name.as_str().to_string());
                                // Only `__init__` proves an attribute: a fresh instance always
                                // runs its class's `__init__` before any other method can
                                // possibly see it, so a `self.attr = ...` there really is on
                                // every instance. An assignment in any OTHER method (`arm()`,
                                // say) proves nothing — an instance can reach `read()` without
                                // ever having called `arm()` first, so treating that as a
                                // guarantee is exactly the unsoundness a probe caught (see
                                // `temp/probe_selfattr.py`: `Gadget.read` genuinely raises
                                // `AttributeError` on a fresh, un-`arm`ed instance).
                                if def.name.as_str() == "__init__"
                                    && let Some(self_name) = first_param_name(&def.parameters)
                                {
                                    collect_self_attrs(&def.body, self_name, attrs);
                                }
                                ctx.declarations
                                    .push(decl_info(def, Some(class.name.as_str().to_string())));
                            }
                            ast::Stmt::Assign(assign) => {
                                for target in &assign.targets {
                                    note_class_level_attr(target, attrs);
                                }
                            }
                            ast::Stmt::AnnAssign(ann) => note_class_level_attr(&ann.target, attrs),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

fn decl_info(def: &ast::StmtFunctionDef, owner: Option<String>) -> DeclInfo {
    let decorators: Vec<String> = def
        .decorator_list
        .iter()
        .filter_map(|d| decorator_name(&d.expression))
        .collect();
    let is_static = decorators.iter().any(|d| d == "staticmethod");
    let is_classmethod = decorators.iter().any(|d| d == "classmethod");
    let receiver = if owner.is_none() || is_static {
        ReceiverKind::None
    } else if is_classmethod {
        ReceiverKind::Cls
    } else {
        ReceiverKind::SelfParam
    };
    DeclInfo {
        name: def.name.as_str().to_string(),
        owner,
        params: param_names(&def.parameters),
        receiver,
        decorators,
    }
}

/// The decorator's bare name (`staticmethod`), attribute name (`app.route` -> `route`), or the
/// name of a call decorator's callee (`@app.route(...)` -> `route`).
fn decorator_name(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Attribute(a) => Some(a.attr.as_str().to_string()),
        ast::Expr::Call(c) => decorator_name(&c.func),
        _ => None,
    }
}

/// Find the unique declaration named `name` owned by `owner` (`None` for a free function,
/// `Some(class)` for a method of `class`). Returns `None` — deliberately, not a best guess — if
/// there is no match or more than one (an ambiguous name, e.g. a module-level function redefined
/// later): resolving to the wrong one could misattribute a callee's effects, so an unresolved
/// call stays unresolved rather than risk that.
pub(in crate::analyze) fn resolve_unique(
    declarations: &[DeclInfo],
    owner: Option<&str>,
    name: &str,
) -> Option<usize> {
    let mut found = None;
    for (i, d) in declarations.iter().enumerate() {
        if d.owner.as_deref() == owner && d.name == name {
            if found.is_some() {
                return None;
            }
            found = Some(i);
        }
    }
    found
}

fn param_names(params: &ast::Parameters) -> Vec<String> {
    let mut out = Vec::new();
    for p in &params.posonlyargs {
        out.push(p.parameter.name.as_str().to_string());
    }
    for p in &params.args {
        out.push(p.parameter.name.as_str().to_string());
    }
    if let Some(v) = &params.vararg {
        out.push(v.name.as_str().to_string());
    }
    for p in &params.kwonlyargs {
        out.push(p.parameter.name.as_str().to_string());
    }
    if let Some(k) = &params.kwarg {
        out.push(k.name.as_str().to_string());
    }
    out
}

/// A class-level `attr = ...` / `attr: T = ...` statement (directly in the class body, not
/// inside any method) — every instance has this attribute the moment the class object itself is
/// defined, before `__init__` even runs, so it's proven unconditionally.
fn note_class_level_attr(target: &ast::Expr, out: &mut HashSet<String>) {
    if let ast::Expr::Name(n) = target {
        out.insert(n.id.as_str().to_string());
    }
}

fn first_param_name(params: &ast::Parameters) -> Option<&str> {
    params
        .posonlyargs
        .first()
        .or(params.args.first())
        .map(|p| p.parameter.name.as_str())
}

/// Collect every name assigned as `self.<attr> = ...` (or annotated/augmented) anywhere in
/// `body`, where `self_name` is this method's own receiver parameter name. Recurses through
/// `if`/`for`/`while`/`try`/`with`/`match` the same way the Effects walk does, but — like the
/// Effects walk — does not descend into a nested `def`/`class`'s own body: that introduces a
/// separate scope, and a closure's `self`-attribute writes are out of scope for this table (see
/// `ModuleAnalysis::class_attrs`'s doc: under-counting here only widens the resulting may-set,
/// never narrows it).
fn note_self_attr(target: &ast::Expr, self_name: &str, out: &mut HashSet<String>) {
    if let ast::Expr::Attribute(a) = target
        && let ast::Expr::Name(n) = a.value.as_ref()
        && n.id.as_str() == self_name
    {
        out.insert(a.attr.as_str().to_string());
    }
}

fn collect_self_attrs(body: &[ast::Stmt], self_name: &str, out: &mut HashSet<String>) {
    for stmt in body {
        match stmt {
            ast::Stmt::Assign(assign) => {
                for target in &assign.targets {
                    note_self_attr(target, self_name, out);
                }
            }
            ast::Stmt::AugAssign(aug) => note_self_attr(&aug.target, self_name, out),
            ast::Stmt::AnnAssign(ann) => note_self_attr(&ann.target, self_name, out),
            ast::Stmt::If(if_stmt) => {
                collect_self_attrs(&if_stmt.body, self_name, out);
                for clause in &if_stmt.elif_else_clauses {
                    collect_self_attrs(&clause.body, self_name, out);
                }
            }
            ast::Stmt::For(for_stmt) => {
                collect_self_attrs(&for_stmt.body, self_name, out);
                collect_self_attrs(&for_stmt.orelse, self_name, out);
            }
            ast::Stmt::While(while_stmt) => {
                collect_self_attrs(&while_stmt.body, self_name, out);
                collect_self_attrs(&while_stmt.orelse, self_name, out);
            }
            ast::Stmt::With(with_stmt) => collect_self_attrs(&with_stmt.body, self_name, out),
            ast::Stmt::Try(try_stmt) => {
                collect_self_attrs(&try_stmt.body, self_name, out);
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_self_attrs(&h.body, self_name, out);
                }
                collect_self_attrs(&try_stmt.orelse, self_name, out);
                collect_self_attrs(&try_stmt.finalbody, self_name, out);
            }
            ast::Stmt::Match(match_stmt) => {
                for case in &match_stmt.cases {
                    collect_self_attrs(&case.body, self_name, out);
                }
            }
            _ => {}
        }
    }
}
