//! Loop-source and loop-target binding: recognizes the parameter-derived iterables `for` can
//! walk beyond a plain alias (slices, `enumerate`, `reversed`/`sorted`, `str.split`), binds the
//! loop target's per-element derivation into `Aliases`, and detects the loop-state-flag pattern.
//! Feeds [`super::extraction::walk`].

use ruff_python_ast as ast;

use super::{Aliases, BoolInits, Derivation, FlagPreds, Predicate};
use super::extraction::{extract, extract_deriv, literal_int, resolve_param};

/// A parameter-derived iterable recognized as a loop source beyond [`extract_deriv`]'s plain-alias
/// forms: a literal-bounded slice of a parameter, or a name-preserving wrapper call around one.
/// Binds only the loop target's per-element derivation -- no [`Predicate::ForIter`], since the
/// container itself isn't what a branch test compares against here.
pub(super) enum LoopSource {
    /// `p[start:end]`: `start`/`end` literal non-negative ints or omitted, no `step`, and
    /// `start < end` when both are given. `leading` is `start` (0 when omitted).
    Sliced { param: String, leading: usize },
    /// `enumerate(p)` / `enumerate(p, start)` (`start` a literal int) -- the index name stays
    /// unbound; only the element name binds.
    Enumerate { param: String },
    /// `reversed(p)` / `sorted(p)` -- order doesn't matter for a single synthesized element.
    Wrapped { param: String },
}

/// `p.split(sep)` / `p.split()` where `p` resolves directly to a parameter -- the receiver of a
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

/// The one [`Derivation`] a per-element predicate carries, unwrapping [`Predicate::Not`] -- `None`
/// for forms that don't name a single derivation this way (`Truthy`, `ForIter`, ...).
fn predicate_deriv(pred: &Predicate) -> Option<&Derivation> {
    match pred {
        Predicate::Compare { deriv, .. } | Predicate::StrMethod { deriv, .. } => Some(deriv),
        Predicate::Not(inner) => predicate_deriv(inner),
        _ => None,
    }
}

/// Count of `Stmt::Assign`s anywhere in `stmts` (recursively) whose sole target is `Name(name)` --
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
pub(super) fn detect_loop_flag(
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

/// Recognize one of [`LoopSource`]'s forms in a `for ... in <iter>:` header. `None` for anything
/// else (zip, dict methods, a nested combination like `reversed(p[1:])`), which stays unbound.
pub(super) fn extract_loop_source(iter: &ast::Expr, params: &[String], aliases: &Aliases) -> Option<LoopSource> {
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

/// Bind a for loop's target to a [`LoopSource`]'s element -- a plain `Name` target for `Sliced`
/// and `Wrapped`, the second name of a two-name `Tuple` target (the first, the index, stays
/// unbound) for `Enumerate`. Any other target shape stays unbound.
pub(super) fn bind_for_derived_target(target: &ast::Expr, source: &LoopSource, aliases: &mut Aliases) {
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

/// Bind a for loop's target name as Derivation::SplitElement of param -- the `part` in
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
