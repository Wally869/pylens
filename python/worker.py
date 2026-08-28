"""pylens execution harness — runs INSIDE the nsjail sandbox.

Two modes:

* **one-shot** (default): read one JSON request from stdin, execute it, write one JSON
  response to stdout, exit. One jailed process per call — maximum isolation.
* **--serve** (fork-server): read newline-delimited JSON requests from stdin and write one
  newline-delimited JSON response per request. Each request is executed in a *forked child*,
  so untrusted code cannot leak interpreter state (monkeypatched builtins, globals, imported
  modules) into the measurement of any other request. The parent never `exec`s untrusted
  source. This is what the persistent worker pool drives; it amortizes interpreter + import
  startup while preserving per-request isolation. A `batch` request goes through an
  intermediate child that execs the module once and forks one grandchild per item from that
  primed state, so only the batch's first item pays a module exec — the parent still never
  execs untrusted source, and every item's mutations still die with its own grandchild.

Captured per request: the return value, pre- and post-call argument state (mutation is detected
by diffing the tagged-encoded snapshot taken before the call against the one after), any raised
exception, captured stdout and stderr (separately), and whether the return value *is* (identity)
one of the arguments. Non-JSON-native types (set / tuple / dict / objects) use a tagged encoding
so they round-trip and compare stably. Harness/setup failures are returned as a structured
`error` ({stage, kind, message, module?}), never as a bare string.

Resource exhaustion (`MemoryError`, `RecursionError`, a call that exceeds the wall-time budget)
is a sandbox artifact, not the function's behavior — it is reported through `error` with
`stage="resource"`, distinct from a genuine `raise` inside the function (which stays on
`exception`). See `RESOURCE_EXCEPTIONS` below.
"""

import sys
import json
import os
import io
import time
import select
import signal
import contextlib


def deserialize(v):
    if isinstance(v, dict) and "__t__" in v:
        t = v["__t__"]
        items = v.get("items", [])
        if t == "set":
            return set(deserialize(x) for x in items)
        if t == "tuple":
            return tuple(deserialize(x) for x in items)
        if t == "dict":
            return {deserialize(k): deserialize(val) for k, val in items}
        if t == "float":
            return {"nan": float("nan"), "inf": float("inf"), "-inf": float("-inf")}[v["v"]]
        return v
    if isinstance(v, list):
        return [deserialize(x) for x in v]
    if isinstance(v, dict):
        return {k: deserialize(val) for k, val in v.items()}
    return v


def serialize(v):
    if isinstance(v, float) and (v != v or v in (float("inf"), float("-inf"))):
        # json.dumps(allow_nan=False) rejects NaN/Infinity; the Rust side's serde_json can't
        # hold them either, so a non-finite float needs the same tagged encoding as set/tuple/dict.
        tag = "nan" if v != v else ("inf" if v > 0 else "-inf")
        return {"__t__": "float", "v": tag}
    if isinstance(v, bool) or v is None or isinstance(v, (int, float, str)):
        return v
    if isinstance(v, list):
        return [serialize(x) for x in v]
    if isinstance(v, tuple):
        return {"__t__": "tuple", "items": [serialize(x) for x in v]}
    if isinstance(v, (set, frozenset)):
        items = [serialize(x) for x in v]
        items.sort(key=lambda x: json.dumps(x, sort_keys=True))
        return {"__t__": "set", "items": items}
    if isinstance(v, dict):
        items = [[serialize(k), serialize(val)] for k, val in v.items()]
        items.sort(key=lambda kv: json.dumps(kv[0], sort_keys=True))
        return {"__t__": "dict", "items": items}
    try:
        d = vars(v)
    except TypeError:
        d = None
    return {"__t__": "obj", "repr": repr(v), "dict": serialize(d) if d is not None else None}


def base_response():
    return {
        "ok": False,
        "return": None,
        "args_post": None,
        "kwargs_post": None,
        "exception": None,
        "return_aliases_arg": None,
        "error": None,
        "lines": [],
        "arcs": [],
    }


def make_error(stage, kind, message, module=None):
    """A structured harness/setup error: a stage, a kind (the exception type or a harness
    code), a message, and — for import failures — the missing module name."""
    e = {"stage": stage, "kind": kind, "message": message}
    if module:
        e["module"] = module
    return e


def exc_error(stage, exc):
    """Structured error from a caught exception. ModuleNotFoundError/ImportError carry the
    missing module in `.name`, which we surface as `module` so callers needn't parse strings."""
    return make_error(stage, type(exc).__name__, str(exc), getattr(exc, "name", None))


