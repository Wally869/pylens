//! Per-function setup helpers: collecting parameter names/defs from the AST, reading a return
//! annotation's name, and extracting a def's decorator names — all consumed once at the start of
//! [`super::analyze_function`], before the body walk begins.

use std::collections::HashSet;

use ruff_python_ast as ast;

use crate::model::{ParamInfo, ParamKind, Shape};

use super::super::super::collect::aliases::{dotted_attr, leftmost_name};
use super::super::super::collect::guards::literal_value;

pub(super) fn collect_param_names(params: &ast::Parameters) -> Vec<String> {
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

/// A literal default value (number/string/bool/`None`) — a non-literal default (a call, a name,
/// a list display, …) contributes nothing.
fn default_literal(default: Option<&ast::Expr>) -> Option<serde_json::Value> {
    literal_value(default?)
}

fn param_info(
    name: String,
    has_default: bool,
    kind: ParamKind,
    ann: Option<&ast::Expr>,
    default_literal: Option<serde_json::Value>,
) -> ParamInfo {
    ParamInfo {
        name,
        shape: Shape::Any,
        has_default,
        kind,
        declared: annotation_name(ann),
        guard_samples: Vec::new(),
        default_literal,
        hints: Vec::new(),
        declared_shape_hint: declared_shape_hint(ann),
    }
}

pub(super) fn collect_param_defs(params: &ast::Parameters, skip: Option<&str>) -> Vec<ParamInfo> {
    let mut raw: Vec<ParamInfo> = Vec::new();
    for p in &params.posonlyargs {
        raw.push(param_info(
            p.parameter.name.as_str().to_string(),
            p.default.is_some(),
            ParamKind::Positional,
            p.parameter.annotation.as_deref(),
            default_literal(p.default.as_deref()),
        ));
    }
    for p in &params.args {
        raw.push(param_info(
            p.parameter.name.as_str().to_string(),
            p.default.is_some(),
            ParamKind::Positional,
            p.parameter.annotation.as_deref(),
            default_literal(p.default.as_deref()),
        ));
    }
    if let Some(v) = &params.vararg {
        raw.push(param_info(
            v.name.as_str().to_string(),
            false,
            ParamKind::VarPositional,
            v.annotation.as_deref(),
            None,
        ));
    }
    for p in &params.kwonlyargs {
        raw.push(param_info(
            p.parameter.name.as_str().to_string(),
            p.default.is_some(),
            ParamKind::KeywordOnly,
            p.parameter.annotation.as_deref(),
            default_literal(p.default.as_deref()),
        ));
    }
    if let Some(k) = &params.kwarg {
        raw.push(param_info(
            k.name.as_str().to_string(),
            false,
            ParamKind::VarKeyword,
            k.annotation.as_deref(),
            None,
        ));
    }
    raw.into_iter().filter(|p| Some(p.name.as_str()) != skip).collect()
}

/// Dotted decorator names applied to this def, in source order (e.g. `@app.route(...)` ->
/// `"app.route"`), for the Purity pass to check against the recognized-transparent set.
pub(super) fn decorator_names(def: &ast::StmtFunctionDef) -> Vec<String> {
    def.decorator_list
        .iter()
        .filter_map(|d| decorator_dotted_name(&d.expression))
        .collect()
}

fn decorator_dotted_name(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Call(c) => decorator_dotted_name(&c.func),
        _ => dotted_attr(expr),
    }
}

/// Names bound to a nested `def`/`class` statement anywhere in `body` (any depth, recursing
/// through `if`/`for`/`while`/`try`/`with`/`match` the way the Effects walk's own `visit_stmt`
/// does) — see `FunctionFacts::local_defs`'s doc for why this exists: the Effects walk never
/// descends into a nested def/class's own body ("a separate scope"), so nothing else notices a
/// local `def len(...): ...` shadowing the builtin `len` for the rest of this function.
pub(super) fn local_def_names(body: &[ast::Stmt]) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_local_defs(body, &mut out);
    out
}

