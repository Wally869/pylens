//! Walks a function body's AST to find handled branch predicates: derivation resolution
//! (resolve_param, extract_deriv), the compare/call/literal extraction family (extract,
//! extract_compare, single_compare, extract_call, extract_literal, literal_container,
//! literal_int, to_cmp_op, flip), and the sequential-walk alias tracking (walk, bind_for_target,
//! bind_assign, insert_test) that lets a later branch test resolve a local back to the parameter
//! it derives from.

use std::collections::HashMap;

use ruff_python_ast as ast;
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextSize};

use super::{Aliases, CmpOp, Derivation, LinePredicates, Literal, Predicate, StrMethod};

/// name resolved to a parameter: either name itself is one, or Aliases maps it directly
/// (not through len/index/mod) to one.
pub(super) fn resolve_param(name: &str, params: &[String], aliases: &Aliases) -> Option<String> {
    if params.iter().any(|p| p == name) {
        return Some(name.to_string());
    }
    match aliases.get(name) {
        Some((param, Derivation::Direct)) => Some(param.clone()),
        _ => None,
    }
}

/// Find name's (optionally owner's method) body in a parsed module.
pub fn find_function_body<'a>(
    module: &'a ast::ModModule,
    name: &str,
    owner: Option<&str>,
) -> Option<&'a [ast::Stmt]> {
    match owner {
        None => module.body.iter().find_map(|s| match s {
            ast::Stmt::FunctionDef(f) if f.name.as_str() == name => Some(f.body.as_slice()),
            _ => None,
        }),
        Some(class) => module.body.iter().find_map(|s| match s {
            ast::Stmt::ClassDef(c) if c.name.as_str() == class => {
                c.body.iter().find_map(|m| match m {
                    ast::Stmt::FunctionDef(f) if f.name.as_str() == name => Some(f.body.as_slice()),
                    _ => None,
                })
            }
            _ => None,
        }),
    }
}

/// Collect every handled predicate in body, keyed by the branch point's line.
pub fn collect_predicates(
    body: &[ast::Stmt],
    line_index: &LineIndex,
    params: &[String],
) -> HashMap<u32, LinePredicates> {
    let mut out = HashMap::new();
    let mut aliases = Aliases::new();
    walk(body, line_index, params, &mut aliases, &mut out);
    out
}

pub(super) fn walk(
    body: &[ast::Stmt],
    li: &LineIndex,
    params: &[String],
    aliases: &mut Aliases,
    out: &mut HashMap<u32, LinePredicates>,
) {
    for stmt in body {
        match stmt {
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
            ast::Stmt::Assign(assign) => bind_assign(assign, params, aliases),
            ast::Stmt::If(if_stmt) => {
                insert_test(out, line_at(if_stmt.range().start(), li), &if_stmt.test, params, aliases);
                walk(&if_stmt.body, li, params, aliases, out);
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        insert_test(out, line_at(clause.range().start(), li), test, params, aliases);
                    }
                    walk(&clause.body, li, params, aliases, out);
                }
            }
            ast::Stmt::While(w) => {
                insert_test(out, line_at(w.range().start(), li), &w.test, params, aliases);
                walk(&w.body, li, params, aliases, out);
                walk(&w.orelse, li, params, aliases, out);
            }
            ast::Stmt::For(f) => {
                let line = line_at(f.range().start(), li);
                let iter_deriv = extract_deriv(&f.iter, params, aliases);
                if let Some((param, Derivation::Direct)) = &iter_deriv {
                    out.insert(line, LinePredicates::ForIter(Predicate::ForIter { param: param.clone() }));
                }
                let pre_loop = aliases.clone();
                if let Some((param, Derivation::Direct)) = iter_deriv {
                    bind_for_target(&f.target, &param, aliases);
                }
                walk(&f.body, li, params, aliases, out);
                walk(&f.orelse, li, params, aliases, out);
                *aliases = pre_loop;
            }
            ast::Stmt::With(w) => walk(&w.body, li, params, aliases, out),
            ast::Stmt::Try(t) => {
                walk(&t.body, li, params, aliases, out);
                for handler in &t.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk(&h.body, li, params, aliases, out);
                }
                walk(&t.orelse, li, params, aliases, out);
                walk(&t.finalbody, li, params, aliases, out);
            }
            ast::Stmt::Match(m) => {
                for case in &m.cases {
                    walk(&case.body, li, params, aliases, out);
                }
            }
            _ => {}
        }
    }
}

/// Bind a for loop's target name(s) as Derivation::Element(s) of param.
pub(super) fn bind_for_target(target: &ast::Expr, param: &str, aliases: &mut Aliases) {
    match target {
        ast::Expr::Name(n) => {
            aliases.insert(n.id.to_string(), (param.to_string(), Derivation::Element { field: None, arity: 1 }));
        }
        ast::Expr::Tuple(t) if !t.elts.is_empty() && t.elts.iter().all(|e| matches!(e, ast::Expr::Name(_))) => {
            let arity = t.elts.len();
            for (i, elt) in t.elts.iter().enumerate() {
                let ast::Expr::Name(n) = elt else { unreachable!("checked all Name above") };
                aliases.insert(n.id.to_string(), (param.to_string(), Derivation::Element { field: Some(i), arity }));
            }
        }
        _ => {}
    }
}

