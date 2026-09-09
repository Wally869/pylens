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

For the full JSON schema, refer to [SCHEMA.md](SCHEMA.md). For the reasons behind the analysis,
refer to [DESIGN.md](DESIGN.md). This guide tells you how to install and how to use the tool.

## What pylens does

pylens examines Python functions, one function or method at a time.

- `analyze` — static analysis. It reports what each function returns, changes, and raises, and
  if the function does I/O. It does not need a sandbox.
- `record` — runs each function in a sandbox on generated inputs and records the actual results.
- `validate` — makes sure that no result from `record` disagrees with the prediction from
  `analyze`.

The static analysis over-approximates. It can list effects that do not occur, but it must never
miss an effect that does occur. If it cannot examine an item (an imported call, `eval`, an
unknown decorator), it records the item in `unresolved_effects`. It does not guess.

## Install

You need Rust. The first build compiles the ruff parser. This takes 1 to 2 minutes.

```sh
cargo build --release          # the binary is at target/release/pylens
```

`record` and `validate` execute code that pylens does not trust. Thus they run only in an
[nsjail](https://github.com/google/nsjail) sandbox — directly on Linux, or through WSL2 on
Windows. There is no unsandboxed mode. Do this setup one time:

```sh
# Windows: run in your WSL2 distribution
wsl -d Ubuntu -- bash /mnt/c/Projects/ai/pylens/scripts/provision-sandbox.sh
# Linux:
bash scripts/provision-sandbox.sh
```

**Important:** the sandbox runs a deployed copy of `python/worker.py`, not the file in the
repository. If you change the file in the repository, run the provision script again to deploy
it. If you do not, the sandbox continues to use the old copy.

## analyze

```sh
pylens analyze <file.py> [--format json|summary|pyi|html]
```

`analyze` prints a JSON report with one signature for each function. For this file:

```python
def normalize(items, scale=1.0):
    if not items:
        raise ValueError("empty")
    total = sum(items)
    for i in range(len(items)):
        items[i] = items[i] / total * scale
    return items
```

the signature shows this:

- the function uses `items` as a sequence of floats;
- it changes `items` in place;
- it raises `ValueError` explicitly;
- it can raise `IndexError`, `ZeroDivisionError`, or `TypeError`; and
- it is `impure`.

Read a report with this data in mind:

- `purity` is `pure`, `impure`, or `unknown`. `unknown` means that one or more items stayed
  unresolved. pylens never assumes that a function is pure.
- pylens infers the parameter shapes from the operations on each parameter. It does not use the
  type annotations. It shows the annotations, but it does not trust them.
- `type_mismatches` shows the annotations that disagree with the inferred behavior. This data is
  advisory only.

For a field-by-field reference, refer to [SCHEMA.md](SCHEMA.md).

## record

```sh
pylens record <file.py> [--inputs N] [--replay <cases.json>] [--value-domain <profile.json>]
              [--cover-branches] [--stability-runs N] [--time-budget <s>] [--no-shrink]
              [--format json|summary|pyi|html]
```

`record` generates a maximum of N input vectors for each function (the default is 12), runs each
input in the sandbox, and adds the results to the report:

- `dependencies` — each import, and its resolution status. If a module-level import is missing,
  the file cannot load. Then pylens marks each function `uncallable` and gives the reason.
- `cases` — one entry for each input. An entry has the arguments, the outcome (`returned`,
  `raised`, or `error`), the return value or the exception, the mutations of the arguments or of
  `self` (before and after), and the captured stdout and stderr.
- `coverage` — how many lines of the function the inputs reached: `executed`, `total`, and the
  `missed` line numbers.
- `branches` and `branch_coverage` — the per-branch-outcome accounting: each outcome is
  `covered`, `uncovered` (with a `reason`), or `unobservable_line_granularity`.

The optional flags:

- `--replay <cases.json>` executes externally supplied input tuples in addition to the
  generated ones. These cases carry `source: "replay"`; pylens never shrinks them and never
  filters them by the value domain.
- `--value-domain <profile.json>` restricts the generated values to a declared domain: the
  allowed scalar kinds, the allowed list elements, and size and depth caps.
- `--cover-branches` turns `--inputs` into the total case budget and adds targeted inputs for
  each uncovered branch outcome until the outcomes are covered or the budget ends.
- `--stability-runs N` runs each case N times and drops the cases whose runs disagree. The
  report then carries the `dropped_cases` count.
- `--time-budget <seconds>` sets a hard deadline for one function: generated batches, shrinking,
  the `--cover-branches` loop, and `--stability-runs` re-runs all stop drawing new work once it
  passes. It does not cover `--replay` cases, which always run in full. The value must be greater
  than 1 second (the minimum lease floor); pylens rejects a smaller one.
- `--no-shrink` skips input minimization for cases that raised. `minimized` is then absent from
  every case. Shrinking runs by default, and now runs under `--time-budget` like everything else,
  so leaving it on no longer risks an unbounded tail.

pylens makes the inputs from the inferred shapes, then adds better candidates and tries them
first:

- the literal and boundary values from the guards of the function (`if qty > 10:` gives 9, 10
  and 11), and the literal default of the parameter;
- values that match the domain of the parameter — a URL, an e-mail address, a file path, a JSON
  document, a date, a numeric string, a regular expression, or HTML. pylens selects these from
  the calls that take the parameter (`json.loads(s)`, `urlparse(u)`), the methods called on it,
  and its name. Each corpus has correct and incorrect members, because the incorrect ones reach
  the error branch;
- structural properties that a random value almost never has: a sorted list, a descending list,
  a palindrome, an all-equal list, a prime, a power of two, a float trap.

pylens varies one parameter at a time and holds the other parameters at a typical value. At a
small budget this reaches much more of the function than a diagonal through each combination.

For a case that raises, pylens also shrinks the input to a smaller input that raises the same
exception (`minimized`).

If the sandbox stops the code (out of memory, recursion, or timeout), pylens reports `error`. It
does not report that the function raised.

### How long one function can run

`record` runs these phases for one function, always in this order:

1. **Per-module probes.** Once per file, before any function runs: one load probe for each
   distinct imported module, and — unless the first function's own initial batch already proves
   the module loads — one whole-module load probe.
2. **The method constructor probe.** For a method, the first method of each class probes its
   constructor once; the result is cached for the rest of the class.
3. **The initial generated batches.** Generated inputs are dispatched in chunks of 6
   (`BATCH_SIZE`, `src/record/mod.rs`), one sandboxed batch call per chunk.
4. **Shrinking.** Each case that raised is shrunk right away: one sandbox call per candidate, up
   to 32 calls per case (`SHRINK_BUDGET`, `src/shrink.rs`). `--no-shrink` skips this phase.
5. **`--replay` cases.** Run once, after the initial batches, as a single batch call.
6. **The `--cover-branches` loop**, if requested: one sandbox call per targeted candidate, until
   every observable outcome is covered, a round adds nothing new, or the case count reaches
   `--inputs`.
7. **`--stability-runs` re-run rounds**, if requested: one batch call per round, covering every
   case still alive, for up to `N - 1` further rounds.

**Per-batch limits.** Phases 2, 3, 4, 6, and 7 each ask `--time-budget`'s deadline for a limit
before they start a new piece of work: a per-call limit (`min(PYLENS_CALL_TIMEOUT, time left
before the deadline)`) and a whole-batch limit (the time left before the deadline). Without
`--time-budget`, every call instead uses the plain default: a per-call limit of
`PYLENS_CALL_TIMEOUT` (an environment variable, 10 seconds if unset — the same fallback
`worker.py` uses on its side) and no whole-batch limit at all, so the worker derives its own,
`PYLENS_CALL_TIMEOUT` times the number of items in the batch.

**Where the deadline applies.** Once a phase's request for a limit is refused, that phase (and
every later phase for that function) stops starting new work; everything already recorded stays.
The per-module probes (phase 1) sit outside every function's deadline by design — they run once
per module, before any function's deadline exists. `--replay` cases (phase 5) always run at full,
un-leased limits: external evidence must never disappear because a deadline had already passed.
(If `--stability-runs` is also set, a replayed case's re-run rounds share the same per-round limit
request as every other still-alive case, so a refusal late in the deadline can leave a replayed
case's stability check unfinished too — only a replayed case's own first execution is
deadline-proof.)

