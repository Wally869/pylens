//! Predicate-targeted synthesis for `pylens record --cover-branches`: extracts the handled forms
//! of a branch test expression (`if`/`elif`/`while`'s test, `for`'s iterated expression), and
//! produces satisfying/violating values for the parameter(s) it names. Feeds `record::cover`'s
//! loop, which builds a full input vector around the synthesized value(s) and executes it as an
//! ordinary generated case — synthesis never bypasses the sandbox, so it can't break soundness; a
//! wrong guess just wastes a slot of the case budget like any other candidate.
//!
//! Extraction is over the raw AST, mostly independent of the analyzer's alias tracking. A
//! predicate is recognized when it names a parameter directly (`p`, `not p`, `len(p)`, `p[i]`
//! with a literal index, `p % k` with a literal modulus), through one of the string methods in
//! [`StrMethod`] called on a parameter, through membership either direction (`p in C` for a
//! literal container `C`, or `v in p` for a literal `v`), or — the one piece of cross-variable
//! reasoning this module does — through a local variable this function's own body assigns
//! directly from one of the forms above (`n = len(s)`) and then tests, or a comparison between
//! two parameters (`a < b`). A `for` loop's target, when the iterated expression resolves
//! directly to a parameter, is bound the same way: a plain target names an element
//! ([`Derivation::Element`], `field: None`), a flat tuple-of-names target names each field, and a
//! tuple unpack of a still-whole loop element one statement later (`for t in p: a, b = t`) gets
//! the same field-wise binding. Both bindings are scoped strictly to the loop body — see `walk`'s
//! `For` arm. Two-variable coordination and attribute-based forms (`x.attr`, `hasattr`) stay out
//! of scope — see [`Predicate::ParamCompare`] and the module's absence of any attribute-truthiness
//! form. [`Predicate::ParamCompare`] between two [`Derivation::Element`]s is likewise unhandled
//! (see [`synthesize_pair`]'s guard) — an element paired with an element or a non-parameter local
//! (`balance >= amount` in the ATM-style example) stays `no_synthesizer`. So do elements of a
//! locally *derived* container (`parts = ip.split('.')`, then `for part in parts:`) — the iterable
//! must resolve to a parameter directly, not through a derivation.

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
    /// A loop-bound element of the parameter (`for x in p:`, or a tuple unpack of that whole
    /// element, `for t in p: a, b = t` / `for a, b in p:`). `field` is `None` for a plain,
    /// un-unpacked loop target and `Some(i)` for field `i` of an `arity`-wide tuple unpack;
    /// `arity` is 1 for a plain target. Synthesis builds a one-element list around the field's
    /// value (see [`element_value`]) — never the empty list, so the `for` body actually runs.
    Element { field: Option<usize>, arity: usize },
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

/// A no-argument or single-string-literal-argument string method called on a parameter — the
/// method forms this module recognizes as a branch predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrMethod {
    StartsWith,
    EndsWith,
    IsDigit,
    IsAlpha,
    IsUpper,
    IsLower,
    IsSpace,
    IsAlnum,
}

impl StrMethod {
    fn parse(name: &str) -> Option<StrMethod> {
        match name {
            "startswith" => Some(StrMethod::StartsWith),
            "endswith" => Some(StrMethod::EndsWith),
            "isdigit" => Some(StrMethod::IsDigit),
            "isalpha" => Some(StrMethod::IsAlpha),
            "isupper" => Some(StrMethod::IsUpper),
            "islower" => Some(StrMethod::IsLower),
            "isspace" => Some(StrMethod::IsSpace),
            "isalnum" => Some(StrMethod::IsAlnum),
            _ => None,
        }
    }

    /// Whether this method takes the one string-literal argument this module can extract
    /// (`startswith`/`endswith`) rather than none (the `is*` predicates).
    fn takes_str_arg(self) -> bool {
        matches!(self, StrMethod::StartsWith | StrMethod::EndsWith)
    }
}