RESOURCE_EXCEPTIONS = (MemoryError, RecursionError)
"""Exceptions that mean the sandbox ran out of a resource, not that the function under test
raised as part of its behavior. Caught ahead of the generic `except Exception` in `run_request`
and reported through `error` (stage `"resource"`), never through `exception` — so a consumer
can never mistake "ran out of memory/stack" for a semantic raise. `KeyboardInterrupt` is left
alone: it's a harness signal (never something the function under test does on purpose), so it
still falls outside every `except` below and crashes the (forked) child, surfacing as
`no_output` — the error side, never `exception`. `SystemExit` is different: `sys.exit()` is
genuine function behavior (e.g. a CLI entry point), so `run_request` catches it explicitly,
below, and reports it through `exception` like any other raise, even though it's a
`BaseException` the generic `except Exception` wouldn't otherwise see."""


def _clip(text):
    return text if len(text) <= 4096 else text[:4096] + "...<truncated>"


def _state(obj):
    """The receiver's attribute dict, or None if it has none."""
    try:
        return vars(obj)
    except TypeError:
        return None


def _make_tracer(executed, arcs):
    """A `sys.settrace` global trace function that records every line reached in the compiled
    module under test (`co_filename == "<pylens>"`), skipping frames from anywhere else (stdlib,
    the harness itself). Python 3.10 predates `sys.monitoring` (3.12+), so `settrace` is the only
    line-level hook available.

    Also records every `(prev_line, cur_line)` arc within a frame — the transition line coverage
    can't see (e.g. an else-less `if`'s false path never has its own line, but the arc from the
    test line straight to whatever follows it does). `prev_line` is tracked per-frame (keyed by
    `id(frame)`, cleared on `return`) so a call into another function under trace never creates a
    spurious arc from the caller's line into the callee's.
    """
    last_line = {}

    def local_trace(frame, event, arg):
        if event == "line":
            cur = frame.f_lineno
            executed.add(cur)
            key = id(frame)
            prev = last_line.get(key)
            if prev is not None:
                arcs.add((prev, cur))
            last_line[key] = cur
        elif event == "return":
            last_line.pop(id(frame), None)
        return local_trace

    def global_trace(frame, event, arg):
        # Only frames of the module under test get a local tracer at all. A foreign frame
        # (stdlib, third-party) returns None, so its per-line events never fire — the win is
        # large when the function under test leans on pure-Python stdlib code. A foreign frame
        # calling back into "<pylens>" code still traces: every new call re-enters here.
        if frame.f_code.co_filename != "<pylens>":
            return None
        return local_trace

    return global_trace


def _load_module(source):
    """Compile and exec the module source, returning its namespace dict. Compiling executes
    nothing; the exec is what runs the module's top-level side effects, so callers that share a
    namespace across many calls (the primed-fork batch path) must only ever call this once, in
    the process that will be forked from — never in the long-lived serve parent."""
    ns = {}
    exec(compile(source, "<pylens>", "exec"), ns)
    return ns


