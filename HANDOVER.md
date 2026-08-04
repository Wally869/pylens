# Handover — open follow-ups

Current state: the analyzer, jailed recorder, `observed ⊆ static` validation harness, multi-file
mode, `.pyi`/HTML output, and the CLI are in place. `cargo test` is green (jail-gated ones skip
when the sandbox isn't provisioned) and `cargo clippy --all-targets -- -D warnings` is clean. See
`CLAUDE.md` for the architecture and codemap. The whole `examples/` tree (not just
`inventory.py`/`normalize.py`) is at zero hard defects — implicit-`TypeError` inference now
consults the Shapes pass's full params+locals env (membership, subscript-base, and the existing
ordered-compare/arithmetic/subscript-key rules all key off it), closing the `graph.py`
`walk`/`merge_into` gap that used to be tracked here. `tests/validate.rs` and `tests/project.rs`
assert zero hard defects across the full corpus — keep it there.

## Feature follow-ups

- **Cross-file effect propagation** is done (`src/project/interproc.rs`): a project-wide symbol
  table (free functions only) plus a per-file import-binding resolver map calls to project-local
  imported functions onto their target, and a fixpoint (mirroring the intra-file
  `Interprocedural` pass's mapping rules) propagates the callee's effects onto the caller across
  file boundaries — resolving the `call_import` unresolved effect the intra-file Effects pass
  otherwise leaves. Wired into `analyze_project`, `record_project`, and `validate_project` (via
  `record::record_with_signatures`); single-file mode is unaffected. Keyword-argument -> param
  mapping is future work, same as the intra-file pass.
- **`.pyi` / type model depth.** Stubs render from the recursive `Shape` + return may-set today.
  Richer output (param-annotation mismatch, union/optional in the `Shape` model itself, observed
  dynamic types folded in) is future work.

## Minor / hygiene

- A few files sit over the ~500-line soft budget after feature growth
  (`src/analyze/passes/effects/mod.rs`, `src/html.rs`). Cohesive splits are fine when convenient;
  do not mechanically fragment (no `part_N`).
- Honoring `.gitignore` in the project walk is future work (currently a built-in skip-list of
  common dirs + dotfiles).
- Parallel *jailed* record across files is future work (analyze is parallel; record reuses one
  pool sequentially across a project's files).
