# pylens JSON schema

The contract for everything `analyze`, `record`, and `validate` emit as `--format json`.
Field names below are verified against the serde definitions in `src/model/`, `src/record.rs`,
and `src/validate.rs`. Optional fields are omitted from the output when empty/absent.

## Versioning

Every top-level output carries `schema_version` (the `SCHEMA_VERSION` const in `src/lib.rs`).

- **When to bump:** only after a release has shipped and a change alters the emitted contract.
  Pre-release (now — nothing has shipped, the contract has no consumers) the version is pinned
  at **`0.1`** and every change, additive or breaking, folds into it without a bump. The first
  shipped release freezes the contract as `1.0`.
- **How to bump (post-release):** **major** for anything that can break an existing parser —
  removing/renaming a field, changing a field's type, tag, or value shape, changing closed enum
  string sets; **minor** for purely additive changes — new optional fields, new values in
  fields documented as open sets.
- **Compatibility rules (post-release):** within a major version, existing fields keep their
  name, type, and meaning; consumers must ignore unknown fields, so additions are always safe;
  a removed field name is never reused with a different meaning. No guarantee across majors.

## Top-level shapes

| Command | Single file | Directory (project mode) |
|---|---|---|
| `analyze` | `{ schema_version, imports, functions }` | `{ schema_version, root, files, summary }` |
| `record` | `{ schema_version, dependencies, functions }` | same project wrapper |
| `validate` | `{ schema_version, functions, summary }` | same project wrapper |

In project mode each entry in `files` is that file's single-file body plus a `path`, or
`{ path, error }` if the file failed to read/parse/record. `summary` aggregates file counts,
function counts, a purity histogram, and (validate) `hard_defects` / `soft_defects` /
`functions_checked` / `uncallable`. In project mode, import/dependency entries also gain
`resolution` (`project_local` / `external` / `unresolved_relative`) and, when project-local,
a `project_target` path.

## Effect signature (`functions[]`, the static side)

```jsonc
{
  "name": "normalize",
  "kind": "method",                  // "function" | "method"
  "owner": "Normalizer",             // methods only — the defining class
  "params": [ /* ParamInfo, below */ ],
  "declared_return": "list",         // from the annotation — UNTRUSTED, never used for inference
  "is_generator": false,
  "returns": ["sequence"],           // union over all return exits; "opaque" when unknowable
  "raises": {
    "explicit": ["ValueError"],      // from raise / assert
    "implicit": ["TypeError"]        // statically-inferred may-set (operator- and call-induced)
  },
  "mutations": [ /* Mutation, below */ ],
  "global_writes": ["COUNTER"],
  "io": ["stdout"],
  "unresolved_effects": [ /* UnresolvedEffect, below */ ],
  "uses": [ { "binding": "np", "module": { "package": "numpy" } } ],
  "may_use_star": false,             // omitted when false
  "decorators": [],
  "type_mismatches": [ /* TypeMismatch, below */ ],
  "purity": "unknown"                // "pure" | "impure" | "unknown"
}
```

`purity` is `pure` only when nothing unresolved remains; any `unresolved_effects` entry or
unrecognized decorator makes it `unknown`.

### ParamInfo

| Field | Meaning |
|---|---|
| `name` | parameter name |
| `shape` | inferred `Shape` (below) |
| `has_default` | has a default value |
| `kind` | `positional` (omitted, the default) \| `var_positional` \| `var_keyword` \| `keyword_only` |
| `declared` | the annotation, untrusted; omitted when absent |
| `guard_samples` | literal/boundary values pulled from the function's own guards; omitted when empty |

### Shape

Scalars and `Any` are bare tag strings: `"int"`, `"float"`, `"bool"`, `"str"`, `"bytes"`,
`"none"`, `"any"`. Containers are single-key tagged objects: `{"seq": <Shape>}`,
`{"set": <Shape>}`, `{"map": {"key": <Shape>, "value": <Shape>}}`.