def run_request(req, ns=None):
    """Execute one request in the current process. Returns a response dict.

    Free function: call `fn(*args)`. Method: instantiate `class(*ctor_args)`, snapshot its
    state, then call `receiver.fn(*args)` and snapshot again (self_pre / self_post).

    Load probe: when `fn` is null, only execute the module source (and, if `class` is given,
    instantiate the receiver) and report whether it loaded — no call. This is how the caller
    learns a module won't import, or a constructor can't be built, *once* instead of per case.

    `ns` is the module's already-loaded namespace, when the caller primed it (the primed-fork
    batch path: the intermediate child execs the module once and forks a grandchild per item,
    each grandchild passing its inherited `ns` here so it skips its own exec). When `ns` is
    None (the default, and always true for a single unbatched request), the module is loaded
    here instead, so the unbatched path's behavior — and its `stage="setup"` error on a load
    failure — is unchanged.
    """
    resp = base_response()
    try:
        source = req["source"]
        fn_name = req.get("fn")
        args_in = req.get("args", [])
        kwargs_in = req.get("kwargs", {})
        class_name = req.get("class")
        ctor_in = req.get("ctor_args", [])
    except Exception as e:
        resp["error"] = make_error("bad_request", type(e).__name__, str(e))
        return resp

    # Stage timing, requested per call (`"timing": true`): exact perf_counter_ns spans per
    # stage, attached as `resp["timings"]` (µs). The Rust client ignores unknown fields, so
    # production output is unaffected; only the successful-call path is stamped.
    timing = bool(req.get("timing"))
    tm = {}

    # Load/ctor probe: exec the module, optionally build the receiver, report load status.
    if fn_name is None:
        try:
            if ns is None:
                ns = _load_module(source)
        except Exception as e:
            resp["error"] = exc_error("setup", e)
            return resp
        if class_name is not None:
            cls = ns.get(class_name)
            if cls is None:
                resp["error"] = make_error("harness", "class_not_found", f"class {class_name!r} not found")
                return resp
            try:
                cls(*[deserialize(a) for a in ctor_in])
            except Exception as e:
                resp["error"] = exc_error("ctor", e)
                return resp
        resp["ok"] = True
        return resp

    receiver = None
    try:
        if ns is None:
            t0 = time.perf_counter_ns()
            ns = _load_module(source)
            if timing:
                tm["module_exec_us"] = (time.perf_counter_ns() - t0) // 1000
        t0 = time.perf_counter_ns()
        args = [deserialize(a) for a in args_in]
        kwargs = {k: deserialize(v) for k, v in kwargs_in.items()}
        if timing:
            tm["deser_us"] = (time.perf_counter_ns() - t0) // 1000
        if class_name is not None:
            cls = ns.get(class_name)
            if cls is None:
                resp["error"] = make_error("harness", "class_not_found", f"class {class_name!r} not found")
                return resp
            receiver = cls(*[deserialize(a) for a in ctor_in])
            resp["self_pre"] = serialize(_state(receiver))
            fn = getattr(receiver, fn_name, None)
            if fn is None:
                resp["error"] = make_error("harness", "method_not_found", f"method {fn_name!r} not found")
                return resp
        else:
            fn = ns.get(fn_name)
            if fn is None:
                resp["error"] = make_error("harness", "function_not_found", f"function {fn_name!r} not found")
                return resp
    except Exception as e:
        resp["error"] = exc_error("setup", e)
        return resp

    # Snapshot the arguments in the SAME tagged encoding used for args_post, so mutation
    # detection compares like-with-like (not the raw plain-JSON input against tagged output).
    t0 = time.perf_counter_ns()
    resp["args_pre"] = [serialize(a) for a in args]
    resp["kwargs_pre"] = {k: serialize(v) for k, v in kwargs.items()}
    if timing:
        tm["pre_ser_us"] = (time.perf_counter_ns() - t0) // 1000
    t_call = time.perf_counter_ns()

    # Capture the function's stdout/stderr on SEPARATE buffers so they can't corrupt the JSON
    # protocol channel and so each is recorded as the channel it actually is.
    out_buf = io.StringIO()
    err_buf = io.StringIO()
    executed_lines = set()
    executed_arcs = set()
    try:
        with contextlib.redirect_stdout(out_buf), contextlib.redirect_stderr(err_buf):
            # Traced region: the call under test and (if it returns a generator) draining it —
            # not the module load or the receiver construction above.
            sys.settrace(_make_tracer(executed_lines, executed_arcs))
            try:
                ret = fn(*args, **kwargs)
                if hasattr(ret, "__next__"):  # drain generators/iterators to realize effects
                    collected = []
                    for i, x in enumerate(ret):
                        if i >= 1000:
                            collected.append("__truncated__")
                            break
                        collected.append(x)
                    ret = collected
            finally:
                sys.settrace(None)
        if timing:
            tm["call_us"] = (time.perf_counter_ns() - t_call) // 1000
        resp["ok"] = True
        t0 = time.perf_counter_ns()
        resp["return"] = serialize(ret)
        if timing:
            tm["ret_ser_us"] = (time.perf_counter_ns() - t0) // 1000
        alias = None
        if isinstance(ret, (list, dict, set)):
            for i, a in enumerate(args):
                if ret is a:
                    alias = i
                    break
        resp["return_aliases_arg"] = alias
    except RESOURCE_EXCEPTIONS as e:
        resp["ok"] = False
        resp["error"] = exc_error("resource", e)
    except SystemExit as e:
        resp["ok"] = False
        resp["exception"] = {"type": type(e).__name__, "message": "" if e.code is None else str(e.code)}
    except Exception as e:
        resp["ok"] = False
        resp["exception"] = {"type": type(e).__name__, "message": str(e)}

    resp["lines"] = sorted(executed_lines)
    resp["arcs"] = sorted(executed_arcs)

    out_text = out_buf.getvalue()
    if out_text:
        resp["stdout"] = _clip(out_text)
    err_text = err_buf.getvalue()
    if err_text:
        resp["stderr"] = _clip(err_text)

    if receiver is not None:
        try:
            resp["self_post"] = serialize(_state(receiver))
        except Exception as e:
            resp["error"] = exc_error("serialize", e)

    try:
        t0 = time.perf_counter_ns()
        resp["args_post"] = [serialize(a) for a in args]
        resp["kwargs_post"] = {k: serialize(v) for k, v in kwargs.items()}
        if timing:
            tm["post_ser_us"] = (time.perf_counter_ns() - t0) // 1000
    except Exception as e:
        resp["error"] = exc_error("serialize", e)

    if timing and tm:
        resp["timings"] = tm
    return resp


