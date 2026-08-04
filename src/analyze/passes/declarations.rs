//! Declarations pass: builds a function/method symbol table — name, owner class, params (with
//! receiver kind), and decorator names — for every top-level function and method. Groundwork
//! for a future Interprocedural pass that resolves intra-file calls; the table is collected here
//! but not yet consumed by anything downstream, and it is never serialized.

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
                    for member in &class.body {
                        if let ast::Stmt::FunctionDef(def) = member {
                            ctx.declarations
                                .push(decl_info(def, Some(class.name.as_str().to_string())));
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
