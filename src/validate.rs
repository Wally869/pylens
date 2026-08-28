//! The `observed ⊆ static` self-validation harness: measures analyzer honesty by checking
//! that every effect a function was **observed** to have (in its recorded [`Case`]s) is
//! predicted by its **static** may-set ([`EffectSignature`]). Anything observed but not
//! statically predicted is a soundness defect — see docs/DESIGN.md "Principles and the
//! soundness invariant".
//!
//! Pure: no jail, no I/O. Given a signature and its cases, [`validate_signature`] returns the
//! list of defects found.

use serde::Serialize;
use serde_json::Value;

use crate::model::{EffectSignature, Mutation, MutationTarget, ReturnKind};
use crate::record::{Case, FunctionRecord};

/// Which effect dimension a defect was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Mutation,
    Raise,
    Io,
    Return,
}

/// How serious a defect is.
///
/// `Hard`: the function's static may-set claimed to be complete (no `unresolved_effects`),
/// yet an observed effect wasn't predicted — a true soundness bug, must be driven to zero.
///
/// `Soft`: the function already declared blind spots via `unresolved_effects`; the observed
/// effect is "explained" by an acknowledged unknown, but is still reported since a wildcard
/// blind spot could be hiding imprecision worth tightening.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Hard,
    Soft,
}

/// One observed effect that the static may-set failed to predict.
#[derive(Debug, Clone, Serialize)]
pub struct Defect {
    /// Index into the function's `cases` where this was observed.
    pub case_index: usize,
    pub dimension: Dimension,
    /// A description of what was observed.
    pub observed: String,
    /// A description of why the static may-set doesn't cover it.
    pub expected: String,
    pub severity: Severity,
}

/// Check every case of `rec` against its static signature.
pub fn validate_function(rec: &FunctionRecord) -> Vec<Defect> {
    validate_signature(&rec.signature, &rec.cases)
}

/// Check `cases` (observed effects) against `sig` (the static may-set). The core, pure
/// entry point — no jail, no I/O.
pub fn validate_signature(sig: &EffectSignature, cases: &[Case]) -> Vec<Defect> {
    let severity = severity_for(sig);
    let mut defects = Vec::new();
    for (index, case) in cases.iter().enumerate() {
        check_mutations(sig, case, index, severity, &mut defects);
        if case.outcome == "raised" {
            check_raise(sig, case, index, severity, &mut defects);
        }
        check_io(sig, case, index, severity, &mut defects);
        if case.outcome == "returned" {
            check_return(sig, case, index, severity, &mut defects);
        }
    }
    defects
}

/// `Hard` when the signature's may-set claims to be complete (no acknowledged unknowns);
/// `Soft` when it already declares `unresolved_effects`.
fn severity_for(sig: &EffectSignature) -> Severity {
    if sig.unresolved_effects.is_empty() {
        Severity::Hard
    } else {
        Severity::Soft
    }
}

/// Whether some static mutation covers an observed mutation of `target` (a param name, or
/// `"self"` for the receiver). An observed `"self"` matches any `SelfAttr` (the observed
/// mutation carries no attribute name, only the root); an observed param name matches a
/// `Param` mutation with the same name.
fn mutation_covered(target: &str, muts: &[Mutation]) -> bool {
    muts.iter().any(|m| match &m.target {
        MutationTarget::Param { name } => name == target,
        MutationTarget::SelfAttr { .. } => target == "self",
        MutationTarget::Global { .. } | MutationTarget::Nonlocal { .. } | MutationTarget::Unknown => {
            false
        }
    })
}

fn check_mutations(
    sig: &EffectSignature,
    case: &Case,
    index: usize,
    severity: Severity,
    defects: &mut Vec<Defect>,
) {
    for m in &case.mutations {
        if !mutation_covered(&m.target, &sig.mutations) {
            defects.push(Defect {
                case_index: index,
                dimension: Dimension::Mutation,
                observed: format!("mutation of '{}'", m.target),
                expected: format!(
                    "no static mutation targets '{}' (static mutations: {:?})",
                    m.target, sig.mutations
                ),
                severity,
            });
        }
    }
}

