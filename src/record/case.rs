//! The serialization-side output types of one recorded module: a single executed [`Case`], the
//! per-function [`FunctionRecord`] it rolls up into, and the whole-file [`ModuleRecord`]. Also
//! `build_case` (turns one [`crate::exec::CallResult`] into a `Case`) and the coverage/output-type
//! aggregation helpers `record::mod`'s orchestration calls per function.

use std::collections::HashSet;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::exec::{CallResult, HarnessError};
use crate::generate::{GenInput, keyword_only_params, positional_params};
use crate::model::branch::FineHit;
use crate::model::{EffectSignature, Import, ReturnKind};
use crate::validate::observable_io_kind;

use super::stability::DroppedCases;
use super::{BranchCoverage, BranchReport};

/// Where an executed case's input came from — see `Case::source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseSource {
    /// Sampled by [`crate::generate::gen_inputs`] from the inferred shapes.
    Generated,
    /// Supplied externally via `--replay` — see [`super::parse_replay`].
    Replay,
}

/// A mutation observed by diffing a value before vs. after the call.
#[derive(Serialize)]
pub struct ObservedMutation {
    /// The mutated root: a parameter name, or `"self"` (the receiver).
    pub target: String,
    pub before: Value,
    pub after: Value,
}

/// A shrunk variant of a raised case's input that still raises the same exception type — see
/// [`crate::shrink::shrink_case`]. Present on a `Case` only when at least one argument was
/// successfully shrunk; this is a reporting aid and is never fed back into `validate`.
#[derive(Serialize)]
pub struct MinimizedInput {
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub kwargs: Map<String, Value>,
}

/// One executed case: an input vector and what the function did with it.
#[derive(Serialize)]
pub struct Case {
    pub input: Vec<Value>,
    /// Keyword-only arguments passed this call, name → value.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub kwargs: Map<String, Value>,
    /// Constructor arguments used to build the receiver (methods only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctor_args: Option<Vec<Value>>,
    /// `generated` (sampled from the inferred shapes) or `replay` (supplied via `--replay`) —
    /// see [`CaseSource`].
    pub source: CaseSource,
    /// `returned` | `raised` | `error`. **`raised` means the function itself raised a semantic
    /// exception** (part of its behavior/spec) — `raises` carries the exception type. A
    /// **resource kill** (out-of-memory, recursion limit, timeout — an artifact of the sandbox,
    /// not the function's semantics) is never `raised`: it is always `error`, with the
    /// structured `error.stage == "resource"` (see [`crate::exec::HarnessError::is_resource`]).
    /// Other harness/setup failures (bad source, missing function, timeout-unrelated crashes)
    /// are also `error`, with a different `stage`.
    pub outcome: String,
    #[serde(rename = "return", skip_serializing_if = "Option::is_none")]
    pub ret: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raises: Option<String>,
    /// Mutations observed this run (arguments and/or `self`).
    pub mutations: Vec<ObservedMutation>,
    /// Index of the argument the return value is identical to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_aliases_arg: Option<i64>,
    /// Captured stdout produced by the call, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// Captured stderr produced by the call, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<HarnessError>,
    /// Present only for `outcome == "raised"` cases where shrinking found a smaller input that
    /// still raises the same exception type. See [`crate::shrink::shrink_case`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimized: Option<MinimizedInput>,
    /// Lines this case reached in the module under test — an input to [`coverage_for`]'s
    /// per-function aggregate, not something a consumer needs per case (noisy at N cases).
    #[serde(skip)]
    pub lines: Vec<u32>,
    /// Line-transition arcs this case traced — an input to `super::cover::branch_report_for`'s
    /// per-function aggregate, not something a consumer needs per case (noisy at N cases).
    #[serde(skip)]
    pub arcs: Vec<(u32, u32)>,
    /// Fine-grained (opcode-resolved) same-line outcomes this case's call observed — an input to
    /// `super::cover::branch_report_for`, same as `arcs`. Empty unless the call's request
    /// carried `fine_targets` (only functions with same-line branch constructs do).
    #[serde(skip)]
    pub fine_hits: Vec<FineHit>,
}

