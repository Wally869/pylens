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

use super::{Aliases, BoolInits, CmpOp, Derivation, FlagPreds, LinePredicates, Literal, Predicate, StrMethod};

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

/// A string-method receiver's derivation: the parameter itself, a plain loop element, or a
/// [`Derivation::SplitElement`] — the forms [`element_value`]/`wrap_receiver_value` can wrap a
/// synthesized string value around.
pub(super) fn resolve_strmethod_receiver(
    name: &str,
    params: &[String],
    aliases: &Aliases,
) -> Option<(String, Derivation)> {
    if params.iter().any(|p| p == name) {
        return Some((name.to_string(), Derivation::Direct));
    }
    match aliases.get(name) {
        Some((param, deriv @ (Derivation::Direct | Derivation::Element { .. } | Derivation::SplitElement(_)))) => {
            Some((param.clone(), deriv.clone()))
        }
        _ => None,
    }
}

/// `p.split(sep)` / `p.split()` where `p` resolves directly to a parameter — the receiver of a
/// [`Derivation::Split`] binding.
pub(super) fn extract_split(
    expr: &ast::Expr,
    params: &[String],
    aliases: &Aliases,
) -> Option<(String, Option<String>)> {
    let ast::Expr::Call(call) = expr else { return None };
    let ast::Expr::Attribute(attr) = call.func.as_ref() else { return None };
    if attr.attr.as_str() != "split" {
        return None;
    }
    let ast::Expr::Name(recv) = attr.value.as_ref() else { return None };
    let param = resolve_param(recv.id.as_str(), params, aliases)?;
    if !call.arguments.keywords.is_empty() {
        return None;
    }
    match &*call.arguments.args {
        [] => Some((param, None)),
        [ast::Expr::StringLiteral(s)] => Some((param, Some(s.value.to_str().to_string()))),
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
    let mut bool_inits = BoolInits::new();
    let mut flag_preds = FlagPreds::new();
    walk(body, line_index, params, &mut aliases, &mut bool_inits, &mut flag_preds, &mut out);
    out
}

pub(super) fn walk(
    body: &[ast::Stmt],
    li: &LineIndex,
    params: &[String],
    aliases: &mut Aliases,
    bool_inits: &mut BoolInits,
    flag_preds: &mut FlagPreds,
    out: &mut HashMap<u32, LinePredicates>,
) {
    for stmt in body {
        match stmt {
            ast::Stmt::FunctionDef(_) | ast::Stmt::ClassDef(_) => {}
            ast::Stmt::Assign(assign) => bind_assign(assign, params, aliases, bool_inits, flag_preds),
            ast::Stmt::If(if_stmt) => {
                insert_test(out, line_at(if_stmt.range().start(), li), &if_stmt.test, params, aliases, flag_preds);
                walk(&if_stmt.body, li, params, aliases, bool_inits, flag_preds, out);
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        insert_test(out, line_at(clause.range().start(), li), test, params, aliases, flag_preds);
                    }
                    walk(&clause.body, li, params, aliases, bool_inits, flag_preds, out);
                }
            }
            ast::Stmt::While(w) => {
                insert_test(out, line_at(w.range().start(), li), &w.test, params, aliases, flag_preds);
                walk(&w.body, li, params, aliases, bool_inits, flag_preds, out);
                walk(&w.orelse, li, params, aliases, bool_inits, flag_preds, out);
            }
            ast::Stmt::For(f) => {
                let line = line_at(f.range().start(), li);
                let iter_deriv = extract_deriv(&f.iter, params, aliases);
                if let Some((param, deriv @ (Derivation::Direct | Derivation::Split(_)))) = &iter_deriv {
                    out.insert(
                        line,
                        LinePredicates::ForIter(Predicate::ForIter { param: param.clone(), deriv: deriv.clone() }),
                    );
                }
                let pre_loop = aliases.clone();
                let direct_param =
                    if let Some((param, Derivation::Direct)) = &iter_deriv { Some(param.clone()) } else { None };
                match iter_deriv {
                    Some((param, Derivation::Direct)) => bind_for_target(&f.target, &param, aliases),
                    Some((param, Derivation::Split(sep))) => bind_for_target_split(&f.target, &param, &sep, aliases),
                    None => {
                        if let Some(source) = extract_loop_source(&f.iter, params, aliases) {
                            bind_for_derived_target(&f.target, &source, aliases);
                        }
                    }
                    _ => {}
                }
                let flag_candidate = direct_param
                    .and_then(|param| detect_loop_flag(&f.body, &param, params, aliases, bool_inits));
                walk(&f.body, li, params, aliases, bool_inits, flag_preds, out);
                walk(&f.orelse, li, params, aliases, bool_inits, flag_preds, out);
                *aliases = pre_loop;
                if let Some((name, pred)) = flag_candidate {
                    flag_preds.insert(name, pred);
                }
            }
            ast::Stmt::With(w) => walk(&w.body, li, params, aliases, bool_inits, flag_preds, out),
            ast::Stmt::Try(t) => {
                walk(&t.body, li, params, aliases, bool_inits, flag_preds, out);
                for handler in &t.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    walk(&h.body, li, params, aliases, bool_inits, flag_preds, out);
                }
                walk(&t.orelse, li, params, aliases, bool_inits, flag_preds, out);
                walk(&t.finalbody, li, params, aliases, bool_inits, flag_preds, out);
            }
            ast::Stmt::Match(m) => {
                for case in &m.cases {
                    walk(&case.body, li, params, aliases, bool_inits, flag_preds, out);
                }
            }
            _ => {}
        }
    }
}

