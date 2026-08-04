# pylens — agent onboarding

Static **effect + shape analysis** of Python functions (in Rust, via the ruff parser), plus
**jailed execution** to record what a function *actually* does on generated inputs. Consumers
(type-hint suggestion, refactoring, ML dataset building) build on the records through a
documented JSON contract. pylens itself is domain-agnostic.

## The one invariant that governs everything

> **`observed_effects ⊆ static_may_set`**

The static analysis over-approximates (**may-sets**). It must **never** report an effect
(`pure`, a missing mutation, an unlisted raise) that a real execution can contradict. A function
is `pure` only when nothing statically unresolved remains; anything the analyzer can't see
through degrades to `unresolved`/`Unknown`, never silently to `pure`. `pylens validate` measures
this invariant — see below. When you change the analyzer, run it and keep the corpus sound.

## Two engines, one core

- **`analyze` (static, pure Rust, no sandbox)** — the scale path: instant, parallel, no setup.
- **`record` (dynamic, nsjail-jailed)** — the ground-truth path: expensive, proves the static
  engine honest and supplies observed behavior.

`record` = `analyze` + the dynamic layer. There is one static core; the commands are not
separate pipelines.

## The analyzer is a pass pipeline

`analyze_module` (in `src/analyze/mod.rs`) runs an ordered list of passes over a shared
`ModuleAnalysis` context, in this order:

**Imports → Declarations → Shapes → Effects → Interprocedural → TypeCheck → Purity**

- **Imports** — import binding table.
- **Declarations** — function/method symbol table (`DeclInfo`/`ReceiverKind`); the enabler for
  interprocedural resolution.
- **Shapes** — fixpoint inference of a recursive `Shape` for every param **and local**
  (e.g. `matrix : Seq(Seq(Float))`), depth-capped. Runs before Effects so Effects reads final
  shapes.
- **Effects** — the single AST walk that builds each `EffectSignature`, delegating to the
  `collect/` collectors (one traversal, not N).
- **Interprocedural** — propagates effects of **locally-defined** callees to their callers
  (fixpoint; handles recursion). Imports / unknown callees stay unresolved. (Cross-file is future.)
- **TypeCheck** — flags declared-vs-inferred return mismatches (`type_mismatches`).
- **Purity** — derives `Purity` from the accumulated facts.

Injection points (where to extend without a rewrite): the `Pass` trait (`pass.rs`) for new
pipeline stages; the `collect/` collectors for the effect walk; `generate.rs` for input
strategy; the `Sandbox` trait (`exec.rs`) for launchers; `report`/`stub`/`html` for output.

## Codemap

- `src/lib.rs` — crate root: `analyze_source`, `imports_of`, `SCHEMA_VERSION`.
- `src/main.rs` — CLI. `analyze`/`record`/`validate`; `--format json|summary|pyi|html`;
  `--inputs N`; a **directory** arg triggers project mode, a file/stdin is single-file.
- `src/parse.rs` — the ruff parser boundary. **All `ruff_*` usage is isolated here** (swappable).
- `src/model.rs` — the data model: `EffectSignature`, recursive `Shape`, `ParamInfo`/`ParamKind`
  (Positional/VarPositional/VarKeyword/KeywordOnly), `Mutation`/`MutationTarget`, `Raises`
  (explicit/implicit), `ReturnKind`, `Import`/`ModuleRef`, `Purity`, `TypeMismatch`,
  `UnresolvedEffect`.
- `src/analyze/`
  - `mod.rs` — pipeline driver (`analyze_module`), `collect_imports`.
  - `pass.rs` — the `Pass` trait. `context.rs` — `ModuleAnalysis` + `FunctionFacts`.
  - `passes/imports.rs`, `passes/declarations.rs`, `passes/interprocedural.rs`,
    `passes/type_check.rs`, `passes/purity.rs`.
  - `passes/shapes/` — fixpoint shape inference: `mod.rs` (driver), `shape_of.rs`, `state.rs`,
    `pinning.rs`.
  - `passes/effects/` — the effect walk: `mod.rs` (walker), `setup.rs` (params/decorators),
    `dedup.rs`, `builtins.rs`.
  - `collect/` — per-walk collectors: `aliases`, `mutations`, `exceptions`, `shapes`, `returns`,
    `guards`.
