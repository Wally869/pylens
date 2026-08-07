# pylens — agent onboarding

Static **effect + shape analysis** of Python functions (in Rust, via the ruff parser), plus
**jailed execution** to record what a function *actually* does on generated inputs. Consumers
build on the JSON output; pylens itself is domain-agnostic.

## The one invariant that governs everything

> **`observed_effects ⊆ static_may_set`**

The static analysis over-approximates (**may-sets**). It must **never** report an effect
(`pure`, a missing mutation, an unlisted raise) that a real execution can contradict. Anything
the analyzer can't see through degrades to `unresolved`/`Unknown`, never silently to `pure`.
`pylens validate` measures this invariant — when you change the analyzer, run it and keep the
corpus sound.

## Two engines, one core

- **`analyze`** (static, pure Rust, no sandbox) — the scale path: instant, parallel, no setup.
- **`record`** (dynamic, nsjail-jailed) — the ground-truth path: proves the static engine
  honest and supplies observed behavior.

`record` = `analyze` + the dynamic layer. One static core; the commands are not separate
pipelines.

## The analyzer is a pass pipeline

`analyze_module` (`src/analyze/mod.rs`) runs ordered passes over a shared `ModuleAnalysis`:

**Imports → Declarations → Shapes → Effects → Interprocedural → TypeCheck → Purity**

- **Imports** — import binding table.
- **Declarations** — function/method symbol table; the enabler for call resolution.
- **Shapes** — fixpoint inference of a recursive `Shape` (incl. unions) for every param and
  local; runs before Effects so Effects reads final shapes.
- **Effects** — the single AST walk building each `EffectSignature`, delegating to the
  `collect/` collectors (one traversal, not N).
- **Interprocedural** — propagates locally-defined callees' effects to callers (fixpoint,
  positional + keyword mapping; unpacked calls keep an acknowledged blind spot). Project mode
  extends this across files (`src/project/interproc.rs`).
- **TypeCheck** — flags declared-vs-inferred mismatches (return and param); advisory only.
- **Purity** — derives `Purity` from the accumulated facts.

Injection points: the `Pass` trait for pipeline stages; `collect/` for the effect walk;
`generate.rs` for input strategy; the `Sandbox` trait for launchers; `report`/`stub`/`html`
for output.

## Codemap

- `src/lib.rs` — crate root: `analyze_source`, `imports_of`, `SCHEMA_VERSION`, `strip_bom`.
- `src/main.rs` — CLI; a directory arg triggers project mode, file/stdin is single-file.
- `src/parse.rs` — the ruff parser boundary; all `ruff_*` usage is isolated here.
- `src/model/` — the data model: `shape.rs` (the recursive `Shape` lattice: join, unions,
  serde), `mod.rs` (everything else: `EffectSignature`, params, mutations, raises, imports,
  `TypeMismatch`, `Purity`, …).
- `src/analyze/` — `mod.rs` (pipeline driver), `pass.rs` (`Pass` trait), `context.rs` (shared
  state), `passes/` (one file per pass; `shapes/` and `effects/` are split into submodules),
  `collect/` (per-walk collectors: aliases, mutations, exceptions, shapes, returns, guards).
- `src/generate.rs` — shape-directed, guard-guided input generation; shrink candidates.
- `src/exec.rs` — jailed execution: `Sandbox` trait, `Nsjail`, `NsjailPool` fork-server.
  **No unsandboxed launcher exists.**
- `src/record.rs` — static signature + jailed cases (`ModuleRecord`/`Case`), before/after
  mutation diffing.
- `src/shrink.rs` — greedy input minimization for raised cases (reporting aid only).
- `src/validate.rs` — the `observed ⊆ static` harness; `Defect` severity: Hard = may-set
  claimed completeness yet missed an effect, Soft = covered by an acknowledged unresolved.
- `src/project.rs` + `src/project/` — multi-file mode: `.gitignore`-honoring walk, parallel
  analyze and jailed record, cross-file propagation (`interproc.rs`), import resolution
  (`resolve.rs`).
- `src/report.rs`, `src/stub/`, `src/html/` — output formatters: terminal summary, `.pyi`
  stubs (incl. observed-type folding for `record --format pyi`), self-contained HTML.
- `python/worker.py` — the in-jail CPython harness: JSON-over-stdio; `--serve` fork-server.
- `nsjail/pylens.nsjail.cfg` — the jail policy. `scripts/provision-sandbox.sh` — builds
  nsjail and **deploys** worker + policy to `$HOME/.local/share/pylens/`.
- `tests/` — `analyze`/`imports`/`generate` are pure (always run); `pool`/`record`/
  `validate`/`project` are jail-gated (skip when the sandbox isn't provisioned).

## Commands

```sh
cargo build --release
cargo test                          # jail-gated tests skip if sandbox absent
cargo clippy --all-targets -- -D warnings

pylens analyze  <file.py|dir> [--format json|summary|pyi|html]
pylens record   <file.py|dir> [--inputs N] [--format json|summary|pyi|html]  # runs the jail; pyi single-file only
pylens validate <file.py|dir> [--inputs N] [--format json|summary|html]      # exits non-zero on hard defects
```

## Sandbox (read before touching `record`/`worker.py`)

Every Python execution is nsjail-jailed on a Linux kernel — native on Linux, **via WSL2 on
Windows**. One-time setup: `bash scripts/provision-sandbox.sh` (on Windows, inside WSL).

**Gotcha that bites everyone:** the jail runs the **deployed copy** of `python/worker.py`, not
the repo file. After editing it, re-run the provision script (or redeploy with
`wsl -d Ubuntu -- install -m 0644 /mnt/c/Projects/ai/pylens/python/worker.py "$HOME/.local/share/pylens/worker.py"`),
or jail-gated tests run against the stale worker.

## Conventions

- **May-set semantics, soundness first.** Over-approximation is tolerated and kept low;
  under-approximation is a bug — a **hard defect** in `pylens validate`. The `examples/`
  corpus stays at zero hard defects, enforced by `tests/validate.rs` and `tests/project.rs` —
  don't weaken them to hide new findings; report them.
- **Greenfield.** No backwards-compat baggage, no dead code, no "previous approach" comments.
- Considering a `SCHEMA_VERSION` bump? Read the versioning section in `docs/SCHEMA.md` first.
- **Rust edition 2024.** ruff is pinned to a fixed rev in `Cargo.toml`.
- Doc-comment each pass/collector/module with its single responsibility.
- Resource kills (OOM/recursion/timeout) are `outcome:"error"`, `error.stage:"resource"` —
  never conflated with a semantic `raised`.

## Extending

- **New analysis** → a collector under `collect/`, or a new `Pass` (insert in
  `analyze/mod.rs`). Keep the single-traversal design.
- **New output** → a formatter module (mirror `report`/`stub`/`html`) + a `--format` value.
- **New implicit exception / shape rule** → the relevant collector; then run `pylens validate`
  over the corpus to confirm the soundness gap is closed, not opened.