**The worst case.** For one function with `--time-budget B` set, expect at most `B` plus one more
per-call limit: an item can start a moment before its batch's deadline and still run to its own
per-call limit (at most `PYLENS_CALL_TIMEOUT`, 10 seconds by default) before it is killed. For a
whole file, multiply that per-function worst case by the number of functions in the file — the
deadline restarts fresh for every function, so `--time-budget` bounds one function, never the
file. Without `--time-budget`, there is no deadline at all: wall time scales with however many
sandbox calls the function needs (generated batches, up to 32 shrink calls per raised case, the
cover loop, stability rounds), each up to `PYLENS_CALL_TIMEOUT`, with nothing to stop the total.

## validate

```sh
pylens validate <file.py> [--inputs N] [--format json|summary|html]
```

`validate` does `record`, then shows each observed effect that the static signature missed:

- **hard** defect — the signature said that it was complete, but an observation disagrees. This
  is a pylens bug. The exit code is not zero, thus you can use this command as a CI gate.
- **soft** defect — an `unresolved_effects` entry covers the missed effect. This is a known
  limit, not a bug.

The summary also gives the aggregate coverage. Read it together with the defect counts: a result
of zero defects at low coverage only means that the inputs did not reach much of the code.

## Directory mode

Give a directory in place of a file to any command:

