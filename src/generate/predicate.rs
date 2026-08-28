//! Predicate-targeted synthesis for `pylens record --cover-branches`: extracts the handled forms
//! of a branch test expression (`if`/`elif`/`while`'s test, `for`'s iterated expression) over a
//! single positional parameter, and produces satisfying/violating values for it. Feeds
//! `record::cover`'s loop, which builds a full input vector around a synthesized value and
//! executes it as an ordinary generated case — synthesis never bypasses the sandbox, so it can't
//! break soundness; a wrong guess just wastes a slot of the case budget like any other candidate.
//!
//! Extraction is over the raw AST, independent of the analyzer's alias tracking: a predicate is
//! recognized only when it names a parameter directly (`p`, `len(p)`, `p[i]` with a literal
//! index, `p % k` with a literal modulus) — no cross-variable reasoning, matching the scope the
//! task spec draws.

use std::collections::HashMap;

use ruff_python_ast as ast;
use ruff_source_file::LineIndex;
use ruff_text_size::Ranged;
use serde_json::{Value, json};

use crate::model::Shape;

use super::seeds;

/// How a comparison's derived value relates to the named parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Derivation {
    /// The parameter's own value.
    Direct,
    /// `len(p)`.
    Len,
    /// `p[i]`, `i` a literal integer index.
    Index(i64),
    /// `p % k`, `k` a literal integer modulus.
    Mod(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Str(String),
}

/// One handled predicate over a single named parameter, extracted from a branch test.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Compare { param: String, deriv: Derivation, op: CmpOp, literal: Literal },
    /// A bare-name truthiness test (`if p:`).
    Truthy { param: String },
    /// `p in C` / `p not in C` against a literal container.
    Membership { param: String, negated: bool, items: Vec<Literal> },
    /// `for p2 in p:` — iterating a parameter.
    ForIter { param: String },
}

impl Predicate {
    pub fn param(&self) -> &str {
        match self {
            Predicate::Compare { param, .. }
            | Predicate::Truthy { param }
            | Predicate::Membership { param, .. }
            | Predicate::ForIter { param } => param,
        }
    }
}

/// The predicates found at one branch-point line: either a test expression's decomposed
/// predicates (`If`/`While`), or a `for` loop's iterated-parameter predicate.
#[derive(Debug, Clone, PartialEq)]
pub enum LinePredicates {
    Test(Vec<Predicate>),
    ForIter(Predicate),
}

/// Find `name`'s (optionally `owner`'s method) body in a parsed module — mirrors the lookup
/// `analyze::passes::effects::driver` does while walking, but standalone since `record::cover`
/// re-parses the source outside the analyze pipeline.
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

/// Collect every handled predicate in `body`, keyed by the branch point's line — the same line
/// `analyze::collect::branches::collect_branches` assigns its `BranchPoint`. `params` names the
/// function's positional parameters; only a test naming one of them directly is handled.
pub fn collect_predicates(
    body: &[ast::Stmt],
    line_index: &LineIndex,
    params: &[String],
) -> HashMap<u32, LinePredicates> {
    let mut out = HashMap::new();
    walk(body, line_index, params, &mut out);
    out
}

fn line_at(offset: ruff_text_size::TextSize, li: &LineIndex) -> u32 {
    li.line_index(offset).get() as u32
}

