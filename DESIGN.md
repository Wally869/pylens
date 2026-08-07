# pylens — Design

## Purpose

**pylens** statically analyzes Python functions and methods (in Rust) to extract their
**effects** — what they return, which arguments they mutate in place, which exceptions
they raise, their I/O, whether they are generators — as a normalized **effect signature**.

pylens then runs the function in a jail on generated inputs and **records the effects it
actually has** (the `record` flow). The signature plus these observed cases are the
**behavioral record** of a function.

The motivating consumer is RL-training an LLM against reference functions — but pylens stops
at producing records. Turning records into a reward (comparing a candidate to a reference,
scoring) is **out of scope**: it's the training loop's decision, not this tool's. Effect
extraction is the engine; jailed execution backstops it with real observed behavior.

## Guiding principles

- **The downstream signal is adversarial.** Anything the analysis cannot see becomes a hole a
  consumer's reward could be hacked through. The analyzer must be *honest about its blind spots*
  rather than silently assume purity — see `unresolved_effects`.
- **Static effect *shape* is necessary but not sufficient.** Two functions can share an
  effect shape yet return different values. Dynamic value-level checking (phases 3–6)
  backstops the static signal.
- **Over-approximate (may-sets), so the analyzer is checkable.** See the soundness
  invariant below.

## Locked decisions

| Decision | Choice | Rationale |
|---|---|---|
| Analysis method | **Static AST**, no execution in the core | Fast, deterministic, the reason to use Rust |
| Parser | **ruff** (`ruff_python_parser` + `ruff_python_ast`), behind a `parse/` boundary | Best grammar fidelity for arbitrary Python; boundary keeps it swappable |
| Approximation | **may-sets** (over-approximate) | Enables the soundness invariant `observed ⊆ static` |
| Signature shape | **path-aware behavior set** | Returns/raises vary by path; union over all exits, not one flat type |
| Annotations | **untrusted** | Recorded separately; inferred-vs-declared mismatches are flagged, not trusted |
| Honesty | **`unresolved_effects`** | Unknown calls / dynamic constructs recorded explicitly, never assumed pure |
| Scope | Extractor emits signatures (JSON); **test-gen folded into the project** | Generation validates the analyzer and produces the observed cases in each record |
| Execution locus (phase 3+) | **nsjail on a Linux kernel**, always | See "Execution layer" below |
| Sandbox policy | **Mandatory** — every Python execution is jailed; **no unsandboxed launcher exists** | All gold *and* candidate code is untrusted at dataset-build time |
| First milestone | **Analyzer foundation first** (phases 1–2) | Lock the spec/contract before building dynamic layers on it |

## Effect taxonomy (caller's point of view)

1. **Return** — `None` vs value; coarse inferred kind. Rebinding a parameter name is *not*
   an effect.
2. **Argument mutation** — `p[i]=…`, `p.attr=…`, `del p[i]`, mutating methods
   (`append`/`sort`/`update`/…), augmented assignment on an element/attr.
3. **Global / module-state mutation** — `global x; x=…`, writes to module-level names.
4. **Nonlocal / closure mutation.**
5. **`self`-attribute mutation** — special case of (2) where the arg is `self` (methods).
6. **World I/O** — stdout/print, files, network, env; plus nondeterminism (`random`, `time`).
7. **Exceptions raised** — *explicit* (`raise`) vs *implicit* (operator-induced:
   `ZeroDivisionError`/`KeyError`/`IndexError`/`TypeError`).
8. **Generator / async** — `yield`, `await`.

## Effect signature schema (draft)

A single function is a **set of behaviors**, not one flat signature. Returns and raises are
unioned over all exits; mutations are may-sets.

