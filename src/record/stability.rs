//! `record --stability-runs N`: re-executes every case (generated, cover-loop, and replayed
//! alike) until it has run `N` times total, dropping any case whose runs disagree so a consumer
//! building test pools gets only deterministic cases. See [`stabilize_cases`].

use serde::Serialize;
use serde_json::Value;

use crate::exec::{CallInput, Sandbox};
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
/// Each round's batch leases wall limits from `budget` before it runs; when a lease is refused,
/// every case still alive is kept as-is, unverified, and re-running stops. A re-run that comes
/// back `deadline_skipped` never disagreed with anything — it is kept as-is, unverified, not
/// counted as unstable.
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
        let Some(limits) = budget.lease() else { break };
        let kwargs_owned: Vec<Vec<(String, Value)>> = alive
            .iter()
            .map(|c| c.kwargs.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .collect();
        let call_inputs: Vec<CallInput> = alive
            .iter()
            .zip(&kwargs_owned)
            .map(|(c, kw)| (c.input.as_slice(), kw.as_slice()))
            .collect();
        let ctor_args_for_batch = alive[0].ctor_args.clone();
        // A stability re-run's traces are discarded (see the doc comment above) — fine-grained
        // hits would be too, so `fine_targets` stays empty and the worker never pays
        // opcode-tracing overhead here.
        let results = match &ctor_args_for_batch {
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
                )?
            }
            None => sandbox.call_batch(src, &sig.name, &call_inputs, None, &[], limits)?,
        };

        let mut next_alive = Vec::with_capacity(alive.len());
        for ((case, kwargs), result) in alive.into_iter().zip(kwargs_owned).zip(&results) {
            if result.is_deadline_skipped() {
                budget.mark_hit();
                next_alive.push(case);
                continue;
            }
            let gen_input = GenInput { positional: case.input.clone(), kwargs };
            let rerun = build_case(sig, &gen_input, case.ctor_args.clone(), result, case.source);
            if cases_agree(&case, &rerun) {
                next_alive.push(case);
            } else {
                dropped.unstable += 1;
            }
        }
        alive = next_alive;
    }
    kept.extend(alive);
    Ok((kept, dropped))
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
