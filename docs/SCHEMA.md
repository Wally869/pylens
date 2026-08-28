# pylens JSON schema

This is the contract for the `--format json` output of `analyze`, `record`, and `validate`. The
field names below agree with the serde definitions in `src/model/`, `src/record/`, and
`src/validate.rs`. pylens does not emit the optional fields if they are empty or absent.

## Versions

Each top-level output has a `schema_version` field (the `SCHEMA_VERSION` constant in
`src/lib.rs`).

- **When to increase the version:** only after a release, and only if a change alters the
  emitted contract. Before the first release (now — nothing has shipped and the contract has no
  consumers), the version stays at **`0.1`**. Each change, additive or breaking, goes into that
  version. The first release makes the contract fixed as `1.0`.
- **How to increase the version (after the first release):** increase the **major** number for
  each change that can break an existing parser, for example:
  - you remove or rename a field;
  - you change the type, the tag, or the value shape of a field; or
  - you change a closed set of enum strings.

  Increase the **minor** number for an additive change only — a new optional field, or a new
  value in a field that this document specifies as an open set.
- **Compatibility rules (after the first release):** in one major version, each field keeps its
  name, its type, and its meaning. Consumers must ignore the unknown fields, thus an addition is
  always safe. pylens never uses the name of a removed field again with a different meaning.
  There is no guarantee between major versions.

## Top-level shapes

| Command | One file | Directory (project mode) |
|---|---|---|
| `analyze` | `{ schema_version, imports, functions }` | `{ schema_version, root, files, summary }` |
| `record` | `{ schema_version, dependencies, functions }` | the same project wrapper |
| `validate` | `{ schema_version, functions, summary }` | the same project wrapper |

In project mode, each entry in `files` is the single-file body of that file plus a `path`. If
the file did not read, parse, or record, the entry is `{ path, error }`. `summary` collects the
file counts, the function counts, a purity histogram and, for validate, `hard_defects`,
`soft_defects`, `functions_checked`, and `uncallable`. In project mode, each import or
dependency entry also has `resolution` (`project_local`, `external`, or `unresolved_relative`)
and, for a project-local entry, a `project_target` path.

## Effect signature (`functions[]`, the static side)

```jsonc
{
  "name": "normalize",
  "kind": "method",                  // "function" | "method"
  "owner": "Normalizer",             // methods only — the class that defines the method
  "params": [ /* ParamInfo, below */ ],
  "declared_return": "list",         // from the annotation — NOT TRUSTED, never used for inference
  "is_generator": false,
  "returns": ["sequence"],           // the union over each return exit; "opaque" if unknowable
  "raises": {
    "explicit": ["ValueError"],      // from raise or assert
    "implicit": ["TypeError"]        // the static may-set from the operators and the calls
  },
  "mutations": [ /* Mutation, below */ ],
  "global_writes": ["COUNTER"],
  "io": ["stdout"],                  // "stdout" | "stderr" | "stdin" | "filesystem"
  "unresolved_effects": [ /* UnresolvedEffect, below */ ],
  "uses": [ { "binding": "np", "module": { "package": "numpy" } } ],
  "may_use_star": false,             // omitted if false
  "decorators": [],
  "type_mismatches": [ /* TypeMismatch, below */ ],
  "purity": "unknown"                // "pure" | "impure" | "unknown"
}
```

`purity` is `pure` only if no item stays unresolved. An `unresolved_effects` entry or an unknown
decorator makes it `unknown`.

### ParamInfo

| Field | Meaning |
|---|---|
| `name` | the name of the parameter |
| `shape` | the inferred `Shape` (below) |
| `has_default` | the parameter has a default value |
| `kind` | `positional` (the default, omitted) \| `var_positional` \| `var_keyword` \| `keyword_only` |
| `declared` | the annotation, not trusted; omitted if absent |
| `guard_samples` | the literal and boundary values from the guards of the function; omitted if empty |
| `hints` | inferred domain tags — `url`, `email`, `path`, `json`, `date`, `numeric_str`, `regex`, `html`; omitted if empty. **Advisory only**, like `type_mismatches`: they direct input generation and never change the may-set or the purity |