fn check_raise(
    sig: &EffectSignature,
    case: &Case,
    index: usize,
    severity: Severity,
    defects: &mut Vec<Defect>,
) {
    let Some(ty) = &case.raises else {
        return;
    };
    let predicted = sig
        .raises
        .explicit
        .iter()
        .chain(sig.raises.implicit.iter())
        .any(|e| e == ty);
    if !predicted {
        defects.push(Defect {
            case_index: index,
            dimension: Dimension::Raise,
            observed: ty.clone(),
            expected: format!(
                "'{ty}' not in raises.explicit ∪ raises.implicit (explicit: {:?}, implicit: {:?})",
                sig.raises.explicit, sig.raises.implicit
            ),
            severity,
        });
    }
}

/// Captured stdout and stderr are each checked against the static `io` may-set (mapped the
/// same way the analyzer records it: `print` ⇒ `"stdout"`, `print(..., file=sys.stderr)` ⇒
/// `"stderr"`, any other `file=` target ⇒ both).
///
/// What this check cannot see:
/// - An interpreter-emitted warning (e.g. a `DeprecationWarning` raised by a call inside the
///   function) also lands in captured stderr and is attributed to the function here, even
///   though no `print`/`file=` construct produced it. That's defensible — calling the function
///   does produce it — but it's usually explained by an `unresolved_effects` entry on the
///   triggering call, so it reports as a soft defect rather than going unnoticed.
/// - The `"filesystem"` token has no observation channel at all: the jail's filesystem is
///   read-only, so no execution can confirm or contradict a filesystem claim. `io` is only
///   partially validated by this check — stdout and stderr, never filesystem.
fn check_io(
    sig: &EffectSignature,
    case: &Case,
    index: usize,
    severity: Severity,
    defects: &mut Vec<Defect>,
) {
    check_io_stream(sig, case, index, severity, defects, "stdout", |c| &c.stdout);
    check_io_stream(sig, case, index, severity, defects, "stderr", |c| &c.stderr);
}

fn check_io_stream(
    sig: &EffectSignature,
    case: &Case,
    index: usize,
    severity: Severity,
    defects: &mut Vec<Defect>,
    channel: &str,
    captured: impl FnOnce(&Case) -> &Option<String>,
) {
    if let Some(out) = captured(case)
        && !out.is_empty()
        && !sig.io.iter().any(|c| c == channel)
    {
        defects.push(Defect {
            case_index: index,
            dimension: Dimension::Io,
            observed: format!("{channel} captured"),
            expected: format!("'{channel}' not in static io (static io: {:?})", sig.io),
            severity,
        });
    }
}

fn check_return(
    sig: &EffectSignature,
    case: &Case,
    index: usize,
    severity: Severity,
    defects: &mut Vec<Defect>,
) {
    let Some(ret) = &case.ret else {
        return;
    };
    let kind = classify_return(ret);
    let predicted = sig.returns.contains(&kind) || sig.returns.contains(&ReturnKind::Opaque);
    if !predicted {
        defects.push(Defect {
            case_index: index,
            dimension: Dimension::Return,
            observed: format!("{kind:?}"),
            expected: format!("{kind:?} not in static returns: {:?}", sig.returns),
            severity,
        });
    }
}

