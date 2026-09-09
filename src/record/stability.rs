//! `record --stability-runs N`: re-executes every case (generated, cover-loop, and replayed
//! alike) until it has run `N` times total, dropping any case whose runs disagree so a consumer
//! building test pools gets only deterministic cases. See [`stabilize_cases`].

use serde::Serialize;
use serde_json::Value;

use crate::exec::{CallInput, CallResult, Limits, Sandbox};
use crate::generate::GenInput;
use crate::model::EffectSignature;

use super::{Budget, Case, CaseSource, build_case};

/// The closed count of cases `--stability-runs` dropped from a function's `cases` — see
/// [`stabilize_cases`]. Present on [`super::FunctionRecord`] exactly when `--stability-runs` was
/// passed.
#[derive(Serialize, Default)]
pub struct DroppedCases {
    /// A case whose outcome/return/raises/mutations/stdout/stderr disagreed across its re-runs.
    pub unstable: usize,
    /// A case whose original outcome was already `error` (a resource kill or other harness
    /// failure) — dropped without re-execution, since an artifact of the sandbox is never a
    /// stable observation.
    pub resource: usize,
}

/// Re-execute every `returned`/`raised` case in `cases` until it has run `runs` times total
/// (the case's own execution counts as run 1), dropping any case whose runs disagree on
/// `outcome`/`ret`/`raises`/`mutations`/`stdout`/`stderr` — see [`cases_agree`]. A case whose
/// outcome is already `error` is dropped without re-execution: a resource kill (or other harness
/// failure) is an artifact of the sandbox, never a stable observation. Re-execution goes through
/// the same call path (free function, or method against the case's own `ctor_args`) the case
/// itself ran on. Line/arc data from a stable case's re-runs is discarded — the kept case is the
/// original, whose arcs already contributed to coverage/branch accounting; re-run traces are
/// diagnostics, not observations, and value-level agreement is what stability checks.
///
/// `budget` (the `--time-budget` cap, if any) applies only to `CaseSource::Generated` cases: a
/// generated case whose stability re-runs haven't started yet when the deadline has already
/// passed (`budget.expired()`) is kept as-is, unverified, rather than dropped — the case itself
/// was already recorded before the deadline tripped, and "stop starting new generated work" means
/// the re-run checks, not discarding what's already there. A replayed case's re-runs always
/// execute in full, regardless of the deadline: external evidence must not silently vanish.
///
/// Each round splits the cases still alive into a replayed batch and a generated batch before it
/// dispatches either. The replayed batch always runs at `Limits::default()` and never leases from
/// `budget` — it runs every round, in full, until `runs` is reached, exactly as it would with no
/// `--time-budget` at all. The generated batch leases wall limits from `budget`; when a lease is
/// refused, that round's still-alive generated cases are kept as-is, unverified, and drop out of
/// re-running for good — the replayed batch is unaffected and keeps re-running in later rounds. A
/// re-run that comes back `deadline_skipped` (expected only for a generated case, since a replayed
/// one no longer leases a shortened batch limit) never disagreed with anything — it is kept as-is,
/// unverified, not counted as unstable.
pub(super) fn stabilize_cases(
    sandbox: &dyn Sandbox,
    src: &str,
    sig: &EffectSignature,
    cases: Vec<Case>,
    runs: usize,
    budget: &Budget,
) -> Result<(Vec<Case>, DroppedCases), String> {
    let mut kept = Vec::with_capacity(cases.len());
    let mut dropped = DroppedCases::default();
    // Whether a case's re-runs have started is decided ONCE, up front, in original iteration
    // order — exactly as the pre-batching per-case loop did (it never rechecked the deadline
    // once a case's own `for _ in 1..runs` loop had begun). Round-batching below then runs
    // every "started" case through every remaining round together, so this single up-front
    // decision reproduces the old timing behavior instead of only approximating it.
    let mut alive: Vec<Case> = Vec::with_capacity(cases.len());
    for case in cases {
        if case.outcome == "error" {
            dropped.resource += 1;
            continue;
        }
        if case.source == CaseSource::Generated && budget.expired() {
            kept.push(case);
            continue;
        }
        alive.push(case);
    }

    for _ in 1..runs {
        if alive.is_empty() {
            break;
        }

        // Splitting must not reorder `kept`/the surviving list relative to what a single
        // undivided batch would have produced: `is_replay` records each case's position in
        // `this_round`'s original order, and the two partitions below (`Vec::partition`
        // preserves relative order within each output) are walked back in lockstep with it,
        // so the reassembly below reproduces the pre-split interleaving exactly.
        let this_round = std::mem::take(&mut alive);
        let is_replay: Vec<bool> = this_round.iter().map(|c| c.source == CaseSource::Replay).collect();
        let (replay_batch, generated_batch): (Vec<Case>, Vec<Case>) =
            this_round.into_iter().partition(|c| c.source == CaseSource::Replay);

        // Replayed cases never lease from `budget`: external evidence must not silently vanish
        // regardless of the deadline, in a stability re-run exactly as in the initial recording.
        let replay_results = dispatch_batch(sandbox, src, sig, &replay_batch, Limits::default())?;

        // Generated cases lease as before; a refused lease removes them from further re-running
        // (they are kept, unverified) without touching the replayed batch above.
        let generated_lease = if generated_batch.is_empty() { None } else { budget.lease() };
        let generated_results = match generated_lease {
            Some(limits) => Some(dispatch_batch(sandbox, src, sig, &generated_batch, limits)?),
            None => None,
        };

        let mut next_alive = Vec::with_capacity(replay_batch.len() + generated_batch.len());
        let mut replay_iter = replay_batch.into_iter().zip(replay_results);
        let mut generated_iter = generated_batch.into_iter();
        let mut generated_results_iter = generated_results.map(IntoIterator::into_iter);
        for replayed in is_replay {
            if replayed {
                let (case, result) = replay_iter.next().expect("is_replay tracked this case as replayed");
                process_rerun(sig, case, &result, &mut next_alive, &mut kept, &mut dropped, budget);
            } else {
                let case = generated_iter.next().expect("is_replay tracked this case as generated");
                match generated_results_iter.as_mut().and_then(Iterator::next) {
                    Some(result) => {
                        process_rerun(sig, case, &result, &mut next_alive, &mut kept, &mut dropped, budget);
                    }
                    None => kept.push(case),
                }
            }
        }
        alive = next_alive;
    }
    kept.extend(alive);
    Ok((kept, dropped))
}

