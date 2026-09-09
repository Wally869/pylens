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

Line-level coverage (`lines`, `arcs`) comes from a plain `sys.settrace` line tracer scoped to the
module under test. A request can additionally carry `fine_targets` — same-line branch points
(ternaries, boolop short-circuits, single-line `if x: y`, comprehension guards) that line arcs
can't tell apart — which turns on opcode-level tracing (`f_trace_opcodes`) for the traced call and
resolves each one to a `fine_hits` entry by watching which bytecode offset a probed jump
instruction lands on next. See `_build_fine_plan`/`_make_tracer`. Empty `fine_targets` (the
common case: most functions have no same-line construct) costs nothing — opcode tracing only
turns on when the request asks for it.

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
import dis


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
        "fine_hits": [],
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
still falls outside every `except` below and crashes the (forked) child, surfacing as a
harness-side error — `no_output`, or the signal-specific classification (`resource`/`cpu_limit`,
`resource`/`killed`, or `harness`/`crash`) if the OS terminated the child by signal — the error
side, never `exception`. `SystemExit` is different: `sys.exit()` is
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


_COMPREHENSION_CODE_NAMES = ("<listcomp>", "<setcomp>", "<dictcomp>", "<genexpr>", "<lambda>")
_FINE_TEST_OPS = ("POP_JUMP_IF_FALSE", "POP_JUMP_IF_TRUE")
_FINE_CHAIN_OPS = ("JUMP_IF_FALSE_OR_POP", "JUMP_IF_TRUE_OR_POP")


def _nested_fine_codes(code):
    """`code` plus every nested code object CPython compiles as part of the SAME function body —
    comprehensions, generator expressions, lambdas — never a nested `def`'s own code object,
    matching `analyze::collect::branches`, which never descends into one either."""
    yield code
    for const in code.co_consts:
        if hasattr(const, "co_code") and const.co_name in _COMPREHENSION_CODE_NAMES:
            yield from _nested_fine_codes(const)


def _line_candidates(codes, line, fallthrough_of):
    """Every `_FINE_TEST_OPS` instruction compiled from `line`, across `codes`, in encounter
    order — also fills `fallthrough_of` for every instruction along the way."""
    out = []
    for code in codes:
        instrs = list(dis.get_instructions(code))
        for i, instr in enumerate(instrs):
            fallthrough_of[(id(code), instr.offset)] = instrs[i + 1].offset if i + 1 < len(instrs) else None
        current_line = None
        for instr in instrs:
            if instr.starts_line is not None:
                current_line = instr.starts_line
            if current_line == line and instr.opname in _FINE_TEST_OPS:
                out.append((code, instr))
    return out


