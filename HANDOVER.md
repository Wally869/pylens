# Handover — open follow-ups

Current state: the analyzer, jailed recorder, `observed ⊆ static` validation harness, multi-file
mode, `.pyi`/HTML output, and the CLI are in place. `cargo test` is green (100 tests, jail-gated
ones skip when the sandbox isn't provisioned) and `cargo clippy --all-targets -- -D warnings` is
clean. See `CLAUDE.md` for the architecture and codemap.

## Known soundness gap (tracked)

- **`examples/graph.py` — `walk` / `merge_into` observe `TypeError` the static may-set doesn't
  predict.** The cause is deeper than the shipped param-root `TypeError` rules: a param flows
  through a **local** (`node = stack.pop()`) into a hashable/subscript context, and a subscript
  **base** (`dst[k] = v` where `dst` is `Any`) can be non-subscriptable. The fix is to generalize
  implicit-`TypeError` inference to consult the full Shapes-pass env (params **and** locals) and
  cover membership (`in`), hashable-key operations (`set.add`, `dict.get`/key methods), and
  `Any` subscript bases — aiming for the whole `examples/` tree at zero hard defects.
  `tests/validate.rs` currently asserts zero hard defects on the known-clean files
  (`inventory.py`, `normalize.py`) only. **Do not weaken it to mask new findings** — report and
  close them.

## Feature follow-ups

- **Cross-file effect propagation.** Import *resolution* across a project is done
  (`src/project/resolve.rs`); the remaining half is building a project-wide symbol table and
  propagating effect summaries across files (the cross-file analogue of the intra-file
  Interprocedural pass), with a fixpoint over the cross-file call graph.
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
