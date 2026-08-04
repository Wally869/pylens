//! Behavioral tests for the effect analyzer. Targeted assertions (not full snapshots) so
//! they stay robust as the schema grows.

use pylens::analyze_source;
use pylens::model::*;

fn sig<'a>(sigs: &'a [EffectSignature], name: &str) -> &'a EffectSignature {
    sigs.iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no signature named {name}"))
}

fn analyze(src: &str) -> Vec<EffectSignature> {
    analyze_source(src).expect("parse")
}

fn has_mutation(s: &EffectSignature, target: &MutationTarget, via: MutationKind) -> bool {
    s.mutations
        .iter()
        .any(|m| &m.target == target && m.via == via)
}

#[test]
fn detects_param_subscript_mutation_and_explicit_raise() {
    let s = analyze(
        "def normalize(items, scale=1.0):\n\
         \x20   if not items:\n\
         \x20       raise ValueError('empty')\n\
         \x20   for i in range(len(items)):\n\
         \x20       items[i] = items[i] * scale\n\
         \x20   return items\n",
    );
    let n = sig(&s, "normalize");
    assert!(has_mutation(
        n,
        &MutationTarget::Param { name: "items".into() },
        MutationKind::SubscriptSet
    ));
    assert_eq!(n.raises.explicit, vec!["ValueError".to_string()]);
    assert_eq!(n.purity, Purity::Impure);
    // The body ends in `return items`, so no implicit None return.
    assert!(!n.returns.contains(&ReturnKind::None));
}

#[test]
fn local_collection_mutation_is_not_an_effect() {
    let s = analyze(
        "def count_words(text):\n\
         \x20   counts = {}\n\
         \x20   for w in text.split():\n\
         \x20       counts[w] = counts.get(w, 0) + 1\n\
         \x20   return counts\n",
    );
    let c = sig(&s, "count_words");
    assert!(c.mutations.is_empty(), "local dict is not a param mutation");
    assert_eq!(c.purity, Purity::Pure);
}

#[test]
fn rebinding_a_param_is_not_a_mutation() {
    let s = analyze(
        "def f(x):\n\
         \x20   x = x + 1\n\
         \x20   return x\n",
    );
    let f = sig(&s, "f");
    assert!(f.mutations.is_empty());
    assert_eq!(f.purity, Purity::Pure);
}

#[test]
fn alias_to_param_is_tracked() {
    let s = analyze(
        "def f(items):\n\
         \x20   q = items\n\
         \x20   q.append(1)\n",
    );
    let f = sig(&s, "f");
    assert!(has_mutation(
        f,
        &MutationTarget::Param { name: "items".into() },
        MutationKind::Method
    ));
}

#[test]
fn mutating_method_on_param_is_detected() {
    let s = analyze("def f(xs):\n    xs.sort()\n");
    let f = sig(&s, "f");
    assert!(f.mutations.iter().any(|m| m.via == MutationKind::Method
        && m.name.as_deref() == Some("sort")
        && m.target == MutationTarget::Param { name: "xs".into() }));
}

#[test]
fn generator_is_flagged() {
    let s = analyze("def squares(n):\n    for i in range(n):\n        yield i * i\n");
    assert!(sig(&s, "squares").is_generator);
}

#[test]
fn return_kinds_union_over_exits() {
    let s = analyze(
        "def f(x):\n\
         \x20   if x:\n\
         \x20       return []\n\
         \x20   return 0\n",
    );
    let f = sig(&s, "f");
    assert!(f.returns.contains(&ReturnKind::Sequence));
    assert!(f.returns.contains(&ReturnKind::Int));
}

#[test]
fn unknown_call_with_param_arg_is_unresolved() {
    let s = analyze("def f(data):\n    helper(data)\n");
    let f = sig(&s, "f");
    assert!(f.unresolved_effects.iter().any(|u| u.callee.as_deref() == Some("helper")
        && u.may_affect.contains(&MutationTarget::Param { name: "data".into() })));
    assert_eq!(f.purity, Purity::Unknown);
}

#[test]
fn qualified_import_call_is_a_foreign_effect() {
    let s = analyze("import numpy as np\ndef f(rows):\n    return np.array(rows)\n");
    let f = sig(&s, "f");
    // A call through an imported name is opaque — the function is not pure.
    assert_eq!(f.purity, Purity::Unknown);
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_import" && u.callee.as_deref() == Some("np.array"))
    );
    // …and it is linked to the import it uses.
    assert!(
        f.uses
            .iter()
            .any(|u| u.binding == "np" && u.module.package == "numpy")
    );
}

#[test]
fn method_self_attr_write_is_detected() {
    let s = analyze("class C:\n    def set(self, v):\n        self.value = v\n");
    let m = sig(&s, "set");
    assert_eq!(m.kind, DefKind::Method);
    assert!(has_mutation(
        m,
        &MutationTarget::SelfAttr { name: "value".into() },
        MutationKind::AttrSet
    ));
}

#[test]
fn print_is_stdout_io() {
    let s = analyze("def f():\n    print('hi')\n");
    assert!(sig(&s, "f").io.contains(&"stdout".to_string()));
}

#[test]
fn staticmethod_first_param_is_not_self() {
    let s = analyze(
        "class C:\n\
         \x20   @staticmethod\n\
         \x20   def f(x):\n\
         \x20       x.append(1)\n",
    );
    let f = sig(&s, "f");
    // `x` is an ordinary param, not a receiver, so this is a param mutation.
    assert!(has_mutation(
        f,
        &MutationTarget::Param { name: "x".into() },
        MutationKind::Method
    ));
}
