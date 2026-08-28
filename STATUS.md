# Status

Tests: 236. `validate examples`: 0 hard defects, 33 soft, coverage 92/107 lines.

## Done in the last pass

- Ranked input generation (Base, Guard, Hint, Edge, Property, Filler) with one-at-a-time
  sampling. `--inputs` defaults to 12.
- Property corpora (sorted, palindrome, all-equal, primes, float traps) and domain corpora
  (URL, e-mail, path, JSON, date, numeric string, regular expression, HTML) driven by an
  inferred `hints` tag per parameter.
- Executed-line coverage from the jailed worker, reported per function and in the summaries.
- `Shape::Instance`, with method and constructor resolution for same-module classes.
- The standard-library effect table (`src/analyze/models.rs`).
- Two soundness fixes: a parameter's inferred shape no longer narrows a raise may-set, and
  `validate` now checks captured stderr.
- Two worker fixes: non-finite floats no longer produce invalid JSON, and `sys.exit()` records
  as a semantic raise instead of crashing the forked child.
- The model table's return kind now feeds the Shapes pass (`models::return_kind`, queried
  through `resolve_stdlib_return`, which shares `resolve_dotted` with `resolve_stdlib_call` so
  both passes agree on which stdlib function a call refers to): `os.path.join(...)` and similar
  calls now give the assigned local a `str`/`bool`/`int`/`float`/sequence shape instead of `any`.
  Only entries whose return type is unambiguous across the whole raises/io group are modelled;
  everything else stays `Any`.
- Unbound superclass calls `Base.method(self, ...)` resolve when `Base` is a class declared in
  the same module and the first positional argument is the caller's receiver (`calls.rs`, with
  `positional_arg_roots_skip_first` for the argument mapping), AND across files in project mode
  when `Base` is imported from a sibling project file and that file declares `Base` as a class
  with `method` (`src/project/interproc.rs`'s `build_method_table` + `ResolvedReceiver::
  CallerSelfSkipFirst`, driven by `ImportCallSite::unbound_receiver`). A builtin base
  (`Exception`) still stays unresolved. On the stdlib corpus the same-module form converts 26
  functions out of purity `unknown` (25 to `impure`, 1 to `pure`).
- A parameter rebound to an unrelated value (`def f(x): x = []`) no longer reports the rebound
  shape as its own declared/generated shape: the Shapes pass's name->shape env stays unfrozen
  (a rebound name's post-rebind evidence is sound, exactly like any other local's), and
  `ParamInfo::shape` alone is reset to `any` for a frozen parameter, in
  `passes::effects::finalization::finish` (driven by `ModuleAnalysis::frozen_params`, which the
  Shapes pass now returns alongside its env instead of baking the reset into it).
  The env is flow-INSENSITIVE, though — one merged shape per name over the whole function body —
  so resolving a call through that post-rebind evidence is only sound when the call is textually
  guaranteed to run after the rebind. This is gated with a dominance check rather than a full CFG:
  `ShapeState`/`FunctionFacts` each track the current statement's nesting depth and, for a
  top-level (depth-0, directly-in-the-function-body) statement, its index in that top-level
  sequence. A parameter's FIRST unrelated rebind only qualifies the name for the shortcut when
  that rebind itself sits at depth 0 (`ModuleAnalysis::frozen_dominance`, name -> that rebind's
  top-level index); a rebind nested inside ANY compound statement (`if`/`for`/`while`/`try`/
  `with`/`match`) never qualifies — the name stays fully frozen for the whole function, because on
  a loop's first iteration a later-looking use can run before that iteration's own rebind. For a
  qualifying name, `FunctionFacts::env_shape` only returns the post-rebind evidence when the
  current call site's own top-level index is STRICTLY GREATER than the rebind's — a later
  top-level statement can never execute before an earlier one completes, so this needs no CFG.
  Everywhere the gate isn't satisfied, `env_shape` returns `Shape::Any` (full width), matching the
  original fully-frozen behavior exactly. Net effect: `def f(x): x = Box(); x.bump()` resolves
  `bump()` (rebind at index 0, use at index 1 — dominated); `def f(x): x.bump(); x = Box();
  x.bump()` resolves only the SECOND `bump()` (the first, textually before the rebind, keeps its
  `call_method_unknown` acknowledgment — dropping it there was a soundness regression caught by
  validate on `temp/probe_flow2.py` before this gate existed, since `f(3)` genuinely raises
  `AttributeError` at that call and nothing acknowledged it); a rebind inside a loop body never
  resolves at all. A self-referential rebind (`x = x.strip()`) still votes; locals are unaffected.
- An aliased from-import resolves in the model table: `from os.path import join as j; j(a, b)`
  now looks up `os.path.join`, not the miss `os.path.j`. `ModuleAnalysis::import_names` records
  the original imported name next to the local alias binding; both `resolve_stdlib_call`
  (Effects) and `resolve_stdlib_return` (Shapes) consult it before formatting the dotted lookup
  key, so they stay in agreement.
- An `import`/`from ... import ...` statement inside a function body now adds `ImportError` and
  `ModuleNotFoundError` to the function's implicit raise may-set (a module-level import still
  doesn't — it fails the whole module at load time, handled as uncallable). Closes the
  `lazy_deps.py` soft-defect class; corpus soft defects dropped 44 -> 33 (0 hard, unchanged).

## Measurement note — the acknowledgment counts are no longer comparable

Interprocedural propagation inherits a callee's `unresolved_effects` into every caller. A
change that resolves more calls therefore *raises* the flat site counts: one removed
acknowledgment at the call site pulls in all of the callee's own acknowledgments, once per
caller. After this pass the counts are `call_method_unknown` 16265, `call_unknown_callee`
10168, `call_import` 9876 (baseline at the previous commit, same corpus and script: 16202,
10088, 9742). Use the purity distribution as the precision gauge instead: `unknown` 8246
(64.9%, was 8272 / 65.1%), `pure` 3238, `impure` 1221, over 12705 functions.
`temp/remeasure.py` reproduces both tables.

## Open work

None currently tracked here. The three items from the previous pass (flow-insensitive rebind
freeze, aliased from-import missing the model table, same-module-only superclass resolution) are
folded into "Done in the last pass" above.

## Measurements to repeat after a change

`temp/callee_freq.md` holds the method and the baselines: the callee histogram, the purity
distribution, and the counts for each unresolved reason over the standard library. Repeat them
after any change to the analyzer that is intended to move precision.
