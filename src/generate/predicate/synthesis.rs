//! Synthesizes concrete values that satisfy or violate an extracted [`super::Predicate`]: the
//! per-literal builders (compare_value, element_value, index_value, mod_value, len_value,
//! str_method_value, container_membership_value and their siblings) and the two entry points,
//! synthesize (one predicate, one parameter) and synthesize_pair (a ParamCompare's two
//! parameters together).

use serde_json::{Value, json};

use crate::model::Shape;

use super::super::seeds;
use super::{CmpOp, Derivation, EXCLUSION_SENTINEL, Literal, Predicate, StrMethod};

fn str_eq_ne(op: CmpOp, s: &str, want: bool) -> Option<Value> {
    match op {
        CmpOp::Eq => Some(json!(if want { s.to_string() } else { format!("{s}_") })),
        CmpOp::Ne => Some(json!(if want { format!("{s}_") } else { s.to_string() })),
        _ => None,
    }
}

/// A string that `.split(sep)` (or `.split()` when `sep` is `None`) yields exactly `count` parts
/// for -- `count` placeholder parts joined by `sep` (or by a single space for whitespace-split).
/// `count == 0` is only realizable for whitespace split (an empty/blank string); an explicit
/// separator's `.split(sep)` always yields at least one part.
fn split_join_value(sep: &Option<String>, count: i64) -> Option<Value> {
    if count < 0 {
        return None;
    }
    let n = count as usize;
    match sep {
        Some(s) => {
            if s.is_empty() || n == 0 {
                return None;
            }
            Some(json!(vec!["a"; n].join(s.as_str())))
        }
        None => {
            if n == 0 {
                return Some(json!("   "));
            }
            Some(json!(vec!["a"; n].join(" ")))
        }
    }
}

