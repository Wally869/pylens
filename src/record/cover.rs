//! The predicate-targeted coverage loop for `pylens record --cover-branches`: after the initial
//! ranked generated batch, uncovered branch outcomes drive additional targeted inputs, built
//! around a value `generate::predicate::synthesize` produces for the branch's test expression.
//! Runs only when `--cover-branches` is set — otherwise `record`'s behavior and runtime are
//! unchanged, and every uncovered outcome carries `reason: "loop_not_run"`.
//!
//! Also hosts the per-branch-outcome report ([`BranchReport`]/[`BranchOutcomeReport`]) — its
//! `reason` field needs [`CoverContext`]'s bookkeeping of which outcomes had a synthesizer and
//! were actually attempted, so it lives next to the loop that produces that bookkeeping.

use std::collections::{HashMap, HashSet};

use ruff_source_file::LineIndex;
use serde::Serialize;
use serde_json::Value;

use crate::exec::{CallResult, Sandbox};
use crate::generate::predicate::{self, LinePredicates, Predicate};
use crate::generate::{GenInput, ValueDomain, gen_inputs, positional_params};
use crate::model::branch::OutcomeEvidence;
use crate::model::{BranchKind, EffectSignature, Shape};

use super::{Case, CaseSource, GenOptions, build_case, minimize_raised};

/// One outcome of a [`BranchReport`], and whether the aggregated cases proved it happened.
#[derive(Debug, Serialize)]
pub struct BranchOutcomeReport {
    pub outcome: String,
    /// `covered` (some case's traced arc/line is the evidence for this outcome) | `uncovered`
    /// (the evidence was never observed) | `unobservable_line_granularity` (line-level tracing
    /// cannot distinguish this outcome from its siblings — see
    /// [`crate::model::branch::OutcomeEvidence::Unobservable`]).
    pub status: String,
    /// Present exactly when `status == "uncovered"`: `"loop_not_run"` (`--cover-branches` wasn't
    /// passed), `"no_synthesizer"` (no handled predicate form covers this outcome, or every
    /// synthesized value was excluded by `--value-domain`), `"candidates_exhausted"` (synthesized
    /// values were tried and the outcome still didn't fire), or `"budget"` (the loop's total-case
    /// budget ran out before this outcome got a synthesized case).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One branch point of a function's body — see [`crate::analyze::collect::branches`] — with each
/// of its outcomes' observed status.
#[derive(Debug, Serialize)]
pub struct BranchReport {
    pub kind: BranchKind,
    pub line: u32,
    pub outcomes: Vec<BranchOutcomeReport>,
}

/// The per-function rollup over every outcome of every [`BranchReport`] — a closed count: every
/// outcome is exactly one of `covered`, `uncovered`, or `unobservable`.
#[derive(Debug, Serialize, Default)]
pub struct BranchCoverage {
    pub covered: usize,
    pub uncovered: usize,
    pub unobservable: usize,
}

/// What `run_loop` learned about a function's branch outcomes: whether the loop ran at all, which
/// outcomes had at least one admissible synthesized candidate (`synthesizable`), and which of
/// those were actually executed before the budget ran out (`attempted`) — the three facts
/// [`branch_report_for`] needs to fill in an uncovered outcome's `reason`.
#[derive(Default)]
pub(super) struct CoverContext {
    cover_branches: bool,
    synthesizable: HashSet<(u32, String)>,
    attempted: HashSet<(u32, String)>,
}

impl CoverContext {
    pub(super) fn not_run() -> Self {
        CoverContext::default()
    }

    fn reason_for(&self, line: u32, outcome: &str) -> String {
        if !self.cover_branches {
            return "loop_not_run".to_string();
        }
        let key = (line, outcome.to_string());
        if !self.synthesizable.contains(&key) {
            return "no_synthesizer".to_string();
        }
        if self.attempted.contains(&key) {
            "candidates_exhausted".to_string()
        } else {
            "budget".to_string()
        }
    }
}

/// Aggregate `cases`' traced lines and arcs against `sig.branch_points`, deciding each outcome's
/// status from the union of evidence over every case (generated and replayed alike — a branch
/// outcome reached by any input counts as covered). `None` when there's nothing to measure: no
/// branch points, or no cases to have measured them with — mirrors `coverage_for`.
pub(super) fn branch_report_for(
    sig: &EffectSignature,
    cases: &[Case],
    ctx: &CoverContext,
) -> Option<(Vec<BranchReport>, BranchCoverage)> {
    if cases.is_empty() || sig.branch_points.is_empty() {
        return None;
    }
    let (lines_seen, arcs_seen) = seen_sets(cases);

    let mut rollup = BranchCoverage::default();
    let mut reports = Vec::with_capacity(sig.branch_points.len());
    for bp in &sig.branch_points {
        let mut outcomes = Vec::with_capacity(bp.outcomes.len());
        for o in &bp.outcomes {
            let status = evidence_status(o.evidence, &lines_seen, &arcs_seen);
            match status {
                "covered" => rollup.covered += 1,
                "uncovered" => rollup.uncovered += 1,
                _ => rollup.unobservable += 1,
            }
            let reason = (status == "uncovered").then(|| ctx.reason_for(bp.line, &o.outcome));
            outcomes.push(BranchOutcomeReport { outcome: o.outcome.clone(), status: status.to_string(), reason });
        }
        reports.push(BranchReport { kind: bp.kind, line: bp.line, outcomes });
    }
    Some((reports, rollup))
}

fn evidence_status(evidence: OutcomeEvidence, lines_seen: &HashSet<u32>, arcs_seen: &HashSet<(u32, u32)>) -> &'static str {
    match evidence {
        OutcomeEvidence::Unobservable => "unobservable_line_granularity",
        OutcomeEvidence::Arc(a, b) => {
            if arcs_seen.contains(&(a, b)) { "covered" } else { "uncovered" }
        }
        OutcomeEvidence::Line(l) => {
            if lines_seen.contains(&l) { "covered" } else { "uncovered" }
        }
    }
}