### Shape

The scalars and `Any` are single tag strings: `"int"`, `"float"`, `"bool"`, `"str"`, `"bytes"`,
`"none"`, and `"any"`. The containers are objects with one tagged key: `{"seq": <Shape>}`,
`{"set": <Shape>}`, and `{"map": {"key": <Shape>, "value": <Shape>}}`.

`{"instance": "Foo"}` is a value built from a class declared in the module. pylens infers it for
a local bound to a constructor call. A parameter receives it only when the function rebinds the
parameter name to such a call, which is a known defect — refer to the limitations in
[DESIGN.md](DESIGN.md).

`{"union": [<Shape>, ...]}` is canonical. pylens flattens it (a union never contains a union),
merges the members with the same constructor, sorts them, and removes the duplicates. It
collapses the union to `"any"` if a member is `Any` or if the width is more than 8.
`Optional[X]` is `{"union": [X, "none"]}`. There is no separate optional shape. The
deserialization makes the union canonical again, thus pylens never trusts a union that a person
wrote.

### Mutation

`{ "target": <MutationTarget>, "via": <kind>, "name": <str?> }`

- **target** — has a `root` tag: `{"root": "param", "name": ...}` · `{"root": "self_attr",
  "name": ...}` · `{"root": "global", "name": ...}` · `{"root": "nonlocal", "name": ...}` ·
  `{"root": "unknown"}`.
- **via** — `subscript_set` · `subscript_del` · `attr_set` · `attr_del` · `method` (`name` is
  the method) · `aug_subscript` · `aug_attr` · `aug_name`.

### UnresolvedEffect

`{ "reason": <str>, "callee": <str?>, "may_affect": [<MutationTarget>...] }`

The `reason` values are an open set:

- `call_import` — a call through an imported binding.
- `call_unknown_callee`
- `call_method_unknown` — an unknown method on a tracked root.
- `call_unpacked_args` — a resolved call that unpacks `*xs` or `**kw`; the mapping cannot assign
  those roots.
- `dynamic_setattr`, `dynamic_delattr`, `dynamic_exec`, `dynamic_eval`
- `decorator` — an unknown decorator can replace the function.

`may_affect` lists each tracked root that goes to the opaque call: positional, keyword, and
unpacked.

### TypeMismatch (advisory only — it never changes the may-set or the purity)

`{ "kind": "return" | "param", "declared": <str>, "inferred": [<ReturnKind>...],
"param": <str?>, "inferred_shape": <Shape?> }`

pylens reports a mismatch only if the two sides are fully disjoint. A wildcard annotation never
gives a mismatch. Both checks accept `bool` ⊆ `int` ⊆ `float` (the PEP 484 numeric tower).

## Record additions (the dynamic side)

The `record` output flattens each static signature and adds these fields:

### Dependency (`dependencies[]`)

The catalogued import (each style; `module` is `{ package, path? }`, together with `alias`,
`scope` `module` or `function`, and the relative `level`) with a `status`: `resolved`,
`unresolved` (with a structured `error`), or `not_probed` (a relative import outside project
mode).

### FunctionRecord (`functions[]`)

The signature fields, and also:

- `uncallable` — present if the function never executed:
  `{ "reason": "module_not_loadable" | "constructor_failed", "error": <HarnessError> }`.
- `cases` — one entry for each executed input.
- `coverage` — `{ "executed": <int>, "total": <int>, "missed": [<line>...] }`, the executed
  lines of the function body over all its cases. `total` counts the statement lines of the body,
  without the bodies of the nested definitions and without a bare string-literal statement,
  which CPython never traces. Omitted when the function is `uncallable`, has no cases, or has an
  empty body count.