def _resolve_landing_chain(codes, line, fallthrough_of):
    """The landing-offset scheme (see `analyze::collect::branches`'s module doc): resolves every
    `_FINE_TEST_OPS` instruction on `line` to a `(true, false)` landing-offset pair PROVEN by the
    bytecode itself, or gives up entirely (returns `None`) if that proof fails — never a guess.
    Returns `(entries, canonical)` — `entries` maps `(id(code), offset) -> per-instruction
    resolution info` (consumed by [`_resolve_fine_probe`]); `canonical` is the `(true_offset,
    false_offset)` pair (needed by callers that also want the associated test-position `BoolOp`'s
    own short-circuit signal — same instructions, different outcome labels, no separate scan).

    Does NOT try to split same-line `while`'s loop-rotation-duplicated test into "pre-loop" vs
    "retest" instruction groups — a same-line `while`'s BODY shares the exact physical line as its
    test, so there is no line-table boundary between them, and no reliable OPCODE-based signal
    either: a store-free body (`while xs: xs.pop()`, `while x: print(x.next())`) compiles to
    `LOAD_METHOD`/`CALL_METHOD`/`POP_TOP`, none of which differ in kind from a test operand's own
    value computation. A prior version of this scheme tried exactly that grouping and, for
    `while xs: xs.pop()`, silently INVERTED `enter`/`skip` (derived the canonical pair from the
    wrong, polarity-flipped retest run) — the bug this scheme now avoids by construction, not just
    by convention:

    1. A retest's own loop-continuation jump is the ONLY instruction on the line that can ever
       jump BACKWARD (`argval < offset`) — a pre-loop test's jump always targets code that comes
       AFTER it (skip the body, or an intermediate operand still ahead). This is unconditionally
       true of CPython's codegen, not a heuristic: nothing exists before the very first instruction
       of a straight-line test for an EARLIER instruction to jump back into. So: at most one
       backward-jumping instruction is trusted (more than one is an unrecognized shape — refuse);
       when found, its OWN jump target IS the true-landing (jumping backward always means
       "continue the loop" = enter), read directly off that one instruction — no run grouping, no
       "which instructions precede the body" reasoning needed at all.
    2. Every OTHER (forward-jumping) instruction on the line must target the SAME single offset —
       that shared offset is the false-landing. This holds for BOTH the pre-loop test's forward
       jump and a retest's own EARLIER, still-forward-jumping members (e.g. `while a and b:`'s
       retest also forward-jumps if `a` alone is false at the retest) — grouping never mattered to
       this per-instruction check.
    3. With NO backward-jumping instruction at all (every ordinary same-line construct — ternary,
       inline-if, comprehension-if, or a while whose test CPython didn't rotate), the true/false
       landing pair is instead the ORIGINAL derivation: the LAST instruction (offset order)'s own
       fallthrough is the true-landing, its jump target the false-landing.
    4. Every instruction's own jump target must land on the canonical pair, whichever way it was
       derived (step 1+2 or step 3) — this alone still rejects a nested ternary's duplicated
       instructions, `(a and b) or (c and d)`'s intermediate jump into the right OR's own test, and
       a chained comparison's `a < b < c` `POP_TOP`-cleanup shape (an intermediate offset, not
       either landing) — none of those flatten regardless of which derivation path found the pair.
    5. A given instruction's FALLTHROUGH (the non-jump case) is only ever trusted as evidence when
       it ALSO lands on the canonical pair — never inferred from "is this the run's last
       instruction" (there are no runs). A non-last operand's fallthrough naturally lands on
       another instruction still evaluating the same test (an intermediate offset) and is
       correctly never treated as meaningful.
    """
    candidates = _line_candidates(codes, line, fallthrough_of)
    if not candidates:
        return None

    backward = [(code, instr) for code, instr in candidates if instr.argval < instr.offset]
    forward = [(code, instr) for code, instr in candidates if instr.argval >= instr.offset]
    if len(backward) > 1:
        return None

    if backward:
        true_offset = backward[0][1].argval
        if not forward:
            return None
        false_offset = forward[0][1].argval
        if any(instr.argval != false_offset for _code, instr in forward):
            return None
    else:
        last_code, last_instr = candidates[-1]
        fallthrough = fallthrough_of.get((id(last_code), last_instr.offset))
        if fallthrough is None:
            return None
        true_offset, false_offset = fallthrough, last_instr.argval
    if true_offset == false_offset:
        return None
    canonical = (true_offset, false_offset)

    entries = {}
    for code, instr in candidates:
        if instr.argval not in canonical:
            return None
        own_fallthrough = fallthrough_of.get((id(code), instr.offset))
        fallthrough_meaningful = own_fallthrough in canonical
        entries[(id(code), instr.offset)] = {
            "line": line,
            "jump_target": instr.argval,
            "outcome_if_jump": "true" if instr.argval == true_offset else "false",
            "fallthrough_meaningful": fallthrough_meaningful,
            "outcome_if_fallthrough": ("true" if own_fallthrough == true_offset else "false")
            if fallthrough_meaningful
            else None,
        }
    return entries, canonical


