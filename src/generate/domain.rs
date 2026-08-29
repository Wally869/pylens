//! `ValueDomain`: a declared restriction on the values [`super::gen_inputs`] and
//! [`crate::shrink::shrink_case`] may produce, parsed once from a `--value-domain` JSON profile
//! file. A consumer whose downstream can only represent certain values (e.g. JSON-safe scalars
//! and lists) uses this to stop generation from ever proposing a dict, a set, or an oversized
//! value, instead of discovering the mismatch as a wasted wrong-type raise.
//!
//! Enforcement is a filter on candidate *values*, not on shapes: every candidate produced by any
//! corpus in `seeds`, plus the guard/hint/default injections in `super::candidates_for`, is
//! checked against the domain before it can be sampled. This can only narrow what generation
//! tries — it never changes the analyzer's may-set, so soundness is unaffected.

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use super::{Candidate, base, filler};

/// One value kind a `--value-domain` profile can name. `List` is only meaningful for
/// `list_elements` (nesting) and, implicitly, at the top level (see [`ValueDomain::kind_allowed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Int,
    Float,
    Bool,
    Str,
    None,
    List,
    Tuple,
}

impl Kind {
    fn parse(s: &str) -> Result<Kind, String> {
        match s {
            "int" => Ok(Kind::Int),
            "float" => Ok(Kind::Float),
            "bool" => Ok(Kind::Bool),
            "str" => Ok(Kind::Str),
            "none" => Ok(Kind::None),
            "list" => Ok(Kind::List),
            "tuple" => Ok(Kind::Tuple),
            other => Err(format!(
                "value-domain profile: unknown kind {other:?} (expected one of int, float, bool, str, none, list, tuple)"
            )),
        }
    }

    fn literal(self) -> Value {
        match self {
            Kind::Int => json!(1),
            Kind::Float => json!(1.5),
            Kind::Bool => json!(true),
            Kind::Str => json!("x"),
            Kind::None => Value::Null,
            Kind::List => json!([]),
            Kind::Tuple => json!({"__t__": "tuple", "items": [1, "x"]}),
        }
    }
}

/// Every kind classifiable outside a list wrapper — used to build [`ValueDomain::fallback_candidates`].
const SCALAR_KINDS: [Kind; 5] = [Kind::Int, Kind::Float, Kind::Bool, Kind::Str, Kind::None];

const FIELDS: [&str; 5] =
    ["scalars", "list_elements", "max_list_len", "max_str_len", "max_list_depth"];

/// A parsed `--value-domain` profile. Every field is optional; an absent field leaves that
/// dimension unrestricted (see the module doc). Dicts, sets and bytes have no kind string at
/// all — they are unnameable in `scalars`/`list_elements` and so are always rejected once a
/// domain exists, per the spec's "excluded whenever a domain is given and not listed". Tuples are
/// nameable via `"tuple"`; a tagged tuple is allowed only when named at its nesting position and
/// each of its items is allowed there too (same depth/length caps as a list).
#[derive(Debug, Clone, Default)]
pub struct ValueDomain {
    scalars: Option<HashSet<Kind>>,
    list_elements: Option<HashSet<Kind>>,
    max_list_len: Option<usize>,
    max_str_len: Option<usize>,
    max_list_depth: Option<usize>,
}

