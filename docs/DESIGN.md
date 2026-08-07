# pylens — Design

## Purpose

pylens statically analyzes Python functions and methods (in Rust) to extract their **effects**
— returns, in-place argument and `self` mutations, raised exceptions, I/O, generator-ness — as
a normalized **effect signature**. It then runs each function in a jail on generated inputs and
records the effects it **actually** has. Signature + observed cases = the function's
**behavioral record**.

pylens stops at producing records. Scoring, comparing a candidate against a reference, turning
records into a training reward — all of that belongs to the consumer. The JSON contract the
records use is specified in [SCHEMA.md](SCHEMA.md).

## Principles and the soundness invariant

The downstream signal is adversarial: anything the analysis cannot see is a hole a consumer's
reward can be hacked through. So the analyzer is honest about its blind spots
(`unresolved_effects`) and **over-approximates** — every reported set is a *may-set*:

> **observed_effects ⊆ static_may_set**

Every observed effect the static side did not predict is a **soundness bug** and must be driven
to zero (`pylens validate` measures exactly this; a gap in a signature that claimed
completeness is a **hard** defect, a gap covered by an acknowledged unresolved effect is
**soft**). The reverse — predicted but never observed — is imprecision: tolerated, kept low.
Dynamic testing falsifies soundness; it cannot prove it.

Two consequences drive everything else:

- A function is `pure` only when nothing statically unresolved remains. Unknown calls, dynamic
  constructs, and unrecognized decorators degrade purity to `unknown`, never silently to `pure`.
- Static effect *shape* is necessary but not sufficient — two functions can share a shape and
  return different values. The jailed `record` layer supplies the value-level ground truth.

## Effect taxonomy (caller's point of view)

1. **Return** — value vs `None`; coarse inferred kind. Rebinding a parameter name is not an
   effect.
2. **Argument mutation** — `p[i]=…`, `p.attr=…`, `del`, mutating methods, augmented assignment.
3. **Global / module-state writes.**
4. **Nonlocal / closure mutation.**
5. **`self`-attribute mutation** (methods).
6. **World I/O** — stdout, files, network, env.
7. **Exceptions** — explicit (`raise`/`assert`) vs implicit (operator- and call-induced
   may-sets).
8. **Generator / async.**

Annotations are **untrusted** throughout: recorded, compared against the inferred side
(`type_mismatches`, advisory only), never used for inference.

## Architecture

```
parse (ruff) → pass pipeline → effect signatures → input generation → jail → observed cases
                                        └────────── validate: observed ⊆ static ──────────┘
```

- **`parse/`** — the ruff boundary; all `ruff_*` usage is isolated here so the parser is
  swappable. Every source-ingestion point strips a leading UTF-8 BOM first (CPython tolerates
  one in a file, but the jailed worker's `compile()` on text does not; analyzer and jail must
  see identical source).
- **`model/`** — the pure data model: the effect-signature types (`mod.rs`) and the recursive
  `Shape` lattice with canonical width-capped unions (`shape.rs`). Serialization is the schema.
- **`analyze/`** — an ordered pass pipeline over a shared context, driven by one AST walk with
  per-concern collectors (aliases, mutations, exceptions, shapes, returns, guards):
  **Imports → Declarations → Shapes → Effects → Interprocedural → TypeCheck → Purity**.
  Shapes runs a fixpoint before Effects so the walk reads settled shapes. Interprocedural
  propagates locally-defined callees' effects onto callers to a fixpoint (recursion included),
  mapping arguments by position and by keyword name; a call that unpacks (`f(*xs)`/`f(**kw)`)
  keeps an acknowledged `call_unpacked_args` blind spot plus an implicit `TypeError` — the
  unpack itself can raise before the callee runs — instead of silently dropping effects.
  TypeCheck is advisory; Purity runs last so it sees complete effect sets.
- **`generate/`** — shape-directed input generation, spread over the cartesian product of
  per-parameter candidates, recursive and breadth-capped. **Guard-guided**: literal and
  boundary values pulled from the function's own guard expressions (`if qty > 10` seeds
  9/10/11) join the candidate pool so generated inputs reach guarded branches. Also provides
  shrink candidates for minimization.
