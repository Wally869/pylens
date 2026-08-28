# pylens

Static effect and shape analysis of Python functions, in Rust, on the ruff parser. Also
sandboxed execution, which records the actual behavior of a function on generated inputs.
Output is JSON. pylens is independent of the domain; consumers build on the output.

## Soundness rule

> **`observed_effects ⊆ static_may_set`**

The static analysis over-approximates (may-sets). It must never report an effect (`pure`, a
missing mutation, an unlisted raise) that an execution can disprove. What the analyzer cannot
examine becomes `unresolved` or `Unknown`, never `pure`. `pylens validate` measures this rule.
Run it after a change to the analyzer.

## Commands

```sh
cargo build --release
cargo test                          # the sandbox tests skip if the sandbox is absent
cargo clippy --all-targets -- -D warnings

pylens analyze  <file.py|dir> [--format json|summary|pyi|html]
pylens record   <file.py|dir> [--inputs N] [--cover-branches] [--stability-runs N] [--format json|summary|pyi|html]  # sandbox; pyi is for one file only
pylens validate <file.py|dir> [--inputs N] [--format json|summary|html]      # exits non-zero on a hard defect
```

`--inputs` defaults to 12. `record` and `validate` also report executed-line coverage per
function, which tells you how much of the function the generated inputs actually reached.
`record --cover-branches` (off by default) turns `--inputs` into the TOTAL per-function case
budget and runs a predicate-targeted loop after the initial batch: every uncovered branch outcome
whose test expression `generate::predicate` can parse gets extra, targeted inputs, until every
observable outcome is covered, an iteration adds none, or the budget runs out. Each still-
uncovered outcome then carries a `reason` (see `docs/SCHEMA.md`).
`record --stability-runs N` (N >= 2, off by default; not available on `validate`) re-executes
every case (generated, cover-loop, and replayed alike) N times total and drops any whose runs
disagree on outcome/return/raises/mutations/stdout/stderr, so a consumer building test pools gets
only deterministic cases. Coverage and branch accounting run on the surviving cases only. See
`dropped_cases` in `docs/SCHEMA.md`.

`analyze` is static and needs no sandbox. `record` is `analyze` plus the dynamic layer, on one
static core. A directory argument starts project mode.

## Pipeline

`analyze_module` (`src/analyze/mod.rs`) runs ordered passes over a shared `ModuleAnalysis`:

**Imports → Declarations → Shapes → Effects → Interprocedural → TypeCheck → Purity**

- **Imports** — the table of the import bindings.
- **Declarations** — the symbol table of the functions and methods. Call resolution needs it.
- **Shapes** — fixpoint inference of a recursive `Shape` (unions included) for each parameter
  and local. Runs before Effects, thus Effects reads the final shapes.
- **Effects** — the single AST walk that builds each `EffectSignature`. It delegates to the
  collectors in `collect/`.
- **Interprocedural** — propagates the effects of the local callees to the callers (a fixpoint;
  mapping by position and by keyword). A call that unpacks keeps an acknowledged limit. Project
  mode extends this between files (`src/project/interproc.rs`).
- **TypeCheck** — reports the disagreements between the declared and the inferred types.
  Advisory only.
- **Purity** — derives the `Purity` from the collected facts.

Extension points: the `Pass` trait, `collect/`, `generate/`, `models.rs`, the `Sandbox` trait,
and the
`report`, `stub`, and `html` formatters.

## Codemap

- `src/lib.rs` — the crate root: `analyze_source`, `imports_of`, `SCHEMA_VERSION`, `strip_bom`.
- `src/main.rs` — the CLI. A directory starts project mode; a file or stdin starts single-file
  mode.
- `src/parse.rs` — the boundary to the ruff parser. All the `ruff_*` code is only here.
- `src/model/` — the data model: `shape.rs` (the `Shape` lattice: join, unions, serde) and
  `mod.rs` (`EffectSignature`, params, mutations, raises, imports, `TypeMismatch`, `Purity`).
- `src/analyze/` — `mod.rs` (the driver), `pass.rs` (the `Pass` trait), `context.rs` (the shared
  state), `passes/` (one file for each pass; `shapes/` and `effects/` have submodules),
  `collect/` (aliases, mutations, exceptions, shapes, returns, guards, hints, body_lines).
- `src/analyze/models.rs` — the stdlib effect table (raises and io), keyed on the resolved
  module path. An entry removes an unresolved acknowledgment, so each entry over-approximates.
