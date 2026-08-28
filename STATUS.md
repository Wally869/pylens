# Status

Tests: 177. `validate examples`: 0 hard defects, 44 soft, coverage 92/107 lines.

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
  `positional_arg_roots_skip_first` for the argument mapping). Same-module only; a builtin or
  imported base (`Exception`) stays unresolved. On the stdlib corpus this converts 26 functions
  out of purity `unknown` (25 to `impure`, 1 to `pure`).
- A parameter rebound to an unrelated value (`def f(x): x = []`) no longer reports the rebound
  shape. The Shapes pass freezes the parameter on the rebind and resets it to `any` after the
  fixpoint. A self-referential rebind (`x = x.strip()`) still votes; locals are unaffected.

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

### 1. The rebind freeze is flow-insensitive and drops sound facts

The conservative fix for the rebound parameter discards the shape entirely. But after
`x = Box()` the name really is a `Box` — that is a local binding fact, and method resolution
through it was sound. Measured cost on the stdlib corpus: about 24 `call_method_unknown` sites
that previously resolved. A flow-sensitive treatment would keep the parameter *annotation* at
`any` while the post-rebind *local* shape keeps resolving calls.

### 2. An aliased from-import misses the model table

`from os.path import join as j; j(...)` looks up `os.path.j`, which fails, and safely falls
back to unresolved/`any`. The Effects and Shapes passes share the miss identically (both go
through `resolve_dotted`), so they stay consistent — just imprecise for this one form. Fix in
the import-binding table: record the original name next to the local alias.

### 3. Superclass resolution across files

The `Base.method(self, ...)` resolution is same-module only. Most stdlib bases are imported or
builtin (`Exception`), so project mode (`src/project/interproc.rs`) is where the remaining
volume is.

## Measurements to repeat after a change

`temp/callee_freq.md` holds the method and the baselines: the callee histogram, the purity
distribution, and the counts for each unresolved reason over the standard library. Repeat them
after any change to the analyzer that is intended to move precision.