- **`record/`** — joins static signatures with jailed execution: dependency probing,
  module-load and constructor probing (an unloadable module marks every function `uncallable`
  once), case construction, and greedy **input minimization** for raised cases (shrink while
  the same exception type reproduces, bounded budget; a reporting aid, never fed to validate).
  Resource kills (OOM/recursion/timeout) are sandbox artifacts — `outcome:"error"`,
  `error.stage:"resource"` — and are never conflated with a semantic `raised`.
- **`exec/`** — the `Sandbox` trait and the nsjail launchers (native / WSL-wrapped), the
  JSON-over-stdio worker protocol, structured `HarnessError`. **No unsandboxed launcher
  exists.**
- **`validate/`** — the pure `observed ⊆ static` checker; see the invariant above.
- **`project/`** — directory mode: `.gitignore`-honoring walk, parallel analyze, parallel
  jailed record/validate (a small pool of worker threads, each owning its own jail), static
  project-local import resolution (absolute and relative, `__init__.py` semantics), and
  **cross-file effect propagation** for `project_local`-imported free functions — the
  intra-file mapping rules applied over a project-wide fixpoint.
- **`report/` / `stub/` / `html/`** — formatters over the same data: terminal summary, `.pyi`
  stubs (PEP 604 unions; `record --format pyi` folds in observed types where the static side
  stayed unresolved, marked `# observed:` — only when every observation agrees and the worker's
  tagged serialization makes the type unambiguous), self-contained HTML.

## Execution layer

**Threat model.** Runs at dataset-build time. All input code is untrusted — reference and
candidate alike. Therefore every Python execution is jailed and there is no unsandboxed path,
not even for dev or tests.

**The sandbox is nsjail on a Linux kernel.** Namespaces + a seccomp-bpf filter + rlimits
(memory/CPU/time/pids caps, no network, read-only FS). The seccomp filter is a **denylist** of
the unambiguously dangerous syscalls (ptrace, mount, module load, setns/unshare, bpf, …) on top
of dropped capabilities and `NO_NEW_PRIVS` — a strict CPython allowlist is fragile across
interpreter versions and breaks the worker silently; tightening to one is future hardening.

The host only changes how a Linux kernel is reached:

| Host | Command shape |
|---|---|
| Linux | `nsjail <policy> -- python worker.py` |
| Windows | `wsl -d <distro> -- nsjail <policy> -- python worker.py` |

Same policy, same worker, same protocol. **No Docker**: on Windows the untrusted code already
sits behind nsjail *and* the WSL2 VM; a container would add a third layer and a daemon
dependency for no security gain. (Its one value — hermetic provisioning for CI — can slot in
later behind the same `Sandbox` trait.) Rejected interpreters (research in
`docs/research/python-execution.md`): CPython-WASI (cold start, stdlib gaps), RustPython
(fidelity), Monty (immature).

**Execution mechanics:**

- Real CPython for oracle fidelity; a persistent fork-server worker pool amortizes interpreter
  startup while keeping per-call isolation (each request runs in a fresh `fork()`).
- **Mutation observability:** arguments are serialized to tagged JSON before and after the
  call; the diff (structural equality — float tolerance, set/dict ordering) is the observed
  mutation. Tagged JSON covers non-JSON-native types (tuple/set/bytes/objects).
- **Aliasing:** identity relations (`return is arg[i]`) are reported explicitly — serialization
  alone destroys identity, which the effect model tracks.
- **Methods:** the receiver is built from `__init__` and `self` pre/post state is captured like
  any argument.
- The function's stdout/stderr are captured as effects and kept off the protocol channel.
- Exception observations compare by **exception type** (not message) — the same rule the
  validator and the minimizer use.

## Known limitations

- Library calls are not modelled — a call through an import is an honest `unresolved_effects`
  entry, and `record` shows what it actually did.
- Cross-file propagation covers free functions; imported *methods* and deeper dotted call
  chains stay unresolved.
- Input generation is heuristic: it will not reach every branch, and validate only checks paths
  the generated inputs actually reach.
