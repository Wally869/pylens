# Handover — open follow-ups

Current state: the analyzer, jailed recorder, `observed ⊆ static` validation harness, multi-file
mode (parallel analyze **and** parallel jailed record), cross-file effect propagation with
keyword-argument mapping, `Shape` unions / param-annotation mismatch / observed-type-folding
`.pyi` output, and input minimization for raised cases are in place. `cargo test` is green
(jail-gated ones skip when the sandbox isn't provisioned) and
`cargo clippy --all-targets -- -D warnings` is clean. See `CLAUDE.md` for the architecture and
codemap. The whole `examples/` corpus is at zero hard defects; `tests/validate.rs` and
`tests/project.rs` assert that — keep it there. JSON contract is at `SCHEMA_VERSION = "0.1"`
(pre-release: it absorbs all contract changes until a first release ships).

Recently closed (kept here one handover-cycle for context):

- **Unpacked call arguments** (`f(*xs)` / `f(**kw)`) at resolved call sites were a real
  soundness hole (a resolved callee's effect through an unpacked arg was silently dropped —
  reproducible hard defects). Now: positional mapping stops at the first `*`-unpack, every
  unpacked call gets an implicit `TypeError` (the unpack operation itself can raise it) plus a
  `call_unpacked_args` acknowledgment carrying the unpacked roots, and cross-file resolution
  keeps the `call_import` acknowledgment for unpacked sites (`ImportCallSite::has_unpack`).
- Opaque-call `may_affect` now scans keyword arguments and unpacked roots, not just positional.
- `bool` ⊆ `int` ⊆ `float` (PEP 484 numeric tower) accepted by the TypeCheck advisory checks.
- UTF-8 BOM is stripped at every source-ingestion point (a BOM'd file used to reach the jailed
  worker's `compile()` as text and silently record zero cases); `validate` summaries now also
  surface uncallable-function counts instead of letting vacuous passes read as validated.

## Feature follow-ups

None currently tracked.