def _build_fine_plan(top_code, fine_targets):
    """Resolve every requested same-line branch point (`{"line", "kind", "ordinal", "compound"}`,
    from `model::branch::FineTarget`) to the bytecode probe that proves which outcome fires — see
    `OutcomeEvidence::FineGrained`. Returns `{(id(code), offset): entry}` where `entry` is what
    `_resolve_fine_probe` needs to classify the outcome once the *next* opcode executed in that
    frame is known.

    `compound: false` — the original single-instruction shapes: `ternary`/`inline_if`/
    `comprehension_if` compile the test to a single `POP_JUMP_IF_FALSE`/`POP_JUMP_IF_TRUE`
    (landing on its jump target is the negative outcome, falling through the positive one, or the
    reverse for `_IF_TRUE`); `bool_op` in VALUE position compiles to a *chain* of
    `JUMP_IF_*_OR_POP` instructions sharing one target — any instruction jumping means
    `short_circuit`, falling through the chain's LAST instruction means `full_evaluation`.
    Multiple same-`compound: false` points of the same kind can share a physical line (rare — two
    ternaries via a tuple literal); `ordinal` (assigned by AST left-to-right order in
    `collect_branches`) then picks the `ordinal`-th matching instruction/chain in bytecode offset
    order — the same order for ordinary code.

    `compound: true` — the landing-offset scheme (see [`_resolve_landing_chain`] and
    `analyze::collect::branches`'s module doc): `ternary`/`inline_if`/`comprehension_if`/`while`
    resolved via the construct's own (true, false) landing pair; `bool_op` in TEST position gets
    its `short_circuit`/`full_evaluation` from the SAME resolved chain. Only ever requested for a
    line hosting exactly one test position, so there is at most one `compound: true` target per
    `(line, kind)` — no ordinal disambiguation needed, and the landing-chain resolution (or its
    failure) is computed once per line and shared between a Test-category target and its
    associated `bool_op` target.

    A target with no bytecode counterpart, or whose shape the worker's own dynamic verification
    can't prove safe (see [`_resolve_landing_chain`]), is silently left unresolved rather than
    guessed: its outcome then just never appears in `fine_hits`, and the outcome stays whatever the
    aggregate evidence already says (`uncovered`, never falsely `covered`).
    """
    wanted = {}
    for t in fine_targets:
        wanted.setdefault(t["line"], {}).setdefault(t["kind"], []).append((t["ordinal"], t["compound"]))

    codes = list(_nested_fine_codes(top_code))
    test_groups = {}
    chain_groups = {}
    fallthrough_of = {}
    for code in codes:
        instrs = list(dis.get_instructions(code))
        current_line = None
        for i, instr in enumerate(instrs):
            # `starts_line` is only set on the FIRST instruction of a source line — every later
            # instruction compiled from that same line (e.g. the `POP_JUMP_IF_FALSE` after the
            # `LOAD_FAST` that pushed its operand) carries `None` and inherits the line of the
            # most recent instruction that did set it.
            if instr.starts_line is not None:
                current_line = instr.starts_line
            next_offset = instrs[i + 1].offset if i + 1 < len(instrs) else None
            fallthrough_of[(id(code), instr.offset)] = next_offset
            if current_line is None:
                continue
            if instr.opname in _FINE_TEST_OPS:
                test_groups.setdefault(current_line, []).append((code, instr))
            elif instr.opname in _FINE_CHAIN_OPS:
                chain_groups.setdefault(current_line, {}).setdefault(
                    instr.argval, []
                ).append((code, instr))

    plan = {}
    landing_cache = {}
    for line, by_kind in wanted.items():
        single_targets = (
            by_kind.get("ternary", [])
            + by_kind.get("inline_if", [])
            + by_kind.get("comprehension_if", [])
            + by_kind.get("while", [])
        )
        simple_ordinals = [ordinal for ordinal, compound in single_targets if not compound]
        wants_compound_test = any(compound for _ordinal, compound in single_targets)
        wants_compound_boolop = any(compound for _ordinal, compound in by_kind.get("bool_op", []))

        group = sorted(test_groups.get(line, []), key=lambda ci: ci[1].offset)
        for ordinal in simple_ordinals:
            if ordinal >= len(group):
                continue
            code, instr = group[ordinal]
            fallthrough = fallthrough_of.get((id(code), instr.offset))
            if fallthrough is None:
                continue
            plan[(id(code), instr.offset)] = {
                "line": line,
                "ordinal": ordinal,
                "jump_target": instr.argval,
                "true_on_fallthrough": instr.opname == "POP_JUMP_IF_FALSE",
            }

        if wants_compound_test or wants_compound_boolop:
            if line not in landing_cache:
                landing_cache[line] = _resolve_landing_chain(codes, line, fallthrough_of)
            resolved = landing_cache[line]
            if resolved is not None:
                entries, _canonical = resolved
                # `_resolve_landing_chain` labels generically ("true"/"false" — the true-landing
                # is always the construct's ENTER/true side); `while`'s own outcome names are
                # "enter"/"skip", not "true"/"false", so its request must remap the label here —
                # everything else (ternary/inline_if/comprehension_if) already agrees.
                while_compound = any(compound for _ordinal, compound in by_kind.get("while", []))
                relabel = {"true": "enter", "false": "skip"} if while_compound else {}
                for (code_id, offset), entry in entries.items():
                    remapped = dict(entry)
                    remapped["outcome_if_jump"] = relabel.get(entry["outcome_if_jump"], entry["outcome_if_jump"])
                    if entry["outcome_if_fallthrough"] is not None:
                        remapped["outcome_if_fallthrough"] = relabel.get(
                            entry["outcome_if_fallthrough"], entry["outcome_if_fallthrough"]
                        )
                    remapped["ordinal"] = 0
                    plan[(code_id, offset)] = remapped
                    if wants_compound_boolop:
                        plan[(code_id, offset)]["boolop_ordinal"] = 0

        chains = sorted(
            chain_groups.get(line, {}).values(),
            key=lambda members: min(ci[1].offset for ci in members),
        )
        for ordinal, compound in by_kind.get("bool_op", []):
            if compound:
                continue  # handled by the landing-chain branch above, shared with the Test target.
            if ordinal >= len(chains):
                continue
            chain = sorted(chains[ordinal], key=lambda ci: ci[1].offset)
            last_offset = chain[-1][1].offset
            for code, instr in chain:
                fallthrough = fallthrough_of.get((id(code), instr.offset))
                if fallthrough is None:
                    continue
                plan[(id(code), instr.offset)] = {
                    "line": line,
                    "ordinal": ordinal,
                    "jump_target": instr.argval,
                    "boolop_last": instr.offset == last_offset,
                }
    return plan