/// Dispatches one stability-round batch (either the replayed or the generated cases still
/// alive) at `limits`. A stability re-run's traces are discarded (see [`stabilize_cases`]'s doc
/// comment) — fine-grained hits would be too, so `fine_targets` stays empty and the worker never
/// pays opcode-tracing overhead here.
fn dispatch_batch(
    sandbox: &dyn Sandbox,
    src: &str,
    sig: &EffectSignature,
    batch: &[Case],
    limits: Limits,
) -> Result<Vec<CallResult>, String> {
    if batch.is_empty() {
        return Ok(Vec::new());
    }
    let kwargs_owned: Vec<Vec<(String, Value)>> = batch
        .iter()
        .map(|c| c.kwargs.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .collect();
    let call_inputs: Vec<CallInput> = batch
        .iter()
        .zip(&kwargs_owned)
        .map(|(c, kw)| (c.input.as_slice(), kw.as_slice()))
        .collect();
    match &batch[0].ctor_args {
        Some(ctor_args) => {
            let class = sig
                .owner
                .as_deref()
                .ok_or_else(|| format!("method {:?} has no owning class", sig.name))?;
            sandbox.call_batch(
                src,
                &sig.name,
                &call_inputs,
                Some((class, ctor_args.as_slice())),
                &[],
                limits,
            )
        }
        None => sandbox.call_batch(src, &sig.name, &call_inputs, None, &[], limits),
    }
}

/// Applies one case's re-run `result`: a `deadline_skipped` result (never expected for a
/// replayed case now that it always runs at `Limits::default()`, but handled the same way if it
/// somehow occurs) marks the budget hit and keeps the case as-is, unverified; otherwise the
/// re-run is compared against the original and the case either survives into `next_alive` or is
/// counted as unstable and dropped.
fn process_rerun(
    sig: &EffectSignature,
    case: Case,
    result: &CallResult,
    next_alive: &mut Vec<Case>,
    kept: &mut Vec<Case>,
    dropped: &mut DroppedCases,
    budget: &Budget,
) {
    if result.is_deadline_skipped() {
        // Never re-enters `alive` for a later round: a `deadline_skipped` re-run is not "this
        // round's attempt failed, try again next round" — the case must leave the round loop
        // entirely and be kept exactly as it stands, unverified, the same as the up-front
        // `budget.expired()` path above. Anything else risks ending with fewer than `runs`
        // executions while being treated as fully verified, or being dropped as unstable on the
        // strength of an incomplete comparison.
        budget.mark_hit();
        kept.push(case);
        return;
    }
    let kwargs = case.kwargs.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    let gen_input = GenInput { positional: case.input.clone(), kwargs };
    let rerun = build_case(sig, &gen_input, case.ctor_args.clone(), result, case.source);
    if cases_agree(&case, &rerun) {
        next_alive.push(case);
    } else {
        dropped.unstable += 1;
    }
}

/// Whether two runs of the same case agree on every field that counts as an observation:
/// `outcome`, `ret`, `raises`, `mutations`, `stdout`, `stderr`. Traced `lines`/`arcs` are
/// deliberately excluded — they're diagnostics, not part of the observed behavior.
///
/// Comparison is EXACT structural equality, not the float-tolerant `value_eq`: a re-run of the
/// same computation on the same interpreter is bit-identical, so any numeric difference is
/// nondeterminism. A relative tolerance would swallow large-magnitude jitter — two
/// `time.time_ns()` results differ by a relative ~1e-12 and must still count as disagreement.
fn cases_agree(a: &Case, b: &Case) -> bool {
    a.outcome == b.outcome
        && a.raises == b.raises
        && a.stdout == b.stdout
        && a.stderr == b.stderr
        && a.ret == b.ret
        && a.mutations.len() == b.mutations.len()
        && a.mutations.iter().zip(&b.mutations).all(|(x, y)| {
            x.target == y.target && x.before == y.before && x.after == y.after
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{CaseSource, ObservedMutation};
    use serde_json::json;

    fn returned_case(ret: Value, mutations: Vec<ObservedMutation>) -> Case {
        Case {
            input: vec![json!(1)],
            kwargs: serde_json::Map::new(),
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
            fine_hits: Vec::new(),
        }
    }

    fn raised_case(ty: &str) -> Case {
        Case {
            input: vec![json!(1)],
            kwargs: serde_json::Map::new(),
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
            fine_hits: Vec::new(),
        }
    }

    fn mutation(target: &str, before: Value, after: Value) -> ObservedMutation {
        ObservedMutation { target: target.to_string(), before, after }
    }

    #[test]
    fn equal_returns_agree() {
        let a = returned_case(json!([1, 2, 3]), Vec::new());
        let b = returned_case(json!([1, 2, 3]), Vec::new());
        assert!(cases_agree(&a, &b));
    }

    #[test]
    fn differing_returns_disagree() {
        let a = returned_case(json!(1), Vec::new());
        let b = returned_case(json!(2), Vec::new());
        assert!(!cases_agree(&a, &b));
    }

    #[test]
    fn equal_raises_agree() {
        let a = raised_case("ValueError");
        let b = raised_case("ValueError");
        assert!(cases_agree(&a, &b));
    }

    #[test]
    fn differing_raises_disagree() {
        let a = raised_case("ValueError");
        let b = raised_case("TypeError");
        assert!(!cases_agree(&a, &b));
    }

    #[test]
    fn equal_mutations_agree() {
        let a = returned_case(json!(null), vec![mutation("xs", json!([1]), json!([1, 2]))]);
        let b = returned_case(json!(null), vec![mutation("xs", json!([1]), json!([1, 2]))]);
        assert!(cases_agree(&a, &b));
    }

    #[test]
    fn differing_mutations_disagree() {
        let a = returned_case(json!(null), vec![mutation("xs", json!([1]), json!([1, 2]))]);
        let b = returned_case(json!(null), vec![mutation("xs", json!([1]), json!([1, 3]))]);
        assert!(!cases_agree(&a, &b));
    }

    #[test]
    fn missing_vs_present_mutation_disagrees() {
        let a = returned_case(json!(null), vec![mutation("xs", json!([1]), json!([1, 2]))]);
        let b = returned_case(json!(null), Vec::new());
        assert!(!cases_agree(&a, &b));
    }

    #[test]
    fn differing_stdout_disagrees() {
        let mut a = returned_case(json!(null), Vec::new());
        a.stdout = Some("hi\n".to_string());
        let mut b = returned_case(json!(null), Vec::new());
        b.stdout = Some("bye\n".to_string());
        assert!(!cases_agree(&a, &b));
    }

    #[test]
    fn differing_outcome_disagrees() {
        let a = returned_case(json!(null), Vec::new());
        let b = raised_case("ValueError");
        assert!(!cases_agree(&a, &b));
    }

    #[test]
    fn differing_traces_alone_do_not_disagree() {
        let mut a = returned_case(json!(1), Vec::new());
        a.lines = vec![1, 2, 3];
        a.arcs = vec![(1, 2)];
        let mut b = returned_case(json!(1), Vec::new());
        b.lines = vec![1, 2];
        b.arcs = vec![(1, 3)];
        assert!(cases_agree(&a, &b));
    }
}
