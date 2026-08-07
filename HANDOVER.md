# Handover — open follow-ups

Current state: the analyzer, jailed recorder, `observed ⊆ static` validation harness, multi-file
mode (parallel analyze **and** parallel jailed record), cross-file effect propagation with
keyword-argument mapping, `Shape` unions / param-annotation mismatch / observed-type-folding
`.pyi` output, input minimization for raised cases, and the CLI are in place. `cargo test` is
green (jail-gated ones skip when the sandbox isn't provisioned) and
`cargo clippy --all-targets -- -D warnings` is clean. See `CLAUDE.md` for the architecture and
codemap. The whole `examples/` corpus is at zero hard defects; `tests/validate.rs` and
`tests/project.rs` assert that — keep it there. JSON contract is at `SCHEMA_VERSION = "1.2"`.

## Feature follow-ups

- **`**kwargs`-unpacking call arguments** (`f(**extra)`) are not mapped by either
  interprocedural propagator — no single name to key by, consistent with `*args`-unpacking
  being unmapped on the positional side. Sound (nothing is claimed about a real root), just
  imprecise.
- **Opaque-call `may_affect` scanning** (`call_unknown_callee`/`call_import` on genuinely
  unresolved callees) only scans positional args, not keywords. Sound today because those
  unresolved effects acknowledge incompleteness; extending it would tighten the may-set.
- **`bool` vs `int` advisory asymmetry**: the TypeCheck param/return mismatch checks treat a
  declared `int` as excluding an inferred `bool` (Python's `bool` is an `int` subtype), which
  can produce a noisy advisory `TypeMismatch`. Advisory-only, never touches the may-set.

## Minor / hygiene

- `src/model.rs` sits just over the ~500-line soft budget (~547) after the `Union` growth. A
  cohesive split (e.g. shape vs signature halves) is fine when convenient; no `part_N`.
