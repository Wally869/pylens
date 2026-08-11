# Status

Tests: 165. `validate examples`: 0 hard defects, 44 soft, coverage 92/107 lines.

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

## Open work

### 1. Resolve unbound superclass calls — `Base.method(self, ...)`

`call_method_unknown` is now the largest unresolved category: 16202 sites over the CPython 3.13
standard library, against 9742 for `call_import`. A large part is the unbound superclass form,
which is common in exception hierarchies. `calls.rs` already resolves `self.method(...)`; the
same resolution applies when the attribute base is a declared class name and the first
positional argument is the receiver of the caller.

Note that the count went from 8756 to 16202 because of the fix that stopped these
acknowledgments disappearing when the receiver was not trackable. They are newly visible blind
spots, not new ones.

### 2. A rebound parameter reports the rebound shape

`def f(x): x = []` reports `x` as a sequence, and `def f(x): x = Box()` reports it as an
instance of `Box`, which the `.pyi` stub then writes as an annotation. The caller can give
anything. `validate` cannot catch this, because it concerns a declared shape and not an effect.
Confirmed to be older than the instance work. Fix: the shape of a parameter must stop
accumulating votes after the name is rebound to an unrelated value.

### 3. Feed the model table's return kinds into the Shapes pass

`src/analyze/models.rs` records a return kind for each entry, but nothing reads it. Shapes runs
before Effects, so this needs its own wiring. It would give `os.path.join(...)` a `str` shape
instead of `any`, which improves both the inference and the generated inputs.

## Measurements to repeat after a change

`temp/callee_freq.md` holds the method and the baselines: the callee histogram, the purity
distribution, and the counts for each unresolved reason over the standard library. Repeat them
after any change to the analyzer that is intended to move precision.
