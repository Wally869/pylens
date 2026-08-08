# pylens

Behavioral analysis of Python code. pylens has two engines that share one core:

- **`analyze`** — static effect and shape analysis, written in Rust on the
  [ruff](https://github.com/astral-sh/ruff) parser. For each function it infers an **effect
  signature**: return kinds, argument and `self` mutations, raised exceptions (explicit and
  implicit), I/O, recursive parameter shapes, and the calls it cannot see through. No sandbox,
  instant, runs on whole directories.
- **`record`** — runs the function on generated inputs inside an
  [nsjail](https://github.com/google/nsjail) sandbox and records what it **actually** does:
  outcome, return value, mutation diffs, raised exceptions.

`validate` ties them together: it checks that every observed effect was statically predicted
(`observed ⊆ static`). The static side over-approximates; it must never miss an effect a real
run can show.

Output is versioned JSON. Other formats: terminal summary, `.pyi` stubs, self-contained HTML.

## Requirements

- Rust (edition 2024). The first build fetches and compiles the pinned ruff parser (~1–2 min).
- A sandbox, only for `record`/`validate`. Executed code is treated as untrusted, so every run
  is jailed with nsjail on a Linux kernel — native on Linux, via WSL2 on Windows. There is no
  unsandboxed path. One-time setup:

  ```sh
  # Windows: run inside your WSL2 distro
  wsl -d Ubuntu -- bash /mnt/c/Projects/ai/pylens/scripts/provision-sandbox.sh
  # Linux:
  bash scripts/provision-sandbox.sh
  ```

  `analyze` is pure Rust and needs none of this.

## Quick start

```sh
cargo build --release

pylens analyze examples/inventory.py   # static signatures, JSON, no jail
pylens record  examples/inventory.py   # + observed cases, runs the jail
pylens validate examples/inventory.py  # checks observed ⊆ static
```

A recorded case from `Inventory.add(name, qty)` — a call that mutates the receiver:

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
| `pylens analyze <file.py\|dir> [--format json\|summary\|pyi\|html]` | Static effect signature of every function/method, plus catalogued imports. |
| `pylens record <file.py\|dir> [--inputs N] [--format json\|summary\|pyi\|html]` | `analyze` plus observed cases per function and import resolution probes. Runs the jail. |
| `pylens validate <file.py\|dir> [--inputs N] [--format json\|summary\|html]` | Runs `record`, then checks `observed ⊆ static`. Exits non-zero on a hard soundness defect. |

A directory argument switches to project mode: all `*.py` files, analyzed in parallel, with
imports resolved against the other project files and effects propagated across them.

## Docs

- [User guide](docs/USER_GUIDE.md) — install, sandbox setup, commands, output formats, limits, troubleshooting
- [SCHEMA.md](docs/SCHEMA.md) — the JSON output contract, field by field, and versioning policy
- [DESIGN.md](docs/DESIGN.md) — effect taxonomy, soundness invariant, architecture, sandbox rationale
- [examples/README.md](examples/README.md) — the example corpus, kept at zero hard defects