impl ValueDomain {
    /// Parse a profile from JSON text. Unknown top-level fields and unknown kind strings are
    /// errors — a typo must fail fast rather than silently generate an unrestricted domain.
    pub fn parse(text: &str) -> Result<ValueDomain, String> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| format!("value-domain profile: {e}"))?;
        let obj = value
            .as_object()
            .ok_or_else(|| "value-domain profile: expected a JSON object".to_string())?;
        for key in obj.keys() {
            if !FIELDS.contains(&key.as_str()) {
                return Err(format!(
                    "value-domain profile: unknown field {key:?} (expected one of {})",
                    FIELDS.join(", ")
                ));
            }
        }
        Ok(ValueDomain {
            scalars: parse_kind_set(obj, "scalars")?,
            list_elements: parse_kind_set(obj, "list_elements")?,
            max_list_len: parse_usize(obj, "max_list_len")?,
            max_str_len: parse_usize(obj, "max_str_len")?,
            max_list_depth: parse_usize(obj, "max_list_depth")?,
        })
    }

    /// Whether `value` (a JSON value in the wire encoding the worker deserializes — see
    /// `seeds`'s tagged `set`/`dict`/`float` encodings) is admissible under this domain.
    pub fn allows(&self, value: &Value) -> bool {
        self.allows_at(value, 0)
    }

    /// `list_depth` counts how many list levels enclose `value` — 0 at the top level, so
    /// `scalars` governs top-level kinds and `list_elements` governs everything inside a list.
    fn allows_at(&self, value: &Value, list_depth: usize) -> bool {
        if let Value::Array(items) = value {
            let this_depth = list_depth + 1;
            if let Some(max_depth) = self.max_list_depth
                && this_depth > max_depth
            {
                return false;
            }
            if !self.kind_allowed(Kind::List, list_depth) {
                return false;
            }
            if let Some(max_len) = self.max_list_len
                && items.len() > max_len
            {
                return false;
            }
            return items.iter().all(|item| self.allows_at(item, this_depth));
        }
        if let Value::Object(map) = value
            && map.get("__t__").and_then(Value::as_str) == Some("tuple")
        {
            let Some(items) = map.get("items").and_then(Value::as_array) else {
                return false;
            };
            let this_depth = list_depth + 1;
            if let Some(max_depth) = self.max_list_depth
                && this_depth > max_depth
            {
                return false;
            }
            if !self.kind_allowed(Kind::Tuple, list_depth) {
                return false;
            }
            if let Some(max_len) = self.max_list_len
                && items.len() > max_len
            {
                return false;
            }
            return items.iter().all(|item| self.allows_at(item, this_depth));
        }
        if let Value::String(s) = value
            && let Some(max) = self.max_str_len
            && s.chars().count() > max
        {
            return false;
        }
        match scalar_kind_of(value) {
            Some(kind) => self.kind_allowed(kind, list_depth),
            // A dict, set, or other tagged encoding (`__t__` values other than "float"/"tuple") —
            // unnameable in a profile, so always rejected once a domain is active.
            None => false,
        }
    }

    fn kind_allowed(&self, kind: Kind, list_depth: usize) -> bool {
        if list_depth == 0 {
            if kind == Kind::List {
                return self.scalars.as_ref().is_some_and(|s| s.contains(&Kind::List))
                    || self.list_elements.is_some()
                    || self.scalars.is_none();
            }
            return self.scalars.as_ref().is_none_or(|s| s.contains(&kind));
        }
        self.list_elements.as_ref().is_none_or(|s| s.contains(&kind))
    }

    /// Shape-independent, domain-compliant candidates: used when a parameter's shape-derived
    /// candidates are entirely filtered out by this domain (e.g. a `Dict`-shaped parameter under
    /// a scalars-only domain), so [`super::gen_inputs`] never indexes an empty candidate list.
    /// One literal per allowed scalar kind, plus `[]` if lists are allowed at the top level.
    pub(super) fn fallback_candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        for kind in SCALAR_KINDS {
            if self.kind_allowed(kind, 0) {
                out.push(kind.literal());
            }
        }
        if self.kind_allowed(Kind::List, 0) {
            out.push(Value::Array(Vec::new()));
        }
        if self.kind_allowed(Kind::Tuple, 0) {
            out.push(Kind::Tuple.literal());
        }
        out.into_iter()
            .enumerate()
            .map(|(i, v)| if i == 0 { base(v) } else { filler(v) })
            .collect()
    }
}

/// The `__t__`-tagged `{"v": "nan"|"inf"|"-inf"}` encoding (see `seeds::nan_val`/`inf_val`) is
/// the only non-`float`-typed JSON representation of a `Float` shape's value, so it classifies
/// as `Float` here; every other tagged object (`set`, `dict`, ...) is a container, not a scalar.
fn scalar_kind_of(v: &Value) -> Option<Kind> {
    match v {
        Value::Null => Some(Kind::None),
        Value::Bool(_) => Some(Kind::Bool),
        Value::Number(n) => Some(if n.is_i64() || n.is_u64() { Kind::Int } else { Kind::Float }),
        Value::String(_) => Some(Kind::Str),
        Value::Object(m) if m.get("__t__").and_then(Value::as_str) == Some("float") => {
            Some(Kind::Float)
        }
        Value::Object(_) => None,
        Value::Array(_) => None,
    }
}

fn parse_kind_set(obj: &Map<String, Value>, field: &str) -> Result<Option<HashSet<Kind>>, String> {
    let Some(v) = obj.get(field) else { return Ok(None) };
    let arr = v
        .as_array()
        .ok_or_else(|| format!("value-domain profile: {field:?} must be an array of kind strings"))?;
    let mut set = HashSet::new();
    for item in arr {
        let s = item
            .as_str()
            .ok_or_else(|| format!("value-domain profile: {field:?} entries must be strings"))?;
        set.insert(Kind::parse(s)?);
    }
    Ok(Some(set))
}

fn parse_usize(obj: &Map<String, Value>, field: &str) -> Result<Option<usize>, String> {
    let Some(v) = obj.get(field) else { return Ok(None) };
    let n = v
        .as_u64()
        .ok_or_else(|| format!("value-domain profile: {field:?} must be a non-negative integer"))?;
    Ok(Some(n as usize))
}
