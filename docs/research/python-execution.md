# Python Execution Strategy for pylens

**Date:** June 2026
**Status:** Research recommendation

## Background

pylens is a Rust-based static analyzer that extracts effect signatures (return type, in-place mutations, raised exceptions, I/O, generators) from Python functions as over-approximating may-sets. The execution layer must serve two roles:

1. **Offline oracle phase**: run gold functions on generated inputs; record ground-truth effects. Also validates the static analyzer by checking `observed_effects ⊆ static_may_set`.
2. **RL hot-loop phase**: replay many candidate functions against frozen test suites to compute behavioral-equivalence reward signals. Per-candidate latency dominates here.

Both roles require CPython-semantic fidelity: any divergence from real CPython silently corrupts the reward signal.

---

## 1. Comparison Table

| Criterion | PyO3 (in-process) | Subprocess + CPython worker pool | CPython-WASI via wasmtime | RustPython | OS sandbox (nsjail/gVisor) wrapping CPython |
|---|---|---|---|---|---|
| **CPython fidelity** | Exact — it IS CPython | Exact — it IS CPython | Near-exact but stdlib gaps (threading, multiprocessing, subprocess, sockets blocked by WASI) | Poor — explicitly not production-ready, known gaps in stdlib and builtins | Exact — it IS CPython, sandbox is at kernel boundary only |
| **Isolation / safety** | None — runs in the Rust process address space; PyO3 docs warn "never pass untrusted code" | Process-boundary isolation; OS-level sandbox (seccomp, namespaces, cgroups) applied to worker | Strong: WASM memory model + capability-based WASI; no file/network unless explicitly granted | N/A for the oracle role; RustPython is itself not sandboxed | Strong: nsjail uses Linux namespaces + seccomp-bpf + cgroups; near-zero compute overhead, filesystem fully controlled |
| **Mutation observability** | Full: `copy.deepcopy()` before call, deep-compare after, within the same Python runtime | Full: serialize pre-call deep copy via pickle, execute, serialize post-call state back | Full in principle; serialization same as subprocess path. WASI startup overhead makes it less practical | Full in principle but fidelity risk negates its usefulness as an oracle | Full: same deep-copy pattern applied inside the worker script; pre/post state passed over stdin/stdout |
| **Per-call latency** | Lowest: ~tens of microseconds for GIL acquire + call; no IPC, no process spawn | Persistent worker: IPC round-trip ~1-5 ms (pickle + Unix socket/pipe). Fresh spawn: 50-200 ms (Python startup). Worker pool amortizes startup across calls | High: ~1.2 s cold per-invocation overhead measured in practice; not suitable for hot loop without pre-warmed instances | N/A (fidelity disqualifies) | Same as subprocess worker path if the nsjailed Python process is kept alive (persistent worker). Fresh nsjail spawn + Python start: ~100-300 ms. |
| **Rust interop** | Tight: `pyo3` crate directly drives the CPython C API; return values extracted as typed Rust values or JSON | Clean: Rust spawns/manages worker pool, communicates via JSON or pickle over stdin/stdout or Unix socket | Moderate: `wasmtime` Rust crate can host the WASM module; communication via WASI file descriptors or component model | `rustpython-vm` crate exists but API is unstable; GIL semantics differ | Rust manages nsjail process pool via `std::process::Command`; same JSON-over-pipe contract as plain subprocess |
| **Maturity / maintenance** | Production-grade; v0.23 (2026), widely deployed, active maintenance | CPython subprocess: decades of production use. Worker pool pattern well-understood. | CPython WASI tier-2 in 3.13. Threading absent in WASI <= 0.2; 0.3 targets threading (not yet released). Many stdlib modules unavailable. PEP 816 formalizes WASI support but gaps remain. | Development-phase ("should not be used in production"). Active commits but explicitly experimental. | nsjail: Google-maintained, production use (Windmill, Discord's snekbox, various AI code execution services). gVisor: 50-100 ms startup, 5-20% CPU overhead on I/O-heavy paths. |
| **Implementation effort** | Low-medium: well-documented PyO3 APIs; hard to add isolation (requires forking or OS sandbox layer separately) | Low-medium: worker harness is ~200 lines of Python + Rust process manager; isolation is layered on separately via nsjail config | High: WASI build of CPython requires careful configuration; stdlib gaps may silently break test functions; debugging is harder | High and risky: any CPython incompatibility found in production corrupts the reward signal | Low-medium: nsjail config is declarative; snekbox is an open-source reference implementation. Complexity is in the nsjail policy file. |

---

## 2. Recommended Approach

### Primary: Persistent nsjail-sandboxed CPython worker pool, orchestrated from Rust via JSON over stdin/stdout

#### Architecture

```
Rust analyzer (pylens)
    |
    | JSON task spec
    v
Worker pool manager (Rust, std::process)
    |---> nsjail  [Worker-0: python worker_harness.py]
    |---> nsjail  [Worker-1: python worker_harness.py]
    |---> ...
    |---> nsjail  [Worker-N: python worker_harness.py]
```

Each worker is a long-lived Python process running `worker_harness.py`, wrapped at start-up by nsjail. The Rust pool manager:

- Spawns N workers at start-up (N = CPU count or configured concurrency).
- Sends tasks as newline-delimited JSON to a worker's stdin.
- Reads results from stdout as newline-delimited JSON.
- Tracks timeouts; kills and replaces a worker that exceeds its wall-clock budget.
- Reuses workers across calls — Python interpreter startup cost is paid once per worker, not once per call.

The nsjail invocation wraps the Python binary with:

- `--time_limit` (wall-clock per execution)
- `--rlimit_as` (memory cap, e.g. 512 MB)
- `--iface_no_lo` (no network)
- `--disable_proc` (no /proc access)
- `--seccomp_policy` (syscall allowlist)
- Read-only bind mounts for CPython stdlib and the project venv; a private tmpfs for /tmp

This is the exact architecture used in production by Discord's snekbox ([GitHub](https://github.com/python-discord/snekbox)) and AI code execution services ([Morph nsjail blog](https://www.morphllm.com/nsjail-sandbox)).

#### JSON contract (Rust analyzer <-> Python worker)

**Request (Rust sends):**
```json
{
  "task_id": "uuid",
  "source": "def f(xs):\n    xs.append(1)\n    return len(xs)\n",
  "fn_name": "f",
  "inputs": [
    {"kind": "list", "value": [1, 2, 3]}
  ],
  "timeout_ms": 2000
}
```

**Response (worker sends back):**
```json
{
  "task_id": "uuid",
  "ok": true,
  "return_value": {"type": "int", "value": 4},
  "args_post": [
    {"kind": "list", "value": [1, 2, 3, 1]}
  ],
  "exception": null,
  "timeout": false
}
```

Or on exception:
```json
{
  "task_id": "uuid",
  "ok": false,
  "return_value": null,
  "args_post": null,
  "exception": {"type": "ValueError", "message": "..."},
  "timeout": false
}
```

#### Mutation observability (concrete implementation)

`worker_harness.py` captures mutations using deep-copy before / deep-compare after:

```python
import copy, json, sys, types, traceback, signal

def execute(source: str, fn_name: str, inputs: list) -> dict:
    ns = {}
    exec(compile(source, "<fn>", "exec"), ns)
    fn = ns[fn_name]

    args = [deserialize(inp) for inp in inputs]
    args_pre = [copy.deepcopy(a) for a in args]

    ret = None
    exc = None
    try:
        ret = fn(*args)
        # Drain generators to observe their effects
        if hasattr(ret, '__next__'):
            ret = list(ret)
    except Exception as e:
        exc = {"type": type(e).__name__, "message": str(e)}

    args_post = [serialize(a) for a in args]
    return {
        "return_value": serialize(ret),
        "args_post": args_post,
        "args_pre": [serialize(a) for a in args_pre],
        "exception": exc,
    }
```

Notes:
- `copy.deepcopy()` handles nested lists, dicts, sets, and instances with `__deepcopy__`. Objects that define `__copy__` but not `__deepcopy__` may need special handling.
- For classes whose instances are not deep-copyable (e.g., open file handles), the harness should catch `TypeError` from `deepcopy` and fall back to a best-effort attribute snapshot.
- Generators: the harness should call `fn(*args)` and, if the result is a generator/iterator, exhaust it (`list(ret)`) so that any side-effects from `yield` expressions are realized. The consumed sequence becomes the observable return.
- The serialization layer (`serialize`/`deserialize`) converts Python values to/from JSON-safe representations. For non-JSON-native types (sets, tuples, custom classes), use a tagged encoding: `{"__type__": "set", "items": [...]}`.

#### How it fits the offline-generate / hot-loop-replay split

**Offline phase (expensive, run once per gold):**
- Hypothesis drives test-case generation. A harness Python script uses `hypothesis.find()` or `@given` + `settings(max_examples=500)` to discover diverse, shrunk inputs for the gold function.
- Worker pool executes gold on each input; results are serialized to a frozen test suite (JSON file or SQLite table).
- The frozen suite includes: input vectors, gold return value, gold args-post state, gold exception type.

**RL hot-loop phase (many candidates, latency-critical):**
- For each candidate, the Rust manager dispatches all frozen inputs in parallel across the worker pool.
- Each worker executes candidate on one input and returns the observed effects.
- Rust computes the reward signal by comparing candidate effects against the frozen gold suite (field-by-field JSON diff).
- Because workers are persistent (not respawned per call), per-candidate latency is dominated by IPC round-trip + Python execution, not process startup.
- At 8 workers and 1-5 ms round-trip per call, the system can sustain ~1,600-8,000 calls/second.

#### Hypothesis as the input generator

Hypothesis ([docs](https://hypothesis.readthedocs.io/)) generates and shrinks inputs in the offline phase. The harness runs inside the nsjail worker so generated test cases automatically capture the gold behavior under sandboxed conditions. Strategies should be parameterized by the static effect signature: if pylens reports the function takes a `list[int]`, the Hypothesis strategy is `st.lists(st.integers())`. The frozen corpus includes the raw Hypothesis example database entry so inputs can be reproduced deterministically.

#### Fallback: PyO3 for the validator path only

For the static-analyzer validation task (checking `observed_effects ⊆ static_may_set`), the trusted-code assumption relaxes slightly: the gold functions are from a known corpus. In this context, PyO3 in-process execution is acceptable and avoids IPC overhead. The Rust analyzer can call PyO3 to run gold-only code with tight `resource` limits set from Rust, using `Python::with_gil(|py| { ... })`. This fallback is not suitable for candidate code.

---

## 3. Candidates Evaluated — Detailed Notes

### PyO3 (in-process embedding)
PyO3 v0.23+ provides Rust bindings to the CPython C API. Calling a Python function from Rust requires acquiring the GIL (`Python::attach()`), loading the module, and extracting the return value. Fidelity is perfect. However, PyO3's own documentation warns "Never pass untrusted code to this function" ([PyO3 guide](https://pyo3.rs/v0.28.3/python-from-rust/calling-existing-code)). The Rust forum confirms that when Python runs inside the Rust process, OS-level sandboxing cannot be applied to Python alone — the entire Rust process would need to be sandboxed ([Rust forum thread](https://users.rust-lang.org/t/secure-arbitrary-code-execution-pyo3/106126)). Subinterpreter support in PyO3 is still a tracking issue as of mid-2026 ([PyO3 issue #3451](https://github.com/PyO3/pyo3/issues/3451)), so per-call isolation via subinterpreters is not yet practical.

### Subprocess + CPython (persistent workers)
Running Python as a separate process is the canonical recommendation for untrusted code from the Rust community. With persistent workers, startup cost is amortized. IPC over Unix pipes (or a Unix domain socket) with newline-delimited JSON adds ~1-5 ms per round-trip in practice. Workers can be wrapped with nsjail at spawn time for isolation. This is the chosen primary strategy.

### CPython compiled to WASM (wasmtime)
CPython builds for WASI exist and are tier-2 in CPython 3.13 under PEP 816 ([PEP 816](https://peps.python.org/pep-0816/)). The isolation story is compelling: WASM's memory model + capability-based WASI gives sandboxing without any OS-level configuration. However, practical measurements show ~1.2 s cold-start overhead per invocation ([Atlantbh WASM sandboxing](https://www.atlantbh.com/sandboxing-python-code-execution-with-wasm/)). More critically, the `threading`, `multiprocessing`, `subprocess`, and `socket` modules are entirely absent — WASI 0.2 has no thread model; WASI 0.3 (targeting threading) was expected in early 2026 but is not yet final ([WASI status 2025](https://eunomia.dev/blog/2025/02/16/wasi-and-the-webassembly-component-model-current-status/)). For an oracle that must handle "broadly arbitrary Python," these gaps are disqualifying now. Revisit in 2027 when WASI 0.3 stabilizes.

### RustPython
RustPython targets CPython 3.x semantics but is explicitly "development phase — should not be used in production or a fault-intolerant setting" ([GitHub](https://github.com/RustPython/RustPython)). For a behavioral-equivalence oracle, any CPython incompatibility silently invalidates the reward signal. Disqualified on fidelity grounds.

### OS-level sandboxing (nsjail / gVisor / seccomp) wrapping CPython
nsjail uses Linux namespaces + seccomp-bpf + cgroups. Per-allowed-syscall overhead is near zero (BPF runs in the kernel before syscall dispatch). When the Python worker process is kept alive inside the jail, per-call cost is identical to the plain subprocess case. gVisor interposes all syscalls in a userspace kernel (Sentry), adding 5-20% CPU overhead and 50-100 ms start-up; its filesystem I/O can be 10-100x slower than native. nsjail is preferred over gVisor for this use case because Python function evaluation is CPU-bound, not I/O-heavy, and nsjail's overhead is negligible on CPU-bound workloads. nsjail is production-proven for exactly this pattern (snekbox, Windmill, AI code execution services).

### Pydantic / monty (rejected as stated)
v0.0.18 as of mid-2026: no class/method/generator support, subset reimplementation (fidelity risk), no argument-mutation readback. Future watch item only.

---

## 4. Risks and Open Questions

### Risks

**1. deepcopy incompatibility with certain argument types.** Objects that contain file handles, locks, or C-extension state may raise `TypeError` on `copy.deepcopy()`. The harness must handle this with a fallback (attribute-level snapshot via `vars()` or `__dict__`) and flag the test case as having partial mutation observability. The static analyzer validation check becomes unreliable for such types.

**2. nsjail is Linux-only.** The Rust project builds on Windows (current environment). For development, running the worker pool inside WSL2 or a Docker container is necessary. The RL training environment is assumed to be Linux; document this assumption explicitly.

**3. Pickle security in the reverse direction.** The worker harness receives Python source code (trusted only as far as nsjail allows), executes it, and returns JSON. JSON is safe. If a future optimization uses pickle for richer serialization (e.g., NumPy arrays), the Rust process must never unpickle data originating from the sandboxed worker without re-sandboxing the unpickling process itself.

**4. Non-deep-copyable return values.** If the gold function returns an open file, a socket, or a generator that has already been partially consumed, the serialization layer must detect this and record a symbolic sentinel rather than the value itself.

**5. Nondeterminism in gold functions.** Functions using `random`, `time.time()`, `os.urandom()`, etc. will produce inconsistent results across calls. The harness should seed `random` with a fixed value before each call and optionally mock `time`. This does not fully solve `os.urandom()`. Remaining nondeterminism should be flagged in the frozen test suite metadata.

**6. Resource exhaustion within the nsjail memory limit.** Adversarial candidate code that allocates near the memory cap may cause OOM inside the jail. nsjail's `--rlimit_as` will kill the process; the worker must detect SIGKILL on the child and return a timeout/OOM result rather than hanging.

### Open Questions

- **What is the target call throughput for the RL hot loop?** If 10,000+ calls/second are needed, the JSON + pickle serialization overhead may require switching to a binary protocol (MessagePack) or shared memory.
- **How should class state (object attributes) be captured?** The current design handles function arguments. If the function under test is a method and the receiver object has mutable state, `copy.deepcopy(self)` must be included in the pre/post snapshot. The JSON contract needs a `receiver_pre` / `receiver_post` field.
- **What Python version is pylens targeting?** The worker pool should pin to the same minor version used during static analysis. Mismatches between analyzer's AST assumptions and runtime behavior are a source of soundness holes.
- **How large are typical frozen test suites per gold function?** If each gold function generates 500 Hypothesis examples, and each example's pre/post JSON is ~1 KB, the offline corpus is ~500 KB per function. At 10,000 gold functions this is ~5 GB — manageable, but storage format (SQLite vs flat JSON files) should be decided early.
- **How to handle infinite generators?** `list(gen)` will not terminate for infinite iterables. The harness needs a hard iteration cap (e.g., 10,000 items) with a truncation sentinel in the result.

---

## 5. References

- PyO3 calling existing code guide: https://pyo3.rs/v0.28.3/python-from-rust/calling-existing-code
- PyO3 performance guide: https://pyo3.rs/main/performance
- Rust forum — sandboxing with PyO3: https://users.rust-lang.org/t/secure-arbitrary-code-execution-pyo3/106126
- PyO3 subinterpreter tracking issue: https://github.com/PyO3/pyo3/issues/3451
- snekbox (nsjail-based Python sandbox, production reference): https://github.com/python-discord/snekbox
- nsjail for AI code execution (Morph, 2026): https://www.morphllm.com/nsjail-sandbox
- Atlantbh — sandboxing Python with WASM (perf measurements): https://www.atlantbh.com/sandboxing-python-code-execution-with-wasm/
- PEP 816 — WASI formal support in CPython: https://peps.python.org/pep-0816/
- WASI component model status (Feb 2025): https://eunomia.dev/blog/2025/02/16/wasi-and-the-webassembly-component-model-current-status/
- WASI Preview 2 vs WASIX (2026 overview): https://wasmruntime.com/en/blog/wasi-preview2-vs-wasix-2026
- RustPython GitHub: https://github.com/RustPython/RustPython
- gVisor performance guide: https://gvisor.dev/docs/architecture_guide/performance/
- gVisor vs Kata Containers vs Firecracker (2025): https://onidel.com/blog/gvisor-kata-firecracker-2025
- RL sandbox infrastructure overview: https://www.daytona.io/dotfiles/sandbox-infrastructure-for-reinforcement-learning-agents
- Hypothesis property-based testing docs: https://hypothesis.readthedocs.io/
- Agentic property-based testing (Oct 2025): https://arxiv.org/html/2510.09907v1
- secimport (eBPF per-module syscall sandbox for Python): https://github.com/avilum/secimport
- Python multiprocessing IPC overhead (2026): https://www.techbuddies.io/2026/03/29/how-to-optimize-python-multiprocessing-pool-performance-and-cut-ipc-overhead/
- RL for safe LLM code generation (Berkeley 2025): https://www2.eecs.berkeley.edu/Pubs/TechRpts/2025/EECS-2025-123.pdf