fn walk(body: &[ast::Stmt], li: &LineIndex, params: &[String], out: &mut HashMap<u32, LinePredicates>) {
    for stmt in body {
        match stmt {
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
            ast::Stmt::If(if_stmt) => {
                insert_test(out, line_at(if_stmt.range().start(), li), &if_stmt.test, params);
                walk(&if_stmt.body, li, params, out);
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        insert_test(out, line_at(clause.range().start(), li), test, params);
                    }
                    walk(&clause.body, li, params, out);
                }
            }
            ast::Stmt::While(w) => {
                insert_test(out, line_at(w.range().start(), li), &w.test, params);
                walk(&w.body, li, params, out);
                walk(&w.orelse, li, params, out);
            }
            ast::Stmt::For(f) => {
                let line = line_at(f.range().start(), li);
                if let ast::Expr::Name(n) = f.iter.as_ref()
                    && params.iter().any(|p| p == n.id.as_str())
                {
                    out.insert(
                        line,
                        LinePredicates::ForIter(Predicate::ForIter { param: n.id.to_string() }),
                    );
                }
                walk(&f.body, li, params, out);
                walk(&f.orelse, li, params, out);
            }
            ast::Stmt::With(w) => walk(&w.body, li, params, out),
            ast::Stmt::Try(t) => {
                walk(&t.body, li, params, out);
                for handler in &t.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk(&h.body, li, params, out);
                }
                walk(&t.orelse, li, params, out);
                walk(&t.finalbody, li, params, out);
            }
            ast::Stmt::Match(m) => {
                for case in &m.cases {
                    walk(&case.body, li, params, out);
                }
            }
            _ => {}
        }
    }
}

fn insert_test(out: &mut HashMap<u32, LinePredicates>, line: u32, test: &ast::Expr, params: &[String]) {
    let preds = extract(test, params);
    if !preds.is_empty() {
        out.insert(line, LinePredicates::Test(preds));
    }
}

/// Decompose `expr` into every handled leaf predicate — `and`/`or` operands are treated
/// independently (see the module doc): each one that fits a handled form contributes its own
/// candidate, without trying to jointly satisfy the whole compound expression.
fn extract(expr: &ast::Expr, params: &[String]) -> Vec<Predicate> {
    match expr {
        ast::Expr::BoolOp(b) => b.values.iter().flat_map(|v| extract(v, params)).collect(),
        ast::Expr::Compare(c) => extract_compare(c, params),
        ast::Expr::Name(n) if params.iter().any(|p| p == n.id.as_str()) => {
            vec![Predicate::Truthy { param: n.id.to_string() }]
        }
        _ => Vec::new(),
    }
}

fn extract_compare(c: &ast::ExprCompare, params: &[String]) -> Vec<Predicate> {
    let mut out = Vec::new();
    let mut left: &ast::Expr = &c.left;
    for (op, right) in c.ops.iter().zip(c.comparators.iter()) {
        if let Some(p) = single_compare(left, *op, right, params) {
            out.push(p);
        }
        left = right;
    }
    out
}

fn single_compare(
    left: &ast::Expr,
    op: ast::CmpOp,
    right: &ast::Expr,
    params: &[String],
) -> Option<Predicate> {
    match op {
        ast::CmpOp::In | ast::CmpOp::NotIn => {
            let (param, Derivation::Direct) = extract_deriv(left, params)? else { return None };
            let items = literal_container(right)?;
            Some(Predicate::Membership { param, negated: matches!(op, ast::CmpOp::NotIn), items })
        }
        ast::CmpOp::Is | ast::CmpOp::IsNot => None,
        _ => {
            let cmp_op = to_cmp_op(op)?;
            if let Some((param, deriv)) = extract_deriv(left, params) {
                let literal = extract_literal(right)?;
                return Some(Predicate::Compare { param, deriv, op: cmp_op, literal });
            }
            if let Some((param, deriv)) = extract_deriv(right, params) {
                let literal = extract_literal(left)?;
                return Some(Predicate::Compare { param, deriv, op: flip(cmp_op), literal });
            }
            None
        }
    }
}