fn collect_local_defs(body: &[ast::Stmt], out: &mut HashSet<String>) {
    for stmt in body {
        match stmt {
            ast::Stmt::FunctionDef(def) => {
                out.insert(def.name.as_str().to_string());
            }
            ast::Stmt::ClassDef(class) => {
                out.insert(class.name.as_str().to_string());
            }
            ast::Stmt::If(if_stmt) => {
                collect_local_defs(&if_stmt.body, out);
                for clause in &if_stmt.elif_else_clauses {
                    collect_local_defs(&clause.body, out);
                }
            }
            ast::Stmt::For(for_stmt) => {
                collect_local_defs(&for_stmt.body, out);
                collect_local_defs(&for_stmt.orelse, out);
            }
            ast::Stmt::While(while_stmt) => {
                collect_local_defs(&while_stmt.body, out);
                collect_local_defs(&while_stmt.orelse, out);
            }
            ast::Stmt::With(with_stmt) => collect_local_defs(&with_stmt.body, out),
            ast::Stmt::Try(try_stmt) => {
                collect_local_defs(&try_stmt.body, out);
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_local_defs(&h.body, out);
                }
                collect_local_defs(&try_stmt.orelse, out);
                collect_local_defs(&try_stmt.finalbody, out);
            }
            ast::Stmt::Match(match_stmt) => {
                for case in &match_stmt.cases {
                    collect_local_defs(&case.body, out);
                }
            }
            _ => {}
        }
    }
}

pub(super) fn annotation_name(ann: Option<&ast::Expr>) -> Option<String> {
    match ann? {
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Subscript(s) => Some(leftmost_name(&s.value)?.to_string()),
        _ => None,
    }
}

/// A concrete generation-ranking [`Shape`] for the simple annotation forms `int`, `float`,
/// `str`, `bool`, `bytes`, `list`/`List`/`tuple`/`Tuple`/`Sequence`, `dict`/`Dict`/`Mapping`,
/// `set`/`Set`/`frozenset`, `Optional[T]`, and `T | None` (recursing into `T` for the last two).
/// `None` for anything else (a bare class name, an unparameterized `Union`, `Any`, ...) — see
/// `ParamInfo::declared_shape_hint`'s doc. Deliberately independent of [`annotation_name`]'s
/// coarser `declared` string, which `type_check` relies on staying exactly as it is today.
fn declared_shape_hint(ann: Option<&ast::Expr>) -> Option<Shape> {
    match ann? {
        ast::Expr::Name(n) => base_shape_for_annotation_name(n.id.as_str()),
        ast::Expr::Subscript(s) => {
            let base = leftmost_name(&s.value)?;
            if base == "Optional" {
                declared_shape_hint(Some(&s.slice))
            } else {
                base_shape_for_annotation_name(base)
            }
        }
        ast::Expr::BinOp(b) if b.op == ast::Operator::BitOr => {
            if matches!(b.left.as_ref(), ast::Expr::NoneLiteral(_)) {
                declared_shape_hint(Some(&b.right))
            } else if matches!(b.right.as_ref(), ast::Expr::NoneLiteral(_)) {
                declared_shape_hint(Some(&b.left))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn base_shape_for_annotation_name(name: &str) -> Option<Shape> {
    match name {
        "int" => Some(Shape::Int),
        "float" => Some(Shape::Float),
        "bool" => Some(Shape::Bool),
        "str" => Some(Shape::Str),
        "bytes" => Some(Shape::Bytes),
        "list" | "List" | "tuple" | "Tuple" | "Sequence" => Some(Shape::any_seq()),
        "dict" | "Dict" | "Mapping" => Some(Shape::any_map()),
        "set" | "Set" | "frozenset" => Some(Shape::any_set()),
        _ => None,
    }
}