- `src/generate/` — `mod.rs` (the sampler: ranked candidates, one parameter at a time),
  `seeds.rs` (the shape, property and domain corpora), `domain.rs` (`--value-domain`),
  `predicate.rs` (extracts handled branch-test forms over a single positional parameter and
  synthesizes satisfying/violating values — feeds `record::cover`). Also gives the shrink
  candidates.
- `src/exec.rs` — sandboxed execution: the `Sandbox` trait, `Nsjail`, the `NsjailPool` fork
  server. There is no unsandboxed launcher.
- `src/record/` — `mod.rs` (the static signature and the sandboxed cases: `ModuleRecord`, `Case`,
  with the mutation difference before and after the call), `cover.rs` (the `--cover-branches`
  predicate-targeted coverage loop, and the `branches`/`branch_coverage` report it feeds a
  `reason` into), `stability.rs` (`--stability-runs`: re-executes each case N times and drops any
  whose runs disagree, feeding `FunctionRecord::dropped_cases`).
- `src/shrink.rs` — greedy input minimization for the cases that raise. An aid for the report
  only.
- `src/validate.rs` — the `observed ⊆ static` harness. `Defect` severity is Hard if a may-set
  said that it was complete but missed an effect, Soft if an acknowledged unresolved entry
  covers the effect.
- `src/project.rs`, `src/project/` — multi-file mode: a walk that obeys `.gitignore`, parallel
  analysis and sandboxed record, cross-file propagation (`interproc.rs`), import resolution
  (`resolve.rs`).
- `src/report.rs`, `src/stub/`, `src/html/` — the output formatters: the terminal summary, the
  `.pyi` stubs (with the observed types for `record --format pyi`), one HTML file.
- `python/worker.py` — the CPython harness in the sandbox: JSON over stdio, `--serve` fork
  server.
- `nsjail/pylens.nsjail.cfg` — the sandbox policy. `scripts/provision-sandbox.sh` — builds
  nsjail and deploys the worker and the policy to `$HOME/.local/share/pylens/`.
- `tests/` — `analyze`, `imports`, and `generate` are pure and always run. `pool`, `record`,
  `validate`, and `project` need the sandbox and skip if it is not provisioned.

## Sandbox

Each Python execution occurs in an nsjail sandbox on a Linux kernel — directly on Linux, through
WSL2 on Windows. Setup, one time: `bash scripts/provision-sandbox.sh` (on Windows, in WSL).

The sandbox runs the deployed copy of `python/worker.py`, not the file in the repository. After
a change to that file, run the provision script again, or deploy it with
`wsl -d Ubuntu -- install -m 0644 /mnt/c/Projects/ai/pylens/python/worker.py "$HOME/.local/share/pylens/worker.py"`.
If you do not, the sandbox tests use the old worker.

## Conventions

- Over-approximation is tolerable and stays low. Under-approximation is a bug — a hard defect in
  `pylens validate`. The `examples/` corpus stays at zero hard defects; `tests/validate.rs` and
  `tests/project.rs` enforce this. Do not make them weaker to hide a new finding. Report it.
- **A parameter's inferred shape is a hypothesis, not a guarantee.** It comes from the
  operations inside the function and it constrains no caller, so it must never narrow a
  may-set. A local's shape can narrow one: `xs = []` really is a list. This rule was broken
  once — the implicit-raise set used a parameter's shape to prove the very operation that
  produced that shape could not fail — so do not reintroduce it.
- The harness cannot check everything. Generation draws from the same shapes the analysis
  infers, so a claim justified by that shared assumption is unfalsifiable rather than true.
  `io` `filesystem` claims have no observation channel at all, because the jail's file system
  is read only.
- Greenfield: no backwards compatibility, no dead code, no comments about a previous approach.
- Before you increase `SCHEMA_VERSION`, read the version section in `docs/SCHEMA.md`.
- Rust edition 2024. ruff is pinned to a fixed revision in `Cargo.toml`.
- Give each pass, collector, and module a doc comment with its one responsibility.
- If the sandbox stops the code (out of memory, recursion, timeout), the result is
  `outcome:"error"` with `error.stage:"resource"`. Never record it as a semantic `raised`.

## Extending

- A new analysis → a collector in `collect/`, or a new `Pass` (insert it in `analyze/mod.rs`).
  Keep the single-traversal design.
- A new output → a formatter module (`report`, `stub`, and `html` are the examples) and a
  `--format` value.
- A new implicit exception or shape rule → the applicable collector. Then run `pylens validate`
  over the corpus to confirm that the gap is closed.