```jsonc
{
  "name": "normalize",
  "kind": "method",                  // function | method
  "owner": "Normalizer",             // class (methods only) — needed to build a receiver
  "params": [
    { "name": "items", "shape": {"seq": "any"}, "has_default": false, "kind": "positional" }
  ],
  "declared_return": "list",         // from annotation — UNTRUSTED
  "is_generator": false,
  "returns": ["sequence"],           // union over all return exits (incl. "none"); else "opaque"
  "raises": {
    "explicit": ["ValueError"],      // from `raise` / `assert` statements — high confidence
    "implicit": ["TypeError"]        // operator-induced — statically-inferred may-set
  },
  "mutations": [
    { "target": {"root": "self_attr", "name": "cache"}, "via": "attr_set",   "name": "cache" },
    { "target": {"root": "param", "name": "items"},      "via": "method",     "name": "append" }
  ],
  "global_writes": ["COUNTER"],
  "io": ["stdout"],
  "unresolved_effects": [
    { "reason": "call_import", "callee": "np.sort", "may_affect": [{"root": "param", "name": "items"}] }
  ],
  "uses": [ { "binding": "np", "module": { "package": "numpy" } } ],
  "decorators": [],                  // dotted decorator names; unrecognized ⇒ purity downgrade
  "type_mismatches": [],             // declared_return / a param's declared annotation contradicts
                                      // the inferred may-set (see below)
  "purity": "unknown"                // any unresolved effect ⇒ unknown, never "pure"
}
```

Calls through an imported name (`np.sort(items)`) are opaque foreign effects: recorded as a
`call_import` unresolved effect and linked back to the import via `uses`, so a function that
shells out to a library is never mistaken for pure. The companion **import catalog** (every
style, package/path-split, with module/function `scope`) is emitted alongside, and the `record`
flow probes each for resolution; see the README/USER_GUIDE for the `dependencies` shape.

Mutation **targets** (roots — internally serialized with `serde(tag = "root")`): `param(name)` ·
`self_attr(name)` · `global(name)` · `nonlocal(name)` · `unknown`. Mutation **kinds** (`via`):
`subscript_set` · `subscript_del` · `attr_set` · `attr_del` · `method(name)` · `aug_subscript` ·
`aug_attr` · `aug_name` (`+=`/`-=`/… on a plain name — a may-mutation, since the operator's
effect on the underlying object depends on its runtime type).

### Shapes