/// Bind (or clear) aliases for one Assign statement.
pub(super) fn bind_assign(assign: &ast::StmtAssign, params: &[String], aliases: &mut Aliases) {
    match assign.targets.as_slice() {
        [ast::Expr::Name(target)] => match extract_deriv(&assign.value, params, aliases) {
            Some(deriv) => {
                aliases.insert(target.id.to_string(), deriv);
            }
            None => {
                aliases.remove(target.id.as_str());
            }
        },
        [ast::Expr::Tuple(t)] if !t.elts.is_empty() && t.elts.iter().all(|e| matches!(e, ast::Expr::Name(_))) => {
            let names: Vec<&str> = t
                .elts
                .iter()
                .map(|e| {
                    let ast::Expr::Name(n) = e else { unreachable!("checked all Name above") };
                    n.id.as_str()
                })
                .collect();
            let rhs = match assign.value.as_ref() {
                ast::Expr::Name(n) => aliases.get(n.id.as_str()).cloned(),
                _ => None,
            };
            match rhs {
                Some((param, Derivation::Element { field: None, arity: 1 })) => {
                    let arity = names.len();
                    for (i, name) in names.iter().enumerate() {
                        aliases.insert(name.to_string(), (param.clone(), Derivation::Element { field: Some(i), arity }));
                    }
                }
                _ => {
                    for name in names {
                        aliases.remove(name);
                    }
                }
            }
        }
        _ => {}
    }
}

pub(super) fn insert_test(
    out: &mut HashMap<u32, LinePredicates>,
    line: u32,
    test: &ast::Expr,
    params: &[String],
    aliases: &Aliases,
) {
    let preds = extract(test, params, aliases);
    if !preds.is_empty() {
        out.insert(line, LinePredicates::Test(preds));
    }
}

/// Decompose expr into every handled leaf predicate.
pub(super) fn extract(expr: &ast::Expr, params: &[String], aliases: &Aliases) -> Vec<Predicate> {
    match expr {
        ast::Expr::BoolOp(b) => b.values.iter().flat_map(|v| extract(v, params, aliases)).collect(),
        ast::Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::Not) => extract(&u.operand, params, aliases)
            .into_iter()
            .map(|p| Predicate::Not(Box::new(p)))
            .collect(),
        ast::Expr::Compare(c) => extract_compare(c, params, aliases),
        ast::Expr::Call(call) => match extract_call(call, params, aliases) {
            Some(p) => vec![p],
            None => match extract_deriv(expr, params, aliases) {
                Some((param, Derivation::Direct | Derivation::Len)) => vec![Predicate::Truthy { param }],
                _ => Vec::new(),
            },
        },
        _ => match extract_deriv(expr, params, aliases) {
            Some((param, Derivation::Direct | Derivation::Len)) => vec![Predicate::Truthy { param }],
            _ => Vec::new(),
        },
    }
}

/// p.method(...) where method is one of StrMethod's recognized forms and p resolves to
/// a parameter directly (through resolve_param) -- no subscript/attribute-chained receiver.
pub(super) fn extract_call(call: &ast::ExprCall, params: &[String], aliases: &Aliases) -> Option<Predicate> {
    let ast::Expr::Attribute(attr) = call.func.as_ref() else { return None };
    let ast::Expr::Name(recv) = attr.value.as_ref() else { return None };
    let param = resolve_param(recv.id.as_str(), params, aliases)?;
    let method = StrMethod::parse(attr.attr.as_str())?;
    if !call.arguments.keywords.is_empty() {
        return None;
    }
    if method.takes_str_arg() {
        if call.arguments.args.len() != 1 {
            return None;
        }
        let ast::Expr::StringLiteral(s) = &call.arguments.args[0] else { return None };
        Some(Predicate::StrMethod { param, method, arg: Some(s.value.to_str().to_string()) })
    } else {
        if !call.arguments.args.is_empty() {
            return None;
        }
        Some(Predicate::StrMethod { param, method, arg: None })
    }
}

pub(super) fn extract_deriv(expr: &ast::Expr, params: &[String], aliases: &Aliases) -> Option<(String, Derivation)> {
    match expr {
        ast::Expr::Name(n) if params.iter().any(|p| p == n.id.as_str()) => {
            Some((n.id.to_string(), Derivation::Direct))
        }
        ast::Expr::Name(n) => aliases.get(n.id.as_str()).cloned(),
        ast::Expr::Call(call) => {
            let ast::Expr::Name(fname) = call.func.as_ref() else { return None };
            if fname.id.as_str() != "len" {
                return None;
            }
            if call.arguments.args.len() != 1 || !call.arguments.keywords.is_empty() {
                return None;
            }
            let ast::Expr::Name(argn) = &call.arguments.args[0] else { return None };
            let param = resolve_param(argn.id.as_str(), params, aliases)?;
            Some((param, Derivation::Len))
        }
        ast::Expr::Subscript(sub) => {
            let ast::Expr::Name(n) = sub.value.as_ref() else { return None };
            let param = resolve_param(n.id.as_str(), params, aliases)?;
            let idx = literal_int(&sub.slice)?;
            Some((param, Derivation::Index(idx)))
        }
        ast::Expr::BinOp(bin) if matches!(bin.op, ast::Operator::Mod) => {
            let ast::Expr::Name(n) = bin.left.as_ref() else { return None };
            let param = resolve_param(n.id.as_str(), params, aliases)?;
            let k = literal_int(&bin.right)?;
            Some((param, Derivation::Mod(k)))
        }
        _ => None,
    }
}

