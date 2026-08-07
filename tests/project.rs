//! Directory ("project") mode: walking (incl. `.gitignore` honoring), aggregation,
//! partial-failure handling, and the skip-list. Pure (no jail) except the corpus-level validate
//! check, which is jail-gated.

use std::path::Path;

use pylens::exec::probe;
use pylens::project::{analyze_project, record_project, validate_project};

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
fn analyze_project_aggregates_sorted_files_with_correct_counts() {
    let report = analyze_project(Path::new("tests/fixtures/project"));

    assert_eq!(report["schema_version"], pylens::SCHEMA_VERSION);
    assert!(report.get("files").is_some());
    // The wrapper carries schema_version at the top only, not per-file.
    let files = report["files"].as_array().expect("files array");
    for f in files {
        assert!(f.get("schema_version").is_none());
    }

    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["a.py", "b.py"]);

    let summary = &report["summary"];
    assert_eq!(summary["files"], 2);
    assert_eq!(summary["ok"], 2);
    assert_eq!(summary["errors"], 0);
    assert_eq!(summary["functions"], 2);
    assert_eq!(summary["purity"]["pure"], 1);
    assert_eq!(summary["purity"]["impure"], 1);
    assert_eq!(summary["purity"]["unknown"], 0);
}

#[test]
fn analyze_project_reports_a_bad_file_without_aborting_the_run() {
    let report = analyze_project(Path::new("tests/fixtures/project_partial"));

    let files = report["files"].as_array().expect("files array");
    assert_eq!(files.len(), 2);

    let valid = files.iter().find(|f| f["path"] == "valid.py").expect("valid.py entry");
    assert!(valid.get("error").is_none());
    assert!(valid.get("functions").is_some());

    let broken = files.iter().find(|f| f["path"] == "broken.py").expect("broken.py entry");
    assert!(broken.get("error").is_some());
    assert!(broken.get("functions").is_none());

    let summary = &report["summary"];
    assert_eq!(summary["files"], 2);
    assert_eq!(summary["ok"], 1);
    assert_eq!(summary["errors"], 1);
}

#[test]
fn analyze_project_resolves_project_local_and_external_imports() {
    let report = analyze_project(Path::new("tests/fixtures/pkg"));
    let files = report["files"].as_array().expect("files array");
    let main = files
        .iter()
        .find(|f| f["path"] == "main.py")
        .expect("main.py entry");
    let imports = main["imports"].as_array().expect("imports array");

    let os = imports
        .iter()
        .find(|i| i["module"]["package"] == "os")
        .expect("os import");
    assert_eq!(os["resolution"], "external");
    assert!(os.get("project_target").is_none());

    let util = imports
        .iter()
        .find(|i| i["module"]["package"] == "util")
        .expect(".util import");
    assert_eq!(util["resolution"], "project_local");
    assert_eq!(util["project_target"], "util.py");

    let helper = imports
        .iter()
        .find(|i| i["module"]["package"] == "sub")
        .expect(".sub.helper import");
    assert_eq!(helper["resolution"], "project_local");
    assert_eq!(helper["project_target"], "sub/helper.py");

    let missing = imports
        .iter()
        .find(|i| i["module"]["package"] == "missing")
        .expect(".missing import");
    assert_eq!(missing["resolution"], "unresolved_relative");
    assert!(missing.get("project_target").is_none());
}

#[test]
fn analyze_project_propagates_cross_file_mutation_onto_the_caller() {
    let report = analyze_project(Path::new("tests/fixtures/xfile"));
    let files = report["files"].as_array().expect("files array");
    let main = files.iter().find(|f| f["path"] == "main.py").expect("main.py entry");
    let functions = main["functions"].as_array().expect("functions array");

    let caller = functions.iter().find(|f| f["name"] == "caller").expect("caller function");
    assert_ne!(caller["purity"], "unknown");
    let mutations = caller["mutations"].as_array().expect("mutations array");
    assert!(
        mutations.iter().any(|m| m["target"]["root"] == "param" && m["target"]["name"] == "data"),
        "expected caller to show a propagated mutation on `data`, got {mutations:?}"
    );
    let unresolved = caller["unresolved_effects"].as_array().expect("unresolved_effects array");
    assert!(
        !unresolved.iter().any(|u| u["reason"] == "call_import" && u["callee"] == "touch"),
        "expected no call_import unresolved effect for touch, got {unresolved:?}"
    );

    let uses_external =
        functions.iter().find(|f| f["name"] == "uses_external").expect("uses_external function");
    let unresolved = uses_external["unresolved_effects"].as_array().expect("unresolved_effects array");
    assert!(
        unresolved
            .iter()
            .any(|u| u["reason"] == "call_import" && u["callee"] == "os.getcwd"),
        "expected an external import to stay unresolved, got {unresolved:?}"
    );
}