def _resolve_fine_probe(entry, next_offset, fine_hits):
    """Classify one probe's outcome from `next_offset` — the offset the SAME frame executed right
    after the probed instruction (adjacent by construction: a jump/fallthrough always lands on
    the very next instruction actually run, so no other frame's event can appear in between)."""
    jumped = next_offset == entry["jump_target"]
    if "true_on_fallthrough" in entry:
        true_hit = (not jumped) if entry["true_on_fallthrough"] else jumped
        fine_hits.add((entry["line"], entry["ordinal"], "true" if true_hit else "false"))
        return
    if "boolop_last" in entry:
        if jumped:
            fine_hits.add((entry["line"], entry["ordinal"], "short_circuit"))
        elif entry["boolop_last"]:
            fine_hits.add((entry["line"], entry["ordinal"], "full_evaluation"))
        return
    # The landing-offset scheme (`compound: true`) — see `_resolve_landing_chain`.
    outcome = entry["outcome_if_jump"] if jumped else (entry["outcome_if_fallthrough"] if entry["fallthrough_meaningful"] else None)
    if outcome is not None:
        fine_hits.add((entry["line"], entry["ordinal"], outcome))
    boolop_ordinal = entry.get("boolop_ordinal")
    if boolop_ordinal is not None:
        if entry["fallthrough_meaningful"]:
            fine_hits.add((entry["line"], boolop_ordinal, "full_evaluation"))
        elif jumped:
            fine_hits.add((entry["line"], boolop_ordinal, "short_circuit"))