```sh
pylens analyze src/
pylens record src/ --inputs 8
```

- pylens processes each `*.py` file in the directory and below it. It collects the results in
  one report with a summary and a division by file. If a file does not parse, pylens reports the
  file and continues.
- The walk obeys `.gitignore` and ignores hidden files. It always ignores `__pycache__`, `venv`,
  `env`, `node_modules`, `build`, `dist`, and `target`.
- The work occurs in parallel. `record` and `validate` use a maximum of 4 sandboxed workers.
- pylens resolves the imports between the project files, relative imports included. The effects
  of a function that comes from a different project file also apply to the callers.

## Output formats

| Format | Result |
|---|---|
| `json` | the full report (the default) |
| `summary` | a short terminal overview |
| `pyi` | type-hint stubs |
| `html` | one HTML file, which you can open from the disk |

`pyi` operates with `analyze` (a file or a directory), and with `record` on one file. With
`record`, the types from the execution fill the gaps that the static analysis left. pylens marks
them with `# observed:`:

```python
def parse(raw: str) -> dict: ...  # observed: raw: str, -> dict
```

pylens writes the observed type into the signature. The comment at the end shows which parts
come from the execution and not from the static analysis.

`validate` does not support `pyi`.

## Limitations

- pylens models the most frequent standard-library calls (`os.path`, `re`, `math`, `struct`,
  `itertools`, `time`, `json`, and parts of `os` and `sys`). It marks every other call through an
  import unresolved. `record` shows their actual behavior.
- pylens resolves a method call only on a local built from a class declared in the same file. An
  imported class, an inherited method, and a longer dotted chain stay unresolved.
- The input generation uses the inferred shapes, not a constraint solver. It does not reach each
  branch. If a shape stays `any`, pylens gives values of different types, thus some inputs do not
  agree with the true expectation of the function. The case records this correctly. Increase
  `--inputs` for more coverage.
- `validate` finds problems only on the paths that the generated inputs reach. The coverage
  figure tells you how large that limit is for your code.
- A function that rebinds a parameter name reports the new shape as the shape of the parameter.
  `def f(x): x = []` reports `x` as a sequence, although the caller can give anything.
- Relative imports resolve only in directory mode.
- The sandbox has no network, a read-only file system, and limits on the CPU, the memory, and
  the time. Code that needs more than these limits fails. This is the intended behavior.

## Troubleshooting

| Problem | Solution |
|---|---|
| `sandbox not provisioned` | Run `scripts/provision-sandbox.sh` (in WSL2 on Windows). |
| `spawn sandbox (wsl -d Ubuntu -- nsjail): ...` | WSL is missing, or your distribution has a different name. Set `PYLENS_WSL_DISTRO=<name>`. |
| Each function is `uncallable` | A module-level import fails. The report gives the name of the module. Install it. |
| You changed `worker.py`, but the behavior is the same | The sandbox runs the deployed copy. Run the provision script again. |
| A file is not in a directory report | `.gitignore` or the internal ignore list removed the file. |
| The first build is slow | This is usual. The ruff parser compiles one time. |