fn to_cmp_op(op: ast::CmpOp) -> Option<CmpOp> {
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

fn flip(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

fn extract_deriv(expr: &ast::Expr, params: &[String]) -> Option<(String, Derivation)> {
    match expr {
        ast::Expr::Name(n) if params.iter().any(|p| p == n.id.as_str()) => {
            Some((n.id.to_string(), Derivation::Direct))
        }
        ast::Expr::Call(call) => {
            let ast::Expr::Name(fname) = call.func.as_ref() else { return None };
            if fname.id.as_str() != "len" {
                return None;
            }
            if call.arguments.args.len() != 1 || !call.arguments.keywords.is_empty() {
                return None;
            }
            let ast::Expr::Name(argn) = &call.arguments.args[0] else { return None };
            if !params.iter().any(|p| p == argn.id.as_str()) {
                return None;
            }
            Some((argn.id.to_string(), Derivation::Len))
        }
        ast::Expr::Subscript(sub) => {
            let ast::Expr::Name(n) = sub.value.as_ref() else { return None };
            if !params.iter().any(|p| p == n.id.as_str()) {
                return None;
            }
            let idx = literal_int(&sub.slice)?;
            Some((n.id.to_string(), Derivation::Index(idx)))
        }
        ast::Expr::BinOp(bin) if matches!(bin.op, ast::Operator::Mod) => {
            let ast::Expr::Name(n) = bin.left.as_ref() else { return None };
            if !params.iter().any(|p| p == n.id.as_str()) {
                return None;
            }
            let k = literal_int(&bin.right)?;
            Some((n.id.to_string(), Derivation::Mod(k)))
        }
        _ => None,
    }
}

fn literal_int(expr: &ast::Expr) -> Option<i64> {
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

fn extract_literal(expr: &ast::Expr) -> Option<Literal> {
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

fn literal_container(expr: &ast::Expr) -> Option<Vec<Literal>> {
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

/// The shape used to pick a synthesized value's structural kind: a `Union`/`Instance`, which
/// synthesis has no concrete kind for, falls back to a generic spread — mirrors
/// `generate::generation_shape`'s reasoning for the same case.
fn effective_shape(shape: &Shape) -> Shape {
    match shape {
        Shape::Union(_) | Shape::Instance(_) => Shape::Any,
        other => other.clone(),
    }
}

fn is_falsy(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f == 0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => match m.get("__t__").and_then(Value::as_str) {
            Some("dict") | Some("set") => {
                m.get("items").and_then(Value::as_array).is_none_or(|a| a.is_empty())
            }
            Some("float") => false,
            _ => m.is_empty(),
        },
    }
}

fn is_empty_container(v: &Value) -> bool {
    match v {
        Value::Array(a) => a.is_empty(),
        Value::String(s) => s.is_empty(),
        Value::Object(m) => match m.get("__t__").and_then(Value::as_str) {
            Some("dict") | Some("set") => {
                m.get("items").and_then(Value::as_array).is_none_or(|a| a.is_empty())
            }
            _ => m.is_empty(),
        },
        _ => false,
    }
}

fn truthy_value(shape: &Shape, want: bool) -> Value {
    let cands = seeds::candidates(&effective_shape(shape));
    for c in &cands {
        if is_falsy(&c.value) == !want {
            return c.value.clone();
        }
    }
    if want { json!(1) } else { json!(0) }
}

fn container_value(shape: &Shape, want_nonempty: bool) -> Value {
    let target = match shape {
        Shape::Seq(_) | Shape::Set(_) | Shape::Map(..) | Shape::Str => shape.clone(),
        _ => Shape::any_seq(),
    };
    let cands = seeds::candidates(&target);
    for c in &cands {
        if is_empty_container(&c.value) == !want_nonempty {
            return c.value.clone();
        }
    }
    if want_nonempty { json!([1]) } else { json!([]) }
}

/// A candidate integer `p` (or derived integer) such that `p op c` evaluates to `want`.
fn synth_int(op: CmpOp, c: i64, want: bool) -> i64 {
    match (op, want) {
        (CmpOp::Eq, true) | (CmpOp::Le, true) | (CmpOp::Ge, true) => c,
        (CmpOp::Eq, false) | (CmpOp::Le, false) => c + 1,
        (CmpOp::Ne, true) => c + 1,
        (CmpOp::Ne, false) => c,
        (CmpOp::Lt, true) => c - 1,
        (CmpOp::Lt, false) => c,
        (CmpOp::Gt, true) => c + 1,
        (CmpOp::Gt, false) | (CmpOp::Ge, false) => c - 1,
    }
}

fn str_eq_ne(op: CmpOp, s: &str, want: bool) -> Option<Value> {
    match op {
        CmpOp::Eq => Some(json!(if want { s.to_string() } else { format!("{s}_") })),
        CmpOp::Ne => Some(json!(if want { format!("{s}_") } else { s.to_string() })),
        _ => None,
    }
}

fn len_value(shape: &Shape, target_len: i64) -> Value {
    let n = target_len.max(0) as usize;
    match effective_shape(shape) {
        Shape::Str => json!("x".repeat(n)),
        _ => Value::Array(vec![json!(0); n]),
    }
}

fn index_value(idx: i64, op: CmpOp, literal: &Literal, want: bool) -> Option<Value> {
    if idx < 0 {
        return None;
    }
    let i = idx as usize;
    let elem = match literal {
        Literal::Int(c) => json!(synth_int(op, *c, want)),
        Literal::Str(s) => str_eq_ne(op, s, want)?,
    };
    let mut arr = vec![json!(0); i + 1];
    arr[i] = elem;
    Some(Value::Array(arr))
}

fn mod_value(k: i64, op: CmpOp, r: i64, want: bool) -> Option<Value> {
    if k == 0 {
        return None;
    }
    match op {
        CmpOp::Eq => Some(json!(if want { r } else { r + 1 })),
        CmpOp::Ne => Some(json!(if want { r + 1 } else { r })),
        _ => None,
    }
}

fn literal_to_value(l: &Literal) -> Value {
    match l {
        Literal::Int(i) => json!(*i),
        Literal::Str(s) => json!(s.clone()),
    }
}

fn not_in_value(items: &[Literal]) -> Option<Value> {
    if items.iter().all(|l| matches!(l, Literal::Int(_))) {
        let used: std::collections::HashSet<i64> = items
            .iter()
            .map(|l| match l {
                Literal::Int(i) => *i,
                Literal::Str(_) => unreachable!("checked all Int above"),
            })
            .collect();
        let mut cand = 0i64;
        while used.contains(&cand) {
            cand += 1;
        }
        Some(json!(cand))
    } else if items.iter().all(|l| matches!(l, Literal::Str(_))) {
        let used: std::collections::HashSet<&str> = items
            .iter()
            .map(|l| match l {
                Literal::Str(s) => s.as_str(),
                Literal::Int(_) => unreachable!("checked all Str above"),
            })
            .collect();
        let mut cand = "zzznotinzzz".to_string();
        while used.contains(cand.as_str()) {
            cand.push('z');
        }
        Some(json!(cand))
    } else {
        None
    }
}

fn membership_value(items: &[Literal], negated: bool, want: bool) -> Option<Value> {
    if items.is_empty() {
        return None;
    }
    let want_in = if negated { !want } else { want };
    if want_in { Some(literal_to_value(&items[0])) } else { not_in_value(items) }
}

fn compare_value(deriv: &Derivation, op: CmpOp, literal: &Literal, shape: &Shape, want: bool) -> Option<Value> {
    match deriv {
        Derivation::Direct => match literal {
            Literal::Int(c) => Some(json!(synth_int(op, *c, want))),
            Literal::Str(s) => str_eq_ne(op, s, want),
        },
        Derivation::Len => {
            let Literal::Int(c) = literal else { return None };
            Some(len_value(shape, synth_int(op, *c, want)))
        }
        Derivation::Index(i) => index_value(*i, op, literal, want),
        Derivation::Mod(k) => {
            let Literal::Int(r) = literal else { return None };
            mod_value(*k, op, *r, want)
        }
    }
}

/// Synthesize a value for `pred`'s parameter such that `pred` evaluates to `want`. `shape` is the
/// parameter's inferred shape (used to decide a container/string vs. scalar candidate). `None`
/// when the predicate's op/derivation/literal combination isn't handled — see the module doc for
/// the closed set of forms this recognizes.
pub fn synthesize(pred: &Predicate, want: bool, shape: &Shape) -> Option<Value> {
    match pred {
        Predicate::Truthy { .. } => Some(truthy_value(shape, want)),
        Predicate::ForIter { .. } => Some(container_value(shape, want)),
        Predicate::Membership { negated, items, .. } => membership_value(items, *negated, want),
        Predicate::Compare { deriv, op, literal, .. } => compare_value(deriv, *op, literal, shape, want),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_expr_of(src: &str) -> ast::Expr {
        let parsed = crate::parse::parse_source(src).expect("parse");
        let module = parsed.syntax();
        match &module.body[0] {
            ast::Stmt::If(if_stmt) => *if_stmt.test.clone(),
            ast::Stmt::While(w) => *w.test.clone(),
            other => panic!("expected an If/While statement, got {other:?}"),
        }
    }

    fn params(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn eq_int_yields_the_literal_and_a_violator() {
        let test = test_expr_of("if p == 42:\n    pass\n");
        let preds = extract(&test, &params(&["p"]));
        assert_eq!(preds.len(), 1);
        let satisfy = synthesize(&preds[0], true, &Shape::Int).expect("satisfying value");
        let violate = synthesize(&preds[0], false, &Shape::Int).expect("violating value");
        assert_eq!(satisfy, json!(42));
        assert_ne!(violate, json!(42));
    }

    #[test]
    fn len_gt_yields_a_long_and_a_short_list() {
        let test = test_expr_of("if len(p) > 3:\n    pass\n");
        let preds = extract(&test, &params(&["p"]));
        assert_eq!(preds.len(), 1);
        let shape = Shape::any_seq();
        let satisfy = synthesize(&preds[0], true, &shape).expect("satisfying value");
        let violate = synthesize(&preds[0], false, &shape).expect("violating value");
        let Value::Array(long) = satisfy else { panic!("expected an array") };
        let Value::Array(short) = violate else { panic!("expected an array") };
        assert!(long.len() > 3, "expected more than 3 elements, got {}", long.len());
        assert!(short.len() <= 3, "expected at most 3 elements, got {}", short.len());
    }

    #[test]
    fn mod_eq_yields_even_and_odd() {
        let test = test_expr_of("if p % 2 == 0:\n    pass\n");
        let preds = extract(&test, &params(&["p"]));
        assert_eq!(preds.len(), 1);
        let satisfy = synthesize(&preds[0], true, &Shape::Int).expect("satisfying value");
        let violate = synthesize(&preds[0], false, &Shape::Int).expect("violating value");
        assert_eq!(satisfy.as_i64().expect("int") % 2, 0);
        assert_ne!(violate.as_i64().expect("int") % 2, 0);
    }

    #[test]
    fn bare_name_test_yields_truthiness() {
        let test = test_expr_of("if p:\n    pass\n");
        let preds = extract(&test, &params(&["p"]));
        assert_eq!(preds, vec![Predicate::Truthy { param: "p".to_string() }]);
        let truthy = synthesize(&preds[0], true, &Shape::Int).expect("truthy value");
        let falsy = synthesize(&preds[0], false, &Shape::Int).expect("falsy value");
        assert_ne!(truthy, json!(0));
        assert_eq!(falsy, json!(0));
    }

    #[test]
    fn and_decomposes_into_its_operands() {
        let test = test_expr_of("if p == 1 and q == 2:\n    pass\n");
        let preds = extract(&test, &params(&["p", "q"]));
        assert_eq!(preds.len(), 2);
        assert!(preds.iter().any(|p| p.param() == "p"));
        assert!(preds.iter().any(|p| p.param() == "q"));
    }

    #[test]
    fn unhandled_predicate_yields_nothing() {
        let test = test_expr_of("if hash(p) == 0:\n    pass\n");
        let preds = extract(&test, &params(&["p"]));
        assert!(preds.is_empty(), "hash(p) is not a handled derivation");
    }
}