#[test]
fn analyze_project_propagates_cross_file_mutation_via_keyword_arg_onto_the_caller() {
    let report = analyze_project(Path::new("tests/fixtures/xfile_kwargs"));
    let files = report["files"].as_array().expect("files array");
    let main = files.iter().find(|f| f["path"] == "main.py").expect("main.py entry");
    let functions = main["functions"].as_array().expect("functions array");

    let caller = functions.iter().find(|f| f["name"] == "caller").expect("caller function");
    assert_ne!(caller["purity"], "unknown");
    let mutations = caller["mutations"].as_array().expect("mutations array");
    assert!(
        mutations.iter().any(|m| m["target"]["root"] == "param" && m["target"]["name"] == "data"),
        "expected caller to show a propagated mutation on `data`, got {mutations:?}"
    );
    let unresolved = caller["unresolved_effects"].as_array().expect("unresolved_effects array");
    assert!(
        !unresolved.iter().any(|u| u["reason"] == "call_import" && u["callee"] == "touch"),
        "expected no call_import unresolved effect for touch, got {unresolved:?}"
    );
}

#[test]
fn analyze_project_keeps_call_import_acknowledgment_for_unpacked_cross_file_call() {
    // `touch(*lst)` resolves cross-file, but the unpacked argument reaches a parameter the
    // mapping can't attribute — the caller must keep its `call_import` acknowledgment (and its
    // unpack-induced implicit TypeError) instead of claiming a complete may-set.
    let report = analyze_project(Path::new("tests/fixtures/xfile_unpack"));
    let files = report["files"].as_array().expect("files array");
    let main = files.iter().find(|f| f["path"] == "main.py").expect("main.py entry");
    let functions = main["functions"].as_array().expect("functions array");

    let caller = functions.iter().find(|f| f["name"] == "caller").expect("caller function");
    assert_ne!(caller["purity"], "pure");
    let unresolved = caller["unresolved_effects"].as_array().expect("unresolved_effects array");
    assert!(
        unresolved.iter().any(|u| u["reason"] == "call_import" && u["callee"] == "touch"),
        "expected the call_import unresolved effect to survive for touch, got {unresolved:?}"
    );
    let implicit = caller["raises"]["implicit"].as_array().expect("implicit raises array");
    assert!(
        implicit.iter().any(|r| r == "TypeError"),
        "expected unpack-induced implicit TypeError, got {implicit:?}"
    );
}

#[test]
fn analyze_project_cross_file_recursion_terminates() {
    let report = analyze_project(Path::new("tests/fixtures/xfile_recursive"));
    let files = report["files"].as_array().expect("files array");
    for f in files {
        assert!(f.get("error").is_none(), "unexpected per-file error: {f:?}");
    }
    assert_eq!(report["summary"]["files"], 3);
}

#[test]
fn walk_skips_pycache_and_dotfile_directories() {
    let report = analyze_project(Path::new("tests/fixtures/project_skip"));
    let files = report["files"].as_array().expect("files array");
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["top.py"]);
}

#[test]
fn walk_honors_gitignore_and_excludes_matched_paths() {
    let report = analyze_project(Path::new("tests/fixtures/project_gitignore"));
    let files = report["files"].as_array().expect("files array");
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["top.py"]);
}

#[test]
fn record_project_parallelizes_across_files_with_stable_output_order() {
    if !ready("record_project_parallelizes_across_files_with_stable_output_order") {
        return;
    }
    let report = record_project(Path::new("tests/fixtures/project"), 4).expect("record_project");

    let files = report["files"].as_array().expect("files array");
    let paths: Vec<&str> = files.iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, vec!["a.py", "b.py"], "report must list files in stable walk order");

    for f in files {
        assert!(f.get("error").is_none(), "unexpected per-file error: {f:?}");
        assert!(f.get("functions").is_some());
    }

    let a = files.iter().find(|f| f["path"] == "a.py").expect("a.py entry");
    let add_fn = a["functions"].as_array().unwrap().iter().find(|f| f["name"] == "add").expect("add fn");
    assert_eq!(add_fn["purity"], "pure");

    let b = files.iter().find(|f| f["path"] == "b.py").expect("b.py entry");
    let mutate_fn = b["functions"].as_array().unwrap().iter().find(|f| f["name"] == "mutate").expect("mutate fn");
    assert_eq!(mutate_fn["purity"], "impure");

    let summary = &report["summary"];
    assert_eq!(summary["files"], 2);
    assert_eq!(summary["ok"], 2);
    assert_eq!(summary["errors"], 0);
    assert_eq!(summary["functions"], 2);
}

#[test]
fn validate_project_over_examples_aggregates_the_observed_defect_count() {
    if !ready("validate_project_over_examples_aggregates_the_observed_defect_count") {
        return;
    }
    let (report, hard_total) = validate_project(Path::new("examples"), 4).expect("validate_project");
    let files = report["files"].as_array().expect("files array");
    assert!(!files.is_empty());
    for f in files {
        assert!(f.get("error").is_none(), "unexpected per-file error: {f:?}");
    }

    // The entire examples/ tree (config.py, deps.py, graph.py, inventory.py, lazy_deps.py,
    // ledger.py, normalize.py, streaming.py) is sound end to end — every file must be
    // hard-defect-free, individually and in aggregate.
    for f in files {
        let path = f["path"].as_str().unwrap_or_default();
        assert_eq!(f["summary"]["hard_defects"], 0, "{path} should be hard-defect-free");
    }

    let expected: u64 = files
        .iter()
        .map(|f| f["summary"]["hard_defects"].as_u64().unwrap_or(0))
        .sum();
    assert_eq!(expected, 0, "expected zero hard defects across the whole examples/ tree");
    assert_eq!(hard_total as u64, expected);
    assert_eq!(report["summary"]["hard_defects"], expected);
}