fn seen_sets(cases: &[Case]) -> (HashSet<u32>, HashSet<(u32, u32)>) {
    let lines_seen = cases.iter().flat_map(|c| c.lines.iter().copied()).collect();
    let arcs_seen = cases.iter().flat_map(|c| c.arcs.iter().copied()).collect();
    (lines_seen, arcs_seen)
}

fn covered_count(sig: &EffectSignature, cases: &[Case]) -> usize {
    let (lines_seen, arcs_seen) = seen_sets(cases);
    sig.branch_points
        .iter()
        .flat_map(|bp| &bp.outcomes)
        .filter(|o| evidence_status(o.evidence, &lines_seen, &arcs_seen) == "covered")
        .count()
}

/// Every currently-uncovered *observable* outcome (unobservable-evidence outcomes are excluded —
/// they can never be confirmed, so the loop has nothing to aim at).
fn uncovered_outcomes(sig: &EffectSignature, cases: &[Case]) -> Vec<(u32, BranchKind, String)> {
    let (lines_seen, arcs_seen) = seen_sets(cases);
    let mut out = Vec::new();
    for bp in &sig.branch_points {
        for o in &bp.outcomes {
            if evidence_status(o.evidence, &lines_seen, &arcs_seen) == "uncovered" {
                out.push((bp.line, bp.kind, o.outcome.clone()));
            }
        }
    }
    out
}

/// Whether `outcome` of a branch of `kind` means the branch's test predicate was true (`Some`) —
/// `None` for a kind/outcome pairing this loop can't target (the same-line kinds never reach here
/// since their evidence is always `Unobservable`, filtered out by `uncovered_outcomes`).
fn outcome_polarity(kind: BranchKind, outcome: &str) -> Option<bool> {
    match (kind, outcome) {
        (BranchKind::If, "true") => Some(true),
        (BranchKind::If, "false") => Some(false),
        (BranchKind::While, "enter") => Some(true),
        (BranchKind::While, "skip") => Some(false),
        (BranchKind::For, "iterate") => Some(true),
        (BranchKind::For, "empty") => Some(false),
        _ => None,
    }
}

fn param_shape<'a>(sig: &'a EffectSignature, name: &str) -> Option<&'a Shape> {
    sig.params.iter().find(|p| p.name == name).map(|p| &p.shape)
}

/// Every `(parameter name, synthesized value)` pair the predicates at `line` produce for `want`,
/// admissible under `domain` — a domain-excluded synthesized value is dropped here, counting as
/// unsynthesizable for this outcome (see the module doc's `no_synthesizer` reason).
fn candidate_values(
    sig: &EffectSignature,
    predicates: &HashMap<u32, LinePredicates>,
    line: u32,
    want: bool,
    domain: Option<&ValueDomain>,
) -> Vec<(String, Value)> {
    let preds: Vec<&Predicate> = match predicates.get(&line) {
        Some(LinePredicates::Test(ps)) => ps.iter().collect(),
        Some(LinePredicates::ForIter(p)) => vec![p],
        None => Vec::new(),
    };
    let mut out = Vec::new();
    for pred in preds {
        let Some(shape) = param_shape(sig, pred.param()) else { continue };
        let Some(value) = predicate::synthesize(pred, want, shape) else { continue };
        if let Some(d) = domain
            && !d.allows(&value)
        {
            continue;
        }
        out.push((pred.param().to_string(), value));
    }
    out
}

