# Plan — accurate typing and covering test pools, validated on the clir corpus

Written 2026-08-28, reframed the same day. Owner: pylens. The clir corpus
(`C:/Projects/ai/clir`, 103k executable functions with independent test
suites) is the **validation bench** for pylens — not the other way around.
Companion measurements: `temp/pilot_analyze.json`, `temp/pilot_record.json`
(40-function pilot, this date).

## 1. Goal — what "up to par" means

pylens must produce, for an arbitrary Python function:

1. **Accurate typing information** — parameter and return types that real
   executions corroborate, over all reachable paths, not only the happy
   path.
2. **A test pool** — input tuples with recorded expected outputs, with
   executed-line coverage as high as the function permits, deterministic,
   and free of degenerate suites (a suite a constant or passthrough
   function can pass).

Both claims must be **validated against an external dataset**, not against
pylens's own generation. The clir corpus is that dataset.

## 2. Why the clir corpus is the right bench

- **Scale and realism**: 103,121 executable functions scraped from real
  repositories (869,996 raw; 290,826 valid transpiles), in
  `pipeline.db` (local read copy: `temp/pipeline.db`).
- **Independent test suites**: each function carries `{inputs, expected}`
  cases produced by a different generator and a different runtime (the
  CLIR interpreter). These are inputs pylens did **not** derive from its
  own shape inference — external evidence that can refute pylens.
- **A second executor**: the CLIR interpreter gives a cross-check on
  expected values (with known, documented divergences).
- **A hard consumer**: the corpus's own defects (degenerate suites,
  interpreter-oracle drift) define concrete quality bars for pylens to
  beat.

## 3. Where pylens falls short today (the gap list)

Measured on a 40-function pilot from the corpus (12 inputs, single
process): `analyze` 45 ms / 40 files; `record` 0.8 s per function; 0
unresolved effects; 47/90 params concretely shaped; mean line coverage
0.91 with 31/40 functions at 100%; 251 returned / 173 raised / 8 resource
cases.

- **G1 — No external falsifier.** Generation draws from the shapes the
  analysis inferred, so a typing claim justified by generated inputs is
  unfalsifiable (documented pylens limit). pylens cannot currently replay
  an input corpus somebody else wrote.
- **G2 — Fixed input budget.** `--inputs N` is a flat cap. Coverage is
  reported but never drives generation; 9/40 pilot functions stay below
  full line coverage while budget is wasted elsewhere.
- **G3 — No value-domain control.** A consumer cannot restrict generated
  values to a domain (JSON-safe, no dicts, size caps). ~40% of pilot
  cases burned budget raising `TypeError` from wrong-typed inputs.
- **G4 — Declared annotations unused by the sampler.** Inference correctly
  ignores annotations; generation may use them freely as ranking hints and
  does not.
- **G5 — No determinism gate.** A case is recorded once; an unstable
  function (time, randomness, iteration order) produces an unreliable
  expected value.
- **G6 — Known precision items** (STATUS.md): aliased from-import misses
  the model table; the rebind freeze is flow-insensitive. On the executable
  corpus subset these did not fire (0 unresolved effects); they matter for
  the broader 290k set.

## 4. Feature work (pylens, in order)

Each item ends with `cargo test` + `pylens validate examples` at 0 hard
defects before the next starts.

- **F1. Replay mode** (closes G1). `pylens record --replay <cases.json>`:
  execute externally supplied input tuples through the sandbox and record
  them exactly like generated cases. This turns any input corpus into a
  falsifier for the inferred types, and it lets a consumer re-oracle an
  existing test suite under CPython.
- **F2. Coverage-driven generation** (closes G2). Generate ranked batches
  until executed-line coverage stops improving for K batches or a hard
  budget hits (`--target-coverage`, `--max-inputs`). Keep a minimal
  covering subset plus boundary inputs. Honest limit, stated in the docs:
  path coverage is not decidable; executed-line coverage plus boundary
  values is the measurable proxy for "all paths".
- **F3. Value-domain profile** (closes G3). `--value-domain <file>`: a
  declarative restriction on generated values (allowed scalar types, list
  element types, size caps, no dict/set/tuple/bytes). pylens stays
  domain-independent; the consumer supplies the profile.
- **F4. Annotation hints for the sampler** (closes G4). Declared types
  rank candidate values first; inference remains annotation-free.
- **F5. Stability runs** (closes G5). `--stability-runs N`: execute each
  kept case N times; drop cases whose outcome or value differs; drop
  `stage:"resource"` errors (existing policy).
- **F6. Precision items** (closes G6, lower priority): the aliased
  from-import table miss, then the flow-sensitive rebind treatment.

Non-goals: no consumer-specific types or formats inside pylens (mapping
Shape → a consumer's type system lives with the consumer); no test-file
emitters — pylens emits JSON records.

## 5. Validation protocol on the clir corpus

The bench answers two questions: *is the typing right?* and *are the test
pools good?* Falsification first.

1. **Typing vs external inputs** (needs F1). Replay every clir test case
   through pylens's sandbox. An input the function **accepts** while the
   inferred parameter type excludes it refutes that type. An input type
   that always raises `TypeError` corroborates the exclusion. Deliverable:
   refutation rate per shape kind, target ≈0 on `int/float/bool/str/seq`.
2. **Soundness at scale.** `pylens validate` over corpus batches: hard
   defects must stay 0 on all 103k functions; report the soft rate. Every
   hard defect is a pylens bug to fix before any downstream use.
3. **Coverage dominance.** Per function: executed-line coverage of the
   pylens pool vs the existing clir suite. The pylens pool must dominate
   in aggregate; every regression gets a reason.
4. **Trivial-solver rejection.** Run constant and passthrough
   pseudo-solvers (`return arg_i`, `return None`, `return 0`, identity)
   against each pylens pool; a pool that any of them passes is degenerate.
   Report the rejection rate against the clir suites' own rate (known
   weak: clir ISSUES #27).
5. **Determinism.** Re-run a full batch; 0 cases change outcome or value
   after F5.
6. **Oracle cross-check.** Where pylens (CPython) and the CLIR interpreter
   disagree on an expected value, classify: CPython is ground truth for
   Python; the disagreement census is a bench finding, not a pylens
   defect (clir ISSUES #24 sizes it at ~1.2% of bases today).
7. **Robustness.** 103k real functions through analyze and record:
   0 crashes; timeouts and resource stops are recorded outcomes, never
   losses of the batch.

## 6. Milestones

- **M0 — features**: F1→F5 landed and validated on `examples/`.
- **M1 — bench pilot, ~500 corpus functions**: run the full §5 protocol;
  fix what it finds; publish the numbers (typed-param share, refutation
  rate, coverage delta, rejection rate, seconds per function sequential
  and parallel) and a full-corpus runtime projection. Today's
  extrapolation: ~0.8 s/function sequential ≈ 23 h for 103k; parallel
  record must be measured here.
- **M2 — full-corpus validation run**: all 103k, chunked and resumable
  (outputs keyed by function id; a crashed chunk re-runs idempotently).
  Exit: the §5 gates hold at scale. This run's outputs are the material a
  consumer builds on.
- **F6 + the 290k set** come after M2 if the broader corpus is wanted.

## 7. Out of scope — consumer integration

Mapping shapes to CLIR's type system, emitting CLIR `{inputs, expected}`
suites, importing into `pipeline.db`, v56 assembly, typed-GCD wiring, and
re-baselining decisions are clir-side work, planned in the clir repo when
pylens passes the bench. The consumer-side questions recorded earlier
(oracle policy, mutation-only functions, raising cases as a side channel)
move with it.