/// The one [`Derivation`] a per-element predicate carries, unwrapping [`Predicate::Not`] — `None`
/// for forms that don't name a single derivation this way (`Truthy`, `ForIter`, ...).
fn predicate_deriv(pred: &Predicate) -> Option<&Derivation> {
    match pred {
        Predicate::Compare { deriv, .. } | Predicate::StrMethod { deriv, .. } => Some(deriv),
        Predicate::Not(inner) => predicate_deriv(inner),
        _ => None,
    }
}

/// Count of `Stmt::Assign`s anywhere in `stmts` (recursively) whose sole target is `Name(name)` —
/// used to confirm a loop-state flag's only reassignment is the one
/// [`detect_loop_flag`] already found.
fn count_name_assigns(stmts: &[ast::Stmt], name: &str) -> usize {
    let mut count = 0;
    for stmt in stmts {
        match stmt {
            ast::Stmt::Assign(a) => {
                if let [ast::Expr::Name(n)] = a.targets.as_slice()
                    && n.id.as_str() == name
                {
                    count += 1;
                }
            }
            ast::Stmt::If(i) => {
                count += count_name_assigns(&i.body, name);
                for clause in &i.elif_else_clauses {
                    count += count_name_assigns(&clause.body, name);
                }
            }
            ast::Stmt::While(w) => {
                count += count_name_assigns(&w.body, name) + count_name_assigns(&w.orelse, name);
            }
            ast::Stmt::For(f) => {
                count += count_name_assigns(&f.body, name) + count_name_assigns(&f.orelse, name);
            }
            ast::Stmt::With(w) => count += count_name_assigns(&w.body, name),
            ast::Stmt::Try(t) => {
                count += count_name_assigns(&t.body, name);
                for handler in &t.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    count += count_name_assigns(&h.body, name);
                }
                count += count_name_assigns(&t.orelse, name) + count_name_assigns(&t.finalbody, name);
            }
            ast::Stmt::Match(m) => {
                for case in &m.cases {
                    count += count_name_assigns(&case.body, name);
                }
            }
            _ => {}
        }
    }
    count
}

