# pylens — Design

## Purpose

pylens examines Python functions and methods statically, in Rust. It extracts their **effects**
as a normalized **effect signature**:

- the returns;
- the in-place mutations of the arguments and of `self`;
- the raised exceptions;
- the I/O; and
- the generator property.

Then it runs each function in a sandbox on generated inputs and records the actual effects. The
signature and the observed cases together are the **behavioral record** of the function.

pylens stops at the record. The consumer does the subsequent work: it gives scores, it compares
a candidate against a reference, and it changes records into a training reward.
[SCHEMA.md](SCHEMA.md) specifies the JSON contract of the records.

## Principles and the soundness rule

The downstream signal is adversarial. If the analysis cannot examine an item, a consumer can use
that gap to get an incorrect reward. Thus the analyzer records each of its limits in
`unresolved_effects` and it **over-approximates**. Each reported set is a *may-set*:

> **observed_effects ⊆ static_may_set**

Each observed effect that the static side did not predict is a **soundness bug**. The count must
go to zero. `pylens validate` measures this. A gap in a signature that said that it was complete
is a **hard** defect. A gap that an acknowledged unresolved effect covers is **soft**.

The opposite condition is imprecision: pylens predicts an effect that never occurs. This is
tolerable, and it stays low. Dynamic tests can disprove soundness, but they cannot prove it.

Two consequences:

- A function is `pure` only if no static item stays unresolved. Unknown calls, dynamic
  constructs, and unknown decorators change the purity to `unknown`. They never change it to
  `pure`.
- The static effect *shape* is necessary but not sufficient. Two functions can have the same
  shape but return different values. The sandboxed `record` layer supplies the ground truth at
  the value level.

A third rule, learned from a bug: **a parameter's inferred shape is a hypothesis, not a
guarantee.** pylens infers the shape from the operations inside the function. Python lets a
caller pass anything, so the shape must never narrow a may-set. The implicit-raise set once used
it that way: `x < 0` was the only evidence that `x` is an integer, and that conclusion then
proved `x < 0` could not raise `TypeError`. A local's shape can narrow a may-set — `xs = []`
really is a list at that point — but a parameter's shape cannot.

That bug also shows a limit of the harness. Input generation draws its candidates from the same
inferred shapes. Analysis and generation thus shared the assumption, and no generated input
could contradict it. Only a caller with a broader shape broke the symmetry. When a claim is
justified by what pylens itself will generate, it is unfalsifiable, not true.

## Effect types (from the point of view of the caller)

1. **Return** — a value or `None`, and the coarse inferred kind. A new binding of a parameter
   name is not an effect.
2. **Argument mutation** — `p[i]=…`, `p.attr=…`, `del`, methods that change the object, and
   augmented assignment.
3. **Global and module-state writes.**
4. **Nonlocal and closure mutation.**
5. **`self`-attribute mutation** (methods).
6. **World I/O** — the tokens are `stdout`, `stderr`, `stdin` and `filesystem`. `print` gives
   `stdout`, or both console tokens when it has a `file` argument that pylens cannot resolve;
   `input` gives `stdin`; `open` and the modelled file-system calls give `filesystem`.
7. **Exceptions** — explicit (`raise` and `assert`), and implicit (the may-sets from the
   operators and the calls).
8. **Generator and async.**

pylens does not trust the annotations at any point. It records them and compares them against
the inferred side (`type_mismatches`, advisory only). It never uses them for inference.

## Architecture

```
parse (ruff) → pass pipeline → effect signatures → input generation → jail → observed cases
                                        └────────── validate: observed ⊆ static ──────────┘
```

- **`parse.rs`** — the boundary to ruff. All the `ruff_*` code is only here, thus you can replace
  the parser. Each source-input point first removes a UTF-8 BOM at the start. CPython accepts a
  BOM in a file, but the `compile()` function in the sandboxed worker does not accept one in
  text. The analyzer and the sandbox must see the same source.
- **`model/`** — the pure data model: the effect-signature types (`mod.rs`) and the recursive
  `Shape` lattice with canonical unions of limited width (`shape.rs`). The lattice includes
  `Instance(class)`, so a value built from a class declared in the module carries that class.
  The serialization is the schema.
