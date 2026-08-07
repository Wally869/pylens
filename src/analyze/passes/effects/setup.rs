//! Per-function setup helpers: collecting parameter names/defs from the AST, reading a return
//! annotation's name, and extracting a def's decorator names — all consumed once at the start of
//! [`super::analyze_function`], before the body walk begins.

use ruff_python_ast as ast;

use crate::model::{ParamInfo, ParamKind, Shape};

use super::super::super::collect::aliases::{dotted_attr, leftmost_name};

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

pub(super) fn collect_param_defs(params: &ast::Parameters, skip: Option<&str>) -> Vec<ParamInfo> {
    let mut raw: Vec<(String, bool, ParamKind, Option<String>)> = Vec::new();
    for p in &params.posonlyargs {
        raw.push((
            p.parameter.name.as_str().to_string(),
            p.default.is_some(),
            ParamKind::Positional,
            annotation_name(p.parameter.annotation.as_deref()),
        ));
    }
    for p in &params.args {
        raw.push((
            p.parameter.name.as_str().to_string(),
            p.default.is_some(),
            ParamKind::Positional,
            annotation_name(p.parameter.annotation.as_deref()),
        ));
    }
    if let Some(v) = &params.vararg {
        raw.push((
            v.name.as_str().to_string(),
            false,
            ParamKind::VarPositional,
            annotation_name(v.annotation.as_deref()),
        ));
    }
    for p in &params.kwonlyargs {
        raw.push((
            p.parameter.name.as_str().to_string(),
            p.default.is_some(),
            ParamKind::KeywordOnly,
            annotation_name(p.parameter.annotation.as_deref()),
        ));
    }
    if let Some(k) = &params.kwarg {
        raw.push((
            k.name.as_str().to_string(),
            false,
            ParamKind::VarKeyword,
            annotation_name(k.annotation.as_deref()),
        ));
    }
    raw.into_iter()
        .filter(|(n, _, _, _)| Some(n.as_str()) != skip)
        .map(|(name, has_default, kind, declared)| ParamInfo {
            name,
            shape: Shape::Any,
            has_default,
            kind,
            declared,
            guard_samples: Vec::new(),
        })
        .collect()
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

pub(super) fn annotation_name(ann: Option<&ast::Expr>) -> Option<String> {
    match ann? {
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Subscript(s) => Some(leftmost_name(&s.value)?.to_string()),
        _ => None,
    }
}
