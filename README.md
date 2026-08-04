# pylens

Static **effect analysis** of Python functions, plus jailed execution to **record what a
function actually does** on generated inputs.

Given a Python function, pylens extracts its **effect signature** — what it returns, which
arguments and `self`-attributes it mutates, what it raises, whether it's a generator, its I/O,
and the calls it can't see through. It then runs the function in a sandbox on generated inputs
and records the **observed effects** per input. The static analyzer is Rust (via the
[ruff](https://github.com/astral-sh/ruff) parser); execution is real CPython inside
[nsjail](https://github.com/google/nsjail).

```
parse (ruff) ─▶ static effect signature ─▶ infer param shapes ─▶ generate inputs ─▶ run in jail ─▶ record observed effects
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
| `pylens analyze <file.py>` | `{ imports, functions }`: catalogued imports + the static effect signature of every function/method, as JSON. Reads stdin if no file is given. No jail. |
| `pylens record <file.py> [--inputs <N>]` | `{ dependencies, functions }`: imports probed for resolution **plus** observed cases per function/method (generated inputs run in the jail), as JSON. `--inputs` caps generated vectors (default 4). |

## What it detects (static)

Per function/method, as may-sets (over-approximating) unioned over all exits:

- **Return** kinds (`int`, `str`, `sequence`, `none`, …) inferred from the body
- **Argument mutations** — `p[i]=…`, `p.attr=…`, `del`, mutating methods (`append`/`sort`/…),
  with a local alias map (`q = p; q.append(1)` ⇒ `p`)
- **`self`-attribute writes** (methods)
- **Raised exceptions** (explicit `raise`)
- **Generators**, global writes, basic I/O
- **`uses`** — the imports each function references (`{ binding, module }`), linking functions
  to dependencies
- **`unresolved_effects`** — calls through imports (`call_import`, e.g. `np.sort(arr)`) and
  unknown/dynamic calls, recorded honestly: such a function is `unknown`, never assumed pure

## What it records (dynamic)

Per generated input: the outcome (`returned`/`raised`/`error`), the return value, observed
argument and `self` mutations (before/after), return-aliasing, captured stdout and stderr, and
the exception type when raised. A function that can't run at all (a module-scope import that
won't load, or a constructor that can't be built) is marked `uncallable` once, with a structured
reason, rather than producing identical failing cases.

## Status

The static analyzer, the import/dependency report, and the `record` flow work today (see
[examples/](examples/) and `cargo test`). Execution is **always nsjail-jailed** — native on
Linux, via WSL2 on Windows — with no unsandboxed path; `record_file` drives a persistent
fork-server worker pool ([`src/exec.rs`](src/exec.rs)) that amortizes interpreter startup across
the whole file while keeping per-call isolation. Imports are now linked to the functions that
use them (`uses`), and calls through an import (`mod.fn(param)`) are flagged as `call_import`
effects so such functions are never mistaken for pure. Not yet done: pylens still doesn't model
*what* a library call does (whether it mutates its argument, what it returns); relative-import
resolution needs a package-aware, multi-file entry point; interprocedural/cross-file analysis;
and a `observed ⊆ static` soundness check over the records. See [DESIGN.md](DESIGN.md).

## Docs

- [User guide](docs/USER_GUIDE.md) — walkthrough, signature schema, record semantics, limits
- [DESIGN.md](DESIGN.md) — decisions, effect taxonomy, soundness invariant, phase plan
- [examples/README.md](examples/README.md) — the example corpus