def _make_tracer(executed, arcs, fine_plan, fine_hits):
    """A `sys.settrace` global trace function that records every line reached in the compiled
    module under test (`co_filename == "<pylens>"`), skipping frames from anywhere else (stdlib,
    the harness itself). Python 3.10 predates `sys.monitoring` (3.12+), so `settrace` is the only
    line-level hook available.

    Also records every `(prev_line, cur_line)` arc within a frame — the transition line coverage
    can't see (e.g. an else-less `if`'s false path never has its own line, but the arc from the
    test line straight to whatever follows it does). `prev_line` is tracked per-frame (keyed by
    `id(frame)`, cleared on `return`) so a call into another function under trace never creates a
    spurious arc from the caller's line into the callee's.

    `fine_plan` (built by `_build_fine_plan`, empty unless the request carried `fine_targets`):
    when non-empty, every traced frame also gets `f_trace_opcodes = True` for its WHOLE lifetime
    — opcode-level tracing is gated per REQUEST (a function with no same-line branch constructs
    never pays for it), not per line, since a probed instruction's resolution must be the frame's
    very next opcode event with no gap. On each `opcode` event: resolve any probe left pending
    from the previous event via `_resolve_fine_probe`, then arm a new pending entry if the
    instruction about to run is itself a probe (`fine_plan` keyed by `(id(frame.f_code),
    offset)`, so nested code objects — comprehensions, lambdas — never collide with the outer
    function's offsets).
    """
    last_line = {}
    pending = {}

    def local_trace(frame, event, arg):
        if event == "line":
            cur = frame.f_lineno
            executed.add(cur)
            key = id(frame)
            prev = last_line.get(key)
            if prev is not None:
                arcs.add((prev, cur))
            last_line[key] = cur
        elif event == "opcode":
            fid = id(frame)
            entry = pending.pop(fid, None)
            if entry is not None:
                _resolve_fine_probe(entry, frame.f_lasti, fine_hits)
            probe = fine_plan.get((id(frame.f_code), frame.f_lasti))
            if probe is not None:
                pending[fid] = probe
        elif event == "return":
            last_line.pop(id(frame), None)
            pending.pop(id(frame), None)
        return local_trace

    def global_trace(frame, event, arg):
        # Only frames of the module under test get a local tracer at all. A foreign frame
        # (stdlib, third-party) returns None, so its per-line events never fire — the win is
        # large when the function under test leans on pure-Python stdlib code. A foreign frame
        # calling back into "<pylens>" code still traces: every new call re-enters here.
        if frame.f_code.co_filename != "<pylens>":
            return None
        if fine_plan:
            frame.f_trace_opcodes = True
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
        fine_targets_in = req.get("fine_targets", [])
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

    # The fine-grained (opcode-level) tracing plan — built once per call, empty (and so free)
    # unless the request asked for same-line branch points. A failure here is a harness bug, not
    # the function's behavior, so it's reported like any other setup failure rather than let
    # `_build_fine_plan` raising land inside the traced-call `except` below and get mistaken for
    # the function itself raising.
    try:
        fine_plan = _build_fine_plan(fn.__code__, fine_targets_in) if fine_targets_in else {}
    except Exception as e:
        resp["error"] = exc_error("harness", e)
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
    fine_hits = set()
    try:
        with contextlib.redirect_stdout(out_buf), contextlib.redirect_stderr(err_buf):
            # Traced region: the call under test and (if it returns a generator) draining it —
            # not the module load or the receiver construction above.
            sys.settrace(_make_tracer(executed_lines, executed_arcs, fine_plan, fine_hits))
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
    resp["fine_hits"] = [
        {"line": line, "ordinal": ordinal, "outcome": outcome}
        for line, ordinal, outcome in sorted(fine_hits)
    ]

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
    if isinstance(req, dict) and "timeout" in req:
        timeout = _parse_timeout(req.get("timeout"), _default_call_timeout())
        sys.stdout.write(json.dumps(_run_one(req, timeout), allow_nan=False))
        return
    sys.stdout.write(json.dumps(run_request(req), allow_nan=False))


