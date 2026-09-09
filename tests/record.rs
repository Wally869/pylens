//! Record tests: static signature + observed cases, executed in the jail. Skips (does not
//! fall back to unsandboxed) when the sandbox isn't provisioned.

use pylens::exec::{CallResult, Sandbox, probe};
use pylens::generate::ValueDomain;
use pylens::model::ReturnKind;
use pylens::record::{
    CaseSource, DepStatus, RecordFlags, ReplayMap, parse_project_replay, parse_replay, record_file,
    record_with_signatures,
};
use std::time::Duration;
use pylens::{analyze_source, imports_of};
use serde_json::{Value, json};

fn ready(test: &str) -> bool {
    match probe() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("SKIP {test}: {e}");
            false
        }
    }
}

#[test]
fn method_record_captures_self_mutation() {
    if !ready("method_record_captures_self_mutation") {
        return;
    }
    let src = include_str!("../examples/inventory.py");
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let add = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "add")
        .expect("add record");

    assert_eq!(add.signature.owner.as_deref(), Some("Inventory"));
    assert!(
        add.signature
            .raises
            .explicit
            .iter()
            .any(|e| e == "ValueError")
    );
    // At least one generated case must succeed and mutate the receiver.
    let mutated_self = add.cases.iter().any(|c| {
        c.outcome == "returned" && c.mutations.iter().any(|m| m.target == "self")
    });
    assert!(mutated_self, "expected a successful add() mutating self");
}

#[test]
fn function_record_has_union_returns_and_cases() {
    if !ready("function_record_has_union_returns_and_cases") {
        return;
    }
    let src = include_str!("../examples/normalize.py");
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let classify = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "classify")
        .expect("classify record");

    // Multi-path return union includes a string branch and an int branch.
    assert!(classify.signature.returns.contains(&ReturnKind::Str));
    assert!(classify.signature.returns.contains(&ReturnKind::Int));
    assert!(!classify.cases.is_empty());
}

