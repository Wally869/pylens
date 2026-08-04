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
