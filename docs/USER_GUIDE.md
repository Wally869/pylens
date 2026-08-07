# pylens user guide

- [What pylens does](#what-pylens-does)
- [Install](#install)
- [analyze](#analyze)
- [record](#record)
- [validate](#validate)
- [Directory mode](#directory-mode)
- [Output formats](#output-formats)
- [Limitations](#limitations)
- [Troubleshooting](#troubleshooting)

For the full JSON schema see [SCHEMA.md](SCHEMA.md); for the reasoning behind the analysis see
[DESIGN.md](DESIGN.md). This guide covers installing and using the tool.

## What pylens does

pylens analyzes Python functions, one function or method at a time.

- `analyze` — static analysis. Reports what each function returns, mutates, raises, and
  whether it does I/O. No sandbox needed.
- `record` — runs each function in a sandbox on generated inputs and records what actually
  happened.
- `validate` — checks that nothing `record` observed contradicts what `analyze` predicted.

The static analysis over-approximates: it may list effects that never happen, but it must
never miss one that does. When it cannot see through something (an imported call, `eval`,
an unknown decorator), it says so in `unresolved_effects` instead of guessing.

## Install

You need Rust. The first build compiles the ruff parser; it takes a minute or two.

```sh
cargo build --release          # binary at target/release/pylens
```

`record` and `validate` execute untrusted code, so they only run inside an
[nsjail](https://github.com/google/nsjail) sandbox — natively on Linux, through WSL2 on
Windows. There is no unsandboxed mode. Set it up once:

```sh
# Windows: run inside your WSL2 distro
wsl -d Ubuntu -- bash /mnt/c/Projects/ai/pylens/scripts/provision-sandbox.sh
# Linux:
bash scripts/provision-sandbox.sh
```

**Gotcha:** the sandbox runs a deployed copy of `python/worker.py`, not the repo file.
If you edit the repo copy, re-run the provision script to redeploy it. Otherwise you will
chase phantom behavior.

## analyze

```sh
pylens analyze <file.py> [--format json|summary|pyi|html]
```

Prints a JSON report with one signature per function. For this file:

```python
def normalize(items, scale=1.0):
    if not items:
        raise ValueError("empty")
    total = sum(items)
    for i in range(len(items)):
        items[i] = items[i] / total * scale
    return items
```

the signature says, in short: `items` is used as a sequence of floats; the function
mutates `items` in place; it raises `ValueError` explicitly and may raise
`IndexError`, `ZeroDivisionError`, or `TypeError`; it is `impure`.

Things to know when reading a report:

- `purity` is `pure`, `impure`, or `unknown`. `unknown` means something was unresolved —
  the function is never assumed pure.
- Parameter shapes are inferred from how the code uses each parameter, never from type
  annotations. Annotations are shown but not trusted.
- `type_mismatches` flags annotations that contradict the inferred behavior. Advisory
  only.

Field-by-field reference: [SCHEMA.md](SCHEMA.md).

## record

```sh
pylens record <file.py> [--inputs N] [--format json|summary|pyi|html]
```

Generates up to N input vectors per function (default 4), runs each in the sandbox, and
adds the results to the report:

- `dependencies` — every import and whether it resolved. If a module-level import is
  missing, the file cannot load and every function is marked `uncallable` with the reason.
- `cases` — one entry per input: the arguments, the outcome (`returned`, `raised`, or
  `error`), the return value or exception, any argument or `self` mutations
  (before/after), and captured stdout/stderr.

Inputs are derived from the inferred shapes, plus boundary values and literals taken from
the function's own guards (`if qty > 10:` seeds 9, 10, 11). A raised case is also shrunk
to a smaller input that still raises the same exception (`minimized`).

Sandbox kills (out of memory, recursion, timeout) are reported as `error`, never as the
function raising.

## validate

```sh
pylens validate <file.py> [--inputs N] [--format json|summary|html]
```

Runs `record`, then flags every observed effect the static signature missed:

- **hard** defect — the signature claimed to be complete but was contradicted. A pylens
  bug; the exit code is non-zero, so this works as a CI gate.
- **soft** defect — the miss is covered by an `unresolved_effects` entry. A known blind
  spot, not a bug.

## Directory mode

Pass a directory instead of a file to any command:

```sh
pylens analyze src/
pylens record src/ --inputs 8
```

- Every `*.py` file underneath is processed; results are aggregated into one report with a
  per-file breakdown and a summary. A file that fails to parse is reported and skipped.
- The walk honors `.gitignore`, skips hidden files, and always skips `__pycache__`,
  `venv`, `env`, `node_modules`, `build`, `dist`, `target`.
- Work runs in parallel (`record`/`validate` cap at 4 sandboxed workers).
- Imports between project files are resolved, including relative imports, and the effects
  of a function imported from another project file carry over to its callers.

## Output formats

| Format | What you get |
|---|---|
| `json` | full report (default) |
| `summary` | short terminal overview |
| `pyi` | type-hint stubs |
| `html` | self-contained report, open from disk |

`pyi` works with `analyze` (file or directory) and with `record` on a single file. With
`record`, types observed at runtime fill in what static analysis could not, marked
`# observed:`:

```python
def parse(raw: str) -> dict: ...  # observed: raw: str, -> dict
```

The observed type is written into the signature; the trailing comment marks which parts came
from observation rather than static proof.

`validate` does not support `pyi`.

## Limitations

- Calls into libraries are flagged as unresolved, not modelled. `record` shows what they
  actually did.
- Input generation is heuristic. It will not reach every branch, and some generated inputs
  are deliberately ill-typed. Raise `--inputs` for more coverage.
- `validate` only catches problems on paths the generated inputs actually reach.
- Relative imports only resolve in directory mode.
- The sandbox has no network, a read-only filesystem, and CPU/memory/time limits. Code
  that needs more fails by design.

## Troubleshooting

| Problem | Fix |
|---|---|
| `sandbox not provisioned` | Run `scripts/provision-sandbox.sh` (inside WSL2 on Windows). |
| `spawn sandbox (wsl -d Ubuntu -- nsjail): ...` | WSL missing, or your distro has another name: set `PYLENS_WSL_DISTRO=<name>`. |
| Everything is `uncallable` | A module-level import fails; the report names the module. Install it. |
| Edited `worker.py`, nothing changed | The sandbox runs the deployed copy. Re-run the provision script. |
| A file is missing from a directory report | `.gitignore` or the built-in skip-list excluded it. |
| First build is slow | Normal — the ruff parser compiles once. |
