# Plan — pylens readiness for the CLIR dataset pass

Written 2026-08-28. Owner: pylens. Consumer: CLIR (`C:/Projects/ai/clir`,
STATUS.md items 0–1). Companion measurements: `temp/pilot_analyze.json`,
`temp/pilot_record.json` (40-function pilot, this date).

## 1. Goal

CLIR trains small coder models against formal task definitions. pylens must
supply two inputs to that pipeline:

1. **Typing information** for each corpus function: a CLIR-typed signature
   `{"args": [...], "ret": ...}` for the SIGNATURE prompt field and for
   typed grammar-constrained decoding (typed-GCD).
2. **Test suites**: input tuples with expected outputs, executed-line
   coverage as high as the function permits, with CPython as the oracle.
   These suites grade evals and give the RL reward.

The current CLIR test suites come from a generator with known defects
(CLIR ISSUES #27: degenerate inputs front-loaded; #22: one degenerate base;
#24: the CLIR interpreter, not CPython, produced the expected values —
~1.2% of bases drift). The current SIGNATURE types come from transpiler
recovery and drift (CLIR ISSUES #16 note: declared `-> str`, returns
List).

## 2. Corpus facts (measured 2026-08-28)

- `pipeline.db`: 869,996 raw functions; 290,826 valid transpiles;
  **103,121 interpretable functions with test suites** (the target set).
  A local copy for reads is at `temp/pipeline.db`.
- Test format: `{"inputs": [{type, value}...], "expected": {type, value}}`
  with CLIR values only: Int, Float, Bool, Str, List, Some/None.
- CLIR limits that constrain emission: 4 args maximum, no dict/set/tuple
  values, copy-on-bind (a caller never observes argument mutation), no
  exceptions, `assert` swallows failures.

Pilot on 40 sampled interpretable functions (single process, 12 inputs):

| Measure | Result |
|---|---|
| `analyze` wall time | 45 ms for 40 files |
| `record` wall time | 32 s for 40 files (~0.8 s per function) |
| Purity | 34 pure, 6 impure, 0 unknown, 0 unresolved effects |
| Params with a concrete inferred shape | 47 / 90 |
| Case outcomes | 251 returned, 173 raised, 8 resource errors |
| Returned values CLIR-representable | 244 / 251 (97%) |
| Mean line coverage at 12 inputs | 0.91; 31/40 functions at 100% |
| Functions with ≥1 usable case | 36 / 40 |

Reading: the interpretable subset is nearly ideal for pylens (self-contained,
no imports, full effect resolution). The two weak points are (a) the raise
share — 40% of cases burn budget on wrong-typed inputs for `any` params —
and (b) 9 functions below full coverage at a fixed budget of 12.

## 3. Type mapping (pylens Shape → CLIR type)

| pylens `Shape` | CLIR type |
|---|---|
| `Int` / `Float` / `Bool` / `Str` | `int` / `float` / `bool` / `str` |
| `Seq(T)` | `list[T]` (recursive) |
| `Union(None, T)` | `option[T]` |
| `None` (return position) | `void` |
| `Map`, `Set`, `Bytes`, `Instance`, other `Union` | `any` |
| `Any` | `any` |

Rules:

- A **parameter** shape is a hypothesis (pylens soundness rule). For
  SIGNATURE this is acceptable: the field is a generation prior, not a
  soundness claim. Strengthen it with observation: the types of inputs that
  the function **accepted** (case outcome `returned`) confirm the accepted
  domain; a type that always raises `TypeError` refutes it.
- A **return** type joins the static may-set with the observed returns.
  Observed returns are ground truth for the executed inputs.
- The mapping lives on the CLIR side (a consumer script over pylens JSON).
  pylens stays domain-independent; it does not learn CLIR types.

## 4. Workstream A — pylens features (code changes, in order)

Each item ends with `cargo test` + `pylens validate examples` (0 hard
defects) before the next starts.

- **A1. Value-domain profile for generation.** A flag
  (`--value-domain <file>`) that restricts generated inputs to a declared
  domain: allowed scalar types, list element types, size caps, no
  dict/set/tuple/bytes. CLIR supplies a profile file; pylens stays generic.
  Without this, ~3% of cases and many inputs are unrepresentable and the
  raise share stays high.
- **A2. Coverage-driven generation.** Replace the fixed `--inputs N` cap
  with an optional loop: generate ranked batches until executed-line
  coverage stops improving for K consecutive batches or a hard budget
  hits (`--target-coverage`, `--max-inputs`). Keep a minimal covering
  subset plus the boundary inputs. This is the "all paths" lever; note
  honestly: path coverage is not decidable — executed-line coverage plus
  boundary values is the measurable proxy.
- **A3. Declared-annotation hints for generation.** pylens correctly never
  uses annotations for *inference*; generation may use them freely as
  ranking hints. The corpus has declared types (`declared: "int"` on many
  params); sampling them first cuts the wasted-raise share.
