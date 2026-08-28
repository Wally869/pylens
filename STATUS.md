# Status

Tests: 267. `validate examples`: 0 hard defects, 11 soft, coverage 90/107 lines.

## Done in the last pass (2026-08-28/29 — the typing + test-pool readiness push)

Plan and bench artifacts: `temp/TYPING_TESTGEN_PLAN.md`, `temp/bench/` (500-function
clir-corpus pilot: exporter, driver, per-function results).

- **Replay mode** — `record --replay <cases.json>` executes external input tuples through the
  sandbox (`source: "replay"`, never shrunk). External inputs can now refute inferred types.
- **Branch accounting** — the worker traces line arcs; `collect/branches.rs` enumerates every
  branch point; records carry `branches`/`branch_coverage` with each outcome `covered`,
  `uncovered` (with a `reason`), or `unobservable_line_granularity`. Closed accounting, no
  silent gaps.
- **`--cover-branches`** — predicate-targeted synthesis (`generate/predicate.rs`,
  `record/cover.rs`): uncovered outcomes drive satisfying/violating inputs in a loop under the
  `--inputs` total budget.
- **`--value-domain`** — declarative restriction of generated and shrunk values.
- **`--stability-runs N`** — drops nondeterministic cases (exact structural comparison — a
  float-tolerant comparison swallowed `time_ns` jitter and was fixed); closed `dropped_cases`
  count; coverage computed from survivors.
- **Honesty flags** — `observable` on io claims, `validated: false` + summary count for
  never-executed functions, `output_type_coverage: full|partial` with `unobserved_returns`.
- **Precision batch** — aliased from-imports resolve in the model table; the rebind freeze is
  flow-sensitive behind a top-level statement-order dominance gate (a pre-rebind call resolving
  through the post-rebind shape was caught as hard defects by `temp/probe_flow2.py` and gated);
  cross-file superclass resolution in project mode; in-body imports predict
  `ImportError`/`ModuleNotFoundError`.
- **Bench-driven implicit-raise rules** (500-function corpus pilot, hard defects 374 → 44,
  every survivor a corpus export artifact): attribute loads predict `AttributeError` unless
  proven (class-level name, method name, or `__init__` self-assignment; parameters never
  proven); a builtin raise table (`analyze/builtin_raises.rs`); tuple-unpack `ValueError`;
  iteration-protocol `TypeError`; negative-shift `ValueError`; argument-contract `TypeError`
  on readonly method calls.
- **Typing accuracy** — sequence-protocol evidence votes `union(seq, str)` for parameters
  (locals and list-specific evidence keep `seq`); declared annotations rank generation
  candidates first without narrowing inference.

## Pilot results (500 clir functions, before → after the fixes)

- Soundness: 374 hard defects → 44, all 44 `NameError` from truncated corpus exports.
- Typing refutation by external inputs: 54.4% → 5.3% of checked values.
- Branch outcomes covered: 76.9% → 80.6% observable (residual: 290 `no_synthesizer`).
- Trivial-solver rejection: 75.5% → **89.1%** (the clir suites themselves: 77.4%).
- Coverage dominance: pool ≥ clir suite on 362/371 functions.
- Determinism: 0 unstable cases. Oracle census: 199 CLIR-interp-vs-CPython disagreements in
  27 functions (a clir-side finding). Runtime: median 1.3 s/function over 3 invocations,
  0.3 s/function wall at 6 workers.

## Performance pass (T8b, 2026-08-29) — done except the closing measurement

Landed: project-mode `--replay`; `--time-budget`; the record API collapsed to
`record_file`/`record_with_signatures` behind `RecordFlags` (files split:
`record/case.rs`, `analyze/proof.rs`); the worker no longer traces foreign
frames (16x on stdlib-heavy functions); batched case execution over one IPC
round trip; a primed-fork chain (the module execs once per batch, one
grandchild per case, isolation proven by test); request-gated per-stage
timing (`temp/bench/stage_profile.py` renders the exact breakdown).

Measured cost structure per case (1,208 real cases): 1.33 ms median — 65%
fork/pipe/waitpid (the price of per-case isolation), 20% the traced call,
10% amortized module exec, 5% serde. Corpus workload: ~100-115 ms/function
at 4 workers (WSL saturates near 4 jails; native Linux should scale further).
Corpus arithmetic: 103k functions ≈ 3 h at 4 workers.

In flight: `--base-inputs` (the seed batch split from the total budget, so
`--cover-branches` spends the rest only where the accounting demands) and
the module-load probe folded into the first batch. The final speed check
and the closing numbers land here after that.

## Open work

1. Same-line constructs (`ternary`, boolops, inline `if`) stay `unobservable_line_granularity`;
   upgrading observation needs finer-than-line tracing.
2. The full 103k corpus run and the `SCHEMA_VERSION` 1.0 freeze — pending the user's GO.

## Measurements to repeat after a change

`temp/callee_freq.md` holds the stdlib method and baselines; `temp/bench/run_pilot.py` re-runs
the 500-function corpus pilot (gates G1–G8). Repeat the pilot after any analyzer change that
moves precision or soundness.
