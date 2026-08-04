//! Jail-gated integration test for the `observed ⊆ static` harness: runs `validate` logic over
//! the curated example corpus and asserts the analyzer is sound — zero HARD defects for every
//! corpus function. Skips (does not fall back unsandboxed) when the sandbox isn't provisioned.

use pylens::exec::probe;
use pylens::record::record_file;
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
        include_str!("../examples/inventory.py"),
        include_str!("../examples/normalize.py"),
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