def oneshot():
    try:
        req = json.loads(sys.stdin.read())
    except Exception as e:
        resp = base_response()
        resp["error"] = make_error("bad_request", type(e).__name__, str(e))
        sys.stdout.write(json.dumps(resp, allow_nan=False))
        return
    sys.stdout.write(json.dumps(run_request(req), allow_nan=False))


def _fork_bytes(produce, timeout):
    """Fork a child that runs `produce()` — a zero-arg callable returning the bytes to write to
    the pipe — and hand those bytes back to the caller, or None on timeout.

    The generic fork + deadline + pipe machinery underneath every isolation boundary in this
    file: a single request's child (`produce` calls `run_request` once), a batch item's
    grandchild (`produce` calls `run_request` against an already-primed `ns`), and the primed
    batch's own intermediate child (`produce` runs the whole batch and returns the assembled
    `{"results": [...]}` bytes). Bounds the child independently of the jail's wall time_limit,
    which can't bound individual calls in a long-lived --serve process. A timed-out child is
    killed; its (grand)children, if any, die with it.
    """
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:  # child: run untrusted work, write the response, never return
        os.close(r)
        try:
            data = produce()
        except Exception as e:
            resp = base_response()
            resp["error"] = exc_error("harness", e)
            data = json.dumps(resp, allow_nan=False).encode()
        try:
            os.write(w, data)
        finally:
            os.close(w)
            os._exit(0)
    # parent: read the child's output under a deadline, killing it on timeout
    os.close(w)
    chunks = []
    deadline = time.monotonic() + timeout
    timed_out = False
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            timed_out = True
            break
        ready, _, _ = select.select([r], [], [], remaining)
        if not ready:
            continue
        b = os.read(r, 65536)
        if not b:
            break
        chunks.append(b)
    if timed_out:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    os.close(r)
    os.waitpid(pid, 0)
    return None if timed_out else b"".join(chunks)


def _handle_in_child(req, timeout):
    """Fork a child to run one prepared request dict; return its response bytes, or None on
    timeout. Takes an already-parsed request dict (not a raw line) so both a single request and
    (via `run_request`'s `ns` argument) a primed batch item can share this machinery."""
    return _fork_bytes(lambda: json.dumps(run_request(req), allow_nan=False).encode(), timeout)


def _timeout_response(timeout):
    # Wall-time exceeded: as much a resource kill as MemoryError/RecursionError, so it gets the
    # same `stage="resource"` category (kind distinguishes the specific cause).
    resp = base_response()
    resp["error"] = make_error("resource", "timeout", f"request exceeded {timeout}s")
    return resp


def _no_output_response():
    # The child produced nothing at all — most likely SIGKILLed by an rlimit (CPU/mem) or
    # crashed outright. Either way this is a harness-side failure, never a semantic result, so
    # it stays on the `error` channel (never `exception`).
    resp = base_response()
    resp["error"] = make_error("harness", "no_output", "child produced no output")
    return resp


def _collect_result(out, timeout):
    """Turn a `_fork_bytes` outcome (bytes, empty bytes, or None on timeout) into a parsed
    response dict, for one child's output."""
    if out is None:
        return _timeout_response(timeout)
    if not out:
        return _no_output_response()
    try:
        return json.loads(out)
    except ValueError:
        # A child killed mid-write (rlimit SIGKILL) leaves truncated JSON. That must fail this
        # one item on the error channel, never crash the whole serve loop.
        resp = base_response()
        resp["error"] = make_error("harness", "partial_output", "child output was truncated")
        return resp


def _run_one(req, timeout):
    """Run one prepared request dict in a forked child; return the parsed response dict."""
    return _collect_result(_handle_in_child(req, timeout), timeout)