- **A4. Stability runs.** `--stability-runs 2`: execute each kept case
  twice; drop cases whose outcome or value differs. Drop `stage:"resource"`
  errors (already policy). This is the determinism gate for RL reward.
- **A5. Precision items already on STATUS.md.** The aliased from-import
  model-table miss (cheap) and the flow-insensitive rebind freeze. Lower
  priority here: the interpretable subset showed 0 unresolved effects, so
  these matter for the broader 290k set, not the 103k pilot set.

Non-goals: no CLIR-specific code in pylens; no pytest emitter (CLIR
consumes JSON, not test files); no dict/tuple support work for CLIR since
CLIR cannot represent them.

## 5. Workstream B — CLIR-side consumer (scripts in clir, not pylens)

- **B1. Exporter**: materialize each interpretable function from
  `pipeline.db` into one `.py` file per function (keyed by `raw_id`,
  chunked directories). Attach same-file callees where the RL row glues
  them, so pylens resolves them.
- **B2. Signature builder**: pylens JSON → `{"args": [...], "ret": ...}`
  per the §3 mapping, with the observed-type refinement. Output feeds the
  v56 assemble and typed-GCD.
- **B3. Suite builder**: `returned` cases → CLIR `{inputs, expected}`
  suites. Emit only CLIR-representable pairs. Shuffle; never emit a
  degenerate prefix (fixes the #27 trap for every prefix-sampler).
  Functions whose value flows through argument mutation only (return
  `None`, mutate the arg) produce constant-`None` suites — route them to
  an exclusion list with a count; decide their fate separately.
- **B4. Import**: write suites and signatures to `pipeline.db` (backup
  rule applies), assemble as **v56** with a re-derived gold-ceiling
  exclusion set. All prior matrices stay frozen; v56 starts a new
  comparability era (CLIR STATUS item 1 already records this).

## 6. Validation gates (the plan's definition of "validated against the dataset")

1. **pylens self-check**: `pylens validate` on each pilot corpus batch;
   hard defects must stay 0; report the soft rate.
2. **Gold-through-harness**: every emitted suite must pass on the gold
   Python function through CLIR's own sandboxed scorer at ≈100%. Failures
   are oracle bugs, not model data.
3. **Trivial-solver rejection**: run constant and passthrough pseudo-
   solvers (`return arg_i`, `return None`, `return 0`, identity) against
   every suite; require each suite to reject all of them. This directly
   measures the #27 failure class. Report the rejection rate before/after.
4. **Coverage delta**: per-base executed-line coverage, old suites vs new;
   the new suites must dominate.
5. **Interpreter-drift census**: run the emitted suites through the CLIR
   interpreter on gold CLIR; the disagreement set sizes the #24 drift under
   a CPython oracle. Expect the exclusion set to change; this is a
   deliverable number, not a surprise.
6. **Stability**: 0 unstable cases after A4 across a re-run of one full
   pilot batch.

## 7. Milestones

- **M0 — pylens readiness**: A1→A4 landed and validated. Exit: a one-
  command run over a directory emits stable, domain-restricted,
  coverage-driven records.
- **M1 — pilot, ~500 functions** (CLIR STATUS item 0). Exit numbers:
  analyze precision (typed-param share, purity), record success rate,
  wall time per function (sequential and with project-mode parallelism),
  the six gates of §6 on the pilot set, and a full-corpus runtime
  projection. Pilot extrapolation today: ~0.8 s/function sequential ≈ 23 h
  for 103k — parallel record must be measured in M1 before the full pass.
- **M2 — full pass**: 103,121 functions, chunked and resumable (per-
  function output keyed by `raw_id`; a crashed chunk re-runs idempotently).
  Import, assemble v56, re-derive exclusions.
- **M3 — integration**: typed-GCD reads the v56 signatures; RL reward
  reads the v56 suites; the model-ladder runs (CLIR STATUS item 2) start
  on v56.

## 8. Decisions the user must make

1. **Oracle policy**: CPython becomes the oracle of record. The CLIR
   interpreter's divergences then count against CLIR gold (exclusions or
   spec fixes), no longer against the tests. Confirm.
2. **Mutation-only functions** (§5 B3): exclude, or extend the CLIR test
   format with expected argument values (pylens already records the
   mutation diffs).
3. **Raising cases**: CLIR cannot express exception tests. Emit raising
   inputs as a side-channel (negative examples for describe/APPROACH), or
   drop them.
4. **Re-baseline** of the 2026-08-28 matched pair on v56 (cost: two
   training runs) — already flagged in CLIR STATUS item 1.
5. **Scope**: 103k interpretable set only, or also the 290k valid-transpile
   set (needs A5 precision work and longer runtime).
