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

use crate::exec::{CallResult, Limits, Sandbox};
use crate::generate::predicate::{self, BoolOpGroup, LinePredicates, Predicate};
use crate::generate::{GenInput, ValueDomain, gen_inputs, positional_params};
use crate::model::branch::{OutcomeEvidence, fine_targets};
use crate::model::{BranchKind, EffectSignature, Shape};

use super::{Case, CaseSource, GenOptions, build_case, minimize_raised};

/// One outcome of a [`BranchReport`], and whether the aggregated cases proved it happened.
#[derive(Debug, Serialize)]
pub struct BranchOutcomeReport {
    pub outcome: String,
    /// `covered` (some case's traced arc/line/fine-grained opcode hit is the evidence for this
    /// outcome) | `uncovered` (the evidence was never observed) | `unobservable_line_granularity`
    /// (no runtime evidence exists at all — see
    /// [`crate::model::branch::OutcomeEvidence::Unobservable`]; same-line constructs are usually
    /// `FineGrained` instead, resolved from opcode-level tracing, not this state).
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
    let (lines_seen, arcs_seen, fine_seen) = seen_sets(cases);

    let mut rollup = BranchCoverage::default();
    let mut reports = Vec::with_capacity(sig.branch_points.len());
    for bp in &sig.branch_points {
        let mut outcomes = Vec::with_capacity(bp.outcomes.len());
        for o in &bp.outcomes {
            let status = evidence_status(o.evidence, &o.outcome, &lines_seen, &arcs_seen, &fine_seen);
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

fn evidence_status(
    evidence: OutcomeEvidence,
    outcome: &str,
    lines_seen: &HashSet<u32>,
    arcs_seen: &HashSet<(u32, u32)>,
    fine_seen: &HashSet<(u32, u32, String)>,
) -> &'static str {
    match evidence {
        OutcomeEvidence::Unobservable => "unobservable_line_granularity",
        OutcomeEvidence::Arc(a, b) => {
            if arcs_seen.contains(&(a, b)) { "covered" } else { "uncovered" }
        }
        OutcomeEvidence::Line(l) => {
            if lines_seen.contains(&l) { "covered" } else { "uncovered" }
        }
        OutcomeEvidence::FineGrained(line, ordinal, _) => {
            if fine_seen.contains(&(line, ordinal, outcome.to_string())) { "covered" } else { "uncovered" }
        }
    }
}

/// `(lines seen, arcs seen, fine-grained (line, ordinal, outcome) hits seen)` — see `seen_sets`.
type SeenEvidence = (HashSet<u32>, HashSet<(u32, u32)>, HashSet<(u32, u32, String)>);

fn seen_sets(cases: &[Case]) -> SeenEvidence {
    let lines_seen = cases.iter().flat_map(|c| c.lines.iter().copied()).collect();
    let arcs_seen = cases.iter().flat_map(|c| c.arcs.iter().copied()).collect();
    let fine_seen = cases
        .iter()
        .flat_map(|c| c.fine_hits.iter())
        .map(|h| (h.line, h.ordinal, h.outcome.clone()))
        .collect();
    (lines_seen, arcs_seen, fine_seen)
}

/// Every currently-uncovered *observable* outcome (unobservable-evidence outcomes are excluded —
/// they can never be confirmed, so the loop has nothing to aim at).
fn uncovered_outcomes(sig: &EffectSignature, cases: &[Case]) -> Vec<(u32, BranchKind, String)> {
    let (lines_seen, arcs_seen, fine_seen) = seen_sets(cases);
    let mut out = Vec::new();
    for bp in &sig.branch_points {
        for o in &bp.outcomes {
            if evidence_status(o.evidence, &o.outcome, &lines_seen, &arcs_seen, &fine_seen) == "uncovered" {
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

/// [`outcome_polarity`], extended to [`BranchKind::BoolOp`]'s own `short_circuit`/
/// `full_evaluation` outcomes — these don't have a fixed per-kind polarity (an `and`'s
/// `full_evaluation` wants every operand true, an `or`'s wants every operand false), so they
/// resolve through `line`'s own [`BoolOpGroup`] instead: `full_evaluation` wants every operand at
/// the group's own sense (`and` → true, `or` → false — exactly what makes the WHOLE expression
/// true for `and` or false for `or`, so this is also what an enclosing `if`/`while`'s matching
/// outcome wants — see [`candidate_values`]'s merge trigger); `short_circuit` wants the opposite
/// (any ONE operand at that polarity already short-circuits, so it stays on the existing
/// one-predicate-at-a-time path, no merge needed). `None` when `line` has no resolved
/// [`BoolOpGroup`] (a mixed nested `and`/`or`, or a non-`BoolOp`-only test) — the outcome then
/// just stays unsynthesizable, never guessed.
fn want_for_outcome(
    kind: BranchKind,
    outcome: &str,
    predicates: &HashMap<u32, LinePredicates>,
    line: u32,
) -> Option<bool> {
    if kind != BranchKind::BoolOp {
        return outcome_polarity(kind, outcome);
    }
    let Some(LinePredicates::Test { boolop: Some(group), .. }) = predicates.get(&line) else {
        return None;
    };
    match outcome {
        "full_evaluation" => Some(group.and),
        "short_circuit" => Some(!group.and),
        _ => None,
    }
}

fn param_shape<'a>(sig: &'a EffectSignature, name: &str) -> Option<&'a Shape> {
    sig.params.iter().find(|p| p.name == name).map(|p| &p.shape)
}

/// The single override set formed by unioning every operand of `group` at `operand_want`
/// simultaneously — the outcome that needs the WHOLE conjunction/disjunction to resolve one way
/// rather than any single operand: an `and`'s `true`/`full_evaluation` (`operand_want = true`), an
/// `or`'s `false`/`full_evaluation` (`operand_want = false`). `None` when any operand doesn't
/// decompose to exactly one predicate (no single value represents "this whole operand holds" —
/// the smallest form this merge supports), when two operands constrain the SAME parameter (no
/// constraint solving here — a second override would just overwrite the first, silently dropping
/// one operand's own requirement, so this refuses outright rather than guess), or when any
/// operand's value can't be synthesized at all or is excluded by `--value-domain`.
fn merge_group(
    sig: &EffectSignature,
    group: &BoolOpGroup,
    operand_want: bool,
    domain: Option<&ValueDomain>,
) -> Option<Vec<(String, Value)>> {
    let mut merged = Vec::with_capacity(group.operands.len());
    let mut seen_params = HashSet::new();
    for operand in &group.operands {
        let [pred] = operand.as_slice() else { return None };
        if !seen_params.insert(pred.param()) {
            return None;
        }
        let shape = param_shape(sig, pred.param())?;
        let value = predicate::synthesize(pred, operand_want, shape)?;
        if let Some(d) = domain
            && !d.allows(&value)
        {
            return None;
        }
        merged.push((pred.param().to_string(), value));
    }
    (!merged.is_empty()).then_some(merged)
}

/// Every override set (each a set of `(parameter name, value)` overrides that must be applied
/// *together* to one input for the predicate to evaluate to `want`) `pred` can produce — its
/// primary synthesized set, plus a second, structurally different variant where one is principled
/// ([`predicate::synthesize_variant`]) for the still-uncovered outcomes a single deterministic
/// candidate can never flip (an outer guard the primary candidate can't pass, an adjacent
/// branch's own candidate landing on the same value). Empty for [`Predicate::ParamCompare`] when
/// either parameter's shape is unknown or the pair can't be synthesized at all.
fn override_sets(sig: &EffectSignature, pred: &Predicate, want: bool) -> Vec<Vec<(String, Value)>> {
    if let Predicate::ParamCompare { param_a, deriv_a, op, param_b, deriv_b } = pred {
        let (Some(shape_a), Some(shape_b)) = (param_shape(sig, param_a), param_shape(sig, param_b)) else {
            return Vec::new();
        };
        let Some((value_a, value_b)) = predicate::synthesize_pair(deriv_a, *op, deriv_b, shape_a, shape_b, want)
        else {
            return Vec::new();
        };
        return vec![vec![(param_a.clone(), value_a), (param_b.clone(), value_b)]];
    }
    let Some(shape) = param_shape(sig, pred.param()) else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(value) = predicate::synthesize(pred, want, shape) {
        out.push(vec![(pred.param().to_string(), value)]);
    }
    if let Some(value) = predicate::synthesize_variant(pred, want, shape) {
        out.push(vec![(pred.param().to_string(), value)]);
    }
    out
}

/// Every override set the predicates at `line` produce for `want`, admissible under `domain` — a
/// set with any domain-excluded value is dropped, counting as unsynthesizable for this outcome
/// only if every one of its predicate's sets is dropped (see the module doc's `no_synthesizer`
/// reason). When `line`'s test is a single flat `BoolOp` (see [`BoolOpGroup`]) and `want` matches
/// the group's own sense (`want == group.and`), also appends [`merge_group`]'s single merged
/// candidate — the case that needs every operand satisfied TOGETHER (an `and`'s `true`, an `or`'s
/// `false`, and the equivalent `BoolOp` `full_evaluation` outcome via [`want_for_outcome`]), which
/// no single predicate's own override set can produce.
fn candidate_values(
    sig: &EffectSignature,
    predicates: &HashMap<u32, LinePredicates>,
    line: u32,
    want: bool,
    domain: Option<&ValueDomain>,
) -> Vec<Vec<(String, Value)>> {
    let entry = predicates.get(&line);
    let preds: Vec<&Predicate> = match entry {
        Some(LinePredicates::Test { flat, .. }) => flat.iter().collect(),
        Some(LinePredicates::ForIter(p)) => vec![p],
        None => Vec::new(),
    };
    let mut out = Vec::new();
    for pred in preds {
        for set in override_sets(sig, pred, want) {
            if let Some(d) = domain
                && !set.iter().all(|(_, v)| d.allows(v))
            {
                continue;
            }
            out.push(set);
        }
    }
    if let Some(LinePredicates::Test { boolop: Some(group), .. }) = entry
        && want == group.and
        && let Some(merged) = merge_group(sig, group, group.and, domain)
    {
        out.push(merged);
    }
    out
}

/// Clone `template` (the all-base [`GenInput`], see [`super::gen_inputs`]) with every
/// `(parameter name, value)` in `overrides` applied to its slot. `None` if any name in `overrides`
/// names neither a positional nor a keyword-only parameter of `sig` (shouldn't happen — every
/// name always comes from a [`Predicate`] extracted against `sig`'s own parameter names).
fn build_targeted_input(sig: &EffectSignature, template: &GenInput, overrides: &[(String, Value)]) -> Option<GenInput> {
    let mut gi = template.clone();
    let positional = positional_params(sig);
    for (param_name, value) in overrides {
        if let Some(idx) = positional.iter().position(|p| &p.name == param_name) {
            if idx >= gi.positional.len() {
                return None;
            }
            gi.positional[idx] = value.clone();
            continue;
        }
        gi.kwargs.iter_mut().find(|(n, _)| n == param_name)?.1 = value.clone();
    }
    Some(gi)
}

/// How to invoke the function under test — a free function call, or a method call against an
/// already-built receiver — so [`run_loop`] can execute a targeted input the same way
/// `function_cases`/`method_record` executed the initial batch.
pub(super) enum CallTarget<'a> {
    Function,
    Method { class: &'a str, ctor_args: &'a [Value] },
}

/// Run the predicate-targeted coverage loop for one function, extending `cases` in place. Each
/// still-uncovered, synthesizable outcome gets exactly one round of attempts across every override
/// set its predicate(s) produce — `synthesize` is deterministic, so retrying an outcome whose
/// candidates already ran would only reproduce the identical, already-failed case, starving
/// outcomes that budget cut off before their first attempt. Iterates until every observable
/// outcome is covered, a round attempts nothing new (every uncovered outcome already had its
/// round), or `cases.len()` reaches `opts.max_inputs` (the TOTAL per-function case budget once
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
            let targetable = match outcome.evidence {
                OutcomeEvidence::Arc(..) | OutcomeEvidence::Line(_) => true,
                OutcomeEvidence::FineGrained(..) => bp.kind == BranchKind::BoolOp,
                OutcomeEvidence::Unobservable => false,
            };
            if !targetable {
                continue;
            }
            let Some(want) = want_for_outcome(bp.kind, &outcome.outcome, &predicates, bp.line) else { continue };
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
    let fine = fine_targets(&sig.branch_points);
    let call = |pos: &[Value], kw: &[(String, Value)]| -> Result<CallResult, String> {
        match &target {
            CallTarget::Function => sandbox.call(src, &sig.name, pos, kw, &fine, Limits::default()),
            CallTarget::Method { class, ctor_args } => {
                sandbox.call_method(src, (class, ctor_args), &sig.name, (pos, kw), &fine, Limits::default())
            }
        }
    };

    loop {
        if cases.len() >= opts.max_inputs || super::deadline_passed(opts.deadline) {
            break;
        }
        let uncovered = uncovered_outcomes(sig, cases);
        if uncovered.is_empty() {
            break;
        }
        let mut executed_this_round = false;
        'outer: for (line, kind, outcome_name) in &uncovered {
            let key = (*line, outcome_name.clone());
            if !ctx.synthesizable.contains(&key) || ctx.attempted.contains(&key) {
                continue;
            }
            let Some(want) = want_for_outcome(*kind, outcome_name, &predicates, *line) else { continue };
            for overrides in candidate_values(sig, &predicates, *line, want, opts.domain) {
                if cases.len() >= opts.max_inputs || super::deadline_passed(opts.deadline) {
                    break 'outer;
                }
                let Some(gi) = build_targeted_input(sig, &template, &overrides) else { continue };
                let result = call(&gi.positional, &gi.kwargs)?;
                let mut case = build_case(sig, &gi, ctor_args_for_case.clone(), &result, CaseSource::Generated);
                if case.outcome == "raised" {
                    case.minimized = minimize_raised(&case, &gi, opts.domain, |pos, kw| call(pos, kw))?;
                }
                cases.push(case);
                executed_this_round = true;
            }
            // Every candidate for this outcome ran exactly once this round; `synthesize` is
            // deterministic, so a retry next round would only reproduce the same failed case.
            ctx.attempted.insert(key);
        }
        if !executed_this_round {
            break;
        }
    }

    Ok(ctx)
}
