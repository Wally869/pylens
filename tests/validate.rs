//! Jail-gated integration test for the `observed ⊆ static` harness: runs `validate` logic over
//! the FULL curated example corpus (`examples/*.py`) and asserts the analyzer is sound — zero
//! HARD defects for every corpus function. Skips (does not fall back unsandboxed) when the
//! sandbox isn't provisioned.

use pylens::exec::probe;
use pylens::record::record_file;
use pylens::report::{FunctionValidation, validate_summary};
use pylens::validate::{Severity, validate_function};

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
fn example_corpus_has_zero_hard_defects() {
    if !ready("example_corpus_has_zero_hard_defects") {
        return;
    }
    for src in [
        include_str!("../examples/config.py"),
        include_str!("../examples/deps.py"),
        include_str!("../examples/graph.py"),
        include_str!("../examples/inventory.py"),
        include_str!("../examples/lazy_deps.py"),
        include_str!("../examples/ledger.py"),
        include_str!("../examples/normalize.py"),
        include_str!("../examples/streaming.py"),
    ] {
        let rec = record_file(src, 4).expect("record");
        for f in &rec.functions {
            let defects = validate_function(f);
            let hard: Vec<_> = defects
                .iter()
                .filter(|d| d.severity == Severity::Hard)
                .collect();
            assert!(
                hard.is_empty(),
                "hard soundness defects in {}: {:?}",
                f.signature.name,
                hard
            );
        }
    }
}

#[test]
fn uncallable_function_is_reported_unvalidated_in_the_summary() {
    if !ready("uncallable_function_is_reported_unvalidated_in_the_summary") {
        return;
    }
    // A module-scope import that can't load stops the whole file from loading, so every function
    // is `uncallable` — validate observed nothing for it, and that must be surfaced, not read as
    // a silent pass.
    let src = "import definitely_not_a_real_module_xyz as z\ndef f(x):\n    return z.go(x)\n";
    let rec = record_file(src, 3).expect("record");
    let f = rec
        .functions
        .iter()
        .find(|r| r.signature.name == "f")
        .expect("f record");
    assert!(f.uncallable.is_some(), "f should be uncallable");

    let is_validated = |r: &pylens::record::FunctionRecord| r.uncallable.is_none() && !r.cases.is_empty();
    assert!(!is_validated(f), "an uncallable function must report validated: false");

    let unvalidated = rec.functions.iter().filter(|r| !is_validated(r)).count();
    assert_eq!(unvalidated, 1);

    let per_function: Vec<_> = rec.functions.iter().map(|r| (r, validate_function(r))).collect();
    let results: Vec<FunctionValidation> = per_function
        .iter()
        .map(|(r, defects)| FunctionValidation {
            name: &r.signature.name,
            owner: r.signature.owner.as_deref(),
            defects,
            coverage: r.coverage.as_ref(),
        })
        .collect();
    let summary = validate_summary("test.py", rec.functions.len(), unvalidated, 0, 0, &results);
    assert!(
        summary.contains("1 function(s) unvalidated"),
        "expected the unvalidated count in the summary: {summary}"
    );
}

#[test]
fn pre_rebind_method_call_has_zero_hard_defects() {
    // Regression coverage for `temp/probe_flow2.py`: a call through a parameter BEFORE an
    // unrelated rebind of that same parameter (`x.bump()` before `x = Box()`) must never resolve
    // through the post-rebind shape — if it did, the `call_method_unknown` acknowledgment that
    // covers a real `AttributeError` (e.g. `pre_rebind_call(3)`) would vanish, and `validate`
    // would report a hard defect. See the dominance gate in
    // `passes::shapes::state::ShapeState::frozen_dominance` / `context::FunctionFacts::
    // env_shape`.
    if !ready("pre_rebind_method_call_has_zero_hard_defects") {
        return;
    }
    let src = concat!(
        "class Box:\n",
        "    def __init__(self):\n",
        "        self.n = 0\n",
        "    def bump(self):\n",
        "        self.n = 1\n",
        "\n",
        "def pre_rebind_call(x):\n",
        "    x.bump()\n",
        "    x = Box()\n",
        "    x.bump()\n",
    );
    let rec = record_file(src, 12).expect("record");
    for f in &rec.functions {
        let defects = validate_function(f);
        let hard: Vec<_> = defects.iter().filter(|d| d.severity == Severity::Hard).collect();
        assert!(hard.is_empty(), "hard soundness defects in {}: {:?}", f.signature.name, hard);
    }
}