/// A function's executed-line coverage, aggregated over all its cases: how many of its
/// `body_lines` (see [`EffectSignature::body_lines`]) were reached by *some* case, and which
/// ones never were. This is exactly the gap `docs/DESIGN.md` flags as unmeasured — generated
/// inputs are heuristic, so `validate` only checks the paths they happen to reach; `coverage`
/// makes that reach visible instead of leaving it implicit.
#[derive(Serialize)]
pub struct Coverage {
    pub executed: usize,
    pub total: usize,
    pub missed: Vec<u32>,
}

/// Aggregate `cases`' observed lines against `sig.body_lines`, intersecting so lines the trace
/// saw in some *other* function of the same module (the call reached past this function's own
/// body) don't inflate `executed`. `None` when there's nothing to measure: no `body_lines`, or no
/// cases to have measured them with.
pub(super) fn coverage_for(sig: &EffectSignature, cases: &[Case]) -> Option<Coverage> {
    if cases.is_empty() || sig.body_lines.is_empty() {
        return None;
    }
    let body: HashSet<u32> = sig.body_lines.iter().copied().collect();
    let reached: HashSet<u32> = cases
        .iter()
        .flat_map(|c| c.lines.iter().copied())
        .filter(|l| body.contains(l))
        .collect();
    let mut missed: Vec<u32> = sig
        .body_lines
        .iter()
        .copied()
        .filter(|l| !reached.contains(l))
        .collect();
    missed.sort_unstable();
    Some(Coverage {
        executed: reached.len(),
        total: sig.body_lines.len(),
        missed,
    })
}

/// One `io` may-set entry (see [`EffectSignature::io`]) tagged with whether the sandbox has any
/// channel to observe it — see [`crate::validate::observable_io_kind`]. `record`'s and
/// `validate`'s honesty flag: a consumer must not read an unobservable `io` claim's absence from
/// `validate`'s defects as corroboration, since no execution could ever have disproved it.
#[derive(Serialize)]
pub struct IoObservability {
    pub kind: String,
    pub observable: bool,
}

pub(super) fn io_observability(io: &[String]) -> Vec<IoObservability> {
    io.iter()
        .map(|kind| IoObservability {
            kind: kind.clone(),
            observable: observable_io_kind(kind),
        })
        .collect()
}

/// Why a function couldn't be executed at all — recorded once, instead of as N identical
/// per-case failures.
#[derive(Serialize)]
pub struct Uncallable {
    /// `module_not_loadable` (a module-scope import failed) | `constructor_failed`.
    pub reason: String,
    pub error: HarnessError,
}

/// The full record of one function or method: its static signature fields, flattened, plus the
/// observed `cases`. If the function couldn't be executed, `uncallable` says why and `cases` is
/// empty.
#[derive(Serialize)]
pub struct FunctionRecord {
    #[serde(flatten)]
    pub signature: EffectSignature,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncallable: Option<Uncallable>,
    pub cases: Vec<Case>,
    /// Executed-line coverage over `body_lines`, aggregated over `cases`. Omitted when there's
    /// nothing to measure — see [`coverage_for`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    /// Per-branch-outcome accounting over `cases` — see `super::cover::branch_report_for`. Omitted
    /// when there's nothing to measure (no branch points, or no cases), same as `coverage`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches: Option<Vec<BranchReport>>,
    /// The rollup over every outcome in `branches`. Present exactly when `branches` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_coverage: Option<BranchCoverage>,
    /// The closed count of cases `--stability-runs` dropped — see [`DroppedCases`]. Omitted
    /// entirely when `--stability-runs` wasn't passed, so plain `record` output is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_cases: Option<DroppedCases>,
    /// Per-`io`-entry observability — see [`IoObservability`]. Parallel to, and never a
    /// replacement for, the flattened `io: Vec<String>` may-set carried by `signature`.
    pub io_observability: Vec<IoObservability>,
    /// Whether every return kind in `signature.returns` was observed in some surviving case's
    /// return value AND every `return` statement's line was executed by some surviving case —
    /// see [`output_type_coverage_for`]. `None` when nothing was observed at all (uncallable, or
    /// zero cases): `validated: false` in `validate` output already carries that story, and
    /// there is nothing here to be full or partial about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_type_coverage: Option<OutputTypeCoverage>,
    /// Present exactly when `output_type_coverage == Some(Partial)`: the static return kinds no
    /// surviving case's return value matched, and/or the `return` statement lines no surviving
    /// case ever executed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unobserved_returns: Option<UnobservedReturns>,
    /// `true` when `--time-budget` was set and this function's deadline had passed by the time
    /// recording finished — a hint that its cases, coverage, and branch accounting may be
    /// thinner than an unbudgeted run would have produced. Omitted (not just `false`) whenever
    /// the budget wasn't set or wasn't hit, so plain `record` output is unchanged — see
    /// [`super::RecordFlags::time_budget`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_budget_hit: Option<bool>,
}

