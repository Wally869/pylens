# pylens

pylens examines the behavior of Python code. It has two engines with one shared core:

- **`analyze`** — static analysis of effects and shapes. It is written in Rust and uses the
  [ruff](https://github.com/astral-sh/ruff) parser. For each function, it infers an **effect
  signature**:
  - the return kinds;
  - the mutations of the arguments and of `self`;
  - the raised exceptions (explicit and implicit);
  - the I/O;
  - the recursive parameter shapes; and
  - the calls that it cannot examine.

  `analyze` gives an immediate result. It needs no sandbox, and it accepts a full directory.
- **`record`** — runs the function on generated inputs in an
  [nsjail](https://github.com/google/nsjail) sandbox. It records the actual behavior: the
  outcome, the return value, the mutation differences, and the raised exceptions.

`validate` connects the two engines. It makes sure that the static analysis predicted each
observed effect (`observed ⊆ static`). The static side over-approximates. It must never miss an
effect that a true run shows. `record` and `validate` also report how many lines of each
function the generated inputs reached, so you can see how much the check covered.

The output is JSON with a version. The other formats are a terminal summary, `.pyi` stubs, and
one HTML file.

## Requirements

- Rust (edition 2024). The first build gets and compiles the pinned ruff parser (1 to 2 min).
- A sandbox, only for `record` and `validate`. pylens does not trust the code that it executes.
  Thus each run occurs in an nsjail sandbox on a Linux kernel — directly on Linux, or through
  WSL2 on Windows. There is no unsandboxed path. Do this setup one time:

  ```sh
  # Windows: run in your WSL2 distribution
  wsl -d Ubuntu -- bash /mnt/c/Projects/ai/pylens/scripts/provision-sandbox.sh
  # Linux:
  bash scripts/provision-sandbox.sh
  ```

  `analyze` is pure Rust. It does not need this setup.

## Quick start

```sh
cargo build --release

pylens analyze examples/inventory.py   # static signatures, JSON, no sandbox
pylens record  examples/inventory.py   # also observed cases, uses the sandbox
pylens validate examples/inventory.py  # makes sure that observed ⊆ static
```

This is a recorded case from `Inventory.add(name, qty)`, a call that changes the receiver:

```jsonc
{
  "input": [1, 7],
  "outcome": "returned",
  "return": 7,
  "mutations": [
    { "target": "self",
      "before": { "items": {}, "log": [] },
      "after":  { "items": {"1": 7}, "log": [["add", 1, 7]] } }
  ]
}
```

## Commands

| Command | Description |
|---|---|
| `pylens analyze <file.py\|dir> [--format json\|summary\|pyi\|html]` | Gives the static effect signature of each function and method, and a list of the imports. |
| `pylens record <file.py\|dir> [--inputs N] [--format json\|summary\|pyi\|html]` | Does `analyze`, then adds the observed cases for each function and the import probes. Uses the sandbox. |
| `pylens validate <file.py\|dir> [--inputs N] [--format json\|summary\|html]` | Does `record`, then makes sure that `observed ⊆ static`. Exits with a non-zero code if a hard defect occurs. |

A directory argument starts project mode. pylens processes all the `*.py` files in parallel. It
resolves the imports against the other project files, and it propagates the effects between
these files.

## Documents

- [User guide](docs/USER_GUIDE.md) — installation, sandbox setup, commands, output formats,
  limits, and troubleshooting
- [SCHEMA.md](docs/SCHEMA.md) — the JSON output contract, field by field, and the version policy
- [DESIGN.md](docs/DESIGN.md) — the effect types, the soundness rule, the architecture, and the
  reasons for the sandbox
- [examples/README.md](examples/README.md) — the example corpus, which stays at zero hard defects
