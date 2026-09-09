//! One per-function `--time-budget` deadline: leases wall limits for each piece of generated
//! work, so no combination of case generation, the `--cover-branches` loop, `--stability-runs`
//! re-runs, or shrinking can run past `start + budget` in aggregate.

use std::cell::Cell;
use std::time::{Duration, Instant};

use crate::exec::{Limits, default_call_timeout};

/// Once less than this remains before the deadline, nothing more may start — avoids leasing a
/// sliver of time too small for even a trivial call to complete. Also the floor `--time-budget`
/// itself must clear (see `crate::record::MIN_TIME_BUDGET`, re-exported from here): a budget at
/// or under this floor would refuse every lease immediately and record nothing, so the CLI
/// rejects it up front instead of silently producing an empty result.
pub const MIN_TIME_BUDGET: Duration = Duration::from_secs(1);

/// A function's `--time-budget` deadline, and whether it has ever stopped work from starting.
pub(crate) struct Budget {
    deadline: Option<Instant>,
    hit: Cell<bool>,
}

impl Budget {
    /// Starts the deadline now. `None` means unlimited: every [`Budget::lease`] then returns
    /// `Limits::default()` and neither [`Budget::expired`] nor [`Budget::was_hit`] ever trip.
    pub(crate) fn new(budget: Option<Duration>) -> Self {
        Budget {
            deadline: budget.map(|d| Instant::now() + d),
            hit: Cell::new(false),
        }
    }

    /// Wall limits for one piece of generated work about to start, or `None` when nothing more
    /// may start (marks the budget hit).
    pub(crate) fn lease(&self) -> Option<Limits> {
        let Some(deadline) = self.deadline else {
            return Some(Limits::default());
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining <= MIN_TIME_BUDGET {
            self.hit.set(true);
            return None;
        }
        Some(Limits {
            per_call: default_call_timeout().min(remaining),
            batch: Some(remaining),
        })
    }

    /// Whether the deadline has passed. Marks the budget hit when it has.
    pub(crate) fn expired(&self) -> bool {
        match self.deadline {
            None => false,
            Some(deadline) => {
                let expired = Instant::now() >= deadline;
                if expired {
                    self.hit.set(true);
                }
                expired
            }
        }
    }

    /// Whether the deadline has ever stopped work from starting — via a refused [`Budget::lease`],
    /// a tripped [`Budget::expired`], or an explicit [`Budget::mark_hit`].
    pub(crate) fn was_hit(&self) -> bool {
        self.hit.get()
    }

    /// Record that the deadline stopped work discovered another way — e.g. a batch item came
    /// back `deadline_skipped` even though its batch was successfully leased (the worker's own
    /// soft deadline fired mid-batch, after dispatch).
    pub(crate) fn mark_hit(&self) {
        self.hit.set(true);
    }
}