/// One handled predicate, extracted from a branch test. Every variant but
/// [`Predicate::ParamCompare`] names exactly one parameter (see [`Predicate::param`]);
/// `ParamCompare` coordinates two, and is synthesized separately by
/// [`synthesize_pair`] since satisfying it means overriding both parameters' slots together in
/// one input, not choosing one value in isolation.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Compare { param: String, deriv: Derivation, op: CmpOp, literal: Literal },
    /// A bare-name truthiness test (`if p:`), including a local assigned directly or via `len`
    /// from a parameter.
    Truthy { param: String },
    /// `p in C` / `p not in C` against a literal container.
    Membership { param: String, negated: bool, items: Vec<Literal> },
    /// `v in p` / `v not in p` — `p` is the container (a parameter), `v` a literal tested for
    /// membership.
    ContainerMembership { param: String, negated: bool, literal: Literal },
    /// A [`StrMethod`] call on a parameter, e.g. `p.startswith("x")`, `p.isdigit()`.
    StrMethod { param: String, method: StrMethod, arg: Option<String> },
    /// `for p2 in p:` — iterating a parameter.
    ForIter { param: String },
    /// `not <inner>` — negates the inner predicate's outcome polarity at synthesis time.
    Not(Box<Predicate>),
    /// `a <op> b`, both operands resolving to a parameter (or a derivation of one) — a
    /// coordinated pair, synthesized by [`synthesize_pair`], not [`synthesize`].
    ParamCompare { param_a: String, deriv_a: Derivation, op: CmpOp, param_b: String, deriv_b: Derivation },
}

