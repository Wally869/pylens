//! The internal branch-point model used by `record`'s per-branch-outcome accounting (see
//! `analyze::collect::branches` for the enumeration and `record::branch_report_for` for how a
//! function's observed cases turn these into `covered` / `uncovered` /
//! `unobservable_line_granularity`). Not part of the static JSON contract — `EffectSignature`
//! keeps `branch_points` with `#[serde(skip)]`, exactly like `body_lines`.
//!
//! [`OutcomeEvidence::FineGrained`] outcomes (ternaries, boolop short-circuits, single-line
//! `if x: y`, comprehension guards) need opcode-level evidence the sandbox's tracer only
//! collects on request — see [`FineTarget`]/[`FineHit`] and `python/worker.py`'s
//! `_fine_grained` plan.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// The closed set of branch-point constructs pylens enumerates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchKind {
    If,
    While,
    For,
    Except,
    /// A `try`'s `else:` clause (runs only if the `try` body completed without raising).
    TryElse,
    Match,
    /// A conditional expression (`a if cond else b`).
    Ternary,
    /// A `BoolOp` (`and`/`or`) short-circuit point.
    BoolOp,
    /// A single-line `if x: y` body — the test and the body share one traced line.
    InlineIf,
    /// A comprehension's `if` guard.
    ComprehensionIf,
}

/// What runtime evidence, if observed over a function's executed cases, proves an outcome
/// occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeEvidence {
    /// A traced `(prev_line, cur_line)` transition.
    Arc(u32, u32),
    /// A traced line on its own — used where no single "from" line exists (e.g. an `except`
    /// handler can be entered from any line inside the `try` body).
    Line(u32),
    /// A same-line construct (ternary, boolop short-circuit, single-line `if x: y`, comprehension
    /// guard, same-line `while`) resolved via opcode-level tracing instead of line arcs — `(line,
    /// ordinal, compound)`, where `ordinal` disambiguates multiple fine-grained branch points
    /// sharing one physical line (assigned by encounter order in
    /// `analyze::collect::branches::collect_branches`), and `compound` tells the worker which
    /// resolution strategy to use: `false` is the original single-instruction probe (a test whose
    /// own compiled shape is exactly one `POP_JUMP_IF_*`, or a value-position boolop's
    /// `JUMP_IF_*_OR_POP` chain); `true` is the landing-offset, multi-instruction/multi-run chain
    /// resolution (a compound `and`/`or` test, a test-position boolop's own short-circuit signal,
    /// or a same-line `while`'s loop-rotation-duplicated test) — see `python/worker.py`'s
    /// `_resolve_landing_chain`. See [`FineTarget`]/[`FineHit`].
    FineGrained(u32, u32, bool),
    /// No runtime evidence exists at all for this outcome — a branch whose false-arc target
    /// itself isn't known (e.g. an else-less `if` that is a function's last statement, so there
    /// is no line to land on after it). Enumerated like every other outcome, never dropped — the
    /// accounting stays closed.
    Unobservable,
}

/// One same-line branch point the sandbox is asked to resolve via opcode-level tracing for one
/// call — see [`OutcomeEvidence::FineGrained`]. `kind` tells the worker which detection strategy
/// to use (a single test-and-jump for [`BranchKind::Ternary`]/[`BranchKind::InlineIf`]/
/// [`BranchKind::ComprehensionIf`], a short-circuit jump chain for [`BranchKind::BoolOp`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FineTarget {
    pub line: u32,
    pub kind: BranchKind,
    pub ordinal: u32,
    /// See [`OutcomeEvidence::FineGrained`]'s third field.
    pub compound: bool,
}

/// One fine-grained outcome the worker's opcode tracer actually observed during a call — the
/// wire-response counterpart of [`FineTarget`]. `kind` isn't echoed back: `(line, ordinal)` is
/// only unique WITHIN one [`crate::analyze::collect::branches::ProbeCategory`] (ordinals are
/// assigned per `(line, category)`, not per line alone — a ternary and a value-position boolop on
/// the same line can both be ordinal 0), but `outcome`'s name is disjoint across categories
/// (`"true"`/`"false"` vs. `"short_circuit"`/`"full_evaluation"`), so `(line, ordinal, outcome)`
/// together — the key `record::cover::evidence_status` actually matches on — never collides.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FineHit {
    pub line: u32,
    pub ordinal: u32,
    pub outcome: String,
}

/// Every distinct [`FineTarget`] a function's branch points need resolved — deduplicated per
/// `(line, kind, ordinal)`, since a fine-grained branch point's two outcomes share one such
/// triple. Keying on `(line, ordinal)` alone would collide across different
/// `analyze::collect::branches::ProbeCategory`s that legitimately share an ordinal on one line
/// (see [`FineHit`]) and silently drop one of them.
pub fn fine_targets(points: &[BranchPoint]) -> Vec<FineTarget> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for bp in points {
        for o in &bp.outcomes {
            if let OutcomeEvidence::FineGrained(line, ordinal, compound) = o.evidence
                && seen.insert((line, bp.kind, ordinal))
            {
                out.push(FineTarget { line, kind: bp.kind, ordinal, compound });
            }
        }
    }
    out
}

/// One possible outcome of a branch point, and the evidence that would prove it happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchPointOutcome {
    pub outcome: String,
    pub evidence: OutcomeEvidence,
}

/// One branch point in a function's body: where it is, and each of its possible outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchPoint {
    pub kind: BranchKind,
    pub line: u32,
    pub outcomes: Vec<BranchPointOutcome>,
}