- **`analyze/`** — an ordered pass pipeline over a shared context. One AST walk drives it, with
  one collector for each subject (aliases, mutations, exceptions, shapes, returns, guards,
  hints, body lines):
  **Imports → Declarations → Shapes → Effects → Interprocedural → TypeCheck → Purity**. Shapes
  runs to a fixpoint before Effects, so the walk reads the final shapes.

  Interprocedural propagates the effects of the local callees to the callers, to a fixpoint
  (recursion included). It maps the arguments by position and by keyword name. If a call
  unpacks (`f(*xs)` or `f(**kw)`), pylens keeps an acknowledged `call_unpacked_args` limit and
  an implicit `TypeError`, because the unpack operation itself can raise before the callee runs.
  pylens does not remove the effects. TypeCheck is advisory. Purity runs last, so it sees the
  complete effect sets.

  Effects also resolves a method call on a local instance. If a local's shape is exactly one
  `Instance(C)`, and `C.m` is declared in the module, `x.m(...)` resolves like any intra-module
  call. The constructor call `C(...)` resolves to `C.__init__`. A local bound to two different
  classes joins to a union and stays unresolved, because a choice between the members would
  under-approximate the other one. An inherited method stays unresolved.

  A fresh local's self-attribute mutations are dropped, not reported. The taxonomy is from the
  point of view of the caller, and the caller cannot see an object that the function just built.
- **`analyze/models.rs`** — the effect model table for the standard library. The key is the
  resolved module path, thus an aliased import finds the same entry. The table gives the raises
  and the io, and it removes the `call_import` acknowledgment. That is the reason each entry
  over-approximates: after the entry, a missed effect is a hard defect and not a soft one.
  Calls whose behavior depends on runtime state stay out of the table on purpose.
- **`generate/`** — input generation from the shapes: `mod.rs` is the sampler and `seeds.rs`
  holds the candidate values. Each candidate has a rank, and the sampler spends the budget in
  rank order. The ranks, best first:

  1. **Base** — one typical value for each shape, used to hold a parameter fixed.
  2. **Guard** — the literal and boundary values from the guard expressions of the function
     (`if qty > 10` gives 9, 10 and 11), and the parameter's own literal default. A default is
     the source of the function, not an annotation, thus pylens can trust it.
  3. **Hint** — the domain corpora (URL, e-mail, path, JSON, date, numeric string, regular
     expression, HTML), selected by the hints collector.
  4. **Edge** — empty, zero, singleton, negative.
  5. **Property** — sorted, descending, palindrome, all-equal and duplicate-heavy sequences,
     primes, powers of two, float traps. These fill a gap that the guard collector cannot fill:
     it extracts a literal only when the parameter is a direct operand, thus `n % 2 == 0`,
     `len(xs) > 3` and `xs == xs[::-1]` give no sample.
  6. **Filler** — the generic spread.

  The sampler varies one parameter at a time and holds the others at their base value, then
  fills the remaining budget with combinations. One parameter at a time gives much more
  coverage than a diagonal through the cartesian product at a small budget.

  There is no constraint solver: if a shape stays `Any`, pylens gives values of different types,
  and some of them do not agree with the true expectation of the function. This module also
  supplies the candidates for the shrink operation.
- **`record.rs`** (with `shrink.rs`) — connects the static signatures to the sandboxed execution.
  It runs the probes for the dependencies, for the module load, and for the constructor. If a
  module cannot load, pylens marks each function `uncallable` one time. It then constructs the
  cases and runs the greedy **input minimization** for the cases that raise: it shrinks the
  input while the same exception type occurs, with a limited budget. This minimization is an aid
  for the report only; validate never uses it. If the sandbox stops the code (out of memory,
  recursion, or timeout), this is an artifact of the sandbox: `outcome:"error"` with
  `error.stage:"resource"`. pylens never records it as a semantic `raised`.
- **`exec.rs`** — the `Sandbox` trait, the nsjail launchers (direct and through WSL), the
  JSON-over-stdio worker protocol, and the structured `HarnessError`. **There is no unsandboxed
  launcher.**
- **`validate.rs`** — the pure checker for `observed ⊆ static`. Refer to the rule above.
- **`project.rs`** and **`project/`** — directory mode: a walk that obeys `.gitignore`, parallel analysis, parallel
  sandboxed record and validate (a small pool of worker threads, each with its own sandbox),
  static resolution of the project-local imports (absolute and relative, with `__init__.py`
  semantics), and **cross-file effect propagation** for the free functions that come from a
  `project_local` import. The propagation applies the intra-file mapping rules over a fixpoint
  across the project.
- **`report/`, `stub/`, and `html/`** — the formatters for the same data: the terminal summary,
  the `.pyi` stubs, and one HTML file. The stubs use PEP 604 unions. With `record --format pyi`,
  pylens adds the observed types where the static side stayed unresolved and marks them with
  `# observed:`. It does this only if each observation agrees and the tagged serialization of
  the worker makes the type unambiguous.

