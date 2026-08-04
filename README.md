# pylens

A standalone, general-purpose tool for **behavioral analysis of Python code**: static effect
and shape analysis, plus jailed execution to **record what a function actually does** on
generated inputs.

Given a Python function, pylens extracts its **effect signature** — what it returns, which
arguments and `self`-attributes it mutates, what it raises (explicit and statically-inferred
implicit exceptions), whether it's a generator, its I/O, its decorators, and the calls it can't
see through — plus recursive, nested **parameter shapes** (`int`, `str`, `Seq(Float)`,
`Map(Str, Seq(Int))`, …). It then runs the function in a sandbox on generated inputs and records
the **observed effects** per input, and can check that everything observed was statically
predicted. The static analyzer is Rust (via the [ruff](https://github.com/astral-sh/ruff)
parser, single AST walk, pass pipeline); execution is real CPython inside
[nsjail](https://github.com/google/nsjail).

```
parse (ruff) ─▶ pass pipeline (imports/declarations/shapes/effects/purity) ─▶ static effect
signature ─▶ generate inputs ─▶ run in jail ─▶ record observed effects ─▶ validate observed ⊆ static
```

## Requirements

- Rust (edition 2024) — the ruff parser is pinned as a git dependency, so the first build
  fetches and compiles it (~1–2 min).
- **A sandbox for the `record` command.** All executed code is treated as untrusted, so every
  run is jailed with [nsjail](https://github.com/google/nsjail) on a Linux kernel — there is
  **no unsandboxed path**. On Linux that's native; on **Windows it runs in WSL2** (itself a
  Linux kernel and a lightweight VM, so no Docker is needed). One-time setup:

  ```sh
  # Windows: run inside your WSL2 distro (builds nsjail, deploys the worker + policy)
  wsl -d Ubuntu -- bash /mnt/c/Projects/ai/pylens/scripts/provision-sandbox.sh
  # Linux:
  bash scripts/provision-sandbox.sh
  ```

  The `analyze` command is pure Rust and needs none of this.

## Build

```sh
cargo build --release
```

## Quick start

Static effect signatures as JSON (no jail needed):

```sh
pylens analyze examples/inventory.py
```

Full records — `{ dependencies, functions }`: every import probed for resolution, plus each
function's signature and observed cases (runs the jail):

```sh
pylens record examples/deps.py      # which imports resolve
pylens record examples/inventory.py # signatures + cases
```

The `dependencies` section reports each import (every style — plain, `as`, dotted,
`from … import a, b`, `*`, relative). Each carries the structured `module` (`{ package, path }`),
its `scope` (`module` / `function`), and a resolution `status` (`resolved` / `unresolved` /
`not_probed`):

```jsonc
"dependencies": [
  { "from": false, "module": { "package": "os" }, "scope": "module", "status": "resolved" },
  { "from": false, "module": { "package": "numpy" }, "alias": "np",
    "scope": "module", "status": "unresolved",
    "error": { "stage": "setup", "kind": "ModuleNotFoundError",
               "message": "No module named 'numpy'", "module": "numpy" } },
  { "from": false, "module": { "package": "xml", "path": "etree.ElementTree" },
    "alias": "ET", "scope": "module", "status": "resolved" }
]
```

A missing **module-scope** import stops the whole file loading, so each function is marked
`uncallable` once (with the structured reason) instead of emitting identical per-case errors.
Relative imports are `not_probed` — resolving them needs package context a standalone file
doesn't supply.

A case from `Inventory.add(name, qty)` — a successful call that mutates the receiver:

```jsonc
{
  "input": [1, 7],
  "ctor_args": [],
  "outcome": "returned",
  "return": 7,
  "mutations": [
    { "target": "self",
      "before": { "items": {}, "log": [], "capacity": 100 },
      "after":  { "items": {"1": 7}, "log": [["add", 1, 7]], "capacity": 100 } }
  ]
}
```

## Commands

| Command | Description |
|---|---|
| `pylens analyze <file.py> [--format json\|summary]` | `{ imports, functions }`: catalogued imports + the static effect signature of every function/method. Reads stdin if no file is given. No jail. |
| `pylens record <file.py> [--inputs <N>] [--format json\|summary]` | `{ dependencies, functions }`: imports probed for resolution **plus** observed cases per function/method (generated inputs run in the jail). `--inputs` caps generated vectors (default 4). |
| `pylens validate <file.py> [--inputs <N>] [--format json\|summary]` | Runs `record`, then checks `observed ⊆ static` per function: any observed effect the static signature didn't predict is a soundness defect. Exits non-zero on any **hard** defect (a function claiming no unresolved effects that still misses one). |

`--format` defaults to `json` (every output is versioned with a top-level `schema_version`);
`--format summary` renders a thin terminal summary instead.

## What it detects (static)

Per function/method, as may-sets (over-approximating) unioned over all exits, produced by a
pass pipeline (**Imports → Declarations → Shapes → Effects → Purity**) over a single AST walk:

- **Return** kinds (`int`, `str`, `sequence`, `none`, …) inferred from the body
- **Argument mutations** — `p[i]=…`, `p.attr=…`, `del`, mutating methods (`append`/`sort`/…),
  augmented assignment, with a local alias map (`q = p; q.append(1)` ⇒ `p`)
- **`self`-attribute writes** (methods), including `@classmethod` receivers
- **Recursive parameter shapes** — `int`/`float`/`bool`/`str`/`bytes`/`none`, and nested
  containers (`Seq`/`Map`/`Set`) inferred to a fixpoint (e.g. `matrix: Seq(Seq(Float))`);
  `*args`/`**kwargs` are marked as variadic and never generated positionally, keyword-only
  params are generated and passed by name
- **Raised exceptions** — explicit (`raise`, `assert` ⇒ `AssertionError`) and statically-inferred
  **implicit** may-sets (`ZeroDivisionError`, `IndexError`/`KeyError` on subscript,
  `ValueError` from `int()`/`float()`, `TypeError` on ordered-compare/arithmetic over
  `Any`-typed operands)
- **Generators**, global writes, basic I/O, unknown decorators (recorded and downgrade purity —
  a decorator can replace the function entirely)
- **`uses`** — the imports each function references (`{ binding, module }`), linking functions
  to dependencies
- **`unresolved_effects`** — calls through imports (`call_import`, e.g. `np.sort(arr)`) and
  unknown/dynamic calls (including comprehensions, lambdas, and unknown method calls on a
  param/local), recorded honestly: such a function is `unknown`, never assumed pure

## What it records (dynamic)

Per generated input: the outcome (`returned`/`raised`/`error`), the return value, observed
argument and `self` mutations (before/after), return-aliasing, captured stdout and stderr, and
the exception type when raised. A function that can't run at all (a module-scope import that
won't load, or a constructor that can't be built) is marked `uncallable` once, with a structured
reason, rather than producing identical failing cases.

Resource exhaustion is distinguished from a function's own semantics: a **resource kill**
(out-of-memory, recursion limit, timeout — an artifact of the sandbox) is always
`outcome: "error"` with `error.stage: "resource"`, never `outcome: "raised"`. `raised` means the
function itself raised — part of its behavior.

## `pylens validate` — the soundness check

`pylens validate <file.py>` runs `record`, then checks every observed case against the static
signature: every observed mutation/raise/return/I/O must be covered by the static may-set
(`observed ⊆ static`). A gap where the signature claimed no unresolved effects is a **hard**
defect (a true soundness bug); a gap where the signature already flagged
`unresolved_effects` is a **soft** defect (an acknowledged blind spot). The example corpus in
[examples/](examples/) currently validates with zero hard defects.

## Status

The static analyzer, the import/dependency report, the `record` flow, and the `observed ⊆
static` self-validation harness (`pylens validate`) all work today (see [examples/](examples/)
and `cargo test`). Execution is **always nsjail-jailed** — native on Linux, via WSL2 on Windows —
with no unsandboxed path; `record_file` drives a persistent fork-server worker pool
([`src/exec.rs`](src/exec.rs)) that amortizes interpreter startup across the whole file while
keeping per-call isolation. Imports are linked to the functions that use them (`uses`), and calls
through an import (`mod.fn(param)`) are flagged as `call_import` effects so such functions are
never mistaken for pure. All JSON output carries a top-level `schema_version`. Not yet done:
pylens still doesn't model *what* a library call does (whether it mutates its argument, what it
returns); relative-import resolution needs a package-aware, multi-file entry point;
interprocedural/cross-file analysis (the Declarations symbol table lays the groundwork). See
[DESIGN.md](DESIGN.md).

## Docs

- [User guide](docs/USER_GUIDE.md) — walkthrough, signature schema, record semantics, limits
- [DESIGN.md](DESIGN.md) — decisions, effect taxonomy, soundness invariant, phase plan
- [examples/README.md](examples/README.md) — the example corpus
