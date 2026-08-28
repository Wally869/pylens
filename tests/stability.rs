//! `--stability-runs`: sandbox-gated tests — skips gracefully when the sandbox isn't provisioned,
//! same as `tests/record.rs`. The pure comparison logic (`cases_agree`) has its own unit tests in
//! `src/record/stability.rs`.

use pylens::exec::probe;
use pylens::record::{ReplayMap, record_file_with_options, record_file_with_options_and_stability};

fn ready(test: &str) -> bool {
    match probe() {
        Ok(()) => true,
        Err(e) => {
            eprintln!("SKIP {test}: {e}");
            false
        }
    }
}

const DETERMINISTIC_SRC: &str = "\
def add_one(x: int) -> int:
    return x + 1
";

const NONDETERMINISTIC_SRC: &str = "\
import time

def unstable(x: int) -> str:
    return str(time.time_ns())
";

#[test]
fn deterministic_function_keeps_all_cases() {
    if !ready("deterministic_function_keeps_all_cases") {
        return;
    }
    let rec = record_file_with_options_and_stability(
        DETERMINISTIC_SRC,
        6,
        &ReplayMap::new(),
        None,
        false,
        Some(3),
    )
    .expect("record");
    let f = rec
        .functions
        .iter()
        .find(|f| f.signature.name == "add_one")
        .expect("add_one record");

    assert!(!f.cases.is_empty(), "add_one should have produced cases");
    let dropped = f.dropped_cases.as_ref().expect("dropped_cases must be present");
    assert_eq!(dropped.unstable, 0, "add_one is deterministic; nothing should be unstable");
    assert_eq!(dropped.resource, 0, "add_one never resource-kills");
}

#[test]
fn nondeterministic_function_drops_unstable_cases() {
    if !ready("nondeterministic_function_drops_unstable_cases") {
        return;
    }
    let plain = record_file_with_options(NONDETERMINISTIC_SRC, 6, &ReplayMap::new(), None, false)
        .expect("plain record");
    let plain_f = plain
        .functions
        .iter()
        .find(|f| f.signature.name == "unstable")
        .expect("unstable record");
    let total_cases_without_stability = plain_f.cases.len();
    assert!(total_cases_without_stability > 0, "unstable() should have produced cases");

    let rec = record_file_with_options_and_stability(
        NONDETERMINISTIC_SRC,
        6,
        &ReplayMap::new(),
        None,
        false,
        Some(3),
    )
    .expect("record");
    let f = rec
        .functions
        .iter()
        .find(|f| f.signature.name == "unstable")
        .expect("unstable record");

    let dropped = f.dropped_cases.as_ref().expect("dropped_cases must be present");
    assert!(
        dropped.unstable > 0,
        "time.time_ns() differs on every call; some case must be flagged unstable"
    );
    assert_eq!(
        f.cases.len() + dropped.unstable,
        total_cases_without_stability,
        "every case is accounted for: kept + dropped == the total generated"
    );
}

#[test]
fn plain_record_has_no_dropped_cases_field() {
    if !ready("plain_record_has_no_dropped_cases_field") {
        return;
    }
    let rec = record_file_with_options(DETERMINISTIC_SRC, 4, &ReplayMap::new(), None, false)
        .expect("record");
    let f = rec
        .functions
        .iter()
        .find(|f| f.signature.name == "add_one")
        .expect("add_one record");
    assert!(f.dropped_cases.is_none(), "dropped_cases must be omitted when --stability-runs is off");

    let json = serde_json::to_value(&rec.functions).expect("serialize");
    let entry = json
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "add_one")
        .expect("add_one entry");
    assert!(
        !entry.as_object().unwrap().contains_key("dropped_cases"),
        "the JSON output must not carry a dropped_cases key at all"
    );
}