/// Clone `template` (the all-base [`GenInput`], see [`super::gen_inputs`]) with `param_name`'s
/// slot overridden to `value`. `None` if `param_name` names neither a positional nor a
/// keyword-only parameter of `sig` (shouldn't happen — `param_name` always comes from a
/// [`Predicate`] extracted against `sig`'s own parameter names).
fn build_targeted_input(sig: &EffectSignature, template: &GenInput, param_name: &str, value: Value) -> Option<GenInput> {
    let mut gi = template.clone();
    if let Some(idx) = positional_params(sig).iter().position(|p| p.name == param_name) {
        if idx < gi.positional.len() {
            gi.positional[idx] = value;
            return Some(gi);
        }
        return None;
    }
    if let Some(slot) = gi.kwargs.iter_mut().find(|(n, _)| n == param_name) {
        slot.1 = value;
        return Some(gi);
    }
    None
}

/// How to invoke the function under test — a free function call, or a method call against an
/// already-built receiver — so [`run_loop`] can execute a targeted input the same way
/// `function_cases`/`method_record` executed the initial batch.
pub(super) enum CallTarget<'a> {
    Function,
    Method { class: &'a str, ctor_args: &'a [Value] },
}

/// Run the predicate-targeted coverage loop for one function, extending `cases` in place.
/// Iterates until every observable outcome is covered, an iteration adds no newly covered
/// outcome, or `cases.len()` reaches `opts.max_inputs` (the TOTAL per-function case budget once
/// `--cover-branches` is set). A no-op — returns [`CoverContext::not_run`] — when
/// `opts.cover_branches` is false or the function has no branch points.
pub(super) fn run_loop(
    sandbox: &dyn Sandbox,
    src: &str,
    sig: &EffectSignature,
    target: CallTarget,
    opts: GenOptions,
    cases: &mut Vec<Case>,
) -> Result<CoverContext, String> {
    let mut ctx = CoverContext::not_run();
    if !opts.cover_branches || sig.branch_points.is_empty() {
        return Ok(ctx);
    }
    ctx.cover_branches = true;

    let Ok(parsed) = crate::parse::parse_source(src) else { return Ok(ctx) };
    let Some(body) = predicate::find_function_body(parsed.syntax(), &sig.name, sig.owner.as_deref()) else {
        return Ok(ctx);
    };
    let line_index = LineIndex::from_source_text(src);
    let param_names: Vec<String> = positional_params(sig).iter().map(|p| p.name.clone()).collect();
    let predicates = predicate::collect_predicates(body, &line_index, &param_names);

    for bp in &sig.branch_points {
        for outcome in &bp.outcomes {
            if !matches!(outcome.evidence, OutcomeEvidence::Arc(..) | OutcomeEvidence::Line(_)) {
                continue;
            }
            let Some(want) = outcome_polarity(bp.kind, &outcome.outcome) else { continue };
            if !candidate_values(sig, &predicates, bp.line, want, opts.domain).is_empty() {
                ctx.synthesizable.insert((bp.line, outcome.outcome.clone()));
            }
        }
    }
    if ctx.synthesizable.is_empty() {
        return Ok(ctx);
    }

    let template = gen_inputs(sig, 1, opts.domain).into_iter().next().unwrap_or_default();
    let ctor_args_for_case = match &target {
        CallTarget::Function => None,
        CallTarget::Method { ctor_args, .. } => Some(ctor_args.to_vec()),
    };
    let call = |pos: &[Value], kw: &[(String, Value)]| -> Result<CallResult, String> {
        match &target {
            CallTarget::Function => sandbox.call(src, &sig.name, pos, kw),
            CallTarget::Method { class, ctor_args } => {
                sandbox.call_method(src, class, ctor_args, &sig.name, pos, kw)
            }
        }
    };

    loop {
        if cases.len() >= opts.max_inputs {
            break;
        }
        let before_covered = covered_count(sig, cases);
        let uncovered = uncovered_outcomes(sig, cases);
        if uncovered.is_empty() {
            break;
        }
        let mut executed_this_round = false;
        'outer: for (line, kind, outcome_name) in &uncovered {
            let Some(want) = outcome_polarity(*kind, outcome_name) else { continue };
            for (param_name, value) in candidate_values(sig, &predicates, *line, want, opts.domain) {
                if cases.len() >= opts.max_inputs {
                    break 'outer;
                }
                let Some(gi) = build_targeted_input(sig, &template, &param_name, value) else { continue };
                let result = call(&gi.positional, &gi.kwargs)?;
                let mut case = build_case(sig, &gi, ctor_args_for_case.clone(), &result, CaseSource::Generated);
                if case.outcome == "raised" {
                    case.minimized = minimize_raised(&case, &gi, opts.domain, |pos, kw| call(pos, kw))?;
                }
                cases.push(case);
                ctx.attempted.insert((*line, outcome_name.clone()));
                executed_this_round = true;
            }
        }
        if !executed_this_round {
            break;
        }
        if covered_count(sig, cases) <= before_covered {
            break;
        }
    }

    Ok(ctx)
}