def _fork_bytes(produce, timeout):
    """Fork a child that runs `produce()` — a zero-arg callable returning the bytes to write to
    the pipe — and hand back `(bytes, wait_status)`, or None on timeout.

    The generic fork + deadline + pipe machinery underneath every isolation boundary in this
    file: a single request's child (`produce` calls `run_request` once), a batch item's
    grandchild (`produce` calls `run_request` against an already-primed `ns`), and the primed
    batch's own intermediate child (`produce` runs the whole batch and returns the assembled
    `{"results": [...]}` bytes). Bounds the child independently of the jail's wall time_limit,
    which can't bound individual calls in a long-lived --serve process. A timed-out child is
    killed; its (grand)children, if any, die with it. `wait_status` is the raw `os.waitpid`
    status, which callers with empty or truncated output use to tell an rlimit signal kill
    (SIGXCPU/SIGKILL) apart from a normal exit that just wrote nothing.
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
    _, status = os.waitpid(pid, 0)
    return None if timed_out else (b"".join(chunks), status)


def _handle_in_child(req, timeout):
    """Fork a child to run one prepared request dict; return `(bytes, wait_status)`, or None on
    timeout. Takes an already-parsed request dict (not a raw line) so both a single request and
    (via `run_request`'s `ns` argument) a primed batch item can share this machinery."""
    return _fork_bytes(lambda: json.dumps(run_request(req), allow_nan=False).encode(), timeout)


def _default_call_timeout():
    return float(os.environ.get("PYLENS_CALL_TIMEOUT", "10"))


def _parse_timeout(value, default):
    """Parse a wire timeout field (seconds, float) into a positive float, falling back to
    `default` for anything missing, unparseable, or non-positive rather than crashing the
    serve loop over one malformed request."""
    if value is None:
        return default
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        return default
    return parsed if parsed > 0 else default


def _timeout_response(timeout):
    # Wall-time exceeded: as much a resource kill as MemoryError/RecursionError, so it gets the
    # same `stage="resource"` category (kind distinguishes the specific cause). The item DID
    # start and run, so this is a real, kept observation — it still carries whatever `lines` it
    # reached before the kill. Unlike `deadline_skipped` (below), a consumer must never drop it.
    resp = base_response()
    resp["error"] = make_error("resource", "timeout", f"request exceeded {timeout}s")
    return resp


def _signal_error(status):
    """Classify a child's `os.waitpid` status as a signal-kill error, or None if it exited
    normally (in which case the caller falls back to its own no-output/truncated-output
    classification). SIGXCPU is the nsjail CPU rlimit firing; SIGKILL is the OOM killer or an
    explicit kill (e.g. our own timeout path). Both are a sandbox resource limit, not the
    function's behavior. Any other signal (SIGSEGV, SIGABRT, ...) is a harness-side crash."""
    if status is None or not os.WIFSIGNALED(status):
        return None
    sig = os.WTERMSIG(status)
    name = signal.Signals(sig).name
    if sig == signal.SIGXCPU:
        return make_error("resource", "cpu_limit", f"child killed by {name}")
    if sig == signal.SIGKILL:
        return make_error("resource", "killed", f"child killed by {name}")
    return make_error("harness", "crash", f"child died with {name}")


def _no_output_response(status=None):
    # The child produced nothing at all. `status` (from `os.waitpid`) tells a signal kill —
    # SIGXCPU/SIGKILL from an rlimit, or any other signal — apart from a child that simply
    # exited without writing; the former gets the specific resource/crash classification, the
    # latter the generic harness one. Either way this stays on the `error` channel, never
    # `exception`.
    resp = base_response()
    err = _signal_error(status)
    resp["error"] = err if err is not None else make_error("harness", "no_output", "child produced no output")
    return resp


def _collect_result(result, timeout):
    """Turn a `_fork_bytes` outcome (`(bytes, wait_status)`, or None on timeout) into a parsed
    response dict, for one child's output."""
    if result is None:
        return _timeout_response(timeout)
    out, status = result
    if not out:
        return _no_output_response(status)
    try:
        return json.loads(out)
    except ValueError:
        # Truncated JSON: a signal-killed child (rlimit SIGKILL, or a crash) gets that specific
        # classification; a normally-exited child that still wrote a partial line keeps the
        # generic harness classification. Either way this must fail this one item on the error
        # channel, never crash the whole serve loop.
        err = _signal_error(status)
        if err is None:
            err = make_error("harness", "partial_output", "child output was truncated")
        resp = base_response()
        resp["error"] = err
        return resp


def _run_one(req, timeout):
    """Run one prepared request dict in a forked child; return the parsed response dict."""
    return _collect_result(_handle_in_child(req, timeout), timeout)


def _run_one_bytes(req, timeout):
    """Run one prepared request dict in a forked child; return the raw response bytes, so a
    single unbatched request's output never pays a parse/re-serialize round trip."""
    result = _handle_in_child(req, timeout)
    if result is None:
        return json.dumps(_timeout_response(timeout), allow_nan=False).encode()
    out, status = result
    if not out:
        return json.dumps(_no_output_response(status), allow_nan=False).encode()
    return out


def _deadline_skipped_response():
    # The batch's soft deadline passed before this item got a chance to start — distinct from
    # every other resource kind (timeout, cpu_limit, killed, MemoryError, RecursionError), all of
    # which mean the item started and was then stopped. This one means it never ran at all.
    resp = base_response()
    resp["error"] = make_error("resource", "deadline_skipped", "batch deadline reached before this item started")
    return resp


def _run_batch_primed(base_req, batch, timeout, batch_timeout):
    """Run inside the batch's intermediate child (already forked from the serve parent). Execs
    the module ONCE, then forks one grandchild per batch item from that primed state, so each
    item skips its own module re-exec — the cost that dominates for a module with real size.

    Each grandchild still gets its own fork (so its mutations to the shared `ns` die with it —
    fork's copy-on-write means siblings never see each other's writes) and its own per-item
    timeout, so a hanging item is killed without sinking the rest of the batch. `batch_timeout` is
    a SOFT whole-batch deadline computed from the moment this function starts (before the module
    is even exec'd, so a slow module exec is charged against the deadline exactly like the serve
    parent's hard kill timer, which starts at fork — the two must agree on when the clock started,
    or the hard kill can fire while the soft path still has budget left): before starting each
    item, if the deadline has passed, that item (and every item after it) is never started — it
    gets a `deadline_skipped` response instead, never a fresh attempt. This is the mechanism that
    actually enforces the whole-batch limit; the serve parent's own hard kill (see `serve`) is
    only a backstop against this function itself wedging, so completed items are never thrown away
    by that backstop firing. Returns the assembled `{"results": [...]}` bytes, always one entry
    per batch item, in order; a module-load failure here is reported as the same `stage="setup"`
    error, once per item, that a per-case exec would have produced.
    """
    deadline = time.monotonic() + batch_timeout
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
        if time.monotonic() >= deadline:
            results.append(_deadline_skipped_response())
            continue
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
    normal per-item timeout itself, and enforces the whole-batch limit ITSELF as a SOFT deadline,
    computed before the module is even exec'd — once passed, it stops starting new items and
    reports the rest `deadline_skipped`, never losing a result that already completed. That
    whole-batch limit defaults to `timeout * len(batch)`. The serve parent only supervises the
    intermediate child with a HARD kill, set above the soft deadline with slack for one more
    in-flight item plus response assembly (`soft + timeout + 2` seconds), so it fires only if the
    intermediate child itself wedges — the soft path inside it is what normally enforces the
    limit, and only a wedge ever loses a whole batch's results. A request may
    override both limits: `timeout` (per item) and `batch_timeout` (the soft whole-batch
    deadline); either falls back to its default when absent, unparseable, or non-positive. The
    whole batch gets ONE newline-delimited response: `{"results": [<normal response>, ...]}`,
    always one entry per batch item. Unbatched requests (validate, probes, one-shot mode) are
    untouched: one fork, exec in the child, byte-identical output to before.
    """
    default_timeout = _default_call_timeout()
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
            timeout = _parse_timeout(req.get("timeout"), default_timeout)
            soft_batch_timeout = _parse_timeout(req.get("batch_timeout"), timeout * max(len(batch), 1))
            # Slack beyond one more per-item timeout: an item that starts a moment before the
            # soft deadline may run the full `timeout` before the soft path even notices, and the
            # child still has to assemble and write the JSON response after that. Without this
            # margin the hard kill can fire on a batch that was about to return successfully.
            hard_batch_timeout = soft_batch_timeout + timeout + 2
            batch_result = _fork_bytes(
                lambda: _run_batch_primed(base, batch, timeout, soft_batch_timeout),
                hard_batch_timeout,
            )
            if batch_result is None:
                results = [_timeout_response(timeout) for _ in batch]
                out = json.dumps({"results": results}, allow_nan=False).encode()
            elif not batch_result[0]:
                results = [_no_output_response(batch_result[1]) for _ in batch]
                out = json.dumps({"results": results}, allow_nan=False).encode()
            else:
                out = batch_result[0]
        else:
            timeout = _parse_timeout(req.get("timeout") if isinstance(req, dict) else None, default_timeout)
            out = _run_one_bytes(req, timeout)
        sys.stdout.buffer.write(out + b"\n")
        sys.stdout.buffer.flush()


if __name__ == "__main__":
    if "--serve" in sys.argv[1:]:
        serve()
    else:
        oneshot()