/// Wrap a synthesized string method value (the receiver's own value) around the underlying
/// parameter, per `deriv`: `Direct` returns it as-is, `Element` matches [`element_value`]'s
/// one-element-list convention, `SplitElement` returns the value itself as the whole parameter
/// string (so splitting it back out yields exactly this one part) -- `None` if the value would
/// contain the separator, which would produce more than one part.
fn wrap_receiver_value(deriv: &Derivation, elem: Value) -> Option<Value> {
    match deriv {
        Derivation::Direct => Some(elem),
        Derivation::Element { field, arity } => {
            let item = if *arity <= 1 {
                elem
            } else {
                let field = (*field)?;
                if field >= *arity {
                    return None;
                }
                let mut fields = vec![json!(0); *arity];
                fields[field] = elem;
                json!({ "__t__": "tuple", "items": fields })
            };
            Some(Value::Array(vec![item]))
        }
        Derivation::SplitElement(sep) => {
            let s = elem.as_str()?;
            if let Some(sep) = sep
                && s.contains(sep.as_str())
            {
                return None;
            }
            Some(elem)
        }
        _ => None,
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
        // A comparison directly against the split list, or a bare split element, isn't a
        // recognized extracted form (extraction only reaches `Compare` through `len()`, which
        // yields `SplitLen`/`SplitElementLen`) -- only `Truthy`/`ForIter` target `Split`, and only
        // `StrMethod` targets `SplitElement`, at the top level.
        Derivation::Split(_) | Derivation::SplitElement(_) => None,
        Derivation::SplitLen(sep) => {
            let Literal::Int(c) = literal else { return None };
            split_join_value(sep, synth_int(op, *c, want))
        }
        Derivation::SplitElementLen(sep) => {
            let Literal::Int(c) = literal else { return None };
            let n = synth_int(op, *c, want).max(0) as usize;
            let filler = "x".repeat(n);
            match sep {
                Some(s) if !s.is_empty() && filler.contains(s.as_str()) => None,
                _ => Some(json!(filler)),
            }
        }
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

/// A candidate integer p (or derived integer) such that p op c evaluates to want.
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

fn literal_to_value(l: &Literal) -> Value {
    match l {
        Literal::Int(i) => json!(*i),
        Literal::Str(s) => json!(s.clone()),
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

/// A one-element list value for a Derivation::Element: arity <= 1 (a plain, un-unpacked
/// loop target) wraps the field's own value directly; arity > 1 wraps a tagged tuple with
/// field's slot set to the field value and every other slot filled with a neutral 0. Always
/// non-empty by construction.
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

fn membership_value(items: &[Literal], negated: bool, want: bool) -> Option<Value> {
    if items.is_empty() {
        return None;
    }
    let want_in = if negated { !want } else { want };
    if want_in { Some(literal_to_value(&items[0])) } else { not_in_value(items) }
}

/// A value for v in p / v not in p's parameter p (the container) such that the whole
/// expression evaluates to want. None when literal is a string and empty.
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

/// A concrete value for a derivation's underlying parameter such that the derived quantity
/// (p itself, len(p), or p[i]) equals target exactly -- the building block synthesize_pair
/// uses for the other side of a Predicate::ParamCompare, whose own value is picked by
/// target's relation to the pairing's CmpOp, not by equality. None for Derivation::Mod
/// (no obvious single value realizes an exact target through a modulus) or a negative
/// Derivation::Index.
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
        Derivation::Mod(_)
        | Derivation::Element { .. }
        | Derivation::Split(_)
        | Derivation::SplitLen(_)
        | Derivation::SplitElement(_)
        | Derivation::SplitElementLen(_) => None,
    }
}

/// Synthesize a coordinated pair of values for Predicate::ParamCompare's two parameters such
/// that a <op> b evaluates to want: b is pinned to an arbitrary reference integer, and a
/// is synthesized against that reference the same way compare_value synthesizes against any
/// other integer literal. None when either derivation is Derivation::Mod (no reference value
/// composes cleanly through a modulus on both sides), either is Derivation::Element (pairing
/// two loop elements, or an element with another parameter, isn't handled), or either shape
/// can't realize its side.
pub fn synthesize_pair(
    deriv_a: &Derivation,
    op: CmpOp,
    deriv_b: &Derivation,
    shape_a: &Shape,
    shape_b: &Shape,
    want: bool,
) -> Option<(Value, Value)> {
    let unsupported = |d: &Derivation| {
        matches!(
            d,
            Derivation::Mod(_)
                | Derivation::Element { .. }
                | Derivation::Split(_)
                | Derivation::SplitLen(_)
                | Derivation::SplitElement(_)
                | Derivation::SplitElementLen(_)
        )
    };
    if unsupported(deriv_a) || unsupported(deriv_b) {
        return None;
    }
    const REFERENCE: i64 = 5;
    let value_b = value_for_target(deriv_b, shape_b, REFERENCE)?;
    let value_a = compare_value(deriv_a, op, &Literal::Int(REFERENCE), shape_a, want)?;
    Some((value_a, value_b))
}

/// Synthesize a value for pred's parameter (see Predicate::param) such that pred
/// evaluates to want. shape is that parameter's inferred shape (used to decide a
/// container/string vs. scalar candidate). None when the predicate's form isn't handled.
///
/// Predicate::ParamCompare always returns None here -- synthesizing it means choosing values
/// for two parameters together, which synthesize_pair does instead.
pub fn synthesize(pred: &Predicate, want: bool, shape: &Shape) -> Option<Value> {
    match pred {
        Predicate::Not(inner) => synthesize(inner, !want, shape),
        Predicate::Truthy { .. } => Some(truthy_value(shape, want)),
        Predicate::ForIter { deriv, .. } => match deriv {
            Derivation::Split(sep) => split_join_value(sep, if want { 1 } else { 0 }),
            _ => Some(container_value(shape, want)),
        },
        Predicate::Membership { negated, items, .. } => membership_value(items, *negated, want),
        Predicate::ContainerMembership { negated, literal, .. } => {
            container_membership_value(literal, *negated, want, shape)
        }
        Predicate::StrMethod { deriv, method, arg, .. } => {
            let elem = str_method_value(*method, arg.as_deref(), want)?;
            wrap_receiver_value(deriv, elem)
        }
        Predicate::Compare { deriv, op, literal, .. } => compare_value(deriv, *op, literal, shape, want),
        Predicate::ParamCompare { .. } => None,
    }
}

/// The shape used to pick a synthesized value's structural kind: a Union/Instance, which
/// synthesis has no concrete kind for, falls back to a generic spread.
fn effective_shape(shape: &Shape) -> Shape {
    match shape {
        Shape::Union(_) | Shape::Instance(_) => Shape::Any,
        other => other.clone(),
    }
}