- `src/generate.rs` — shape-directed + **guard-guided** input generation (`gen_inputs`,
  `GenInput`, recursive breadth-capped `candidates`).
- `src/exec.rs` — jailed execution: `Sandbox` trait, `Nsjail` (one process/call), `NsjailPool`
  (fork-server), `probe()`, `CallResult`, `HarnessError` (`is_resource()` distinguishes resource
  kills). **No unsandboxed launcher exists.**
- `src/record.rs` — `record_file`/`record_with`: static sig + jailed cases; `ModuleRecord`,
  `Case`, before/after mutation diffing (incl. `self` and kwargs).
- `src/validate.rs` — the `observed ⊆ static` harness: `validate_function`, `Defect`, `Severity`
  (Hard = the may-set claimed completeness yet missed an effect; Soft = explained by an
  acknowledged unresolved).
- `src/project.rs` + `src/project/resolve.rs` — multi-file mode: directory walk (+ skip-list),
  parallel analyze, aggregated project report, project-local import resolution.
- `src/report.rs` (terminal summary), `src/stub.rs` (`.pyi` stubs), `src/html.rs`
  (self-contained HTML) — output formatters.
- `python/worker.py` — the in-jail CPython harness: JSON-over-stdio; serialize-before/after for
  mutation diffs; `--serve` fork-server (per-request `fork()` isolation).
- `nsjail/pylens.nsjail.cfg` — the jail policy (namespaces + seccomp denylist + rlimits).
- `scripts/provision-sandbox.sh` — builds nsjail and **deploys** `worker.py` + policy to
  `$HOME/.local/share/pylens/`.
- `tests/` — `analyze`/`imports`/`generate` are pure (always run); `pool`/`record`/`validate`/
  `project` are **jail-gated** (skip when the sandbox isn't provisioned). `tests/fixtures/`.

## Commands

```sh
cargo build --release
cargo test                          # 100 tests; jail-gated ones skip if sandbox absent
cargo clippy --all-targets -- -D warnings

pylens analyze  <file.py|dir> [--format json|summary|pyi|html]
pylens record   <file.py|dir> [--inputs N] [--format json|summary|html]   # runs the jail
pylens validate <file.py|dir> [--inputs N] [--format json|summary|html]   # exits non-zero on hard defects
```

## Sandbox (read before touching `record`/`worker.py`)

Every Python execution is nsjail-jailed on a Linux kernel — native on Linux, **via WSL2 on
Windows** (`wsl -d <distro> -- nsjail …`). One-time setup:
`bash scripts/provision-sandbox.sh` (on Windows, run it inside WSL).

**Gotcha that bites everyone:** the jail runs a **deployed copy** of `python/worker.py` at
`$HOME/.local/share/pylens/worker.py` inside the distro. Editing the repo copy does **not**
update the jail — re-run the provision script, or redeploy:
`wsl -d Ubuntu -- install -m 0644 /mnt/c/Projects/ai/pylens/python/worker.py "$HOME/.local/share/pylens/worker.py"`.
Otherwise jail-gated tests run against the stale worker.

## Conventions

- **May-set semantics, soundness first.** Over-approximation (predicted-but-never-observed) is
  tolerated and kept low; under-approximation (observed-but-not-predicted) is a bug — a **hard
  defect** in `pylens validate`. The example corpus (`inventory.py`, `normalize.py`) is kept at
  zero hard defects; `tests/validate.rs` enforces it. `graph.py` has a known tracked gap
  (see `HANDOVER.md`) — don't weaken the test to hide new findings; report them.
- **Greenfield.** No backwards-compat baggage, no dead code, no "previous approach" comments.
- **Rust edition 2024.** ruff is pinned to a fixed rev in `Cargo.toml` (reproducible builds).
- Doc-comment each pass/collector/module with its single responsibility.
- Resource kills (`MemoryError`/`RecursionError`/timeout) are `outcome:"error"`,
  `error.stage:"resource"` — never conflated with a semantic `raised`.

## Extending

- **New analysis** → a collector under `collect/` invoked from the Effects walk, or a new `Pass`
  in the pipeline (insert in `analyze/mod.rs`). Keep the single-traversal design.
- **New output** → a formatter module (mirror `report`/`stub`/`html`) + a `--format` value.
- **New implicit exception / shape rule** → the relevant collector; then run `pylens validate`
  over the corpus to confirm you didn't open (or that you closed) a soundness gap.