def _run_one_bytes(req, timeout):
    """Run one prepared request dict in a forked child; return the raw response bytes, so a
    single unbatched request's output never pays a parse/re-serialize round trip."""
    out = _handle_in_child(req, timeout)
    if out is None:
        return json.dumps(_timeout_response(timeout), allow_nan=False).encode()
    if not out:
        return json.dumps(_no_output_response(), allow_nan=False).encode()
    return out


def _run_batch_primed(base_req, batch, timeout):
    """Run inside the batch's intermediate child (already forked from the serve parent). Execs
    the module ONCE, then forks one grandchild per batch item from that primed state, so each
    item skips its own module re-exec — the cost that dominates for a module with real size.

    Each grandchild still gets its own fork (so its mutations to the shared `ns` die with it —
    fork's copy-on-write means siblings never see each other's writes) and its own per-item
    timeout, so a hanging item is killed without sinking the rest of the batch. Returns the
    assembled `{"results": [...]}` bytes; a module-load failure here is reported as the same
    `stage="setup"` error, once per item, that a per-case exec would have produced.
    """
    timing = bool(base_req.get("timing"))
    t0 = time.perf_counter_ns()
    try:
        ns = _load_module(base_req["source"])
    except Exception as e:
        err = exc_error("setup", e)
        results = []
        for _ in batch:
            resp = base_response()
            resp["error"] = err
            results.append(resp)
        return json.dumps({"results": results}, allow_nan=False).encode()

    module_exec_us = (time.perf_counter_ns() - t0) // 1000
    results = []
    for item in batch:
        item_req = dict(base_req)
        item_req["args"] = item.get("args", [])
        item_req["kwargs"] = item.get("kwargs", {})
        t_item = time.perf_counter_ns()
        out = _fork_bytes(
            lambda item_req=item_req: json.dumps(run_request(item_req, ns), allow_nan=False).encode(),
            timeout,
        )
        r = _collect_result(out, timeout)
        if timing:
            # Stamp what the child can't see: its own fork+pipe+scheduling wall, and the
            # batch's one-time module exec (amortized over the batch by the consumer).
            r.setdefault("timings", {})["item_wall_us"] = (time.perf_counter_ns() - t_item) // 1000
            r["timings"]["module_exec_once_us"] = module_exec_us
        results.append(r)
    return json.dumps({"results": results}, allow_nan=False).encode()


def serve():
    """Fork-server loop: newline-delimited requests.

    A request may carry a `batch` field instead of `args`/`kwargs`:
    `{"source":..., "fn":..., "class":..., "ctor_args":..., "batch": [{"args":[...],
    "kwargs":{...}}, ...]}`. A batch is run by an intermediate child (`_run_batch_primed`) that
    execs the module once and forks one grandchild per item from that primed state, so only the
    first item in the batch pays a module exec. The intermediate child enforces each item's
    normal per-item timeout itself; the serve parent supervises the intermediate child with a
    whole-batch deadline of `timeout * len(batch)` as a backstop against the intermediate child
    itself wedging — a timed-out item still only kills its own grandchild, never a sibling. The
    whole batch gets ONE newline-delimited response: `{"results": [<normal response>, ...]}`.
    Unbatched requests (validate, probes, one-shot mode) are untouched: one fork, exec in the
    child, byte-identical output to before.
    """
    timeout = float(os.environ.get("PYLENS_CALL_TIMEOUT", "10"))
    for raw in sys.stdin.buffer:
        line = raw.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception as e:
            resp = base_response()
            resp["error"] = make_error("bad_request", type(e).__name__, str(e))
            sys.stdout.buffer.write(json.dumps(resp, allow_nan=False).encode() + b"\n")
            sys.stdout.buffer.flush()
            continue

        batch = req.get("batch") if isinstance(req, dict) else None
        if batch is not None:
            base = {k: v for k, v in req.items() if k != "batch"}
            whole_timeout = timeout * max(len(batch), 1)
            raw_out = _fork_bytes(lambda: _run_batch_primed(base, batch, timeout), whole_timeout)
            if raw_out is None:
                results = [_timeout_response(timeout) for _ in batch]
                out = json.dumps({"results": results}, allow_nan=False).encode()
            elif not raw_out:
                results = [_no_output_response() for _ in batch]
                out = json.dumps({"results": results}, allow_nan=False).encode()
            else:
                out = raw_out
        else:
            out = _run_one_bytes(req, timeout)
        sys.stdout.buffer.write(out + b"\n")
        sys.stdout.buffer.flush()


if __name__ == "__main__":
    if "--serve" in sys.argv[1:]:
        serve()
    else:
        oneshot()
