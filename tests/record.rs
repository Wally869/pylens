//! Record tests: static signature + observed cases, executed in the jail. Skips (does not
//! fall back to unsandboxed) when the sandbox isn't provisioned.

use pylens::exec::probe;
use pylens::model::ReturnKind;
use pylens::record::{DepStatus, record_file};

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
    let rec = record_file(src, 4).expect("record");
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
    let rec = record_file(src, 4).expect("record");
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
    let rec = record_file(src, 2).expect("record");
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
    let rec = record_file(src, 3).expect("record");
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
    let rec = record_file(src, 3).expect("record");
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
fn semantic_raise_is_unaffected_by_resource_kill_handling() {
    if !ready("semantic_raise_is_unaffected_by_resource_kill_handling") {
        return;
    }
    // A genuine `raise` inside the function is unambiguously semantic and must still surface
    // as outcome "raised" with the exception type in `raises`.
    let src = "def g():\n    raise ValueError('x')\n";
    let rec = record_file(src, 3).expect("record");
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
    let rec = record_file(src, 4).expect("record");
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
    let rec = record_file(src, 4).expect("record");
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
    let rec = record_file(src, 8).expect("record");
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
    let rec = record_file(src, 4).expect("record");
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
    // The `for` loop pins `xs`'s shape to a sequence, so every generated case is a list; `xs[10]`
    // raises IndexError whenever the list has fewer than 11 elements. Shrinking should find a
    // smaller (or equal, for already-minimal) list that still raises IndexError.
    let src = "def f(xs):\n    for _ in xs:\n        pass\n    return xs[10]\n";
    let rec = record_file(src, 6).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    let index_errors: Vec<_> = f
        .cases
        .iter()
        .filter(|c| c.outcome == "raised" && c.raises.as_deref() == Some("IndexError"))
        .collect();
    assert!(!index_errors.is_empty(), "expected at least one IndexError case: {:?}",
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
    use pylens::exec::Sandbox;
    let sandbox = pylens::exec::Nsjail::new();
    let result = sandbox
        .call(src, "f", &minimized.input, &[])
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
    let rec = record_file(nan_src, 1).expect("record");
    let f = rec.functions.iter().find(|r| r.signature.name == "f").expect("f record");
    assert!(!f.cases.is_empty(), "expected generated cases");
    for c in &f.cases {
        assert_eq!(c.outcome, "returned");
        assert_eq!(c.ret, Some(serde_json::json!({ "__t__": "float", "v": "nan" })));
    }

    let inf_src = "def f():\n    return float('inf')\n";
    let rec = record_file(inf_src, 1).expect("record");
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
    let rec = record_file(src, 4).expect("record");
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
    let rec = record_file(src, 4).expect("record");
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
    let rec = record_file(src, 4).expect("record");
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
fn stderr_write_is_captured() {
    if !ready("stderr_write_is_captured") {
        return;
    }
    let src = "import sys\ndef f():\n    print('boom', file=sys.stderr)\n    return None\n";
    let rec = record_file(src, 4).expect("record");
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
