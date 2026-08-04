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
        return v
    if isinstance(v, list):
        return [deserialize(x) for x in v]
    if isinstance(v, dict):
        return {k: deserialize(val) for k, val in v.items()}
    return v


def serialize(v):
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
        "exception": None,
        "return_aliases_arg": None,
        "error": None,
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


def _clip(text):
    return text if len(text) <= 4096 else text[:4096] + "...<truncated>"


def _state(obj):
    """The receiver's attribute dict, or None if it has none."""
    try:
        return vars(obj)
    except TypeError:
        return None


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

    # Capture the function's stdout/stderr on SEPARATE buffers so they can't corrupt the JSON
    # protocol channel and so each is recorded as the channel it actually is.
    out_buf = io.StringIO()
    err_buf = io.StringIO()
    try:
        with contextlib.redirect_stdout(out_buf), contextlib.redirect_stderr(err_buf):
            ret = fn(*args)
            if hasattr(ret, "__next__"):  # drain generators/iterators to realize effects
                collected = []
                for i, x in enumerate(ret):
                    if i >= 1000:
                        collected.append("__truncated__")
                        break
                    collected.append(x)
                ret = collected
        resp["ok"] = True
        resp["return"] = serialize(ret)
        alias = None
        if isinstance(ret, (list, dict, set)):
            for i, a in enumerate(args):
                if ret is a:
                    alias = i
                    break
        resp["return_aliases_arg"] = alias
    except Exception as e:
        resp["ok"] = False
        resp["exception"] = {"type": type(e).__name__, "message": str(e)}

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
    except Exception as e:
        resp["error"] = exc_error("serialize", e)

    return resp


def oneshot():
    try:
        req = json.loads(sys.stdin.read())
    except Exception as e:
        resp = base_response()
        resp["error"] = make_error("bad_request", type(e).__name__, str(e))
        sys.stdout.write(json.dumps(resp))
        return
    sys.stdout.write(json.dumps(run_request(req)))


def _handle_in_child(line, timeout):
    """Fork a child to run one request; return its response bytes, or None on timeout.

    Isolates untrusted state (the child inherits imports but its mutations die with it) and
    bounds each request independently — necessary because the --serve process is long-lived,
    so the jail's wall time_limit cannot bound individual calls.
    """
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:  # child: run the untrusted request, write the response, never return
        os.close(r)
        try:
            resp = run_request(json.loads(line))
        except Exception as e:
            resp = base_response()
            resp["error"] = exc_error("harness", e)
        try:
            os.write(w, json.dumps(resp).encode())
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


def serve():
    """Fork-server loop: one forked child per newline-delimited request."""
    timeout = float(os.environ.get("PYLENS_CALL_TIMEOUT", "10"))
    for raw in sys.stdin.buffer:
        line = raw.strip()
        if not line:
            continue
        out = _handle_in_child(line, timeout)
        if out is None:
            resp = base_response()
            resp["error"] = make_error("timeout", "timeout", f"request exceeded {timeout}s")
            out = json.dumps(resp).encode()
        elif not out:
            resp = base_response()
            resp["error"] = make_error("harness", "no_output", "child produced no output")
            out = json.dumps(resp).encode()
        sys.stdout.buffer.write(out + b"\n")
        sys.stdout.buffer.flush()


if __name__ == "__main__":
    if "--serve" in sys.argv[1:]:
        serve()
    else:
        oneshot()