## Execution layer

**Threat model.** pylens runs when you build the dataset. It does not trust any input code, the
reference code and the candidate code included. Thus each Python execution occurs in a sandbox.
There is no unsandboxed path, not for development and not for the tests.

**The sandbox is nsjail on a Linux kernel.** It uses namespaces, a seccomp-bpf filter, and
rlimits (limits on the memory, the CPU, the time, and the process count, no network, and a
read-only file system). The seccomp filter is a **denylist** of the clearly dangerous syscalls
(ptrace, mount, module load, setns, unshare, bpf, and more), together with dropped capabilities
and `NO_NEW_PRIVS`. A strict allowlist for CPython is fragile between interpreter versions and
stops the worker without a clear message. A change to an allowlist is possible future work.

The host system only changes how pylens reaches a Linux kernel:

| Host | Command shape |
|---|---|
| Linux | `nsjail <policy> -- python worker.py` |
| Windows | `wsl -d <distro> -- nsjail <policy> -- python worker.py` |

The policy, the worker, and the protocol are the same. **There is no Docker.** On Windows, the
untrusted code is already behind nsjail and the WSL2 virtual machine. A container would add a
third layer and a dependency on a daemon, with no increase in security. A container has one
advantage: hermetic provisioning for CI. You can add it later behind the same `Sandbox` trait.

pylens rejects these interpreters (refer to the research in
`docs/research/python-execution.md`): CPython-WASI (slow cold start, missing stdlib modules),
RustPython (insufficient fidelity), and Monty (insufficient maturity).

**Execution mechanics:**

- pylens uses true CPython, because the oracle needs full fidelity. A permanent fork-server
  worker pool decreases the cost of the interpreter start. Each call keeps its isolation,
  because each request runs in a new `fork()`.
- **How pylens sees the mutations:** it serializes the arguments to tagged JSON before and after
  the call. The difference is the observed mutation. The comparison is structural: it accepts a
  tolerance on floats and it ignores the order in sets and dicts. Tagged JSON also covers the
  types that JSON does not have (tuple, set, bytes, and objects).
- **Aliasing:** pylens reports the identity relations (`return is arg[i]`) explicitly. The
  serialization alone removes the identity, but the effect model keeps it.
- **Methods:** pylens builds the receiver with `__init__`. It captures the state of `self`
  before and after the call, as it does for an argument.
- pylens captures the stdout and the stderr of the function as effects. They do not go on the
  protocol channel.
- pylens compares the exceptions by **exception type**, not by message. The validator and the
  minimizer use the same rule.
- **Coverage:** the worker traces the call with `sys.settrace` and returns the executed lines.
  The jail runs Python 3.10, thus `sys.monitoring` is not available. The analyzer supplies the
  denominator: the statement lines of the function body, without the bodies of the nested
  definitions. A bare string-literal statement is not counted, because CPython compiles it away
  and never traces it. An `elif` condition line is counted, because it does execute. `record`
  and `validate` report executed, total and missed for each function.
- `SystemExit` is a `BaseException`, thus `except Exception` does not catch it. The worker
  catches it specially and reports it as a semantic raise, which is what it is.
  `KeyboardInterrupt` stays uncaught: that is a signal to the harness, not the behavior of the
  function.

## Known limitations

- **Library calls are modelled only in part.** `analyze/models.rs` covers the most frequent
  standard-library namespaces. Every other call through an import becomes an
  `unresolved_effects` entry, and `record` shows the actual behavior. Measured over the CPython
  3.13 standard library, the table removes 29% of the `call_import` sites (13761 to 9742).
- **Method resolution covers same-module classes only.** A local built from a class declared in
  the module resolves. An imported class, an inherited method, a union of two classes, and a
  longer dotted chain all stay unresolved. `call_method_unknown` is now the largest unresolved
  category, at 16202 sites over the same corpus, and a large part of it is the unbound
  superclass form `Base.method(self, ...)`.
- **Input generation is heuristic, but the gap is now measured.** Coverage says how much of each
  function the generated inputs reached, so "validate only examines the paths the inputs reach"
  is a number and not a warning. Generation still has no constraint solver.
- **A rebound parameter reports the shape it was rebound to.** `def f(x): x = []` reports `x`
  as a sequence, and the `.pyi` stub then writes that annotation, although the caller may pass
  anything. `validate` cannot catch this, because it concerns a declared shape and not an
  effect.
- **Not every claim can be checked.** An `io` `filesystem` claim has no observation channel at
  all: the jail's file system is read only, thus no execution can confirm or contradict it.