/// `"full"` when [`output_type_coverage_for`]'s two conditions both hold; `"partial"` otherwise,
/// with the gap detailed in [`UnobservedReturns`].
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputTypeCoverage {
    Full,
    Partial,
}

/// What kept `output_type_coverage` from being `full` — see [`output_type_coverage_for`].
#[derive(Serialize)]
pub struct UnobservedReturns {
    /// Static return kinds (from `signature.returns`) no surviving case's return value matched.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<ReturnKind>,
    /// `return` statement lines (from the function's body) no surviving case executed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<u32>,
}

/// Whether `cases` observed every static return kind and executed every `return` statement's
/// line, for one function. `None` when there's nothing to measure: no cases at all. Reuses
/// [`crate::validate::classify_return`] for the observed-return classification so `record` and
/// `validate` never disagree on what an observed return value's kind is.
pub(super) fn output_type_coverage_for(
    sig: &EffectSignature,
    cases: &[Case],
) -> Option<(OutputTypeCoverage, Option<UnobservedReturns>)> {
    if cases.is_empty() {
        return None;
    }
    let observed_kinds: HashSet<ReturnKind> = cases
        .iter()
        .filter(|c| c.outcome == "returned")
        .filter_map(|c| c.ret.as_ref())
        .map(crate::validate::classify_return)
        .collect();
    let mut missing_kinds = Vec::new();
    for kind in &sig.returns {
        if !observed_kinds.contains(kind) && !missing_kinds.contains(kind) {
            missing_kinds.push(*kind);
        }
    }

    let reached_lines: HashSet<u32> = cases.iter().flat_map(|c| c.lines.iter().copied()).collect();
    let mut missing_lines: Vec<u32> = sig
        .return_lines
        .iter()
        .copied()
        .filter(|l| !reached_lines.contains(l))
        .collect();
    missing_lines.sort_unstable();

    if missing_kinds.is_empty() && missing_lines.is_empty() {
        Some((OutputTypeCoverage::Full, None))
    } else {
        Some((
            OutputTypeCoverage::Partial,
            Some(UnobservedReturns { kinds: missing_kinds, lines: missing_lines }),
        ))
    }
}

/// Whether a dependency's module resolves in the jail.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DepStatus {
    /// `import <module>` succeeded.
    Resolved,
    /// `import <module>` failed (e.g. the package isn't installed).
    Unresolved,
    /// Not attempted. Relative imports need a package context that a standalone file doesn't
    /// supply — they aren't failures, just unprobed here.
    NotProbed,
}

/// A catalogued import plus whether its module resolves in the jail.
#[derive(Serialize)]
pub struct Dependency {
    #[serde(flatten)]
    pub import: Import,
    pub status: DepStatus,
    /// Why it didn't resolve (structured: stage/kind/message/module).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<HarnessError>,
}

