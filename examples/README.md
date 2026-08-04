# Example corpus

Non-trivial Python chosen to exercise the analyzer and the record flow. Run either command on
any file:

```sh
pylens analyze examples/<file>.py            # static signatures
pylens record  examples/<file>.py --inputs 4 # signatures + observed cases (jailed)
```

| File | Exercises |
|---|---|
| `inventory.py` | A class with methods: `self`-attribute mutation (dict subscript set, `del`, augmented assign, list append), explicit raises (`ValueError`/`OverflowError`/`KeyError`), receiver pre/post state in cases, a method returning aliased state. |
| `normalize.py` | Complex free functions: in-place nested-sequence mutation, returning a mutated argument (aliasing), multi-path return unions (`str`/`int`/`none`), explicit raise, default parameter. |
| `streaming.py` | A generator (`yield`), a qualified stdlib call (`re.finditer` — an external dep the static layer can't model), an unknown free callee, a method call on a parameter, and captured stdout I/O. |
| `graph.py` | Local aliasing of a parameter that is then mutated (alias tracking must blame the parameter), a `global` write, set/dict/list traversal, a return that aliases an argument. |
| `config.py` | Dynamic attribute writes via `setattr` on `self` and on a parameter (`dynamic_setattr` unresolved effects) alongside concrete self-attribute mutation. |
| `ledger.py` | A class whose methods use **real** libraries (`hashlib`, `datetime`): class-method records (self-attr mutation, raise) alongside resolved dependencies. |
| `deps.py` | Every **import style** mixing resolvable stdlib with missing/fake packages: plain, `as` alias, dotted-path + alias, `from … import a, b`, single, dotted-from-missing, `*` star, and relative. Shows the `dependencies` resolution report. |
| `lazy_deps.py` | Imports **inside function bodies** — the module loads, so a missing import (`matplotlib`) raises `ModuleNotFoundError` at call time while a real one (`json`) works. |

Generated records for these live in `../records/` (regenerate with `pylens record`).

Note: generation is shape-directed, not constraint-solving, so some generated inputs are
ill-typed for a given function and the case records that honestly (e.g. a `TypeError`, or an
`AttributeError` when `setattr` is tried on a non-object). That is the real behavior, not a bug.
