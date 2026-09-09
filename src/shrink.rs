//! Greedy shrinking of a raised case's input toward a smaller vector that still raises the same
//! exception type. A reporting aid attached to `record` output (`Case::minimized`) — it never
//! feeds `validate`, which consumes only the original observation.

use serde_json::Value;

use crate::exec::{CallResult, Limits};
use crate::generate::{ValueDomain, shrink_candidates};
use crate::record::Budget;

/// Hard budget on extra jailed calls spent shrinking one failing case. Shrinking spends exactly
/// one jailed call per candidate tried, so this caps the worst-case extra cost of minimizing a
/// single case.
pub const SHRINK_BUDGET: usize = 32;

/// A shrunk `(positional, keyword-only)` argument pair.
type ShrunkArgs = (Vec<Value>, Vec<(String, Value)>);

/// The context shared, unchanged, across every candidate call of one [`shrink_case`] run —
/// bundled to keep `shrink_pass_positional`/`shrink_pass_kwargs`'s argument counts under
/// clippy's `too_many_arguments` threshold, the same way [`crate::exec::CallInput`] bundles a
/// call's arguments.
struct ShrinkCtx<'a> {
    original_exception: &'a str,
    domain: Option<&'a ValueDomain>,
    budget: &'a Budget,
}

/// Greedily shrink `positional`/`kwargs` toward smaller values while re-execution (via `call`)
/// keeps raising `original_exception`. Tries one argument at a time (positional first, then
/// keyword-only): a candidate is accepted iff re-execution raises the same exception type as
/// `original_exception`, and acceptance restarts the scan of that argument's candidate list at
/// the newly-accepted (smaller) value. A full pass over every argument with no accepted
/// candidate ends the search, bounded by [`SHRINK_BUDGET`] jailed calls total.
///
/// A resource-killed or otherwise-mismatched re-execution rejects the candidate — it is never
/// treated as a new observation, only as "this candidate doesn't reproduce the same failure".
/// Returns `None` if no candidate was ever accepted.
///
/// `domain`, when given, is the same profile that restricted the original generated input — see
/// `pylens record --value-domain`. A minimized input must stay in-domain, so every shrink
/// candidate is filtered through it (see [`shrink_candidates`]).
///
/// `budget` leases wall limits for each candidate call — see [`Budget::lease`]. When a lease is
/// refused, shrinking stops immediately and returns the best reduction found so far: every
/// already-accepted candidate was already verified by its own successful call, so stopping early
/// never returns an error or an unverified partial.
pub(crate) fn shrink_case(
    original_exception: &str,
    positional: &[Value],
    kwargs: &[(String, Value)],
    domain: Option<&ValueDomain>,
    budget: &Budget,
    mut call: impl FnMut(&[Value], &[(String, Value)], Limits) -> Result<CallResult, String>,
) -> Result<Option<ShrunkArgs>, String> {
    let mut cur_pos = positional.to_vec();
    let mut cur_kw = kwargs.to_vec();
    let ctx = ShrinkCtx { original_exception, domain, budget };
    let mut call_budget = SHRINK_BUDGET;
    let mut shrank = false;
    let mut exhausted = false;

    loop {
        let pos_improved =
            shrink_pass_positional(&mut cur_pos, &cur_kw, &ctx, &mut call_budget, &mut exhausted, &mut call)?;
        shrank |= pos_improved;
        if exhausted {
            break;
        }
        let kw_improved =
            shrink_pass_kwargs(&cur_pos, &mut cur_kw, &ctx, &mut call_budget, &mut exhausted, &mut call)?;
        let improved = pos_improved || kw_improved;
        shrank |= kw_improved;
        if !improved || call_budget == 0 || exhausted {
            break;
        }
    }

    Ok(if shrank { Some((cur_pos, cur_kw)) } else { None })
}

fn shrink_pass_positional(
    cur_pos: &mut [Value],
    cur_kw: &[(String, Value)],
    ctx: &ShrinkCtx,
    call_budget: &mut usize,
    exhausted: &mut bool,
    call: &mut impl FnMut(&[Value], &[(String, Value)], Limits) -> Result<CallResult, String>,
) -> Result<bool, String> {
    let mut improved = false;
    for i in 0..cur_pos.len() {
        if *call_budget == 0 || *exhausted {
            break;
        }
        for cand in shrink_candidates(&cur_pos[i], ctx.domain) {
            if *call_budget == 0 || *exhausted {
                break;
            }
            let Some(limits) = ctx.budget.lease() else {
                *exhausted = true;
                break;
            };
            *call_budget -= 1;
            let mut trial = cur_pos.to_vec();
            trial[i] = cand.clone();
            if raises_same(&call(&trial, cur_kw, limits)?, ctx.original_exception) {
                cur_pos[i] = cand;
                improved = true;
                break;
            }
        }
    }
    Ok(improved)
}

fn shrink_pass_kwargs(
    cur_pos: &[Value],
    cur_kw: &mut [(String, Value)],
    ctx: &ShrinkCtx,
    call_budget: &mut usize,
    exhausted: &mut bool,
    call: &mut impl FnMut(&[Value], &[(String, Value)], Limits) -> Result<CallResult, String>,
) -> Result<bool, String> {
    let mut improved = false;
    for i in 0..cur_kw.len() {
        if *call_budget == 0 || *exhausted {
            break;
        }
        for cand in shrink_candidates(&cur_kw[i].1, ctx.domain) {
            if *call_budget == 0 || *exhausted {
                break;
            }
            let Some(limits) = ctx.budget.lease() else {
                *exhausted = true;
                break;
            };
            *call_budget -= 1;
            let mut trial = cur_kw.to_vec();
            trial[i].1 = cand.clone();
            if raises_same(&call(cur_pos, &trial, limits)?, ctx.original_exception) {
                cur_kw[i].1 = cand;
                improved = true;
                break;
            }
        }
    }
    Ok(improved)
}

/// Whether a re-execution matches the original failure: a semantic raise (not a harness/resource
/// error) of exactly `expected`'s type.
fn raises_same(r: &CallResult, expected: &str) -> bool {
    !r.ok && r.error.is_none() && r.exception.as_ref().map(|e| e.ty.as_str()) == Some(expected)
}