/// The record for a whole file: its dependencies (with resolution status) and per-function
/// records.
#[derive(Serialize)]
pub struct ModuleRecord {
    pub dependencies: Vec<Dependency>,
    pub functions: Vec<FunctionRecord>,
}

pub(super) fn build_case(
    sig: &EffectSignature,
    input: &GenInput,
    ctor_args: Option<Vec<Value>>,
    r: &CallResult,
    source: CaseSource,
) -> Case {
    let mut mutations = Vec::new();

    // Positional argument mutations: diff the pre-call snapshot against the post-call state.
    // Both come from the worker in the same tagged encoding, so equal values compare equal.
    // `positional_params(sig)` is the same filter used to build `input.positional`, so the
    // index alignment holds.
    if let (Some(pre), Some(post)) = (&r.args_pre, &r.args_post) {
        for (i, p) in positional_params(sig).into_iter().enumerate() {
            if let (Some(before), Some(after)) = (pre.get(i), post.get(i))
                && !value_eq(before, after)
            {
                mutations.push(ObservedMutation {
                    target: p.name.clone(),
                    before: before.clone(),
                    after: after.clone(),
                });
            }
        }
    }
    // Keyword-only argument mutations: diff by name, symmetric to the positional case above.
    if let (Some(pre), Some(post)) = (&r.kwargs_pre, &r.kwargs_post) {
        for p in keyword_only_params(sig) {
            if let (Some(before), Some(after)) = (pre.get(&p.name), post.get(&p.name))
                && !value_eq(before, after)
            {
                mutations.push(ObservedMutation {
                    target: p.name.clone(),
                    before: before.clone(),
                    after: after.clone(),
                });
            }
        }
    }
    // Receiver mutation: diff self_pre against self_post.
    if let (Some(pre), Some(post)) = (&r.self_pre, &r.self_post)
        && !value_eq(pre, post)
    {
        mutations.push(ObservedMutation {
            target: "self".to_string(),
            before: pre.clone(),
            after: post.clone(),
        });
    }

    let stdout = r.stdout.clone();
    let stderr = r.stderr.clone();
    let kwargs: Map<String, Value> = input.kwargs.iter().cloned().collect();

    if let Some(err) = &r.error {
        return Case {
            input: input.positional.clone(),
            kwargs,
            ctor_args,
            source,
            outcome: "error".to_string(),
            ret: None,
            raises: None,
            mutations,
            return_aliases_arg: None,
            stdout,
            stderr,
            error: Some(err.clone()),
            minimized: None,
            lines: r.lines.clone(),
            arcs: r.arcs.clone(),
            fine_hits: r.fine_hits.clone(),
        };
    }
    if r.ok {
        Case {
            input: input.positional.clone(),
            kwargs,
            ctor_args,
            source,
            outcome: "returned".to_string(),
            ret: Some(r.ret.clone()),
            raises: None,
            mutations,
            return_aliases_arg: r.return_aliases_arg,
            stdout,
            stderr,
            error: None,
            minimized: None,
            lines: r.lines.clone(),
            arcs: r.arcs.clone(),
            fine_hits: r.fine_hits.clone(),
        }
    } else {
        Case {
            input: input.positional.clone(),
            kwargs,
            ctor_args,
            source,
            outcome: "raised".to_string(),
            ret: None,
            raises: r.exception.as_ref().map(|e| e.ty.clone()),
            mutations,
            return_aliases_arg: None,
            stdout,
            stderr,
            error: None,
            minimized: None,
            lines: r.lines.clone(),
            arcs: r.arcs.clone(),
            fine_hits: r.fine_hits.clone(),
        }
    }
}

/// Structural equality with float tolerance; sets/dict items are pre-sorted by the worker, so
/// positional array comparison is order-insensitive for them.
pub(super) fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(xa), Some(yb)) => (xa - yb).abs() <= 1e-9 * (1.0 + xa.abs().max(yb.abs())),
            _ => x == y,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| value_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| value_eq(v, w)))
        }
        _ => a == b,
    }
}
