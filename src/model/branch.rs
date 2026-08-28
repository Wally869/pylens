//! The internal branch-point model used by `record`'s per-branch-outcome accounting (see
//! `analyze::collect::branches` for the enumeration and `record::branch_report_for` for how a
//! function's observed cases turn these into `covered` / `uncovered` /
//! `unobservable_line_granularity`). Not part of the static JSON contract — `EffectSignature`
//! keeps `branch_points` with `#[serde(skip)]`, exactly like `body_lines`.

use serde::Serialize;

/// The closed set of branch-point constructs pylens enumerates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
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
    /// Line-level tracing cannot distinguish this outcome from its siblings: same-line
    /// constructs (ternaries, short-circuit boolops, inline `if x: y`, comprehension guards).
    /// Enumerated like every other outcome, never dropped — the accounting stays closed.
    Unobservable,
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
