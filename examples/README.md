# Example corpus

This is non-trivial Python code that exercises the analyzer and the record flow. Run any command
on one file, or on the full `examples/` directory to exercise project mode (a collected report,
and resolution of the project-local imports):

```sh
pylens analyze  examples/<file>.py            # static signatures
pylens record   examples/<file>.py --inputs 4 # signatures and observed cases (sandboxed)
pylens validate examples/<file>.py --inputs 4 # the observed ⊆ static check (sandboxed)

pylens analyze  examples --format summary     # the same, over each *.py file in examples/
pylens analyze  examples/<file>.py --format pyi   # an inferred .pyi stub in place of JSON
pylens record   examples/<file>.py --format html > report.html  # one HTML report file
```

| File | Exercises |
|---|---|
| `inventory.py` | A class with methods: `self`-attribute mutation (a dict subscript set, `del`, an augmented assignment, a list append), explicit raises (`ValueError`, `OverflowError`, `KeyError`), the receiver state before and after the call, and a method that returns aliased state. |
| `normalize.py` | Complex free functions: in-place mutation of a nested sequence, a return of a changed argument (aliasing), return unions from more than one path (`str`, `int`, `none`), an explicit raise, and a default parameter. |
| `streaming.py` | A generator (`yield`), a qualified stdlib call (`re.finditer` — an external dependency that the static layer cannot model), an unknown free callee, a method call on a parameter, and captured stdout I/O. |
| `graph.py` | A local alias of a parameter that the code then changes (the alias tracking must attribute the mutation to the parameter), a `global` write, traversal of a set, a dict, and a list, and a return that aliases an argument. |
| `config.py` | Dynamic attribute writes with `setattr`, on `self` and on a parameter (`dynamic_setattr` unresolved effects), together with concrete `self`-attribute mutation. |
| `ledger.py` | A class whose methods use **true** libraries (`hashlib` and `datetime`): class-method records (`self`-attribute mutation and a raise), together with resolved dependencies. |
| `deps.py` | Each **import style**, with a mix of resolvable stdlib modules and missing or fake packages: plain, `as` alias, dotted path with an alias, `from … import a, b`, single, dotted-from-missing, `*` star, and relative. It shows the resolution report in `dependencies`. |
| `lazy_deps.py` | Imports **in function bodies**. The module loads, thus a missing import (`matplotlib`) raises `ModuleNotFoundError` at call time, but a true one (`json`) operates correctly. |

The generated records for these files are in `../records/`. To make them again, run
`pylens record`.

Note: the generation uses the shapes, not a constraint solver. Thus some generated inputs have
an incorrect type for a given function, and the case record shows this correctly (for example, a
`TypeError`, or an `AttributeError` if the code tries `setattr` on a non-object). This is the
true behavior, not a bug.