#[test]
fn reports_unresolved_dependency() {
    if !ready("reports_unresolved_dependency") {
        return;
    }
    let src = "import definitely_not_a_real_module_xyz as z\ndef f(x):\n    return z.go(x)\n";
    let rec = record_file(src, 2, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let dep = rec
        .dependencies
        .iter()
        .find(|d| d.import.module.package == "definitely_not_a_real_module_xyz")
        .expect("the fake dependency");
    assert!(
        matches!(dep.status, DepStatus::Unresolved),
        "fake module should not resolve"
    );
    let err = dep.error.as_ref().expect("a structured error");
    assert_eq!(
        err.kind, "ModuleNotFoundError",
        "error kind should be the exception type: {err:?}"
    );
    assert_eq!(
        err.module.as_deref(),
        Some("definitely_not_a_real_module_xyz"),
        "error should name the missing module"
    );
}

#[test]
fn unresolved_module_import_hoists_to_uncallable() {
    if !ready("unresolved_module_import_hoists_to_uncallable") {
        return;
    }
    // A module-scope import that can't load stops the whole file from loading, so the function
    // is marked uncallable ONCE — not with N identical per-case setup errors.
    let src = "import definitely_not_a_real_module_xyz as z\ndef f(x):\n    return z.go(x)\n";
    let rec = record_file(src, 3, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    let unc = f.uncallable.as_ref().expect("f should be uncallable");
    assert_eq!(unc.reason, "module_not_loadable");
    assert_eq!(unc.error.kind, "ModuleNotFoundError");
    assert_eq!(unc.error.module.as_deref(), Some("definitely_not_a_real_module_xyz"));
    assert!(f.cases.is_empty(), "no per-case spam when the module can't load");
}

#[test]
fn recursion_error_is_reported_as_resource_kill_not_raised() {
    if !ready("recursion_error_is_reported_as_resource_kill_not_raised") {
        return;
    }
    // Infinite recursion hits Python's recursion limit and raises RecursionError — a resource
    // kill, an artifact of the sandbox, NOT part of the function's semantics. It must never
    // surface as outcome "raised".
    // `n - 1` votes the param shape to Int (see `analyze/passes/effects.rs`), so every
    // generated case is a plain integer and recursion depth is what exhausts the limit, not a
    // spurious `TypeError` from a mismatched shape guess.
    let src = "def f(n):\n    return f(n - 1)\n";
    let rec = record_file(src, 3, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    for c in &f.cases {
        assert_ne!(
            c.outcome, "raised",
            "recursion exhaustion must not be reported as a semantic raise: {:?}",
            c.raises
        );
        assert_eq!(c.outcome, "error", "expected a resource-kill error outcome");
        let err = c.error.as_ref().expect("structured error for resource kill");
        assert!(
            err.is_resource(),
            "expected stage == \"resource\", got {:?}",
            err.stage
        );
        assert_eq!(err.kind, "RecursionError");
    }
}

#[test]
fn signal_killed_child_is_reported_as_resource_error() {
    if !ready("signal_killed_child_is_reported_as_resource_error") {
        return;
    }
    // The jail's own 10 s CPU rlimit races the harness's 10 s wall timeout, so a plain busy loop
    // is not a reliable SIGXCPU. The function lowers its own CPU rlimit (a process may always
    // lower one) to a 1 s soft / 3 s hard pair: the soft limit sends SIGXCPU, whose default
    // action terminates the child well before either 10 s bound.
    let src = "def burn(n):\n    import resource\n    resource.setrlimit(resource.RLIMIT_CPU, (1, 3))\n    total = 0\n    i = 0\n    while i < n:\n        total = (total + i) % 1000000007\n        i += 1\n    return total\n";
    let mut replay = ReplayMap::new();
    replay.insert("burn".to_string(), vec![vec![json!(100_000_000_000i64)]]);
    let rec = record_file(src, 0, &replay, RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "burn")
        .expect("burn record");
    let replayed: Vec<_> = f.cases.iter().filter(|c| c.source == CaseSource::Replay).collect();
    assert_eq!(replayed.len(), 1, "expected exactly the replayed case");
    let c = replayed[0];
    assert_eq!(c.outcome, "error", "expected an error outcome, got {:?}", c.outcome);
    let err = c.error.as_ref().expect("structured error for a signal-killed child");
    assert!(
        err.is_resource(),
        "expected stage == \"resource\", got {:?}",
        err.stage
    );
    assert_eq!(err.kind, "cpu_limit", "expected a SIGXCPU classification, got {:?}", err.kind);
}

#[test]
fn semantic_raise_is_unaffected_by_resource_kill_handling() {
    if !ready("semantic_raise_is_unaffected_by_resource_kill_handling") {
        return;
    }
    // A genuine `raise` inside the function is unambiguously semantic and must still surface
    // as outcome "raised" with the exception type in `raises`.
    let src = "def g():\n    raise ValueError('x')\n";
    let rec = record_file(src, 3, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let g = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "g")
        .expect("g record");
    assert!(!g.cases.is_empty(), "expected generated cases");
    for c in &g.cases {
        assert_eq!(c.outcome, "raised");
        assert_eq!(c.raises.as_deref(), Some("ValueError"));
        assert!(c.error.is_none(), "a semantic raise carries no structured error");
    }
}

#[test]
fn kwargs_param_does_not_produce_spurious_type_error() {
    if !ready("kwargs_param_does_not_produce_spurious_type_error") {
        return;
    }
    let src = "def f(a, **kw):\n    return a\n";
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    for c in &f.cases {
        assert_eq!(
            c.outcome, "returned",
            "a **kwargs param must not be passed positionally: {:?}",
            c.raises
        );
    }
}

#[test]
fn keyword_only_param_is_passed_by_name_and_returns() {
    if !ready("keyword_only_param_is_passed_by_name_and_returns") {
        return;
    }
    // `b` is keyword-only; before this fix it was generated but passed POSITIONALLY, so every
    // case raised a spurious TypeError ("too many positional arguments" / "missing keyword-only
    // argument"). It must now be generated and passed as a keyword argument instead, so at
    // least the type-compatible cases (e.g. `a` and `b` both strings/numbers) actually return.
    let src = "def f(a, *, b=5):\n    return a + b\n";
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    for c in &f.cases {
        assert!(!c.kwargs.is_empty(), "case should record the kwarg passed");
        assert!(c.kwargs.contains_key("b"));
    }
    assert!(
        f.cases.iter().any(|c| c.outcome == "returned"),
        "expected at least one case to return successfully via keyword passing: {:?}",
        f.cases.iter().map(|c| &c.outcome).collect::<Vec<_>>()
    );
}

#[test]
fn guarded_branch_is_reached_via_guard_sample() {
    if !ready("guarded_branch_is_reached_via_guard_sample") {
        return;
    }
    // Without guard-directed sampling, an even spread over `x`'s generic candidates is unlikely
    // to land exactly on 42, so the guarded `"hit"` branch would rarely (if ever) be exercised.
    let src = "def f(x):\n    if x == 42:\n        return \"hit\"\n    return \"miss\"\n";
    let rec = record_file(src, 8, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    let hit = f.cases.iter().any(|c| {
        c.outcome == "returned"
            && c.input == vec![serde_json::json!(42)]
            && c.ret == Some(serde_json::json!("hit"))
    });
    assert!(
        hit,
        "expected a case with input [42] returning \"hit\": {:?}",
        f.cases.iter().map(|c| (&c.input, &c.ret)).collect::<Vec<_>>()
    );
}

#[test]
fn record_pyi_folds_observed_return_type_when_static_is_opaque() {
    if !ready("record_pyi_folds_observed_return_type_when_static_is_opaque") {
        return;
    }
    // `abs(...)` isn't a recognized builtin in `classify_return`, so the static return type
    // stays unresolved (`Opaque` -> renders `Any`). Every recorded case actually returns an
    // `int`, so `record --format pyi` should fold that observed type in with a `# observed`
    // marker — never claiming it as statically proven.
    let src = "def f():\n    return abs(-5)\n";
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert_eq!(
        pylens::stub::returns_to_pytype(&f.signature.returns),
        "Any",
        "return type should be statically unresolved"
    );
    assert!(
        f.cases.iter().any(|c| c.outcome == "returned" && c.ret == Some(serde_json::json!(5))),
        "expected at least one case returning 5: {:?}",
        f.cases.iter().map(|c| (&c.outcome, &c.ret)).collect::<Vec<_>>()
    );

    let out = pylens::stub::observed::render_record_stub(&rec.functions);
    assert!(
        out.contains("-> int") && out.contains("# observed"),
        "expected an observed int return with a marker comment: {out}"
    );
}

#[test]
fn raised_case_carries_a_smaller_minimized_input() {
    if !ready("raised_case_carries_a_smaller_minimized_input") {
        return;
    }
    // The `for` loop is sequence-protocol evidence, so `xs`'s shape is `Union(Seq, Str)` (it
    // admits a `str` argument too — see `analyze::passes::shapes`'s widening doc) and generated
    // cases are a mix of lists and strings; `xs[10]` raises IndexError whenever either has fewer
    // than 11 elements/characters. This test is about list shrinking specifically, so it only
    // looks at the list-shaped IndexError cases. Shrinking should find a smaller (or equal, for
    // already-minimal) list that still raises IndexError. The budget is 16, not 6: `xs`'s widened
    // corpus is 13 candidates and ranks the `str` half first, so a small budget can crowd out
    // every non-trivial (shrinkable) list candidate before reaching one.
    let src = "def f(xs):\n    for _ in xs:\n        pass\n    return xs[10]\n";
    let rec = record_file(src, 16, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    let index_errors: Vec<_> = f
        .cases
        .iter()
        .filter(|c| {
            c.outcome == "raised"
                && c.raises.as_deref() == Some("IndexError")
                && c.input.first().is_some_and(Value::is_array)
        })
        .collect();
    assert!(!index_errors.is_empty(), "expected at least one list-shaped IndexError case: {:?}",
        f.cases.iter().map(|c| (&c.outcome, &c.raises)).collect::<Vec<_>>());

    let shrunk = index_errors
        .iter()
        .find(|c| c.minimized.is_some())
        .expect("expected at least one IndexError case with a minimized input");
    let minimized = shrunk.minimized.as_ref().expect("minimized present");
    let original_len = shrunk.input[0].as_array().expect("xs input is a list").len();
    let minimized_len = minimized.input[0].as_array().expect("xs minimized is a list").len();
    assert!(
        minimized_len < original_len,
        "expected the minimized input to be strictly smaller: original {original_len}, minimized {minimized_len}"
    );

    // Re-raising the minimized input must independently reproduce the same exception type.
    use pylens::exec::{Limits, Sandbox};
    let sandbox = pylens::exec::Nsjail::new();
    let result = sandbox
        .call(src, "f", &minimized.input, &[], &[], Limits::default())
        .expect("re-raise minimized input");
    assert!(!result.ok, "minimized input should still raise");
    assert_eq!(
        result.exception.map(|e| e.ty),
        Some("IndexError".to_string()),
        "minimized input must reproduce the same exception type"
    );
}

#[test]
fn non_finite_float_return_is_tagged_and_round_trips() {
    if !ready("non_finite_float_return_is_tagged_and_round_trips") {
        return;
    }
    // `json.dumps` emits bare NaN/Infinity, which serde_json rejects; the worker must instead
    // tag them the same way it tags set/tuple/dict so the response is valid JSON.
    let nan_src = "def f():\n    return float('nan')\n";
    let rec = record_file(nan_src, 1, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec.functions.iter().find(|r| r.signature.name == "f").expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    for c in &f.cases {
        assert_eq!(c.outcome, "returned");
        assert_eq!(c.ret, Some(serde_json::json!({ "__t__": "float", "v": "nan" })));
    }

    let inf_src = "def f():\n    return float('inf')\n";
    let rec = record_file(inf_src, 1, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec.functions.iter().find(|r| r.signature.name == "f").expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    for c in &f.cases {
        assert_eq!(c.outcome, "returned");
        assert_eq!(c.ret, Some(serde_json::json!({ "__t__": "float", "v": "inf" })));
    }
}

#[test]
fn unreachable_branch_is_reported_as_missed_coverage() {
    if !ready("unreachable_branch_is_reported_as_missed_coverage") {
        return;
    }
    // The unconditional `return 1` makes every line after it dead code — no input can ever
    // reach it, so it must show up as `missed`, never silently folded into `executed`.
    let src = "def f(x):\n    return 1\n    y = x + 1\n    return y\n";
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    let cov = f.coverage.as_ref().expect("f should carry coverage");
    assert_eq!(cov.total, 3, "body_lines: return 1 / y = x + 1 / return y");
    assert_eq!(cov.executed, 1, "only the unconditional return 1 ever runs");
    assert!(cov.missed.contains(&3), "y = x + 1 must be reported missed: {:?}", cov.missed);
    assert!(cov.missed.contains(&4), "return y must be reported missed: {:?}", cov.missed);
}

#[test]
fn fully_exercised_function_reports_full_coverage() {
    if !ready("fully_exercised_function_reports_full_coverage") {
        return;
    }
    // A single-statement body that always runs, regardless of input, must report executed ==
    // total — nothing to miss.
    let src = "def f(x):\n    return x + 1\n";
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    let cov = f.coverage.as_ref().expect("f should carry coverage");
    assert_eq!(cov.total, 1);
    assert_eq!(cov.executed, cov.total, "expected full coverage: {:?}", cov.missed);
    assert!(cov.missed.is_empty());
}

#[test]
fn keyword_only_mutation_is_detected() {
    if !ready("keyword_only_mutation_is_detected") {
        return;
    }
    // `acc` is keyword-only and mutated in place; the worker must snapshot kwargs before/after
    // the same way it does positional args, or this mutation is invisible.
    let src = "def f(*, acc):\n    acc.append(1)\n    return None\n";
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    let mutated_acc = f.cases.iter().any(|c| {
        c.outcome == "returned" && c.mutations.iter().any(|m| m.target == "acc")
    });
    assert!(mutated_acc, "expected a case mutating the keyword-only `acc` param");
}

#[test]
fn sys_exit_is_reported_as_raised_not_a_resource_kill() {
    if !ready("sys_exit_is_reported_as_raised_not_a_resource_kill") {
        return;
    }
    // `sys.exit()` raises `SystemExit`, a `BaseException` the harness's `except Exception` alone
    // wouldn't catch — a genuine `raise` inside the function's own behavior, so it must surface
    // as outcome "raised" (not crash the child / surface as a harness `error`).
    let src = "import sys\ndef f():\n    sys.exit(1)\n";
    let rec = record_file(src, 3, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(
        f.signature
            .raises
            .implicit
            .iter()
            .any(|e| e == "SystemExit"),
        "static signature should model sys.exit as raising SystemExit: {:?}",
        f.signature.raises
    );
    assert!(!f.cases.is_empty(), "expected generated cases");
    for c in &f.cases {
        assert_eq!(
            c.outcome, "raised",
            "sys.exit() must surface as a semantic raise: {:?}",
            c.error
        );
        assert_eq!(c.raises.as_deref(), Some("SystemExit"));
        assert!(c.error.is_none(), "a semantic raise carries no structured error");
    }
}

#[test]
fn stderr_write_is_captured() {
    if !ready("stderr_write_is_captured") {
        return;
    }
    let src = "import sys\ndef f():\n    print('boom', file=sys.stderr)\n    return None\n";
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(f.signature.io.contains(&"stderr".to_string()));
    let captured_stderr = f
        .cases
        .iter()
        .any(|c| c.stderr.as_deref().is_some_and(|s| s.contains("boom")));
    assert!(captured_stderr, "expected a case with captured stderr");
}

#[test]
fn io_observability_flags_stdout_and_stderr_observable_but_not_filesystem() {
    if !ready("io_observability_flags_stdout_and_stderr_observable_but_not_filesystem") {
        return;
    }
    // stdout/stderr are captured and checked by `validate::check_io`; the jail's filesystem is
    // read-only, so an `open(...)` claim has no execution channel that could ever corroborate or
    // contradict it.
    let src = concat!(
        "def f():\n",
        "    print('hi')\n",
        "    open('/nonexistent', 'r')\n",
        "    return None\n"
    );
    let rec = record_file(src, 2, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(f.signature.io.contains(&"stdout".to_string()));
    assert!(f.signature.io.contains(&"filesystem".to_string()));

    let stdout_entry = f
        .io_observability
        .iter()
        .find(|e| e.kind == "stdout")
        .expect("stdout entry");
    assert!(stdout_entry.observable, "stdout is captured and checked — must be observable");

    let fs_entry = f
        .io_observability
        .iter()
        .find(|e| e.kind == "filesystem")
        .expect("filesystem entry");
    assert!(
        !fs_entry.observable,
        "the jail's filesystem is read-only — filesystem claims have no observation channel"
    );
}

#[test]
fn output_type_coverage_is_full_when_every_return_kind_and_line_is_reached() {
    if !ready("output_type_coverage_is_full_when_every_return_kind_and_line_is_reached") {
        return;
    }
    // A single unconditional return: one return kind, one return line, both always reached.
    let src = "def f(x):\n    return 1\n";
    let rec = record_file(src, 4, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    assert!(
        matches!(
            f.output_type_coverage,
            Some(pylens::record::OutputTypeCoverage::Full)
        ),
        "expected full output-type coverage"
    );
    assert!(f.unobserved_returns.is_none());
}

#[test]
fn output_type_coverage_is_partial_when_a_return_branch_is_never_reached() {
    if !ready("output_type_coverage_is_partial_when_a_return_branch_is_never_reached") {
        return;
    }
    // Restrict generation to ints only, so the `x == "unreachable-marker"` string branch (and
    // its `return "s"` line/kind) is never exercised — the int branch is.
    let src = concat!(
        "def f(x):\n",
        "    if x == \"unreachable-marker\":\n",
        "        return \"s\"\n",
        "    return 1\n"
    );
    let domain = ValueDomain::parse(r#"{"scalars": ["int"]}"#).expect("parse profile");
    let rec = record_file(
        src,
        6,
        &ReplayMap::new(),
        RecordFlags { domain: Some(&domain), ..RecordFlags::default() },
    )
    .expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    assert!(
        f.cases.iter().all(|c| c.ret != Some(serde_json::json!("s"))),
        "the string branch must never be observed under an int-only domain: {:?}",
        f.cases.iter().map(|c| &c.ret).collect::<Vec<_>>()
    );
    assert!(
        matches!(
            f.output_type_coverage,
            Some(pylens::record::OutputTypeCoverage::Partial)
        ),
        "expected partial output-type coverage, got {:?}",
        f.output_type_coverage.as_ref().map(|_| "present")
    );
    let unobserved = f.unobserved_returns.as_ref().expect("unobserved_returns present");
    assert!(
        unobserved.kinds.contains(&ReturnKind::Str),
        "Str return kind should be listed as unobserved: {:?}",
        unobserved.kinds
    );
    assert!(!unobserved.lines.is_empty(), "the never-executed return line should be listed");
}

#[test]
fn parse_replay_parses_valid_mapping() {
    let text = r#"{"my_func": [[1, 2], ["a", null]], "other": [[[1, 2, 3]]]}"#;
    let replay = parse_replay(text).expect("parse");
    assert_eq!(
        replay["my_func"],
        vec![vec![json!(1), json!(2)], vec![json!("a"), Value::Null]]
    );
    assert_eq!(replay["other"], vec![vec![json!([1, 2, 3])]]);
}

#[test]
fn parse_replay_rejects_non_object_top_level() {
    let err = parse_replay("[1, 2, 3]").expect_err("must reject a non-object top level");
    assert!(err.contains("object"), "unexpected message: {err}");
}

#[test]
fn parse_replay_rejects_non_array_mapping_value() {
    let err =
        parse_replay(r#"{"f": "not-an-array"}"#).expect_err("must reject a non-array mapping value");
    assert!(err.contains("f"), "unexpected message: {err}");
}

#[test]
fn parse_replay_rejects_non_array_tuple() {
    let err = parse_replay(r#"{"f": [1, 2]}"#).expect_err("must reject a tuple that isn't an array");
    assert!(err.contains("f"), "unexpected message: {err}");
}

#[test]
fn parse_project_replay_parses_valid_mapping() {
    let text = r#"{
        "a.py": {"add": [[1, 2]]},
        "sub/mod.py": {"f": [[1], [2, 3]], "g": []}
    }"#;
    let replay = parse_project_replay(text).expect("parse");
    assert_eq!(replay["a.py"]["add"], vec![vec![json!(1), json!(2)]]);
    assert_eq!(
        replay["sub/mod.py"]["f"],
        vec![vec![json!(1)], vec![json!(2), json!(3)]]
    );
    assert!(replay["sub/mod.py"]["g"].is_empty());
}

#[test]
fn parse_project_replay_rejects_non_object_top_level() {
    let err = parse_project_replay("[1, 2, 3]").expect_err("must reject a non-object top level");
    assert!(err.contains("object"), "unexpected message: {err}");
}

#[test]
fn parse_project_replay_rejects_non_object_per_file_value() {
    let err = parse_project_replay(r#"{"a.py": [[1, 2]]}"#)
        .expect_err("must reject a per-file value that isn't the nested object shape");
    assert!(err.contains("a.py"), "unexpected message: {err}");
}

#[test]
fn parse_project_replay_rejects_non_array_mapping_value() {
    let err = parse_project_replay(r#"{"a.py": {"f": "not-an-array"}}"#)
        .expect_err("must reject a non-array mapping value");
    assert!(err.contains("f"), "unexpected message: {err}");
}

#[test]
fn parse_project_replay_rejects_non_array_tuple() {
    let err = parse_project_replay(r#"{"a.py": {"f": [1, 2]}}"#)
        .expect_err("must reject a tuple that isn't an array");
    assert!(err.contains("f"), "unexpected message: {err}");
}

#[test]
fn single_file_replay_format_still_parses_via_parse_replay() {
    let text = r#"{"my_func": [[1, 2]]}"#;
    let replay = parse_replay(text).expect("single-file format is unaffected by the project format");
    assert_eq!(replay["my_func"], vec![vec![json!(1), json!(2)]]);
}

/// Never actually invoked in the tests that use it — replay validation (an unknown function
/// name) must fail before any sandbox call happens.
struct PanicSandbox;

impl Sandbox for PanicSandbox {
    fn transport(&self, _body: &[u8]) -> Result<CallResult, String> {
        panic!("sandbox must not be reached when replay name validation already failed");
    }
}

#[test]
fn replay_unmatched_function_name_is_an_error() {
    let src = "def f(a):\n    return a\n";
    let imports = imports_of(src).expect("imports");
    let sigs = analyze_source(src).expect("analyze");
    let mut replay = ReplayMap::new();
    replay.insert("does_not_exist".to_string(), vec![vec![json!(1)]]);

    let result = record_with_signatures(
        &PanicSandbox,
        src,
        imports,
        sigs,
        4,
        &replay,
        RecordFlags::default(),
    );
    let err = result.err().expect("an unmatched replay function name must be an error");
    assert!(err.contains("does_not_exist"), "unexpected message: {err}");
}

#[test]
fn replay_input_executes_and_is_tagged_source_replay() {
    if !ready("replay_input_executes_and_is_tagged_source_replay") {
        return;
    }
    let src = "def add(a, b):\n    return a + b\n";
    let mut replay = ReplayMap::new();
    replay.insert("add".to_string(), vec![vec![json!(3), json!(4)]]);

    let rec = record_file(src, 4, &replay, RecordFlags::default()).expect("record");
    let add = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "add")
        .expect("add record");

    let replay_case = add
        .cases
        .iter()
        .find(|c| c.source == CaseSource::Replay)
        .expect("expected a replayed case");
    assert_eq!(replay_case.input, vec![json!(3), json!(4)]);
    assert_eq!(replay_case.outcome, "returned");
    assert_eq!(replay_case.ret, Some(json!(7)));
    assert!(
        replay_case.minimized.is_none(),
        "replayed cases must never be shrunk"
    );
    assert!(
        add.cases.iter().any(|c| c.source == CaseSource::Generated),
        "generated cases must still be present alongside the replayed one"
    );
}

#[test]
fn value_domain_restricts_generated_cases_but_not_replay() {
    if !ready("value_domain_restricts_generated_cases_but_not_replay") {
        return;
    }
    let src = "def f(x):\n    return x\n";
    let domain = ValueDomain::parse(r#"{"scalars": ["int"]}"#).expect("parse profile");
    let mut replay = ReplayMap::new();
    replay.insert("f".to_string(), vec![vec![json!("not an int")]]);

    let rec = record_file(
        src,
        8,
        &replay,
        RecordFlags { domain: Some(&domain), ..RecordFlags::default() },
    )
    .expect("record");
    let f = rec.functions.iter().find(|r| r.signature.name == "f").expect("f record");

    let generated: Vec<_> = f.cases.iter().filter(|c| c.source == CaseSource::Generated).collect();
    assert!(!generated.is_empty(), "the strict domain must not empty out generation entirely");
    for case in &generated {
        let x = case.input.first().expect("f takes one argument");
        assert!(
            x.as_i64().is_some() || x.as_u64().is_some(),
            "every generated case must be an int under a scalars: [int] domain, got {x:?}"
        );
    }

    let replay_case = f
        .cases
        .iter()
        .find(|c| c.source == CaseSource::Replay)
        .expect("expected a replayed case");
    assert_eq!(
        replay_case.input,
        vec![json!("not an int")],
        "replayed inputs must bypass the value-domain filter entirely"
    );
    assert_eq!(replay_case.outcome, "returned");
}

/// A `time.sleep`-based body: deterministic wall-clock cost per call (unlike an iteration count,
/// which varies with machine speed), so a tight `--time-budget` reliably trips after one case.
const SLOW_SRC: &str = "\
import time

def slow(x: int) -> int:
    time.sleep(0.1)
    return x
";

#[test]
fn tight_time_budget_trips_and_keeps_already_recorded_cases() {
    if !ready("tight_time_budget_trips_and_keeps_already_recorded_cases") {
        return;
    }
    // The budget must clear `budget::FLOOR` (1s) or nothing would ever be leased at all — see
    // `Budget::lease`. Still far under the ~2s a full run of 20 inputs at 0.1s/call would take,
    // so the deadline reliably trips after the first leased batch.
    let rec = record_file(
        SLOW_SRC,
        20,
        &ReplayMap::new(),
        RecordFlags { time_budget: Some(Duration::from_secs_f64(1.3)), ..RecordFlags::default() },
    )
    .expect("record");
    let f = rec.functions.iter().find(|r| r.signature.name == "slow").expect("slow record");

    assert!(
        f.cases.len() < 20,
        "the tight budget must have stopped generation well before the full {} inputs, got {}",
        20,
        f.cases.len()
    );
    assert!(!f.cases.is_empty(), "the case executed before the deadline tripped must be kept");
    assert_eq!(f.time_budget_hit, Some(true), "the budget must be reported as tripped");
}

#[test]
fn plain_record_has_no_time_budget_hit_field() {
    if !ready("plain_record_has_no_time_budget_hit_field") {
        return;
    }
    let rec = record_file(SLOW_SRC, 1, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let f = rec.functions.iter().find(|r| r.signature.name == "slow").expect("slow record");
    assert!(f.time_budget_hit.is_none(), "time_budget_hit must be omitted when --time-budget wasn't set");

    let json = serde_json::to_value(&rec.functions).expect("serialize");
    let entry = json.as_array().unwrap().iter().find(|v| v["name"] == "slow").expect("slow entry");
    assert!(
        !entry.as_object().unwrap().contains_key("time_budget_hit"),
        "the JSON output must not carry a time_budget_hit key at all"
    );
}

#[test]
fn time_budget_never_drops_replay_cases() {
    if !ready("time_budget_never_drops_replay_cases") {
        return;
    }
    let mut replay = ReplayMap::new();
    replay.insert("slow".to_string(), vec![vec![json!(7)]]);

    let rec = record_file(
        SLOW_SRC,
        20,
        &replay,
        RecordFlags { time_budget: Some(Duration::from_secs_f64(0.05)), ..RecordFlags::default() },
    )
    .expect("record");
    let f = rec.functions.iter().find(|r| r.signature.name == "slow").expect("slow record");

    assert_eq!(f.time_budget_hit, Some(true), "the tight budget must still trip for the generated cases");
    let replay_case = f
        .cases
        .iter()
        .find(|c| c.source == CaseSource::Replay)
        .expect("the replay case must execute despite the tripped budget");
    assert_eq!(replay_case.input, vec![json!(7)]);
    assert_eq!(replay_case.outcome, "returned");
    assert_eq!(replay_case.ret, Some(json!(7)));
}

#[test]
fn base_inputs_absent_matches_max_inputs_bit_for_bit() {
    if !ready("base_inputs_absent_matches_max_inputs_bit_for_bit") {
        return;
    }
    let src = "def f(a, b, c):\n    return a + b + c\n";
    let default_rec =
        record_file(src, 6, &ReplayMap::new(), RecordFlags::default()).expect("record");
    let explicit_rec = record_file(
        src,
        6,
        &ReplayMap::new(),
        RecordFlags { base_inputs: Some(6), ..RecordFlags::default() },
    )
    .expect("record");
    assert_eq!(
        serde_json::to_value(&default_rec.functions).unwrap(),
        serde_json::to_value(&explicit_rec.functions).unwrap(),
        "omitting --base-inputs must be bit-for-bit identical to --base-inputs == --inputs"
    );
}

#[test]
fn base_inputs_shrinks_the_initial_batch() {
    if !ready("base_inputs_shrinks_the_initial_batch") {
        return;
    }
    let src = "def f(a, b, c):\n    return a + b + c\n";
    let small = record_file(
        src,
        12,
        &ReplayMap::new(),
        RecordFlags { base_inputs: Some(1), ..RecordFlags::default() },
    )
    .expect("record");
    let full = record_file(src, 12, &ReplayMap::new(), RecordFlags::default()).expect("record");

    let small_f = small.functions.iter().find(|r| r.signature.name == "f").expect("f record");
    let full_f = full.functions.iter().find(|r| r.signature.name == "f").expect("f record");
    assert_eq!(small_f.cases.len(), 1, "a base-inputs of 1 must generate exactly one seed case");
    assert!(
        full_f.cases.len() > small_f.cases.len(),
        "the full budget must generate more cases than a base-inputs of 1: {} vs {}",
        full_f.cases.len(),
        small_f.cases.len()
    );
}

#[test]
fn base_inputs_greater_than_inputs_is_a_usage_error() {
    if !ready("base_inputs_greater_than_inputs_is_a_usage_error") {
        return;
    }
    let src = "def f(a):\n    return a\n";
    let result = record_file(
        src,
        4,
        &ReplayMap::new(),
        RecordFlags { base_inputs: Some(5), ..RecordFlags::default() },
    );
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("--base-inputs > --inputs must be rejected"),
    };
    assert!(
        err.contains("--base-inputs") && err.contains("--inputs"),
        "error should name both flags: {err:?}"
    );
}

#[test]
fn unloadable_module_is_detected_from_the_first_batch_for_every_function() {
    if !ready("unloadable_module_is_detected_from_the_first_batch_for_every_function") {
        return;
    }
    // No standalone `probe_load` runs here (the first signature `f` is a plain function with a
    // non-empty generated batch) — the module-not-loadable verdict must still come out
    // byte-identical to the standalone-probe path, and cover every function in the file, not
    // just the one whose batch surfaced the failure.
    let src = concat!(
        "import definitely_not_a_real_module_xyz as z\n",
        "def f(x):\n",
        "    return z.go(x)\n",
        "\n",
        "def g(y):\n",
        "    return z.go(y)\n",
    );
    let rec = record_file(src, 3, &ReplayMap::new(), RecordFlags::default()).expect("record");
    for name in ["f", "g"] {
        let func = rec.functions.iter().find(|r| r.signature.name == name).expect("record");
        let unc = func.uncallable.as_ref().unwrap_or_else(|| panic!("{name} should be uncallable"));
        assert_eq!(unc.reason, "module_not_loadable");
        assert_eq!(unc.error.kind, "ModuleNotFoundError");
        assert_eq!(unc.error.module.as_deref(), Some("definitely_not_a_real_module_xyz"));
        assert!(func.cases.is_empty(), "no per-case spam when the module can't load: {name}");
        assert!(func.coverage.is_none());
        assert!(func.branches.is_none());
        assert!(func.branch_coverage.is_none());
        assert!(func.output_type_coverage.is_none());
    }
}
