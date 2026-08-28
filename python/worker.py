"""pylens execution harness — runs INSIDE the nsjail sandbox.

Two modes:

* **one-shot** (default): read one JSON request from stdin, execute it, write one JSON
  response to stdout, exit. One jailed process per call — maximum isolation.
* **--serve** (fork-server): read newline-delimited JSON requests from stdin and write one
  newline-delimited JSON response per request. Each request is executed in a *forked child*,
  so untrusted code cannot leak interpreter state (monkeypatched builtins, globals, imported
  modules) into the measurement of any other request. The parent never `exec`s untrusted
  source. This is what the persistent worker pool drives; it amortizes interpreter + import
  startup while preserving per-request isolation.

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


def run_request(req):
    """Execute one request in the current process. Returns a response dict.

    Free function: call `fn(*args)`. Method: instantiate `class(*ctor_args)`, snapshot its
    state, then call `receiver.fn(*args)` and snapshot again (self_pre / self_post).

    Load probe: when `fn` is null, only execute the module source (and, if `class` is given,
    instantiate the receiver) and report whether it loaded — no call. This is how the caller
    learns a module won't import, or a constructor can't be built, *once* instead of per case.
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

    # Load/ctor probe: exec the module, optionally build the receiver, report load status.
    if fn_name is None:
        try:
            ns = {}
            exec(compile(source, "<pylens>", "exec"), ns)
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
        ns = {}
        exec(compile(source, "<pylens>", "exec"), ns)
        args = [deserialize(a) for a in args_in]
        kwargs = {k: deserialize(v) for k, v in kwargs_in.items()}
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
    resp["args_pre"] = [serialize(a) for a in args]
    resp["kwargs_pre"] = {k: serialize(v) for k, v in kwargs.items()}

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
        resp["ok"] = True
        resp["return"] = serialize(ret)
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
        resp["args_post"] = [serialize(a) for a in args]
        resp["kwargs_post"] = {k: serialize(v) for k, v in kwargs.items()}
    except Exception as e:
        resp["error"] = exc_error("serialize", e)

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


def _handle_in_child(req, timeout):
    """Fork a child to run one prepared request dict; return its response bytes, or None on
    timeout.

    Isolates untrusted state (the child inherits imports but its mutations die with it) and
    bounds each request independently — necessary because the --serve process is long-lived,
    so the jail's wall time_limit cannot bound individual calls. Takes an already-parsed
    request dict (not a raw line) so both a single request and each item of a `batch` request
    can share this isolation/timeout machinery.
    """
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:  # child: run the untrusted request, write the response, never return
        os.close(r)
        try:
            resp = run_request(req)
        except Exception as e:
            resp = base_response()
            resp["error"] = exc_error("harness", e)
        try:
            os.write(w, json.dumps(resp, allow_nan=False).encode())
        finally:
            os.close(w)
            os._exit(0)
    # parent: read the child's response under a deadline, killing it on timeout
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


def _run_one(req, timeout):
    """Run one prepared request dict in a forked child; return the parsed response dict."""
    out = _handle_in_child(req, timeout)
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


def _run_one_bytes(req, timeout):
    """Run one prepared request dict in a forked child; return the raw response bytes, so a
    single unbatched request's output never pays a parse/re-serialize round trip."""
    out = _handle_in_child(req, timeout)
    if out is None:
        return json.dumps(_timeout_response(timeout), allow_nan=False).encode()
    if not out:
        return json.dumps(_no_output_response(), allow_nan=False).encode()
    return out


def serve():
    """Fork-server loop: newline-delimited requests, one forked child per item.

    A request may carry a `batch` field instead of `args`/`kwargs`:
    `{"source":..., "fn":..., "class":..., "ctor_args":..., "batch": [{"args":[...],
    "kwargs":{...}}, ...]}`. Each batch item is forked and timed exactly like a standalone
    request (sequentially, one child at a time), and the whole batch gets ONE
    newline-delimited response: `{"results": [<normal response>, ...]}`. A timed-out item
    kills only its own child; the remaining items in the batch still run. Unbatched requests
    (validate, probes, one-shot mode) are untouched and keep byte-identical output.
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
            results = []
            for item in batch:
                item_req = dict(base)
                item_req["args"] = item.get("args", [])
                item_req["kwargs"] = item.get("kwargs", {})
                results.append(_run_one(item_req, timeout))
            out = json.dumps({"results": results}, allow_nan=False).encode()
        else:
            out = _run_one_bytes(req, timeout)
        sys.stdout.buffer.write(out + b"\n")
        sys.stdout.buffer.flush()


if __name__ == "__main__":
    if "--serve" in sys.argv[1:]:
        serve()
    else:
        oneshot()