- `branches` — the per-branch-outcome accounting: one entry per branch point in the function's
  body, `{ "kind": <BranchKind>, "line": <int>, "outcomes": [<BranchOutcome>...] }`. `kind` is
  `"if"` \| `"while"` \| `"for"` \| `"except"` \| `"try_else"` \| `"match"` \| `"ternary"` \|
  `"bool_op"` \| `"inline_if"` \| `"comprehension_if"`. A `BranchOutcome` is `{ "outcome": <str>,
  "status": "covered" | "uncovered" | "unobservable_line_granularity", "reason": <str, uncovered
  only> }`. The outcome names are construct-specific (`"true"`/`"false"` for `if`/ternary,
  `"enter"`/`"skip"` for `while`, `"iterate"`/`"empty"` for `for`, `"entered"` for
  `except`/`try_else`/`match`, `"short_circuit"`/`"full_evaluation"` for boolops).
  `unobservable_line_granularity` means line-level tracing cannot distinguish this outcome from
  its siblings — same-line constructs (ternaries, `and`/`or` short-circuits, single-line `if x: y`
  bodies, comprehension guards) are always in that state; every other kind is decided from the
  traced `(prev_line, cur_line)` arcs (or, for `except`/`try_else`/`match`, the traced
  handler/clause/case line) aggregated over every case. Omitted under the same conditions as
  `coverage` (no branch points, or no cases).
  - `reason` is present exactly when `status == "uncovered"`, and says why the `record
    --cover-branches` loop (`src/record/cover.rs`) didn't confirm this outcome: `"loop_not_run"`
    (`--cover-branches` wasn't passed — every uncovered outcome carries this reason in plain
    `record`), `"no_synthesizer"` (no handled predicate form covers this outcome's branch test —
    see `src/generate/predicate.rs` for the closed set of forms — or every synthesized value was
    excluded by `--value-domain`), `"candidates_exhausted"` (synthesized values were tried and
    executed, and the outcome still didn't fire), or `"budget"` (the loop's total-case budget —
    `--inputs`, reinterpreted as the TOTAL per-function case count once `--cover-branches` is set
    — ran out before this outcome got a synthesized case).
- `branch_coverage` — `{ "covered": <int>, "uncovered": <int>, "unobservable": <int> }`, the
  rollup over every outcome in `branches` — a closed count, always present exactly when `branches`
  is.

### Case

| Field | Meaning |
|---|---|
| `input` | the positional argument values |
| `kwargs` | the keyword-only arguments; omitted if empty |
| `ctor_args` | the constructor arguments for the receiver (methods); omitted in the other conditions |
| `outcome` | `returned` \| `raised` \| `error` |
| `return` | the return value (`returned` only) |
| `raises` | the name of the exception type (`raised` only) |
| `mutations` | the observed mutations: `{ "target": <param name or "self">, "before", "after" }` |
| `return_aliases_arg` | the index of the argument that **is** the return value (identity), if one exists |
| `stdout` / `stderr` | the captured output; omitted if empty |
| `error` | a structured `HarnessError` (the `error` outcome only) |
| `minimized` | `{ "input", "kwargs"? }` — a smaller input that raises the same exception type; `raised` cases only, and present only if the shrink operation succeeded |

`raised` always means that the function itself raised. This is part of its behavior. If the
sandbox stops the code (out of memory, recursion limit, or timeout), the outcome is `error` with
`error.stage: "resource"`. It is never `raised`. A `HarnessError` is
`{ "stage", "kind", "message", "module"? }`.

## Validate additions

For each function: the `hard_defects` and `soft_defects` counts, and a `defects` list:

`{ "case_index": <int>, "dimension": "mutation" | "raise" | "io" | "return",
"observed": <str>, "expected": <str>, "severity": "hard" | "soft" }`

- **hard** — the signature said that it was complete (no `unresolved_effects`), but pylens did
  not predict an observed effect. This is a pylens soundness bug. The CLI exits with a non-zero
  code.
- **soft** — an acknowledged `unresolved_effects` entry covers the missed effect.

Each function also carries the `coverage` object described above. The `summary` carries an
aggregate `coverage` of `{ "executed", "total" }` over the functions that have one.

The `summary` also has an `uncallable` count. A function that never executed gives a result with
zero defects, which has no value. Thus pylens shows the count, and you do not read the result as
"validated".
