# pylens

A standalone, general-purpose tool for **behavioral analysis of Python code**: static effect
and shape analysis, plus jailed execution to **record what a function actually does** on
generated inputs.

Given a Python function, pylens extracts its **effect signature** — what it returns, which
arguments and `self`-attributes it mutates, what it raises (explicit and statically-inferred
implicit exceptions), whether it's a generator, its I/O, its decorators, calls to locally-defined
functions/methods it resolves (interprocedural propagation), and the calls it still can't see
through — plus recursive, nested **parameter shapes** (`int`, `str`, `Seq(Float)`,
`Map(Str, Seq(Int))`, …) and declared-vs-inferred return type mismatches. It then runs the
function in a sandbox on generated inputs (guided by literals/boundaries pulled from the
function's own guards) and records the **observed effects** per input, and can check that
everything observed was statically predicted. It also runs over a whole directory, resolving
project-local imports against the other files, and can emit `.pyi` type stubs or a self-contained
HTML report. The static analyzer is Rust (via the [ruff](https://github.com/astral-sh/ruff)
parser, single AST walk, pass pipeline); execution is real CPython inside
[nsjail](https://github.com/google/nsjail).

```
parse (ruff) ─▶ pass pipeline (imports/declarations/shapes/effects/interprocedural/type
check/purity) ─▶ static effect signature ─▶ guard-guided input generation ─▶ run in jail ─▶
record observed effects ─▶ validate observed ⊆ static
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
| `pylens analyze <file.py\|dir> [--format json\|summary\|pyi\|html]` | `{ imports, functions }`: catalogued imports + the static effect signature of every function/method. Reads stdin if no file is given. No jail. |
| `pylens record <file.py\|dir> [--inputs <N>] [--format json\|summary\|pyi\|html]` | `{ dependencies, functions }`: imports probed for resolution **plus** observed cases per function/method (generated inputs run in the jail). `--inputs` caps generated vectors (default 4). `pyi` is single-file only. |
| `pylens validate <file.py\|dir> [--inputs <N>] [--format json\|summary\|html]` | Runs `record`, then checks `observed ⊆ static` per function: any observed effect the static signature didn't predict is a soundness defect. Exits non-zero on any **hard** defect (a function claiming no unresolved effects that still misses one). |

A directory argument recurses over its `*.py` files and produces an aggregated **project
report** — `{ schema_version, root, files: [...], summary }`, one entry per file (or `{path,
error}` if that file failed to parse/record/read) plus aggregate purity/defect counts — instead
of a single-file report. Project mode also resolves each file's imports (absolute and relative)
against the other project files; see "Multi-file / project mode" below.

`--format` defaults to `json` (every output is versioned with a top-level `schema_version`);
`--format summary` renders a thin terminal summary; `--format pyi` (`analyze`, and single-file
`record`) renders inferred `.pyi` type-hint stubs instead of the JSON signature; `--format html`
renders a self-contained HTML report (all three commands, single-file or project).

## What it detects (static)

Per function/method, as may-sets (over-approximating) unioned over all exits, produced by a
pass pipeline (**Imports → Declarations → Shapes → Effects → Interprocedural → TypeCheck →
Purity**) over a single AST walk:

- **Return** kinds (`int`, `str`, `sequence`, `none`, …) inferred from the body
- **Argument mutations** — `p[i]=…`, `p.attr=…`, `del`, mutating methods (`append`/`sort`/…),
  augmented assignment, with a local alias map (`q = p; q.append(1)` ⇒ `p`)
- **`self`-attribute writes** (methods), including `@classmethod` receivers
- **Recursive parameter shapes** — `int`/`float`/`bool`/`str`/`bytes`/`none`, nested
  containers (`Seq`/`Map`/`Set`), and width-capped **unions** (`Optional[X]` = union with
  `None`) inferred to a fixpoint (e.g. `matrix: Seq(Seq(Float))`, `x: Int | None`);
  `*args`/`**kwargs` are marked as variadic and never generated positionally, keyword-only
  params are generated and passed by name
- **Raised exceptions** — explicit (`raise`, `assert` ⇒ `AssertionError`) and statically-inferred
  **implicit** may-sets (`ZeroDivisionError`, `IndexError`/`KeyError` on subscript,
  `ValueError` from `int()`/`float()`, `TypeError` on ordered-compare/arithmetic over
  `Any`-typed operands, and on a subscript whose key roots to an `Any`-typed param)
- **Declared-vs-inferred type mismatches** (`type_mismatches`) — flagged when a function's
  (untrusted) return or parameter annotation is fully disjoint from the inferred side
  (advisory only; `bool` ⊆ `int` ⊆ `float` per the PEP 484 numeric tower)
- **Interprocedural effect propagation (intra-file)** — a call to a function/method defined in
  the same file inherits that callee's mutations/raises/I/O/unresolved effects, to a fixpoint
  (recursion included), so a caller of a mutating local helper is correctly `impure` rather than
  falsely `unknown`. Arguments map onto callee params by position and by keyword name; a call
  that unpacks (`f(*xs)` / `f(**kw)`) keeps an acknowledged `call_unpacked_args` blind spot
  (plus an implicit `TypeError` — the unpack itself can raise) instead of silently dropping
  effects. Calls through imports or otherwise unresolved callees are still opaque
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
the exception type when raised. A raised case is additionally **minimized**: the input is
greedily shrunk (bounded budget) while it keeps raising the same exception type, and the smaller
reproducer lands in `minimized`. A function that can't run at all (a module-scope import that
won't load, or a constructor that can't be built) is marked `uncallable` once, with a structured
reason, rather than producing identical failing cases.

Input generation is **guard-directed**: besides the usual shape-derived spread, it pulls literal
and boundary values straight out of the function's own `if`/`assert`/`while`/ternary guards on
each parameter (`if qty > 10:` seeds `9`, `10`, `11`), so generated inputs are more likely to
reach a guarded branch instead of missing it by chance.

Resource exhaustion is distinguished from a function's own semantics: a **resource kill**
(out-of-memory, recursion limit, timeout — an artifact of the sandbox) is always
`outcome: "error"` with `error.stage: "resource"`, never `outcome: "raised"`. `raised` means the
function itself raised — part of its behavior.

## Multi-file / project mode

Passing a **directory** instead of a file to `analyze`/`record`/`validate` recurses over its
`*.py` files (honoring `.gitignore`/`.ignore`, skipping hidden files and common noise dirs) and
aggregates every file's report into one
project report: `{ schema_version, root, files: [...], summary }`. A file that fails to read,
parse, or record becomes `{ path, error }` in `files` instead of aborting the whole run, and
`summary` carries file/function/purity counts (plus hard/soft defect totals for `validate`).
Single-file behavior is unchanged.

In project mode, every import (absolute and relative) is additionally resolved against the
other files in the project: each import/dependency entry gains a `resolution` field —
`project_local` (with a `project_target` path into the project), `external` (stdlib/third-party,
not a project file), or `unresolved_relative` (a relative import that doesn't land on a project
file). This is purely static (no jail), so it applies to `analyze` too. Previously, relative
imports were reported `not_probed` outside of a project context; in project mode they now
resolve when the target is another file in the tree.

## `.pyi` stubs and HTML reports

`pylens analyze --format pyi` renders inferred `.pyi` type-hint stubs (Python 3.10+ syntax:
`list[...]`/`dict[...]` builtin generics, `|` unions, `Iterator[Any]` for generators) instead of
JSON — one stub per function/method, grouped under `class <Owner>:` blocks for methods, with an
inline comment when a declared annotation contradicts the inferred type. It works over a
single file or a whole directory (one `# <relative path>` block per file). With **`record`**
(single file), dynamically observed types additionally fill in what static analysis left
unresolved, marked with a trailing `# observed:` comment.

`--format html` renders a self-contained, theme-aware `<!doctype html>` report (no external
assets) for `analyze`, `record`, or `validate`, single-file or project — the same underlying data
as the JSON output, laid out for human reading.

## `pylens validate` — the soundness check

`pylens validate <file.py>` runs `record`, then checks every observed case against the static
signature: every observed mutation/raise/return/I/O must be covered by the static may-set
(`observed ⊆ static`). A gap where the signature claimed no unresolved effects is a **hard**
defect (a true soundness bug); a gap where the signature already flagged
`unresolved_effects` is a **soft** defect (an acknowledged blind spot). The example corpus in
[examples/](examples/) currently validates with zero hard defects.

## Status

The static analyzer, the import/dependency report, the `record` flow, guard-guided input
generation, input minimization, intra-file interprocedural effect propagation,
declared-vs-inferred type checking (returns and params), multi-file/project mode with
project-local import resolution, `.pyi`/HTML output, and
the `observed ⊆ static` self-validation harness (`pylens validate`) all work today (see
[examples/](examples/) and `cargo test`). Execution is **always nsjail-jailed** — native on
Linux, via WSL2 on Windows — with no unsandboxed path; `record_file` drives a persistent
fork-server worker pool ([`src/exec.rs`](src/exec.rs)) that amortizes interpreter startup across
the whole file while keeping per-call isolation; in project mode, files are recorded in
parallel across a small pool of such workers. Imports are linked to the functions that use
them (`uses`), and calls through an import (`mod.fn(param)`) are flagged as `call_import` effects
so such functions are never mistaken for pure; calls to functions/methods defined in the *same*
file are resolved and their effects propagated onto the caller, and in project (directory) mode
calls to `project_local`-imported free functions are propagated **across files** too. All JSON
output carries a top-level `schema_version`. Not yet done: pylens still doesn't model *what* an
external library call does (whether it mutates its argument, what it returns); cross-file
propagation covers free functions (imported methods / deep dotted chains stay unresolved). See
[DESIGN.md](docs/DESIGN.md).

## Docs

- [User guide](docs/USER_GUIDE.md) — install, sandbox setup, commands, limits, troubleshooting
- [SCHEMA.md](docs/SCHEMA.md) — the JSON output contract, field by field, and versioning policy
- [DESIGN.md](docs/DESIGN.md) — effect taxonomy, soundness invariant, architecture, sandbox rationale
- [examples/README.md](examples/README.md) — the example corpus
