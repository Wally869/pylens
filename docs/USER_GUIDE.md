# pylens user guide

- [1. What pylens is for](#1-what-pylens-is-for)
- [2. Install & build](#2-install--build)
- [3. `analyze` — effect signatures](#3-analyze--effect-signatures)
- [4. The effect signature schema](#4-the-effect-signature-schema)
- [5. `record` — signatures + observed cases](#5-record--signatures--observed-cases)
- [6. How a record is built](#6-how-a-record-is-built) (includes `pylens validate`)
- [7. Using pylens from Rust](#7-using-pylens-from-rust)
- [8. Multi-file / project mode](#8-multi-file--project-mode)
- [9. `.pyi` stubs and HTML reports](#9-pyi-stubs-and-html-reports)
- [10. Limitations & gotchas](#10-limitations--gotchas)
- [11. Troubleshooting](#11-troubleshooting)

---

## 1. What pylens is for

pylens answers two questions about a Python function:

1. **What are its effects?** (static) — what it returns, which arguments it mutates in place,
   what it raises, whether it does I/O or is a generator, and which calls it can't see through.
2. **What does it actually do on real inputs?** (dynamic) — execute it in a sandbox on
   generated inputs and record the observed effects per input (`cases`).

pylens produces the **behavioral record of a function**: its static signature plus the
concrete cases. It does **no** comparison and computes **no** score — turning records into a
training signal (a reward, a diff against a reference, whatever) is the consumer's job, not
pylens's. Because the analysis is meant to survive an adversary, it *over-approximates* and
records its blind spots explicitly (`unresolved_effects`) rather than assuming a function is
pure. This over-approximation is a hard guarantee, not a best effort:

> **`observed_effects ⊆ static_may_set`**

Every effect a real execution turns up must already be covered by the static signature. `pylens
validate` measures this invariant directly (§6). A gap where the signature claimed completeness
is a **hard** defect (a soundness bug); a gap the signature already flagged via
`unresolved_effects` is a **soft** defect (an acknowledged blind spot, still worth tightening).

The unit of analysis is always a **single function or method** (including `self`).

---

## 2. Install & build

Requirements:

- **Rust**, edition 2024. The Python parser is the [ruff](https://github.com/astral-sh/ruff)
  parser, pinned as a git dependency, so the first build fetches and compiles it (one-time,
  ~1-2 minutes).
- **A provisioned sandbox** — only for the `record`/`validate` commands (and the jailed tests).
  All executed code is untrusted, so every run is jailed with
  [nsjail](https://github.com/google/nsjail) on a Linux kernel; there is **no unsandboxed path**.
  On Linux nsjail runs natively; on Windows it runs in **WSL2** (a Linux kernel + lightweight
  VM — no Docker required). `analyze` is pure Rust and needs none of this.

```sh
cargo build --release
# binary at target/release/pylens
```

Provision the sandbox once (builds nsjail, deploys the worker + policy into
`$HOME/.local/share/pylens/`):

```sh
# Windows: run inside your WSL2 distro
wsl -d Ubuntu -- bash /mnt/c/Projects/ai/pylens/scripts/provision-sandbox.sh
# Linux:
bash scripts/provision-sandbox.sh
```

**Gotcha that bites everyone:** the jail runs a **deployed copy** of `python/worker.py` at
`$HOME/.local/share/pylens/worker.py` inside the distro. Editing the repo copy does **not**
update the jail — re-run the provision script, or redeploy directly:

```sh
wsl -d Ubuntu -- install -m 0644 /mnt/c/Projects/ai/pylens/python/worker.py \
  "$HOME/.local/share/pylens/worker.py"
```

Otherwise `record`/`validate` (and the jailed tests) run against a stale worker and you'll chase
phantom behavior.

Run the test suite. The record/pool/validate/project tests execute jailed; if the sandbox isn't
provisioned they **skip** (printing why) rather than fall back to an unsandboxed run:

```sh
cargo test
```

---

## 3. `analyze` — effect signatures

```sh
pylens analyze <file.py|dir> [--format json|summary|pyi|html]
```

`analyze` reads a file (or stdin, if no path is given) and prints
`{ "schema_version": "1.2", "imports": [...], "functions": [...] }` — the catalogued imports
(see [§5](#5-record--signatures--observed-cases) for the import shape) plus one
[effect signature](#4-the-effect-signature-schema) per function and per method defined
directly in a class body. Passing a **directory** instead switches to
[project mode](#8-multi-file--project-mode).

Example:

```python
# normalize.py
def normalize(items, scale=1.0):
    if not items:
        raise ValueError("empty")
    total = sum(items)
    for i in range(len(items)):
        items[i] = items[i] / total * scale
    return items
```

```sh
pylens analyze normalize.py
```

```jsonc
{
  "schema_version": "1.2",
  "imports": [],
  "functions": [
    {
      "name": "normalize",
      "kind": "function",
      "params": [
        { "name": "items", "shape": { "seq": "float" }, "has_default": false },
        { "name": "scale", "shape": "any",               "has_default": true  }
      ],
      "is_generator": false,
      "returns": ["opaque"],
      "raises": {
        "explicit": ["ValueError"],
        "implicit": ["IndexError", "ZeroDivisionError", "TypeError"]
      },
      "mutations": [
        { "target": { "root": "param", "name": "items" }, "via": "subscript_set" }
      ],
      "global_writes": [],
      "io": [],
      "unresolved_effects": [],
      "purity": "impure"
    }
  ]
}
```

Read it as: *takes a sequence of floats `items` and an optional `scale`; mutates `items` by item
assignment; may raise `ValueError` explicitly, plus `IndexError`/`ZeroDivisionError`/`TypeError`
as statically-inferred implicit may-sets; returns a value of opaque kind (a returned argument,
so its type isn't statically known).* A function that references imports also carries a `uses`
list (and the calls through them show up in `unresolved_effects`) — see
[§5](#5-record--signatures--observed-cases).

---

## 4. The effect signature schema

All effect sets are **may-sets**: over-approximations meant to be a superset of what any
execution actually does. Returns and raises are **unioned over all exit points** — a single
function is a *set of behaviors*, not one flat type.

| Field | Meaning |
|---|---|
| `name`, `kind` | function name; `"function"` or `"method"` |
| `owner` | for a method, the class it's defined in (needed to build a receiver); absent for free functions |
| `params` | each parameter with a usage-inferred `shape` and whether it has a default. Excludes the method receiver. |
| `declared_return` | the return annotation, if any — **untrusted**, kept only to flag declared-vs-inferred mismatches |
| `is_generator` | the body contains `yield` / `yield from` |
| `returns` | union of inferred return kinds over all `return` statements (plus `none` for a bare/absent return or fall-through) |
| `raises.explicit` | exception types from `raise` statements and `assert` (⇒ `AssertionError`) — high confidence |
| `raises.implicit` | statically-inferred operator-induced may-set: `ZeroDivisionError` (`/`, `//`, `%`), `IndexError`/`KeyError` (subscript read), `ValueError` (`int()`/`float()`), `TypeError` (ordered-compare/arithmetic/membership over an `Any`-typed operand, or a subscript whose base or key is `Any`; keyed off the Shapes-pass env, so `Any`-typed **locals** count, not just params); also folds in a resolved callee's own explicit + implicit raises (interprocedural propagation, below) |
| `mutations` | may-set of in-place mutations (see below); also includes mutations propagated in from a resolved callee |
| `global_writes` | module-level names written under a `global` declaration |
| `io` | observed I/O channels (`"stdout"`, `"stdin"`, `"filesystem"`) |
| `unresolved_effects` | calls/constructs that couldn't be resolved (see below) |
| `type_mismatches` | contradictions between an untrusted declared annotation and the inferred may-set — return **and** param (see below) |
| `purity` | `"pure"`, `"impure"`, or `"unknown"` (the latter when any effect is unresolved, or an unrecognized decorator is applied) |
| `uses` | imports this function references — each `{ binding, module: { package, path } }` — the per-function → dependency edge |
| `may_use_star` | a `from m import *` is in scope and this function calls a name we couldn't otherwise resolve, so it *may* come from the star |
| `decorators` | dotted decorator names applied to the def, in source order. A decorator outside the recognized-transparent set (e.g. `@staticmethod`, `@classmethod`) can replace the function entirely, so it downgrades `purity` to `"unknown"` |

**Return kinds:** `none`, `bool`, `int`, `float`, `str`, `bytes`, `sequence`, `mapping`,
`set`, `opaque` (a value whose kind couldn't be inferred — e.g. a returned attribute, local,
or unknown call; **not** the same as `none`).

**Parameter shapes** (drive input generation) are a recursive `Shape`, inferred to a fixpoint
from usage (subscripting, iteration, `len`, arithmetic, and type-specific methods like
`.split`/`.keys`/`.append`), never from annotations. Scalars (`int`, `float`, `bool`, `str`,
`bytes`, `none`, `any`) serialize as a lowercase string; containers serialize as a tagged
object carrying their element shape(s): `{"seq": <elem>}`, `{"set": <elem>}`,
`{"map": {"key": <k>, "value": <v>}}` — so nested usage (`matrix[i][j]`) infers a nested shape
like `{"seq": {"seq": "float"}}`. `any` means no discriminating usage was observed.

**Shape unions.** When a parameter or local is genuinely used two different ways on different
paths (e.g. compared against an `int` on one branch, a `str` on another), the analyzer keeps
both instead of collapsing to `any`: `{"union": [<shape>, <shape>, ...]}`, sorted, deduplicated,
and capped at 4 members — a union that would exceed the cap collapses to `any` (still sound: `any`
is broader). `Optional[X]` has no separate representation, it's just `{"union": [<X>, "none"]}`.
`.pyi` output (§9) renders a union as PEP 604 (`int | str`, `int | None`).

Each parameter also carries a `kind`: `positional` (default, omitted from JSON), `keyword_only`
(after a bare `*`), `var_positional` (`*args`), or `var_keyword` (`**kwargs`) — variadic
parameters are never generated as a single positional value. A parameter may also carry
`declared` (the untrusted annotation string) and `guard_samples` (literal values pulled from
`if`/`while`/`assert`/ternary guards on that parameter — feeds input generation, §6).

**Mutations.** Each entry is a `target` × a `via` kind:

- `target.root`: `param` · `self_attr` · `global` · `nonlocal` · `unknown`
- `via`: `subscript_set` · `subscript_del` · `attr_set` · `attr_del` · `method` (with a
  `name`, e.g. `append`) · `aug_subscript` · `aug_attr` · `aug_name` (a `+=`-style operator on a
  plain name, e.g. `p += [1]` — a may-mutation since the effect depends on `p`'s runtime type)

Aliasing is tracked locally, so `q = p; q.append(1)` is recorded as a mutation of `p`.
Rebinding a parameter name (`p = p + 1`) is **not** a mutation.

**`unresolved_effects`.** When the analyzer can't see through a construct, it records it
instead of assuming purity:

```jsonc
{ "reason": "call_unknown_callee", "callee": "helper",
  "may_affect": [ { "root": "param", "name": "value" } ] }
```

Reasons include `call_import` (a call through an imported name — `np.array(rows)`,
`json.dumps(obj)` — opaque, so the function is `unknown`, never `pure`), `call_unknown_callee`
(a call to something that isn't a locally-defined function/method and isn't an import either —
e.g. a value received as a parameter), `call_method_unknown` (an unresolved method call on a
param/local), `dynamic_setattr` / `dynamic_exec` / `dynamic_eval`, and `decorator` (an
unrecognized decorator, which can replace the function wholesale). This is the honest record of
where static analysis can't see; `record`'s execution covers these dynamically.

**Calls to functions/methods defined in the same file are not `unresolved_effects`.** The
analyzer resolves them and propagates the callee's own effects onto the caller instead (a
fixpoint over the intra-file call graph, so multi-hop chains and recursion settle too): a
function that calls a mutating local helper is correctly `impure`, not falsely `unknown`. Only
calls through imports or to genuinely unresolvable callees stay in `unresolved_effects`. This
propagation maps each of the callee's **positional and keyword** call arguments back to the
matching caller-side root (e.g. `helper(x, flag=item)` maps the callee's mutation of its
`flag` parameter onto the caller's `item`) — `*args`/`**kwargs` **unpacking** at the call site
(`helper(*more, **extra)`) stays unmapped, since there's no single name to key a mutation by (see
[§10](#10-limitations--gotchas)). In [project mode](#8-multi-file--project-mode) the same mapping
also runs **across files**, for calls to `project_local`-imported free functions.

**`type_mismatches`.** Two kinds, both advisory-only (never affect the may-set or `purity`):

```jsonc
{ "kind": "return", "declared": "str", "inferred": ["int"] }
{ "kind": "param", "param": "count", "declared": "str", "inferred_shape": "int" }
```

- `"return"` — the declared return annotation is fully disjoint from the inferred `returns`
  may-set (e.g. declared `-> str` but every inferred exit is `int`); a partial overlap (e.g.
  declared `-> list` with an implicit `None` fall-through) is never flagged.
- `"param"` — the declared parameter annotation is disjoint from the usage-inferred `shape`;
  carries `param` (the parameter name) and `inferred_shape` instead of `inferred`.

Both checks treat a declared `int` as excluding an inferred `bool` (Python's `bool` is an `int`
subtype at runtime, so this can produce a noisy false-positive advisory in either direction — see
[§10](#10-limitations--gotchas)).

---

## 5. `record` — signatures + observed cases

```sh
pylens record <file.py|dir> [--inputs <N>] [--format json|summary|pyi|html]
```

| Flag | Default | Meaning |
|---|---|---|
| `--inputs` | 4 | cap on generated input vectors per function/method |

Output is `{ "schema_version": "1.2", "dependencies": [...], "functions": [...] }` for a single
file; a directory switches to [project mode](#8-multi-file--project-mode).

**`dependencies`** catalogs every import (all styles — `import x`, `import x as y`,
`import a.b.c`, `from m import a, b`, `from m import *`, relative `from . import x`, and
imports nested inside functions). Each entry carries the structured `module` (`{ package, path }`
— `xml.etree.ElementTree` → package `xml`, path `etree.ElementTree`), its `scope` (`module` or
`function`), and a resolution `status`:

```jsonc
{ "from": false, "module": { "package": "numpy" }, "alias": "np",
  "scope": "module", "status": "unresolved",
  "error": { "stage": "setup", "kind": "ModuleNotFoundError",
             "message": "No module named 'numpy'", "module": "numpy" } }
```

- `status`: `resolved` (imports fine) · `unresolved` (e.g. not installed) · `not_probed`.
  Relative imports are `not_probed` — resolving them needs a package context a standalone file
  doesn't supply; they're not failures.
- `error` is **structured** — `{ stage, kind, message, module? }` — never a bare string. `kind`
  is the Python exception type; `module` is the missing module for import failures.

A missing **module-scope** import stops the whole file from loading, so no function in it can
run. That's reported **once** per function as `uncallable` (with the structured reason), and
the function's `cases` is empty — not as N identical per-case setup errors. A missing
**function-scope** import (one inside a function body) instead surfaces per case as a
`raised: ModuleNotFoundError`, because it executes only when the function is called.

**`functions`** — for every function and method, the static signature **plus** a list of
`cases`: each is a generated input run in the jail, with what was observed. (If the function
is `uncallable`, `cases` is empty and `uncallable` says why.)

```sh
pylens record examples/inventory.py --inputs 4
```

A method case (`Inventory.add`) where the call succeeds and mutates the receiver:

```jsonc
{
  "input": [1, 7],
  "ctor_args": [],                 // receiver built as Inventory() (defaults)
  "outcome": "returned",
  "return": 7,
  "mutations": [
    { "target": "self",
      "before": { "items": {}, "log": [], "capacity": 100 },
      "after":  { "items": {"1": 7}, "log": [["add", 1, 7]], "capacity": 100 } }
  ]
}
```

Each case has: `input` (positional values), `kwargs` (keyword-only values passed by name),
`outcome` (`"returned"` | `"raised"` | `"error"`), `return`, `raises` (exception type),
`mutations` (argument and/or `self`, before→after), `return_aliases_arg`, captured `stdout` and
`stderr` (separate channels), a structured `error` (`{ stage, kind, message, module? }`) when
the harness/setup failed, and (for methods) the `ctor_args` used to build the receiver.

**`minimized` — input shrinking for `raised` cases.** When a case's outcome is `raised` (a
*semantic* raise — never a resource kill), pylens greedily shrinks each argument while
re-running the call in the jail, as long as the same exception **type** keeps reproducing, up to
32 extra jailed calls per case. If at least one argument shrank, the case carries a `minimized`
field:

```jsonc
{
  "outcome": "raised",
  "raises": "ValueError",
  "input": [-8734, "xyz"],
  "minimized": { "input": [0, ""], "kwargs": {} }
}
```

`minimized` is a **reporting aid** for humans/consumers reading the record — a smaller
reproduction of the same failure. `pylens validate` (§6) always checks the *original* observed
cases, never the minimized ones.

---

## 6. How a record is built

First, **probe the file once**: every distinct import module is checked, and the real source is
exec'd to see whether the module loads at all (this respects guards like `try: import numpy
except ImportError: …` that per-import probing can't). If it doesn't load, every function is
marked `uncallable` and no cases are generated. Then, for each loadable function/method:

1. **Generate** — input vectors are an even spread across the cartesian product of the inferred
   per-parameter candidates (boundary cases included: empty containers, `0`, negatives, and
   `None` for defaulted/optional params), so combinations of arguments are exercised, not just
   matched positions. Also **guard-directed**: it pulls literal and boundary values straight out
   of the function's own `if`/`assert`/`while`/ternary guards on each parameter (`if qty > 10:`
   seeds `9`, `10`, `11`), so generated inputs are more likely to reach a guarded branch instead
   of missing it by chance.
2. **Construct (methods only)** — the receiver is built from its `__init__`: no arguments when
   the constructor is fully defaulted, otherwise a generated vector. The constructor is probed
   **once per class**; if it can't be built, the method is `uncallable` (reported once, not per
   case). Otherwise the receiver's attribute state is snapshotted before and after the call.
3. **Execute** — the call runs as `python/worker.py` **inside an nsjail jail** (native on Linux,
   via WSL2 on Windows), reusing one long-lived fork-server worker per file (each request runs in
   its own forked child, so interpreter startup is amortized without leaking state between
   calls). Arguments are serialized before and after to detect mutations; stdout/stderr are
   captured separately; the return value, exception, and return-aliasing are recorded.
4. **Observe** — mutations are derived by diffing each argument (and `self`) before vs. after
   with structural equality (float tolerance; sets/dict items order-insensitive). `outcome:
   "raised"` means the function itself raised (part of its behavior); a **resource kill**
   (out-of-memory, recursion limit, timeout — an artifact of the sandbox) is always `outcome:
   "error"` with `error.stage == "resource"`, never `"raised"`.
5. **Minimize (raised cases only)** — a case that came back `raised` is re-run with a greedy
   per-argument shrink, budgeted at 32 extra jailed calls, keeping the smallest input observed
   so far that still raises the *same exception type* (a resource kill or a differing exception
   type rejects the candidate). See `minimized` in §5.

`record` does no comparison and computes no score. It produces the behavioral record of one
function; turning records into a training signal is the consumer's job, not pylens's.

```sh
pylens validate <file.py|dir> [--inputs <N>] [--format json|summary|html]
```

Runs `record`, then checks every case against its static signature: every observed
mutation/raise/return/I/O must be covered by the static may-set (`observed ⊆ static`, see
DESIGN.md). A gap is a `hard` defect when the signature claimed no unresolved effects (a true
soundness bug), or `soft` when the signature already flagged `unresolved_effects` (an
acknowledged blind spot). Exits non-zero if any hard defect is found — usable as a CI gate.
`--format pyi` is rejected on `validate` (there is no type-hint rendering for a defect report).

---

## 7. Using pylens from Rust

pylens is a library as well as a CLI.

```rust
use pylens::analyze_source;
use pylens::record::record_file;

// Static: effect signatures (no jail)
let sigs = analyze_source("def f(xs):\n    xs.append(1)\n")?;
assert_eq!(sigs[0].name, "f");

// Dynamic: full module record — dependencies + per-function cases (requires the jail)
let m = record_file("import os\ndef add(a, b):\n    return a + b\n", 4)?;
println!("{} deps, {} functions", m.dependencies.len(), m.functions.len());
println!("{} cases", m.functions[0].cases.len());
# Ok::<(), Box<dyn std::error::Error>>(())
```

Key items: `pylens::model` (the `EffectSignature` / `Import` / `ModuleRef` / `ImportUse` / `Shape`
types, including `Shape::Union`), `pylens::analyze_source`, `pylens::imports_of`,
`pylens::record::{record_file, record_with, record_with_signatures, ModuleRecord, Dependency,
DepStatus, FunctionRecord, Uncallable, Case, MinimizedInput}`,
`pylens::validate::{validate_function, validate_signature, Defect, Severity, Dimension}`,
`pylens::shrink::shrink_case` (the input-minimization routine `record` calls on `raised` cases),
and the execution layer `pylens::exec::{Sandbox, Nsjail, NsjailPool, HarnessError, probe}` —
`Sandbox` is the trait, `Nsjail` runs one jailed process per call, `NsjailPool` is the persistent
fork-server pool that `record_file` drives, and `probe()` checks whether the sandbox is
provisioned (all execution requires it — there is no unsandboxed launcher). `pylens::project`
exposes the directory-mode entry points (`analyze_project`, `record_project`, `validate_project`,
`collect_py_files`) and `pylens::project::resolve` the import-resolution types (`ModuleIndex`,
`resolve_import`, `Resolution`); `pylens::stub::render_stub` renders `.pyi` output from static
signatures, `pylens::stub::observed::render_record_stub` renders it enriched with observed types
from a `ModuleRecord`; `pylens::html::render` renders the HTML report.

---

## 8. Multi-file / project mode

```sh
pylens analyze  <dir> [--format json|summary|pyi|html]
pylens record   <dir> [--inputs <N>] [--format json|summary|html]     # --format pyi rejected
pylens validate <dir> [--inputs <N>] [--format json|summary|html]
```

Passing a **directory** instead of a file recurses over its `*.py` files and produces an
aggregated **project report** instead of a single-file one:

```jsonc
{
  "schema_version": "1.2",
  "root": "examples",
  "files": [
    { "path": "inventory.py", "imports": [...], "functions": [...] },
    { "path": "broken.py", "error": "parse error: ..." }
  ],
  "summary": {
    "files": 2, "ok": 1, "errors": 1,
    "functions": 5,
    "purity": { "pure": 2, "impure": 2, "unknown": 1 }
    // validate mode also adds: "hard_defects", "soft_defects", "functions_checked"
  }
}
```

A file that fails to read, parse, or record becomes `{ path, error }` in `files` instead of
aborting the whole run. `files` is always sorted by path, regardless of the order files finished
processing in.

**File discovery honors `.gitignore`.** The directory walk uses the [`ignore`
crate](https://docs.rs/ignore) — the same matcher `ripgrep` uses — so `.gitignore`/`.ignore`
files are respected **hierarchically**, even in a directory that isn't a git repository at all,
and hidden files/directories are skipped by default. A small built-in skip-list always applies
on top: `__pycache__`, `venv`, `env`, `node_modules`, `build`, `dist`, `target`. A directory
that can't be read partway through the walk is silently skipped rather than aborting the run.

**`analyze` is parallelized** across a small worker-thread pool sized to
`available_parallelism` (pure CPU, no jail). **`record`/`validate` are parallelized too**, across
a bounded pool of **jailed worker threads** — each owns its own `NsjailPool` (its own WSL/nsjail
process on Windows), capped at 4 regardless of core count. Files are pulled from a shared queue,
so completion order is nondeterministic, but the final report's `files` is always sorted by
path — deterministic JSON output run to run.

**Project-local import resolution.** In project mode, every import and dependency entry gains a
`resolution` field, computed purely statically (no jail) against the other files in the project:

- `project_local` — the import points at another file in this project; a `project_target` field
  gives its path (relative to the project root, forward-slash separated).
- `external` — a non-relative import that isn't any project file (stdlib/third-party).
- `unresolved_relative` — a relative import (`from . import x`, `from ..pkg import y`) that
  doesn't land on a project file (climbs past the root, or the target isn't indexed).

This supersedes the single-file behavior for relative imports: outside project mode a relative
import is always `not_probed` (§5); in project mode it's resolved against the tree and reported
`project_local` or `unresolved_relative` as appropriate. `__init__.py` files are indexed under
their *containing package* (importing `pkg.sub` runs `pkg/sub/__init__.py`), and relative-import
level climbs package directories accordingly.

**Cross-file effect propagation.** Interprocedural propagation (§4) follows `project_local`
imports across files: a call to a free function imported from another project file has that
callee's effects (mutations, raises, io, generator-ness, unresolved effects) propagated onto the
caller, via a project-wide fixpoint (`project::interproc`) that runs once, over every file
together, before `analyze`/`record`/`validate` build their reports — so all three see the same
enriched signatures. Like the intra-file version, the mapping covers **positional and keyword**
arguments at the call site (`util.helper(x, flag=item)` maps correctly); it covers free
functions only (an imported *method* call, or a call reached through a multi-level dotted chain,
stays unresolved), and `*args`/`**kwargs` unpacking at the call site is unmapped (§10).

---

## 9. `.pyi` stubs and HTML reports

```sh
pylens analyze <file.py|dir> --format pyi
```

Renders inferred `.pyi` type-hint stubs instead of JSON — one `def` per function/method (methods
grouped under `class <Owner>:`), targeting Python 3.10+ syntax: builtin generics (`list[...]`,
`dict[...]`) rather than `typing.List`/`typing.Dict`, and `|` PEP 604 unions (including
`Shape::Union` params, e.g. `x: int | str`, and `Optional[X]` as `X | None`) rather than
`typing.Union`/`typing.Optional`. Generators render as `-> Iterator[Any]`. A function with a
`type_mismatches` entry — return **or** param — gets a trailing `# note: ...` comment (e.g.
`# note: count: declared str, inferred int`); the emitted annotation always reflects the
*inferred* may-set, never the untrusted declaration. Over a directory, each file's stub is
preceded by a `# <relative path>` comment line. `--format pyi` is **`analyze`-only** for a
directory; for a single file, `record` also supports it (below). `validate` rejects it outright.

```sh
pylens record examples/inventory.py --format pyi
```

**`record --format pyi`** (single-file only) renders the same static stub, but additionally
folds **dynamically observed types** into any param/return that stayed statically unresolved
(`Any`/`Opaque`): if every recorded case agrees on a parameter's or the return value's runtime
type, that type is substituted in and the `def` line gets a trailing `# observed: ...` comment,
e.g.:

```python
def parse(raw) -> Any: ...  # observed: raw: str, -> dict
```

A static-known type always wins over an observed one — this only fills in gaps. Only the types
the worker's tagged serialization can distinguish **faithfully** are folded: `None`/`bool`/
`str`/`int`/`float`/`list`/`tuple`/`set`/`dict`. `bytes` and arbitrary Python objects both
serialize through the same generic fallback encoding and are **not** distinguishable from each
other, so they are never folded in — that param/return just stays whatever the static side
inferred (`Any`/`Opaque`), unenriched.

```sh
pylens record examples/inventory.py --format html > report.html
```

`--format html` renders a self-contained `<!doctype html>` document (inline `<style>` only, no
external stylesheets/scripts/fonts) for `analyze`, `record`, or `validate`, over a single file or
a whole project — open it straight from disk. It renders the same data as the JSON output; all
text from analyzed Python source (names, decorators, exception types, error messages) is
HTML-escaped before being written.

---

## 10. Limitations & gotchas

- **Library calls are flagged but not modelled.** A qualified call through an imported name
  (`os.remove(path)`, `np.sort(arr)`) is recorded as a `call_import` unresolved effect and
  makes the function `unknown` — but pylens doesn't know *what* the call does (whether it
  mutates its argument, what it returns). The dynamic layer observes the actual outcome.
- **`*args`/`**kwargs` unpacking at a call site is unmapped.** Interprocedural propagation
  (intra-file and cross-file, §4/§8) maps a callee's effects onto the caller by matching
  positional position and keyword name at the call site. A call like `helper(*more)` or
  `helper(**extra)` has no single name to key a mutation by, so any effect the callee has on
  that unpacked argument is **not** propagated onto the caller — sound (nothing false is
  claimed), just imprecise. This is consistent with ordinary `*args`/`**kwargs` **parameters**
  never being generated as a single positional value either.
- **Opaque-call `may_affect` only scans positional arguments.** For a genuinely unresolved call
  (`call_unknown_callee`/`call_import`), the `may_affect` list on the recorded
  `UnresolvedEffect` is built from positional arguments only, not keyword ones. Sound today
  because these are already-acknowledged blind spots; tightening it would only make the may-set
  more precise, never fix a soundness gap.
- **`bool` vs `int` advisory asymmetry.** The `type_mismatches` checks (§4) treat a declared
  `int` as excluding an inferred `bool` (Python's `bool` is an `int` subtype at runtime), which
  can produce a noisy advisory mismatch either direction. Advisory-only — never touches the
  may-set or `purity`.
- **Relative imports resolve only in project mode.** Outside a directory run, `from . import
  sibling` is catalogued and marked `not_probed` (§3, §5) — a single file has no package context.
  In project mode (§8) it's resolved against the other files in the tree.
- **Cross-file propagation covers free functions.** Imported *methods* and deeper dotted call
  chains are not followed and stay `unresolved` in project mode; outside project mode,
  propagation is intra-file only.
- **Static returns are coarse.** A returned local variable or a returned argument is reported
  as `opaque`; precise return-type tracking is intentionally out of scope (the dynamic layer,
  and `record --format pyi`'s observed-type folding, cover actual values).
- **Generation is guard-guided, not constraint-solving.** It's heuristic, not a solver — it
  won't reliably reach a deeply nested or cross-parameter guard. A parameter with no
  discriminating usage (`any` shape) still gets a spread of types, so some generated inputs are
  ill-typed and the case records that honestly (a `TypeError`, etc.). Raise `--inputs` for more
  coverage.
- **`minimized` inputs never feed back into `validate`.** Shrinking (§5, §6) is a reporting aid
  for humans reading a `raised` case; `pylens validate` always checks the original observed
  cases the analyzer generated, not the shrunk ones.
- **`pylens validate` measures soundness, it doesn't guarantee it.** It checks the observed
  cases from a given `--inputs` run against the static signature; a soundness gap only shows up
  if generation happens to exercise the path that would expose it.
- **Execution requires a provisioned sandbox.** All runs are nsjail-jailed (native Linux / WSL2
  on Windows); there is no unsandboxed fallback. If nsjail or WSL isn't set up, `record` errors
  and the jailed tests skip — run `scripts/provision-sandbox.sh` first. The jail has no network,
  a read-only rootfs, and CPU/memory/time limits; code that needs the network or to write
  outside `/tmp` will fail by design.
- **Exotic argument values** (open files, objects that aren't deep-copyable) aren't fully
  supported by the serializer yet.

---

## 11. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `sandbox not provisioned …` / record tests print `SKIP` | nsjail/WSL isn't set up. Run `scripts/provision-sandbox.sh` (in your WSL2 distro on Windows). `analyze` doesn't need it. |
| `spawn sandbox (wsl -d Ubuntu -- nsjail): …` | WSL isn't installed or the distro name differs. Set `PYLENS_WSL_DISTRO=<name>`, or install WSL2. |
| `sandbox produced no output … policy rejected the run` | nsjail couldn't start the jail (missing mount, kernel without user namespaces). Re-run the nsjail command without `2>/dev/null` to see jail logs. |
| Every function is `uncallable` with `module_not_loadable` | A module-scope import doesn't resolve, so the file can't load. The blocking module is in the `error` (and `dependencies`). Install it, or move the import into the function body if it's optional. |
| A method is `uncallable` with `constructor_failed` | `__init__` couldn't be built from generated arguments. The `error` says why. |
| Edited `python/worker.py` but behavior didn't change | The jail runs a **deployed copy**, not the repo file. Re-run `scripts/provision-sandbox.sh` or redeploy it manually (§2). |
| `record`/`validate` a directory feels slow | Project-mode jailed execution is parallelized across up to 4 worker threads (§8), each spawning its own jail process — throughput is bounded by that cap, not the file count. |
| A file I expected to be analyzed is missing from a project report | Check `.gitignore`/`.ignore` in the tree (§8) — the directory walk honors them hierarchically, even outside a git repo, plus the built-in skip-list (`__pycache__`, `venv`, `env`, `node_modules`, `build`, `dist`, `target`). |
| First build is slow | Expected: it compiles the ruff parser from git once. Subsequent builds are fast. |