`ParamInfo::shape` (and every local's inferred shape) is the recursive `Shape` lattice:
`Int`/`Float`/`Bool`/`Str`/`Bytes`/`None`/`Seq`/`Map`/`Set`/`Any`/`Union`. Scalars and `Any`
serialize as a bare tag string (`"int"`, `"any"`, …); containers as a single-key tagged object
(`{"seq": <Shape>}`, `{"set": <Shape>}`, `{"map": {"key": <Shape>, "value": <Shape>}}`).

`Union` serializes as `{"union": [<Shape>, ...]}` and is **canonical**: members are built by
`Shape::join`/`Shape::union_of`, which flatten nested unions, merge same-constructor members
recursively (so two `Seq` members join their element shapes instead of sitting side by side),
collapse to plain `Any` if any member is `Any` or if the merged member count exceeds
`Shape::UNION_WIDTH_CAP` (8), and collapse to the single member itself if only one remains.
`Optional[X]` is not a distinct shape — it is `Union([X, None])`, rendered by `stub/` as PEP 604
`X | None`. Where earlier versions of the shape lattice collapsed any join conflict straight to
`Any`, a join between different constructors now preserves both as **disjoint evidence** in a
`Union` instead of discarding one side; only a genuinely wide or already-`Any` join gives up and
returns `Any`. Deserializing a `{"union": [...]}` value re-runs it through `union_of`, so a
hand-written or previously-serialized union is always re-canonicalized on read, never trusted
as-is.

### Declared-vs-inferred mismatches (`type_mismatches`)

The TypeCheck pass flags contradictions between an untrusted declared annotation and the
statically inferred may-set, for **both** the return type and each parameter. Each entry has a
`kind` of `"return"` or `"param"`:

- `"return"` — `declared` is `declared_return`; `inferred` is the (possibly multi-member)
  `returns` may-set the declaration contradicts; `param`/`inferred_shape` are absent.
- `"param"` — `declared` is that parameter's own annotation; `param` names it; `inferred_shape`
  is its inferred `Shape`; `inferred` is empty.

Flagged **only** on full disjointness (a declared type with at least one permitted kind present
in the inferred side is never flagged), and never for a wildcard annotation (`Any`, an
unrecognized `Optional`/`Union`). Numeric-tower subtyping is accepted on both sides: `bool` ⊆
`int` ⊆ `float` (PEP 484), so a declared `float` does not conflict with an inferred `int` or
`bool`. Advisory only — `type_mismatches` is a report field, never consulted by the may-set or
the Purity pass.

## Soundness invariant

With may-sets, the static set should be a *superset* of whatever any execution does:

> **observed_effects ⊆ static_may_set**

Every observed effect not predicted statically is a **soundness bug** — a reward-hacking
hole — and must be driven to zero. The reverse (predicted-but-never-observed) is imprecision:
measured, tolerated, kept low. Dynamic testing **falsifies** soundness (finds holes); it does
not prove it.

## Architecture / modules

- `parse/` — ruff wrapper. **All `ruff_*` imports are isolated here**; parses source into
  function/method defs. Swapping parsers means rewriting `parse/` + `analyze/`, not consumers.
  Every source-ingestion point (file read, stdin, per-project-file read) strips a leading UTF-8
  byte-order mark (`lib::strip_bom`) before the source reaches `parse/` or the jail: CPython
  accepts a BOM-prefixed file, but a stray U+FEFF reaching the jailed worker's `compile()` as
  text is a `SyntaxError`, and it's invisible noise to the ruff parser besides — so the analyzer
  and the jail must always see identical, BOM-less source.
- `model/` — split into `mod.rs` (`EffectSignature` and the rest of the JSON-facing types:
  `Mutation`/`MutationTarget`, `Raises`, `UnresolvedEffect`, `TypeMismatch`, `Import`/`ModuleRef`,
  `Purity`, `ParamInfo`/`ParamKind` (`Positional`/`VarPositional`/`VarKeyword`/`KeywordOnly`)) and
  `shape.rs` (the recursive `Shape` lattice — see "Shapes" above — plus its serde `Serialize`/
  `Deserialize` impls and `join`/`union_of`). Pure, parser-independent.
- `analyze/` — an ordered **pass pipeline** (**Imports → Declarations → Shapes → Effects →
  Interprocedural → TypeCheck → Purity**) over a shared `ModuleAnalysis` context, driven by a
  single AST walk with per-concern **collectors** (aliases, mutations, exceptions, shapes,
  returns, guards):
  - **Imports** — builds the import binding table.
  - **Declarations** — builds a function/method **symbol table** (name, params, receiver kind,
    decorators); collected now, consumed by Interprocedural to resolve local call sites.
  - **Shapes** — fixpoint-infers a name→`Shape` environment per function (recursive, nested
    container shapes) before Effects runs, so Effects reads settled shapes instead of voting
    mid-walk.
  - **Effects** — the per-function AST walk, delegating to collectors; also collects
    explicit/implicit raises (including the guard-derived collector, `collect/guards.rs`, which
    pulls literal/boundary samples off `if`/`assert`/`while`/ternary tests for generation to use)
    and unknown decorators. Also records structured intra-module **call sites** (callee index,
    positional-argument roots, whether the call was `self.`/`cls.`-qualified) for Interprocedural
    to consume.
  - **Interprocedural** (`passes/interprocedural.rs`) — resolves calls to functions/methods
    defined in this *same file* (via the call sites Effects recorded) and propagates the
    callee's mutations/raises/I/O/global-writes/`is_generator`/`unresolved_effects` onto the
    caller. A callee mutation on a parameter remaps onto the caller-side root passed for that
    parameter **by position** if the caller passed it positionally (matched against the callee's
    `Positional` params only), **by name** if the caller passed it as a keyword (matched against
    the callee's `Positional` or `KeywordOnly` params by name); it's dropped (not attributed to
    any caller root, never mis-attributed) if the callee param is `*args`/`**kwargs` or the
    keyword names nothing declared. `SelfAttr` mutations propagate unchanged only through a
    `self.`/`cls.`-qualified call on the caller's own receiver; `Global` mutations propagate
    unchanged unconditionally. An unpacked call site (`f(*xs)` / `f(**kw)`) is handled for
    soundness even when the callee resolves: positional-root mapping truncates at the first
    `*`-unpack (arguments after it can't be attributed positionally), the unpacking operation
    itself gets an implicit `TypeError` (a non-iterable splat, non-mapping `**`, or an
    arity/duplicate-keyword mismatch can raise before the callee ever runs), and a
    `call_unpacked_args` unresolved effect acknowledges the roots reaching the callee through the
    unpack that the mapping couldn't attribute. Iterates to a fixpoint (bounded by
    `signatures.len() + 1` sweeps) so multi-hop chains and recursion (direct or mutual) settle
    without an infinite loop. Calls through imports or to otherwise-unresolved callees are left
    as opaque `unresolved_effects` here — this pass is **intra-file only**. Cross-file
    propagation (following a `project_local`-resolved import into another project file's
    free-function signatures, with a project-wide fixpoint, including the same keyword-argument
    mapping) runs at the project level in `project::interproc`, in directory/project mode only;
    an unpacked cross-file call site keeps its `call_import` acknowledgment even after the
    import resolves, for the same reason.
  - **TypeCheck** (`passes/type_check.rs`) — compares each function's final `returns` may-set
    against its untrusted `declared_return` annotation, and separately compares each parameter's
    final inferred `Shape` against its own declared annotation, appending a `type_mismatches`
    entry (`kind: "return"` or `"param"`) only when the two sides are fully disjoint (never on a
    mere subset mismatch, and never for a wildcard annotation like `Any`/`Optional`); numeric-tower
    subtyping (`bool` ⊆ `int` ⊆ `float`) is accepted on both checks. See "Declared-vs-inferred
    mismatches" above for the field shape. Runs after Effects/Interprocedural since it needs the
    final `returns`/param shapes; interprocedural propagation never touches either, so the
    ordering relative to Interprocedural doesn't matter in practice.
  - **Purity** — derived classification from collected facts (runs last, after propagation so it
    sees a caller's *complete* effect set); any unresolved effect or unrecognized decorator ⇒
    `unknown`, never `pure`.
- `generate/` — usage-shape-directed input generation: an even spread over the cartesian
  product of per-parameter candidates (so argument *combinations* are exercised), recursive and
  breadth-capped for nested shapes; `*args`/`**kwargs` are excluded from positional generation,
  keyword-only params are generated and passed by name. **Guard-guided**: each parameter's
  candidate pool is extended with the literal/boundary samples the guards collector pulled off
  the function's own guard expressions, so generation is more likely to land on both sides of a
  guarded branch instead of missing it by chance.
- `record/` — joins static signatures with jailed execution into per-function records:
  dependency probing, module-load + constructor probing (so an unloadable module or
  unbuildable receiver is reported once as `uncallable`, not as N identical per-case failures),
  and case construction. Distinguishes a **resource kill** (`outcome: "error"`,
  `error.stage: "resource"` — OOM/recursion-limit/timeout, an artifact of the sandbox) from a
  **semantic raise** (`outcome: "raised"` — part of the function's own behavior). No comparison,
  no score — that's the consumer's.
  - **Input minimization** (`shrink.rs`) — for every `outcome: "raised"` case, greedily shrinks
    its input toward a smaller one that still raises the *same exception type*: one argument
    (positional, then keyword-only) at a time, trying that argument's `generate::shrink_candidates`
    in order and accepting the first candidate whose re-execution still raises the same exception
    type; accepting one restarts that argument's scan at the new, smaller value, and a full pass
    with no acceptance ends the search. Bounded by a 32-jailed-call budget
    (`shrink::SHRINK_BUDGET`) total per case. A resource-killed or different-exception
    re-execution just rejects that candidate — it is never folded in as a new observation. The
    result (only present when at least one argument actually shrank) is `Case::minimized`
    (`MinimizedInput { input, kwargs }`), a reporting aid attached to the `record` output; it is
    never fed back into `validate`, which checks only the original observation.
- `exec/` — dynamic effect observer. A `Sandbox` trait with launcher-pluggable backends
  (`nsjail` native / `wsl`-wrapped); **no unsandboxed launcher**. Owns the JSON-over-stdio
  worker protocol (including `kwargs`), the structured `HarnessError` (with `is_resource()`),
  and `CallResult` parsing.
- `python/worker.py` — the in-jail CPython harness (serialize-before / serialize-after for
  mutation diffs; stdout and stderr captured separately; load/ctor probe via a null `fn`).
- `validate/` — the `observed ⊆ static` self-validation harness: given a `FunctionRecord`
  (signature + cases), checks that every observed mutation/raise/return/I/O is covered by the
  static may-set. A gap is `Hard` (signature claimed completeness) or `Soft` (signature already
  flagged `unresolved_effects`). Pure, no jail, no I/O — driven by `pylens validate`.
- `report/` — output formatting: versioned JSON (top-level `schema_version`) and a thin
  terminal `--format summary` for `analyze`/`record`/`validate`.

  **Schema versioning.** `schema_version` (the `SCHEMA_VERSION` const in `lib.rs`) names the
  version of the JSON contract, so a downstream consumer can detect that the shape of the data
  it parses has changed. pylens has never shipped a release, so the contract has no consumers
  yet and the version is pinned at **`0.1`**: every contract change — additive or breaking —
  folds into `0.1` without a bump. Do not bump it during development. The policy switches at
  the first release: the shipped contract becomes `1.0`, and from then on any change to what
  the JSON contains bumps the version — major for changes that can break an existing parser
  (removed/renamed fields, changed tags or value shapes), minor for purely additive fields.
- `project/` — multi-file ("directory") mode: walks a directory for `*.py` files using the
  `ignore` crate (honors `.gitignore`/`.ignore` hierarchically, even outside a git repo, plus an
  explicit skip-list — `__pycache__`/`venv`/`env`/`node_modules`/`build`/`dist`/`target` — for
  common noise dirs a project may not have gitignored), runs `analyze`/`record`/`validate` over
  each, and aggregates into one project report (`{ schema_version, root, files: [...], summary }`);
  a file that fails to read/parse/record becomes a `{ path, error }` entry rather than aborting
  the whole run. The CPU-bound, jail-free `analyze` path fans files out across a small
  worker-thread pool. `record`/`validate` also run in **parallel**: a fixed pool of worker
  threads (`available_parallelism`, capped at 4 — `JAIL_WORKER_CAP` — and at the file count),
  each owning its own private single-worker `NsjailPool` (one `wsl`/nsjail fork-server process
  per worker; files are independent, so no pool is ever shared across threads) pulls files off a
  shared work queue. `validate`'s aggregate `summary` also carries `uncallable` — the count of
  functions across the project that never executed (module didn't load, constructor failed), so
  a run with zero defects but many uncallable functions doesn't read as "fully validated".
  - `project::resolve` — static, jail-free **project-local import resolution**: builds a
    `ModuleIndex` (dotted importable path → project file) once per project, then resolves each
    file's imports (absolute *and* relative, including `__init__.py` package semantics and
    `level > 1` climbing) against it, tagging each import/dependency with a `resolution`
    (`project_local` / `external` / `unresolved_relative`) and, when project-local, a
    `project_target` path. Applies equally to `analyze` (no jail) and `record`/`validate`
    (already jailed, but resolution itself needs none). Independent of and unrelated to the
    jailed dependency probing in `record/` (which answers "does this module *load*", not "is it
    a project file").
- `stub/` — pure string rendering of inferred `.pyi` type-hint stubs (Python 3.10+ syntax:
  `list[...]`/`dict[...]` builtin generics, PEP 604 `X | Y` unions — including a `Shape::Union`
  param rendering straight to `A | B`, and `Optional[X]` (`Union([X, None])`) rendering as
  `X | None` with no special case needed — `Iterator[Any]` for generators) from the same
  `EffectSignature`s `analyze` produces; no jail, no I/O. Consumed by `analyze --format pyi`.
  `stub::observed` (consumed by single-file `record --format pyi` only, not directory mode) is
  an additive enrichment layer on top: for every param/return the static side left unresolved
  (`Shape::Any` / `ReturnKind::Opaque`), it folds in the type observed across that function's
  recorded `record` cases — but only when every observation *agrees* (no majority vote, no
  partial credit) and only for encodings the worker's tagged JSON makes faithfully
  distinguishable (a bare JSON array is `list`; `{"__t__": "tuple"/"set"/"dict"}` distinguish
  those from a plain object; `bytes` and arbitrary objects share the same generic `{"__t__":
  "obj", ...}` shape and are never folded in). A folded-in type is marked with a trailing
  `# observed: ...` comment on the `def` line — a sample from generated cases, never a proof — and
  a static-known shape always wins over an observed one.
- `html/` — renders an already-assembled `analyze`/`record`/`validate` JSON body (the same
  `serde_json::Value` the JSON output emits) into one self-contained `<!doctype html>` document
  (inline `<style>` only, no external assets); one renderer handles both the single-file shape
  and the project-report shape (detected by the presence of a top-level `files` array). All
  dynamic text is HTML-escaped before being written, since it originates in analyzed Python
  source. Consumed by `--format html` on all three commands.

## Execution layer (sandbox model — decided)

The dynamic layer must execute gold/candidate functions, observe real effects (return,
argument mutations, raised exceptions), serve as the RL oracle, and validate the analyzer.

**Threat model.** This runs at **dataset-build time**, not in the training loop — so the
execution host is "wherever we build the dataset" (today: Windows; later: Linux). **All
input code is untrusted** — gold *and* candidate alike. Therefore every execution is jailed
and there is **no unsandboxed launcher**, not even for dev or tests.

**The sandbox is nsjail on a Linux kernel — full stop.** It wraps each `worker.py`
invocation in namespaces + a seccomp-bpf filter + rlimits (mem/CPU/time/pids caps, no
network, read-only FS). The seccomp filter is a **denylist** of the unambiguously dangerous
syscalls (ptrace, mount, module load, kexec, setns/unshare, bpf, …) layered on top of dropped
capabilities + `NO_NEW_PRIVS` — chosen over a strict CPython allowlist, which is fragile
across interpreter versions and silently breaks the worker. Tightening to an allowlist is a
tracked future hardening. The launch host only differs in *how a Linux kernel is reached*:

| Host | Launcher | Command shape |
|---|---|---|
| Linux | `nsjail` (native) | `nsjail <policy> -- python worker.py` |
| Windows | `wsl` → `nsjail` | `wsl -d <distro> -- nsjail <policy> -- python worker.py` |

Same nsjail policy, same `worker.py`, same JSON-over-stdio protocol, same `CallResult`
parsing. The only per-host variation is the command prefix.

**Why no Docker.** WSL2 is itself a real Linux kernel *and* a lightweight Hyper-V utility VM,
so on Windows the untrusted code already sits behind two boundaries —
`untrusted python → nsjail → WSL2 VM → Windows host` — without a container. Docker would add
only a third layer plus a daemon dependency; its sole remaining value is *reproducible
provisioning* of nsjail+python (nsjail is a from-source build, not a stock package). That is
a **future** convenience launcher for hermetic CI parity, not a requirement, and is kept
behind the same trait so it can slot in later.

**`Sandbox` trait.** One interface, launcher-pluggable, in priority order:

1. **`nsjail`** — native on Linux, `wsl --`-wrapped on Windows. *The* execution path.
2. *(future)* **`docker`** — only if hermetic provisioning / CI parity is wanted.

There is deliberately **no** `unsandboxed` launcher. The current plain-`python`-subprocess
`Worker` is replaced by the nsjail launcher; the `record` flow and the tests run under nsjail
(via WSL2 on Windows), so a developer must provision a WSL distro with nsjail + Python
(one-time setup script) before `record`/`cargo test` will run.

**Execution mechanics (unchanged by the host split):**

- **Real CPython** for exact oracle fidelity; **persistent workers** amortize interpreter
  startup → IPC-bound per call.
- **Mutation observability:** serialize args to tagged JSON **before** the call and **after**,
  then diff the two snapshots (no `deepcopy` — the pre-snapshot is an independent value); tagged
  JSON covers non-JSON-native types (set/tuple/dict/objects).
- **Aliasing:** the harness reports **identity relations** (`return is arg[i]`, arg–arg
  aliasing) — deepcopy + JSON alone destroys identity, which our static model tracks.
- **Methods:** instantiate the receiver from `__init__` and capture **`self` pre/post** state,
  not just positional args.
- **Mutation derivation** uses **structural equality** (float tolerance, set/dict ordering)
  to diff before/after, rather than raw JSON field diff.
- **Captured stdout/stderr:** the function's own output is captured (recorded as an effect, and
  kept out of the JSON protocol channel).

**Rejected alternatives** (research: `docs/research/python-execution.md`): CPython-WASI
(~1.2 s cold start, stdlib gaps), RustPython (fidelity), Monty (immature, v0.0.18); Docker as
a *required* layer (redundant with the WSL2 VM; daemon dependency).

The Rust analyzer is unaffected — it emits a JSON effect-signature spec the execution layer
consumes.

## Phase plan

1. **Parse + behavior-set signature** (returns/raises/mutations/io/unresolved, may-sets) →
   JSON; snapshot-tested on a hand-written corpus. *(Foundation — current milestone.)*
2. **Generation hints** — usage-based param shapes. *(Done.)*
3. **Execution: jailed effect observer + `record`** — per function/method, run generated
   inputs in the jail and emit static signature + observed cases (returns, raises, argument &
   `self` mutations, aliasing, stdout/stderr). Includes the import catalog, jailed dependency
   resolution, the per-function `uses` edge, and the module-load/constructor `uncallable` hoist.
   *(Done.)*
4. **Self-validation harness** — `observed ⊆ static` over the corpus → soundness-defect count.
   *Keystone: proves the analyzer. The records already carry both sides; this adds the check.*
   *(Done — `pylens validate` and `src/validate.rs`; the example corpus validates with zero
   hard defects.)*
5. **Guided generation** — guard-directed inputs (literals/boundaries pulled from `if`/`assert`/
   `while`/ternary guards on parameters) to reach guarded branches, plus greedy input
   minimization for raised cases. *(Done — see `collect/guards.rs`, `generate/`, and `shrink.rs`
   for minimization.)*

**Out of scope:** comparing a candidate to a reference and scoring it (an RL *reward*). pylens
produces records; how a consumer turns records into a training signal is theirs to decide.

## Open questions

- **Cross-file propagation depth** — cross-file call resolution, including keyword-argument
  mapping, is done for free functions in project mode (`project::interproc`); imported *methods*
  and deeper dotted call chains are still treated as `unresolved`.
- **Equality semantics** for return/exception comparison — float tolerance, set/dict ordering,
  NaN, exception type-only vs message.
