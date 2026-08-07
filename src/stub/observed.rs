//! Folds dynamically **observed** types (from a `record` run's executed [`Case`]s) into `.pyi`
//! rendering wherever the static analysis left a param/return unresolved (`Shape::Any` /
//! `ReturnKind::Opaque`). Purely additive: a static-known shape or return kind always wins, and a
//! folded-in observed type is marked with a trailing `# observed` comment on the `def` line — a
//! sample from generated cases, never a proof.
//!
//! Only JSON encodings the worker's tagged `serialize()` makes faithfully distinguishable are
//! folded in (see `python/worker.py`): a bare JSON array is `list`; `{"__t__": "tuple", ...}` is
//! `tuple`; `{"__t__": "set", ...}` is `set`; `{"__t__": "dict", ...}` or a plain JSON object is
//! `dict`; null/bool/string are `None`/`bool`/`str`; a JSON number is `int` or `float` depending
//! on whether it round-tripped with a fractional representation. `bytes` and arbitrary Python
//! objects both fall back to the worker's generic `{"__t__": "obj", ...}` encoding and are NOT
//! distinguishable from each other — never folded in.

use std::collections::HashMap;

use serde_json::Value;

use crate::generate::positional_params;
use crate::model::{EffectSignature, ParamKind};
use crate::record::{Case, FunctionRecord};

use super::{ObservedTypes, render_stub_generic};

/// The Python type name of a JSON value in the worker's tagged encoding, if — and only if — it's
/// faithfully distinguishable from that encoding alone. See the module doc.
fn json_type_tag(v: &Value) -> Option<&'static str> {
    match v {
        Value::Null => Some("None"),
        Value::Bool(_) => Some("bool"),
        Value::Number(n) => Some(if n.is_i64() || n.is_u64() { "int" } else { "float" }),
        Value::String(_) => Some("str"),
        Value::Array(_) => Some("list"),
        Value::Object(m) => match m.get("__t__").and_then(Value::as_str) {
            Some("tuple") => Some("tuple"),
            Some("set") => Some("set"),
            Some("dict") => Some("dict"),
            None => Some("dict"), // a plain object is the worker's str-keyed dict encoding
            _ => None,             // "obj" (bytes, custom instances, ...) — not distinguishable
        },
    }
}

/// The single type every value in `values` agrees on, if any — `None` if the set is empty, any
/// member's type isn't faithfully distinguishable, or two members disagree. Conservative: no
/// partial credit, no majority vote.
fn agreed_tag<'v>(values: impl Iterator<Item = &'v Value>) -> Option<&'static str> {
    let mut agreed: Option<&'static str> = None;
    for v in values {
        let tag = json_type_tag(v)?;
        match agreed {
            None => agreed = Some(tag),
            Some(prev) if prev == tag => {}
            _ => return None,
        }
    }
    agreed
}

/// The observed return type across `cases`' `returned` outcomes, if they all agree.
fn observed_return_type(cases: &[Case]) -> Option<String> {
    agreed_tag(
        cases
            .iter()
            .filter(|c| c.outcome == "returned")
            .filter_map(|c| c.ret.as_ref()),
    )
    .map(str::to_string)
}

/// The observed type of each positional/keyword-only param across `cases`' passed arguments, for
/// every param that has at least one observation and all observations agree.
fn observed_param_types(sig: &EffectSignature, cases: &[Case]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (i, p) in positional_params(sig).into_iter().enumerate() {
        if let Some(tag) = agreed_tag(cases.iter().filter_map(|c| c.input.get(i))) {
            out.insert(p.name.clone(), tag.to_string());
        }
    }
    for p in sig.params.iter().filter(|p| p.kind == ParamKind::KeywordOnly) {
        if let Some(tag) = agreed_tag(cases.iter().filter_map(|c| c.kwargs.get(&p.name))) {
            out.insert(p.name.clone(), tag.to_string());
        }
    }
    out
}

/// Render a full `.pyi` module the same way [`super::render_stub`] does, but for every
/// function/method with recorded cases, an unresolved (`Any`/`Opaque`) static param/return is
/// enriched with its observed type — see the module doc.
pub fn render_record_stub(functions: &[FunctionRecord]) -> String {
    let sigs: Vec<&EffectSignature> = functions.iter().map(|f| &f.signature).collect();
    let cases_by_key: HashMap<(&str, Option<&str>), &[Case]> = functions
        .iter()
        .map(|f| ((f.signature.name.as_str(), f.signature.owner.as_deref()), f.cases.as_slice()))
        .collect();

    render_stub_generic(&sigs, |sig| {
        let key = (sig.name.as_str(), sig.owner.as_deref());
        let Some(cases) = cases_by_key.get(&key) else {
            return ObservedTypes::default();
        };
        ObservedTypes {
            params: observed_param_types(sig, cases),
            ret: observed_return_type(cases),
        }
    })
}