impl Predicate {
    /// The predicate's one named parameter. For [`Predicate::ParamCompare`] — which names two —
    /// this returns `param_a`; callers that need both must match the variant directly (see
    /// `record::cover::override_set`).
    pub fn param(&self) -> &str {
        match self {
            Predicate::Compare { param, .. }
            | Predicate::Truthy { param }
            | Predicate::Membership { param, .. }
            | Predicate::ContainerMembership { param, .. }
            | Predicate::StrMethod { param, .. }
            | Predicate::ForIter { param } => param,
            Predicate::Not(inner) => inner.param(),
            Predicate::ParamCompare { param_a, .. } => param_a,
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

/// Local-name → derivation aliases discovered so far in a sequential walk of a function body: a
/// local assigned directly from one of the recognized derivations of a parameter (`n = len(s)`,
/// `y = s`) resolves through this map exactly as if the parameter's own name had been written —
/// see the module doc's "one piece of cross-variable reasoning".
type Aliases = HashMap<String, (String, Derivation)>;

/// Collect every handled predicate in `body`, keyed by the branch point's line — the same line
/// `analyze::collect::branches::collect_branches` assigns its `BranchPoint`. `params` names the
/// function's positional parameters; only a test naming one of them (directly, or through an
/// [`Aliases`] entry discovered earlier in the same body) is handled.
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

fn line_at(offset: ruff_text_size::TextSize, li: &LineIndex) -> u32 {
    li.line_index(offset).get() as u32
}

fn walk(
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

/// Bind a `for` loop's target name(s) as [`Derivation::Element`]s of `param` — a plain `Name`
/// target names the whole element (`field: None`), a flat tuple-of-`Name`s target names each
/// field. Any other target shape (a starred target, a nested tuple, a subscript/attribute target)
/// is left unbound. The caller (`walk`'s `For` arm) restores `aliases` to its pre-loop snapshot
/// once the body has been walked, so these bindings never leak past the loop.
fn bind_for_target(target: &ast::Expr, param: &str, aliases: &mut Aliases) {
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

/// Bind (or clear) aliases for one `Assign` statement, walking two shapes: a single-`Name` target
/// (the existing `n = len(s)`-style direct derivation, now also clearing a stale alias when the
/// RHS isn't a recognized derivation — a rebind must not leave a prior binding dangling), and a
/// flat tuple-of-`Name`s target whose RHS is itself a whole, not-yet-unpacked loop element
/// (`Derivation::Element { field: None, arity: 1 }`) — the `for t in p: a, b = t` shape — which
/// fans out into one [`Derivation::Element`] per field. Any other target/RHS shape clears every
/// name in the target rather than leaving a possibly-stale alias in place.
fn bind_assign(assign: &ast::StmtAssign, params: &[String], aliases: &mut Aliases) {
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

fn insert_test(
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

/// Decompose `expr` into every handled leaf predicate — `and`/`or` operands are treated
/// independently (see the module doc): each one that fits a handled form contributes its own
/// candidate, without trying to jointly satisfy the whole compound expression.
fn extract(expr: &ast::Expr, params: &[String], aliases: &Aliases) -> Vec<Predicate> {
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

/// `p.method(...)` where `method` is one of [`StrMethod`]'s recognized forms and `p` resolves to
/// a parameter directly (through [`resolve_param`]) — no subscript/attribute-chained receiver.
fn extract_call(call: &ast::ExprCall, params: &[String], aliases: &Aliases) -> Option<Predicate> {
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

fn extract_compare(c: &ast::ExprCompare, params: &[String], aliases: &Aliases) -> Vec<Predicate> {
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

fn single_compare(
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

/// `name` resolved to a parameter: either `name` itself is one, or [`Aliases`] maps it directly
/// (not through `len`/index/mod) to one — the receiver-resolution `extract_call`'s method
/// predicates and `extract_deriv`'s `len(...)`/`p[i]`/`p % k` forms both use.
fn resolve_param(name: &str, params: &[String], aliases: &Aliases) -> Option<String> {
    if params.iter().any(|p| p == name) {
        return Some(name.to_string());
    }
    match aliases.get(name) {
        Some((param, Derivation::Direct)) => Some(param.clone()),
        _ => None,
    }
}

fn extract_deriv(expr: &ast::Expr, params: &[String], aliases: &Aliases) -> Option<(String, Derivation)> {
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

/// A one-element list value for a [`Derivation::Element`]: `arity <= 1` (a plain, un-unpacked
/// loop target) wraps the field's own value directly; `arity > 1` wraps a tagged tuple with
/// `field`'s slot set to the field value and every other slot filled with a neutral `0`. Always
/// non-empty by construction — an empty list would falsify a membership-style claim, but it would
/// also skip the loop body entirely, so it can never stand in for the `want == false` outcome the
/// caller (`synthesize`, via `record::cover`) is targeting the branch *line* with.
fn element_value(field: Option<usize>, arity: usize, op: CmpOp, literal: &Literal, want: bool) -> Option<Value> {
    let elem = match literal {
        Literal::Int(c) => json!(synth_int(op, *c, want)),
        Literal::Str(s) => str_eq_ne(op, s, want)?,
    };
    let item = if arity <= 1 {
        elem
    } else {
        let field = field?;
        if field >= arity {
            return None;
        }
        let mut fields = vec![json!(0); arity];
        fields[field] = elem;
        json!({ "__t__": "tuple", "items": fields })
    };
    Some(Value::Array(vec![item]))
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

/// A sentinel value overwhelmingly unlikely to appear as a substring/element of an ordinarily
/// generated container — used as the "definitely excludes `literal`" side of
/// [`container_membership_value`], the same role `not_in_value`'s `z`-padding plays for
/// [`Predicate::Membership`].
const EXCLUSION_SENTINEL: &str = "\u{1}\u{2}\u{3}pylens_no_match\u{1}\u{2}\u{3}";

/// A value for `v in p` / `v not in p`'s parameter `p` (the container) such that the whole
/// expression evaluates to `want`. `None` when `literal` is a string and empty — `"" in s` is
/// always `True` for any string `s`, so no string value can violate it, and no non-degenerate
/// value can be constructed to prove `"" not in s`.
fn container_membership_value(literal: &Literal, negated: bool, want: bool, shape: &Shape) -> Option<Value> {
    let want_contains = if negated { !want } else { want };
    match effective_shape(shape) {
        Shape::Str => {
            let Literal::Str(s) = literal else { return None };
            if s.is_empty() {
                return None;
            }
            if want_contains {
                Some(json!(format!("pre_{s}_post")))
            } else {
                Some(json!(EXCLUSION_SENTINEL))
            }
        }
        Shape::Set(_) => {
            let elem = literal_to_value(literal);
            let items = if want_contains { vec![elem] } else { Vec::new() };
            Some(json!({ "__t__": "set", "items": items }))
        }
        _ => {
            let elem = literal_to_value(literal);
            if want_contains { Some(Value::Array(vec![elem])) } else { Some(Value::Array(Vec::new())) }
        }
    }
}

fn str_method_value(method: StrMethod, arg: Option<&str>, want: bool) -> Option<Value> {
    match method {
        StrMethod::StartsWith => {
            let s = arg?;
            if want {
                Some(json!(format!("{s}_rest")))
            } else if s.is_empty() {
                None
            } else {
                Some(json!(EXCLUSION_SENTINEL))
            }
        }
        StrMethod::EndsWith => {
            let s = arg?;
            if want {
                Some(json!(format!("rest_{s}")))
            } else if s.is_empty() {
                None
            } else {
                Some(json!(EXCLUSION_SENTINEL))
            }
        }
        StrMethod::IsDigit => Some(json!(if want { "42" } else { "not42" })),
        StrMethod::IsAlpha => Some(json!(if want { "abc" } else { "abc123" })),
        StrMethod::IsUpper => Some(json!(if want { "ABC" } else { "abc" })),
        StrMethod::IsLower => Some(json!(if want { "abc" } else { "ABC" })),
        StrMethod::IsSpace => Some(json!(if want { "   " } else { "abc" })),
        StrMethod::IsAlnum => Some(json!(if want { "abc123" } else { "abc 123" })),
    }
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
        Derivation::Element { field, arity } => element_value(*field, *arity, op, literal, want),
    }
}

/// A concrete value for a derivation's underlying parameter such that the derived quantity
/// (`p` itself, `len(p)`, or `p[i]`) equals `target` exactly — the building block
/// [`synthesize_pair`] uses for the *other* side of a [`Predicate::ParamCompare`], whose own
/// value is picked by `target`'s relation to the pairing's [`CmpOp`], not by equality. `None` for
/// [`Derivation::Mod`] (no obvious single value realizes an exact target through a modulus) or a
/// negative [`Derivation::Index`].
fn value_for_target(deriv: &Derivation, shape: &Shape, target: i64) -> Option<Value> {
    match deriv {
        Derivation::Direct => Some(json!(target)),
        Derivation::Len => Some(len_value(shape, target)),
        Derivation::Index(i) => {
            if *i < 0 {
                return None;
            }
            let idx = *i as usize;
            let mut arr = vec![json!(0); idx + 1];
            arr[idx] = json!(target);
            Some(Value::Array(arr))
        }
        Derivation::Mod(_) | Derivation::Element { .. } => None,
    }
}

/// Synthesize a coordinated pair of values for [`Predicate::ParamCompare`]'s two parameters such
/// that `a <op> b` evaluates to `want`: `b` is pinned to an arbitrary reference integer, and `a`
/// is synthesized against that reference the same way [`compare_value`] synthesizes against any
/// other integer literal. `None` when either derivation is [`Derivation::Mod`] (no reference value
/// composes cleanly through a modulus on both sides), either is [`Derivation::Element`] (pairing
/// two loop elements, or an element with another parameter, isn't handled — see the module doc),
/// or either shape can't realize its side.
pub fn synthesize_pair(
    deriv_a: &Derivation,
    op: CmpOp,
    deriv_b: &Derivation,
    shape_a: &Shape,
    shape_b: &Shape,
    want: bool,
) -> Option<(Value, Value)> {
    if matches!(deriv_a, Derivation::Mod(_) | Derivation::Element { .. })
        || matches!(deriv_b, Derivation::Mod(_) | Derivation::Element { .. })
    {
        return None;
    }
    const REFERENCE: i64 = 5;
    let value_b = value_for_target(deriv_b, shape_b, REFERENCE)?;
    let value_a = compare_value(deriv_a, op, &Literal::Int(REFERENCE), shape_a, want)?;
    Some((value_a, value_b))
}

/// Synthesize a value for `pred`'s parameter (see [`Predicate::param`]) such that `pred`
/// evaluates to `want`. `shape` is that parameter's inferred shape (used to decide a
/// container/string vs. scalar candidate). `None` when the predicate's form isn't handled — see
/// the module doc for the closed set of forms this recognizes.
///
/// [`Predicate::ParamCompare`] always returns `None` here — synthesizing it means choosing values
/// for *two* parameters together, which [`synthesize_pair`] does instead.
pub fn synthesize(pred: &Predicate, want: bool, shape: &Shape) -> Option<Value> {
    match pred {
        Predicate::Not(inner) => synthesize(inner, !want, shape),
        Predicate::Truthy { .. } => Some(truthy_value(shape, want)),
        Predicate::ForIter { .. } => Some(container_value(shape, want)),
        Predicate::Membership { negated, items, .. } => membership_value(items, *negated, want),
        Predicate::ContainerMembership { negated, literal, .. } => {
            container_membership_value(literal, *negated, want, shape)
        }
        Predicate::StrMethod { method, arg, .. } => str_method_value(*method, arg.as_deref(), want),
        Predicate::Compare { deriv, op, literal, .. } => compare_value(deriv, *op, literal, shape, want),
        Predicate::ParamCompare { .. } => None,
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

    fn no_aliases() -> Aliases {
        Aliases::new()
    }

    #[test]
    fn eq_int_yields_the_literal_and_a_violator() {
        let test = test_expr_of("if p == 42:\n    pass\n");
        let preds = extract(&test, &params(&["p"]), &no_aliases());
        assert_eq!(preds.len(), 1);
        let satisfy = synthesize(&preds[0], true, &Shape::Int).expect("satisfying value");
        let violate = synthesize(&preds[0], false, &Shape::Int).expect("violating value");
        assert_eq!(satisfy, json!(42));
        assert_ne!(violate, json!(42));
    }

    #[test]
    fn len_gt_yields_a_long_and_a_short_list() {
        let test = test_expr_of("if len(p) > 3:\n    pass\n");
        let preds = extract(&test, &params(&["p"]), &no_aliases());
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
        let preds = extract(&test, &params(&["p"]), &no_aliases());
        assert_eq!(preds.len(), 1);
        let satisfy = synthesize(&preds[0], true, &Shape::Int).expect("satisfying value");
        let violate = synthesize(&preds[0], false, &Shape::Int).expect("violating value");
        assert_eq!(satisfy.as_i64().expect("int") % 2, 0);
        assert_ne!(violate.as_i64().expect("int") % 2, 0);
    }

    #[test]
    fn bare_name_test_yields_truthiness() {
        let test = test_expr_of("if p:\n    pass\n");
        let preds = extract(&test, &params(&["p"]), &no_aliases());
        assert_eq!(preds, vec![Predicate::Truthy { param: "p".to_string() }]);
        let truthy = synthesize(&preds[0], true, &Shape::Int).expect("truthy value");
        let falsy = synthesize(&preds[0], false, &Shape::Int).expect("falsy value");
        assert_ne!(truthy, json!(0));
        assert_eq!(falsy, json!(0));
    }

    #[test]
    fn and_decomposes_into_its_operands() {
        let test = test_expr_of("if p == 1 and q == 2:\n    pass\n");
        let preds = extract(&test, &params(&["p", "q"]), &no_aliases());
        assert_eq!(preds.len(), 2);
        assert!(preds.iter().any(|p| p.param() == "p"));
        assert!(preds.iter().any(|p| p.param() == "q"));
    }

    #[test]
    fn unhandled_predicate_yields_nothing() {
        let test = test_expr_of("if hash(p) == 0:\n    pass\n");
        let preds = extract(&test, &params(&["p"]), &no_aliases());
        assert!(preds.is_empty(), "hash(p) is not a handled derivation");
    }
}