/// Recognize the loop-state-flag special case in one `for` loop's body: a name previously
/// initialized to a literal bool (`bool_inits`), reassigned to the opposite literal by exactly
/// one top-level `if <handled per-element predicate>: flag = <opposite>` (no `elif`/`else`), and
/// touched nowhere else in the loop body. `aliases` must already carry the loop target's
/// [`Derivation::Element`] binding. Returns the flag's name and the predicate that fully explains
/// its value after the loop: `flag == <init>` iff the guard never held, so the guard's
/// [`Predicate::Not`] when the flag was initialized `true` (post-loop `true` then means "the
/// guard never held"), or the guard itself when initialized `false`.
fn detect_loop_flag(
    for_body: &[ast::Stmt],
    param: &str,
    params: &[String],
    aliases: &Aliases,
    bool_inits: &BoolInits,
) -> Option<(String, Predicate)> {
    let mut found: Option<(String, Predicate, bool)> = None;
    for stmt in for_body {
        let ast::Stmt::If(if_stmt) = stmt else { continue };
        if !if_stmt.elif_else_clauses.is_empty() {
            continue;
        }
        let [ast::Stmt::Assign(assign)] = if_stmt.body.as_slice() else { continue };
        let [ast::Expr::Name(target)] = assign.targets.as_slice() else { continue };
        let ast::Expr::BooleanLiteral(lit) = assign.value.as_ref() else { continue };
        let Some(&init) = bool_inits.get(target.id.as_str()) else { continue };
        if lit.value == init {
            continue;
        }
        let mut preds = extract(&if_stmt.test, params, aliases, &FlagPreds::new());
        if preds.len() != 1 {
            continue;
        }
        let pred = preds.remove(0);
        if pred.param() != param || !matches!(predicate_deriv(&pred), Some(Derivation::Element { .. })) {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some((target.id.to_string(), pred, init));
    }
    let (name, pred, init) = found?;
    if count_name_assigns(for_body, &name) != 1 {
        return None;
    }
    Some((name, if init { Predicate::Not(Box::new(pred)) } else { pred }))
}

/// Bind a for loop's target name(s) as Derivation::Element(s) of param.
pub(super) fn bind_for_target(target: &ast::Expr, param: &str, aliases: &mut Aliases) {
    match target {
        ast::Expr::Name(n) => {
            aliases.insert(
                n.id.to_string(),
                (param.to_string(), Derivation::Element { field: None, arity: 1, leading: 0 }),
            );
        }
        ast::Expr::Tuple(t) if !t.elts.is_empty() && t.elts.iter().all(|e| matches!(e, ast::Expr::Name(_))) => {
            let arity = t.elts.len();
            for (i, elt) in t.elts.iter().enumerate() {
                let ast::Expr::Name(n) = elt else { unreachable!("checked all Name above") };
                aliases.insert(
                    n.id.to_string(),
                    (param.to_string(), Derivation::Element { field: Some(i), arity, leading: 0 }),
                );
            }
        }
        _ => {}
    }
}

/// A parameter-derived iterable recognized as a loop source beyond [`extract_deriv`]'s plain-alias
/// forms: a literal-bounded slice of a parameter, or a name-preserving wrapper call around one.
/// Binds only the loop target's per-element derivation — no [`Predicate::ForIter`], since the
/// container itself isn't what a branch test compares against here.
enum LoopSource {
    /// `p[start:end]`: `start`/`end` literal non-negative ints or omitted, no `step`, and
    /// `start < end` when both are given. `leading` is `start` (0 when omitted).
    Sliced { param: String, leading: usize },
    /// `enumerate(p)` / `enumerate(p, start)` (`start` a literal int) — the index name stays
    /// unbound; only the element name binds.
    Enumerate { param: String },
    /// `reversed(p)` / `sorted(p)` — order doesn't matter for a single synthesized element.
    Wrapped { param: String },
}

/// Recognize one of [`LoopSource`]'s forms in a `for ... in <iter>:` header. `None` for anything
/// else (zip, dict methods, a nested combination like `reversed(p[1:])`), which stays unbound.
fn extract_loop_source(iter: &ast::Expr, params: &[String], aliases: &Aliases) -> Option<LoopSource> {
    match iter {
        ast::Expr::Subscript(sub) => {
            let ast::Expr::Slice(slice) = sub.slice.as_ref() else { return None };
            if slice.step.is_some() {
                return None;
            }
            let ast::Expr::Name(n) = sub.value.as_ref() else { return None };
            let param = resolve_param(n.id.as_str(), params, aliases)?;
            let start = match slice.lower.as_deref() {
                None => 0,
                Some(e) => literal_int(e).filter(|v| *v >= 0)?,
            };
            if let Some(e) = slice.upper.as_deref() {
                let end = literal_int(e).filter(|v| *v >= 0)?;
                if start >= end {
                    return None;
                }
            }
            Some(LoopSource::Sliced { param, leading: start as usize })
        }
        ast::Expr::Call(call) => {
            if !call.arguments.keywords.is_empty() {
                return None;
            }
            let ast::Expr::Name(fname) = call.func.as_ref() else { return None };
            match fname.id.as_str() {
                "enumerate" => match &*call.arguments.args {
                    [ast::Expr::Name(recv)] => {
                        Some(LoopSource::Enumerate { param: resolve_param(recv.id.as_str(), params, aliases)? })
                    }
                    [ast::Expr::Name(recv), start] => {
                        literal_int(start)?;
                        Some(LoopSource::Enumerate { param: resolve_param(recv.id.as_str(), params, aliases)? })
                    }
                    _ => None,
                },
                "reversed" | "sorted" => {
                    let [ast::Expr::Name(recv)] = &*call.arguments.args else { return None };
                    Some(LoopSource::Wrapped { param: resolve_param(recv.id.as_str(), params, aliases)? })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Bind a for loop's target to a [`LoopSource`]'s element — a plain `Name` target for `Sliced`
/// and `Wrapped`, the second name of a two-name `Tuple` target (the first, the index, stays
/// unbound) for `Enumerate`. Any other target shape stays unbound.
fn bind_for_derived_target(target: &ast::Expr, source: &LoopSource, aliases: &mut Aliases) {
    match source {
        LoopSource::Sliced { param, leading } => {
            if let ast::Expr::Name(n) = target {
                aliases.insert(
                    n.id.to_string(),
                    (param.clone(), Derivation::Element { field: None, arity: 1, leading: *leading }),
                );
            }
        }
        LoopSource::Enumerate { param } => {
            if let ast::Expr::Tuple(t) = target
                && let [ast::Expr::Name(_idx), ast::Expr::Name(elem)] = t.elts.as_slice()
            {
                aliases.insert(
                    elem.id.to_string(),
                    (param.clone(), Derivation::Element { field: None, arity: 1, leading: 0 }),
                );
            }
        }
        LoopSource::Wrapped { param } => {
            if let ast::Expr::Name(n) = target {
                aliases.insert(
                    n.id.to_string(),
                    (param.clone(), Derivation::Element { field: None, arity: 1, leading: 0 }),
                );
            }
        }
    }
}

/// Bind a for loop's target name as Derivation::SplitElement of param — the `part` in
/// `for part in parts:` where `parts` is a [`Derivation::Split`]-bound local. Tuple unpacking of
/// a string element isn't a recognized form, so only a plain `Name` target binds.
pub(super) fn bind_for_target_split(target: &ast::Expr, param: &str, sep: &Option<String>, aliases: &mut Aliases) {
    if let ast::Expr::Name(n) = target {
        aliases.insert(n.id.to_string(), (param.to_string(), Derivation::SplitElement(sep.clone())));
    }
}

/// Bind (or clear) aliases for one Assign statement, and track/invalidate loop-state-flag
/// bookkeeping (`bool_inits`, `flag_preds`) alongside it: a literal-bool RHS records the name as
/// a fresh flag candidate; any other assignment to a name clears it (see `detect_loop_flag`,
/// which relies on a name touched exactly once inside its qualifying loop staying a candidate).
pub(super) fn bind_assign(
    assign: &ast::StmtAssign,
    params: &[String],
    aliases: &mut Aliases,
    bool_inits: &mut BoolInits,
    flag_preds: &mut FlagPreds,
) {
    match assign.targets.as_slice() {
        [ast::Expr::Name(target)] => {
            if let ast::Expr::BooleanLiteral(lit) = assign.value.as_ref() {
                aliases.remove(target.id.as_str());
                bool_inits.insert(target.id.to_string(), lit.value);
                flag_preds.remove(target.id.as_str());
                return;
            }
            let deriv = extract_split(&assign.value, params, aliases)
                .map(|(param, sep)| (param, Derivation::Split(sep)))
                .or_else(|| extract_deriv(&assign.value, params, aliases));
            match deriv {
                Some(deriv) => {
                    aliases.insert(target.id.to_string(), deriv);
                }
                None => {
                    aliases.remove(target.id.as_str());
                }
            }
            bool_inits.remove(target.id.as_str());
            flag_preds.remove(target.id.as_str());
        }
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
                Some((param, Derivation::Element { field: None, arity: 1, leading })) => {
                    let arity = names.len();
                    for (i, name) in names.iter().enumerate() {
                        aliases.insert(
                            name.to_string(),
                            (param.clone(), Derivation::Element { field: Some(i), arity, leading }),
                        );
                    }
                }
                _ => {
                    for name in &names {
                        aliases.remove(*name);
                    }
                }
            }
            for name in names {
                bool_inits.remove(name);
                flag_preds.remove(name);
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
    flag_preds: &FlagPreds,
) {
    let preds = extract(test, params, aliases, flag_preds);
    if !preds.is_empty() {
        out.insert(line, LinePredicates::Test(preds));
    }
}

/// Decompose expr into every handled leaf predicate.
pub(super) fn extract(expr: &ast::Expr, params: &[String], aliases: &Aliases, flag_preds: &FlagPreds) -> Vec<Predicate> {
    match expr {
        ast::Expr::BoolOp(b) => b.values.iter().flat_map(|v| extract(v, params, aliases, flag_preds)).collect(),
        ast::Expr::UnaryOp(u) if matches!(u.op, ast::UnaryOp::Not) => extract(&u.operand, params, aliases, flag_preds)
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
        ast::Expr::Name(n) => match flag_preds.get(n.id.as_str()) {
            Some(pred) => vec![pred.clone()],
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

/// p.method(...) where method is one of StrMethod's recognized forms and p resolves to a
/// parameter, a plain loop element of one, or a split-of-parameter loop element (through
/// resolve_strmethod_receiver) -- no subscript/attribute-chained receiver.
pub(super) fn extract_call(call: &ast::ExprCall, params: &[String], aliases: &Aliases) -> Option<Predicate> {
    let ast::Expr::Attribute(attr) = call.func.as_ref() else { return None };
    let ast::Expr::Name(recv) = attr.value.as_ref() else { return None };
    let (param, deriv) = resolve_strmethod_receiver(recv.id.as_str(), params, aliases)?;
    let method = StrMethod::parse(attr.attr.as_str())?;
    if !call.arguments.keywords.is_empty() {
        return None;
    }
    if method.takes_str_arg() {
        if call.arguments.args.len() != 1 {
            return None;
        }
        let ast::Expr::StringLiteral(s) = &call.arguments.args[0] else { return None };
        Some(Predicate::StrMethod { param, deriv, method, arg: Some(s.value.to_str().to_string()) })
    } else {
        if !call.arguments.args.is_empty() {
            return None;
        }
        Some(Predicate::StrMethod { param, deriv, method, arg: None })
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
            if params.iter().any(|p| p == argn.id.as_str()) {
                return Some((argn.id.to_string(), Derivation::Len));
            }
            match aliases.get(argn.id.as_str()) {
                Some((param, Derivation::Direct)) => Some((param.clone(), Derivation::Len)),
                Some((param, Derivation::Split(sep))) => Some((param.clone(), Derivation::SplitLen(sep.clone()))),
                Some((param, Derivation::SplitElement(sep))) => {
                    Some((param.clone(), Derivation::SplitElementLen(sep.clone())))
                }
                _ => None,
            }
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