pub(super) fn to_cmp_op(op: ast::CmpOp) -> Option<CmpOp> {
    match op {
        ast::CmpOp::Eq => Some(CmpOp::Eq),
        ast::CmpOp::NotEq => Some(CmpOp::Ne),
        ast::CmpOp::Lt => Some(CmpOp::Lt),
        ast::CmpOp::LtE => Some(CmpOp::Le),
        ast::CmpOp::Gt => Some(CmpOp::Gt),
        ast::CmpOp::GtE => Some(CmpOp::Ge),
        _ => None,
    }
}

pub(super) fn literal_int(expr: &ast::Expr) -> Option<i64> {
    match expr {
        ast::Expr::NumberLiteral(n) => match &n.value {
            ast::Number::Int(i) => i.as_i64(),
            _ => None,
        },
        ast::Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::USub) => {
            literal_int(&u.operand).map(|v| -v)
        }
        _ => None,
    }
}

pub(super) fn extract_literal(expr: &ast::Expr) -> Option<Literal> {
    match expr {
        ast::Expr::NumberLiteral(n) => match &n.value {
            ast::Number::Int(i) => i.as_i64().map(Literal::Int),
            _ => None,
        },
        ast::Expr::StringLiteral(s) => Some(Literal::Str(s.value.to_str().to_string())),
        ast::Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::USub) => {
            extract_literal(&u.operand).map(|l| match l {
                Literal::Int(i) => Literal::Int(-i),
                other => other,
            })
        }
        _ => None,
    }
}

pub(super) fn literal_container(expr: &ast::Expr) -> Option<Vec<Literal>> {
    let elts: &[ast::Expr] = match expr {
        ast::Expr::List(l) => &l.elts,
        ast::Expr::Tuple(t) => &t.elts,
        ast::Expr::Set(s) => &s.elts,
        _ => return None,
    };
    if elts.is_empty() {
        return None;
    }
    elts.iter().map(extract_literal).collect()
}

pub(super) fn flip(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

pub(super) fn extract_compare(c: &ast::ExprCompare, params: &[String], aliases: &Aliases) -> Vec<Predicate> {
    let mut out = Vec::new();
    let mut left: &ast::Expr = &c.left;
    for (op, right) in c.ops.iter().zip(c.comparators.iter()) {
        if let Some(p) = single_compare(left, *op, right, params, aliases) {
            out.push(p);
        }
        left = right;
    }
    out
}

pub(super) fn single_compare(
    left: &ast::Expr,
    op: ast::CmpOp,
    right: &ast::Expr,
    params: &[String],
    aliases: &Aliases,
) -> Option<Predicate> {
    match op {
        ast::CmpOp::In | ast::CmpOp::NotIn => {
            let negated = matches!(op, ast::CmpOp::NotIn);
            if let Some((param, Derivation::Direct)) = extract_deriv(left, params, aliases)
                && let Some(items) = literal_container(right)
            {
                return Some(Predicate::Membership { param, negated, items });
            }
            if let Some((param, Derivation::Direct)) = extract_deriv(right, params, aliases)
                && let Some(literal) = extract_literal(left)
            {
                return Some(Predicate::ContainerMembership { param, negated, literal });
            }
            None
        }
        ast::CmpOp::Is | ast::CmpOp::IsNot => None,
        _ => {
            let cmp_op = to_cmp_op(op)?;
            if let Some((param, deriv)) = extract_deriv(left, params, aliases) {
                if let Some(literal) = extract_literal(right) {
                    return Some(Predicate::Compare { param, deriv, op: cmp_op, literal });
                }
                if let Some((param_b, deriv_b)) = extract_deriv(right, params, aliases)
                    && param_b != param
                {
                    return Some(Predicate::ParamCompare {
                        param_a: param,
                        deriv_a: deriv,
                        op: cmp_op,
                        param_b,
                        deriv_b,
                    });
                }
                return None;
            }
            if let Some((param, deriv)) = extract_deriv(right, params, aliases) {
                let literal = extract_literal(left)?;
                return Some(Predicate::Compare { param, deriv, op: flip(cmp_op), literal });
            }
            None
        }
    }
}

pub(super) fn line_at(offset: TextSize, li: &LineIndex) -> u32 {
    li.line_index(offset).get() as u32
}