/// Classify an observed (tagged-JSON-decoded) return value into a [`ReturnKind`]. Mirrors the
/// worker's tagged encoding (`python/worker.py::serialize`): `{"__t__":"set"}` → Set,
/// `{"__t__":"tuple"}` → Sequence, `{"__t__":"dict"}` → Mapping, `{"__t__":"obj"}` → Opaque
/// (unknown-shaped, e.g. bytes or a plain object).
fn classify_return(v: &Value) -> ReturnKind {
    match v {
        Value::Null => ReturnKind::None,
        Value::Bool(_) => ReturnKind::Bool,
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                ReturnKind::Int
            } else {
                ReturnKind::Float
            }
        }
        Value::String(_) => ReturnKind::Str,
        Value::Array(_) => ReturnKind::Sequence,
        Value::Object(obj) => match obj.get("__t__").and_then(Value::as_str) {
            Some("set") => ReturnKind::Set,
            Some("tuple") => ReturnKind::Sequence,
            Some("dict") => ReturnKind::Mapping,
            Some("obj") => ReturnKind::Opaque,
            _ => ReturnKind::Mapping,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DefKind, Mutation, MutationKind, MutationTarget};
    use crate::record::{CaseSource, ObservedMutation};
    use serde_json::json;

    fn sig(name: &str) -> EffectSignature {
        EffectSignature::new(name, DefKind::Function)
    }

    fn returned_case(ret: Value, mutations: Vec<ObservedMutation>) -> Case {
        Case {
            input: Vec::new(),
            kwargs: Default::default(),
            ctor_args: None,
            source: CaseSource::Generated,
            outcome: "returned".to_string(),
            ret: Some(ret),
            raises: None,
            mutations,
            return_aliases_arg: None,
            stdout: None,
            stderr: None,
            error: None,
            minimized: None,
            lines: Vec::new(),
            arcs: Vec::new(),
        }
    }

    fn raised_case(ty: &str) -> Case {
        Case {
            input: Vec::new(),
            kwargs: Default::default(),
            ctor_args: None,
            source: CaseSource::Generated,
            outcome: "raised".to_string(),
            ret: None,
            raises: Some(ty.to_string()),
            mutations: Vec::new(),
            return_aliases_arg: None,
            stdout: None,
            stderr: None,
            error: None,
            minimized: None,
            lines: Vec::new(),
            arcs: Vec::new(),
        }
    }

    fn error_case(raises: Option<&str>) -> Case {
        Case {
            input: Vec::new(),
            kwargs: Default::default(),
            ctor_args: None,
            source: CaseSource::Generated,
            outcome: "error".to_string(),
            ret: None,
            raises: raises.map(str::to_string),
            mutations: Vec::new(),
            return_aliases_arg: None,
            stdout: None,
            stderr: None,
            error: None,
            minimized: None,
            lines: Vec::new(),
            arcs: Vec::new(),
        }
    }

    #[test]
    fn covered_mutation_yields_no_defect() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::None);
        s.mutations.push(Mutation {
            target: MutationTarget::Param {
                name: "items".to_string(),
            },
            via: MutationKind::Method,
            name: Some("append".to_string()),
        });
        let case = returned_case(
            json!(null),
            vec![ObservedMutation {
                target: "items".to_string(),
                before: json!([]),
                after: json!([1]),
            }],
        );
        let defects = validate_signature(&s, std::slice::from_ref(&case));
        assert!(defects.is_empty(), "{defects:?}");
    }

    #[test]
    fn uncovered_mutation_is_hard_defect_without_unresolved_effects() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::None);
        let case = returned_case(
            json!(null),
            vec![ObservedMutation {
                target: "items".to_string(),
                before: json!([]),
                after: json!([1]),
            }],
        );
        let defects = validate_signature(&s, &[case]);
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].dimension, Dimension::Mutation);
        assert_eq!(defects[0].severity, Severity::Hard);
    }

    #[test]
    fn uncovered_mutation_is_soft_defect_with_unresolved_effects() {
        use crate::model::UnresolvedEffect;
        let mut s = sig("f");
        s.returns.push(ReturnKind::None);
        s.unresolved_effects.push(UnresolvedEffect {
            reason: "call_unknown_callee".to_string(),
            callee: Some("helper".to_string()),
            may_affect: Vec::new(),
        });
        let case = returned_case(
            json!(null),
            vec![ObservedMutation {
                target: "items".to_string(),
                before: json!([]),
                after: json!([1]),
            }],
        );
        let defects = validate_signature(&s, &[case]);
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].severity, Severity::Soft);
    }

    #[test]
    fn observed_self_mutation_matches_any_self_attr() {
        let mut s = sig("m");
        s.returns.push(ReturnKind::None);
        s.mutations.push(Mutation {
            target: MutationTarget::SelfAttr {
                name: "cache".to_string(),
            },
            via: MutationKind::AttrSet,
            name: Some("cache".to_string()),
        });
        let case = returned_case(
            json!(null),
            vec![ObservedMutation {
                target: "self".to_string(),
                before: json!({"__t__":"dict","items":[]}),
                after: json!({"__t__":"dict","items":[["log", 1]]}),
            }],
        );
        let defects = validate_signature(&s, &[case]);
        assert!(defects.is_empty(), "{defects:?}");
    }

    #[test]
    fn predicted_raise_yields_no_defect() {
        let mut s = sig("f");
        s.raises.explicit.push("ValueError".to_string());
        let defects = validate_signature(&s, &[raised_case("ValueError")]);
        assert!(defects.is_empty());
    }

    #[test]
    fn unpredicted_raise_is_a_defect() {
        let s = sig("f");
        let defects = validate_signature(&s, &[raised_case("ValueError")]);
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].dimension, Dimension::Raise);
    }

    #[test]
    fn error_outcome_is_never_checked_as_a_raise() {
        // Resource kills and harness/setup failures surface as outcome "error", never
        // "raised" (see record.rs); they must not be flagged as unpredicted raises.
        let s = sig("f");
        let defects = validate_signature(&s, &[error_case(Some("RecursionError"))]);
        assert!(defects.is_empty());
    }

    #[test]
    fn captured_stdout_requires_static_io() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::None);
        let mut case = returned_case(json!(null), Vec::new());
        case.stdout = Some("hello\n".to_string());
        let defects = validate_signature(&s, &[case]);
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].dimension, Dimension::Io);
    }

    #[test]
    fn captured_stdout_covered_by_static_io_yields_no_defect() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::None);
        s.io.push("stdout".to_string());
        let mut case = returned_case(json!(null), Vec::new());
        case.stdout = Some("hello\n".to_string());
        let defects = validate_signature(&s, &[case]);
        assert!(defects.is_empty());
    }

    #[test]
    fn captured_stderr_requires_static_io() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::None);
        let mut case = returned_case(json!(null), Vec::new());
        case.stderr = Some("warning\n".to_string());
        let defects = validate_signature(&s, &[case]);
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].dimension, Dimension::Io);
    }

    #[test]
    fn captured_stderr_covered_by_static_io_yields_no_defect() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::None);
        s.io.push("stderr".to_string());
        let mut case = returned_case(json!(null), Vec::new());
        case.stderr = Some("warning\n".to_string());
        let defects = validate_signature(&s, &[case]);
        assert!(defects.is_empty());
    }

    #[test]
    fn return_kind_covered_yields_no_defect() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::Str);
        let defects = validate_signature(&s, &[returned_case(json!("hi"), Vec::new())]);
        assert!(defects.is_empty());
    }

    #[test]
    fn return_kind_not_covered_is_a_defect() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::Int);
        let defects = validate_signature(&s, &[returned_case(json!("hi"), Vec::new())]);
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].dimension, Dimension::Return);
    }

    #[test]
    fn static_opaque_is_a_return_wildcard() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::Opaque);
        let defects = validate_signature(&s, &[returned_case(json!({"a": 1}), Vec::new())]);
        assert!(defects.is_empty());
    }

    #[test]
    fn tagged_set_return_classifies_as_set() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::Set);
        let ret = json!({"__t__": "set", "items": [1, 2]});
        let defects = validate_signature(&s, &[returned_case(ret, Vec::new())]);
        assert!(defects.is_empty());
    }

    #[test]
    fn tagged_tuple_return_classifies_as_sequence() {
        let mut s = sig("f");
        s.returns.push(ReturnKind::Sequence);
        let ret = json!({"__t__": "tuple", "items": [1, 2]});
        let defects = validate_signature(&s, &[returned_case(ret, Vec::new())]);
        assert!(defects.is_empty());
    }
}