`{"union": [<Shape>, ...]}` is canonical: flattened (never nested), same-constructor members
merged, sorted and deduplicated, collapsed to `"any"` when any member is `Any` or the width
exceeds 8. `Optional[X]` is `{"union": [X, "none"]}` — there is no separate optional shape.
Deserialization re-canonicalizes, so a hand-written union is never trusted as-is.

### Mutation

`{ "target": <MutationTarget>, "via": <kind>, "name": <str?> }`

- **target** — tagged with `root`: `{"root": "param", "name": ...}` · `{"root": "self_attr",
  "name": ...}` · `{"root": "global", "name": ...}` · `{"root": "nonlocal", "name": ...}` ·
  `{"root": "unknown"}`.
- **via** — `subscript_set` · `subscript_del` · `attr_set` · `attr_del` · `method` (with
  `name` = the method) · `aug_subscript` · `aug_attr` · `aug_name`.

### UnresolvedEffect

`{ "reason": <str>, "callee": <str?>, "may_affect": [<MutationTarget>...] }`

`reason` values (open set): `call_import` (call through an imported binding),
`call_unknown_callee`, `call_method_unknown` (unrecognized method on a tracked root),
`call_unpacked_args` (a resolved call unpacking `*xs`/`**kw` — the mapping can't attribute
those roots), `dynamic_setattr` / `dynamic_delattr` / `dynamic_exec` / `dynamic_eval`,
`decorator` (an unrecognized decorator may replace the function). `may_affect` lists every
tracked root handed to the opaque call — positional, keyword, and unpacked.

### TypeMismatch (advisory only — never affects the may-set or purity)

`{ "kind": "return" | "param", "declared": <str>, "inferred": [<ReturnKind>...],
"param": <str?>, "inferred_shape": <Shape?> }`

Flagged only on full disjointness; wildcard annotations never flag; `bool` ⊆ `int` ⊆ `float`
(PEP 484 numeric tower) is accepted on both checks.

## Record additions (the dynamic side)

`record` output flattens each static signature and adds:

### Dependency (`dependencies[]`)

The catalogued import (all styles; `module` is `{ package, path? }`, plus `alias`, `scope`
`module`/`function`, relative `level`) with `status`: `resolved` | `unresolved` (with a
structured `error`) | `not_probed` (relative imports outside project mode).

### FunctionRecord (`functions[]`)

The signature fields, plus:

- `uncallable` — present when the function never executed:
  `{ "reason": "module_not_loadable" | "constructor_failed", "error": <HarnessError> }`.
- `cases` — one entry per executed input.

### Case

| Field | Meaning |
|---|---|
| `input` | positional argument values |
| `kwargs` | keyword-only arguments; omitted when empty |
| `ctor_args` | constructor args used to build the receiver (methods); omitted otherwise |
| `outcome` | `returned` \| `raised` \| `error` |
| `return` | return value (`returned` only) |
| `raises` | exception type name (`raised` only) |
| `mutations` | observed mutations: `{ "target": <param name or "self">, "before", "after" }` |
| `return_aliases_arg` | index of the argument the return value **is** (identity), if any |
| `stdout` / `stderr` | captured output, omitted when empty |
| `error` | structured `HarnessError` (`error` outcome only) |
| `minimized` | `{ "input", "kwargs"? }` — a shrunk input still raising the same exception type; `raised` cases only, present only when shrinking succeeded |

`raised` always means the function itself raised — part of its behavior. A resource kill
(OOM, recursion limit, timeout) is `outcome: "error"` with `error.stage: "resource"`, never
`raised`. `HarnessError` is `{ "stage", "kind", "message", "module"? }`.

## Validate additions

Per function: `hard_defects` / `soft_defects` counts and a `defects` list:

`{ "case_index": <int>, "dimension": "mutation" | "raise" | "io" | "return",
"observed": <str>, "expected": <str>, "severity": "hard" | "soft" }`

- **hard** — the signature claimed completeness (no `unresolved_effects`) yet an observed
  effect wasn't predicted. A pylens soundness bug; the CLI exits non-zero.
- **soft** — the miss is covered by an acknowledged `unresolved_effects` entry.

The `summary` also carries `uncallable`: functions that never executed produce vacuous
zero-defect results, so the count is surfaced rather than reading as "validated".
