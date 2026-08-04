//! Pure-Rust tests for input generation (no jail).

use pylens::analyze_source;
use pylens::generate::gen_inputs;

fn sig(sigs: &[pylens::model::EffectSignature], name: &str) -> pylens::model::EffectSignature {
    sigs.iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no signature named {name}"))
        .clone()
}

#[test]
fn varargs_and_kwargs_get_no_positional_slot() {
    let sigs = analyze_source("def f(a, *args, **kw):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 4);
    assert!(!vectors.is_empty());
    for v in &vectors {
        assert_eq!(v.positional.len(), 1, "only `a` should get a positional slot: {v:?}");
    }
}

#[test]
fn ordinary_params_unaffected() {
    let sigs = analyze_source("def f(a, b):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 4);
    assert!(!vectors.is_empty());
    for v in &vectors {
        assert_eq!(v.positional.len(), 2, "both ordinary params get a slot: {v:?}");
    }
}

#[test]
fn guard_samples_appear_in_generated_vectors() {
    let sigs = analyze_source("def f(x):\n    if x == 42:\n        return 1\n    return 0\n")
        .expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 32);
    assert!(!vectors.is_empty());
    assert!(
        vectors
            .iter()
            .any(|v| v.positional.first() == Some(&serde_json::json!(42))),
        "expected the guard literal 42 among generated inputs for `x`: {vectors:?}"
    );
}

#[test]
fn keyword_only_params_go_in_kwargs_not_positional() {
    let sigs = analyze_source("def f(a, *, b):\n    return a\n").expect("parse");
    let f = sig(&sigs, "f");
    let vectors = gen_inputs(&f, 4);
    assert!(!vectors.is_empty());
    for v in &vectors {
        assert_eq!(v.positional.len(), 1, "`a` is the only positional param: {v:?}");
        assert_eq!(v.kwargs.len(), 1, "`b` should be the only kwarg: {v:?}");
        assert_eq!(v.kwargs[0].0, "b");
    }
}
