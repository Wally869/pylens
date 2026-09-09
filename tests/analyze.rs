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
    let f = sig(&s, "f");
    assert!(f.io.contains(&"stdout".to_string()));
    assert!(!f.io.contains(&"stderr".to_string()));
}

#[test]
fn print_with_unresolved_file_is_stdout_and_stderr_io() {
    let s = analyze("def f(x, f):\n    print(x, file=f)\n");
    let sf = sig(&s, "f");
    assert!(sf.io.contains(&"stdout".to_string()));
    assert!(sf.io.contains(&"stderr".to_string()));
}

#[test]
fn comprehension_call_is_descended_into() {
    let s = analyze("def f(items):\n    return [helper(x) for x in items]\n");
    let f = sig(&s, "f");
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_unknown_callee" && u.callee.as_deref() == Some("helper"))
    );
    assert_eq!(f.purity, Purity::Unknown);
    // The comprehension's `for` iterable votes shape like an ordinary `for` loop's — widened to
    // admit `str` too, since this is sequence-protocol evidence on a parameter.
    let widened_seq = Shape::union_of([Shape::any_seq(), Shape::Str]);
    assert!(f.params.iter().any(|p| p.name == "items" && p.shape == widened_seq));
}

#[test]
fn unknown_method_on_param_is_unresolved() {
    let s = analyze("def g(obj):\n    obj.frobnicate()\n");
    let g = sig(&s, "g");
    assert!(g.unresolved_effects.iter().any(|u| u.reason == "call_method_unknown"
        && u.callee.as_deref() == Some("frobnicate")
        && u.may_affect.contains(&MutationTarget::Param { name: "obj".into() })));
    assert_eq!(g.purity, Purity::Unknown);
}

#[test]
fn aug_assign_on_param_name_is_a_mutation() {
    let s = analyze("def h(p):\n    p += [1]\n    return p\n");
    let h = sig(&s, "h");
    assert!(has_mutation(
        h,
        &MutationTarget::Param { name: "p".into() },
        MutationKind::AugName
    ));
    assert_eq!(h.purity, Purity::Impure);
}

#[test]
fn classmethod_cls_attr_write_is_a_receiver_mutation() {
    let s = analyze(
        "class C:\n\
         \x20   @classmethod\n\
         \x20   def register(cls, x):\n\
         \x20       cls.registry.append(x)\n",
    );
    let m = sig(&s, "register");
    assert!(has_mutation(
        m,
        &MutationTarget::SelfAttr { name: "registry".into() },
        MutationKind::Method
    ));
    // `cls` is the receiver, not a generatable param.
    assert!(!m.params.iter().any(|p| p.name == "cls"));
    assert_eq!(m.purity, Purity::Impure);
}

#[test]
fn unknown_decorator_downgrades_purity() {
    let s = analyze("@some_decorator\ndef g(x):\n    return x\n");
    let g = sig(&s, "g");
    assert_eq!(g.decorators, vec!["some_decorator".to_string()]);
    assert!(
        g.unresolved_effects
            .iter()
            .any(|u| u.reason == "decorator" && u.callee.as_deref() == Some("some_decorator"))
    );
    assert_eq!(g.purity, Purity::Unknown);
}

#[test]
fn assert_yields_explicit_assertion_error() {
    let s = analyze("def f(a, b):\n    assert a > 0\n    return a / b\n");
    let f = sig(&s, "f");
    assert_eq!(f.raises.explicit, vec!["AssertionError".to_string()]);
    assert!(f.raises.implicit.contains(&"ZeroDivisionError".to_string()));
}

#[test]
fn division_yields_implicit_zero_division_error() {
    let s = analyze("def f(a, b):\n    return a / b\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"ZeroDivisionError".to_string()));
    assert!(f.raises.explicit.is_empty());
}

#[test]
fn ordered_compare_on_any_param_yields_implicit_type_error() {
    let s = analyze("def f(a, b):\n    if a > b:\n        return 1\n    return 0\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn ordered_compare_on_pinned_shape_param_still_yields_type_error() {
    // `a`'s shape gets pinned to `int` by this very comparison — the analyzer must not use that
    // self-derived pin to prove the comparison safe. A caller owes the parameter's declared
    // shape nothing, so the may-set still carries `TypeError`.
    let s = analyze("def f(a):\n    if a > 0:\n        return 1\n    return 0\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn mapping_shaped_subscript_read_on_param_yields_both_key_and_index_error() {
    // `d` is a parameter pinned to `Map` by its own `.get(k)` usage; a caller can still pass a
    // sequence, so the may-set must not narrow to `KeyError` alone.
    let s = analyze("def f(d, k):\n    d.get(k)\n    return d[k]\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"KeyError".to_string()));
    assert!(f.raises.implicit.contains(&"IndexError".to_string()));
}

#[test]
fn sequence_shaped_subscript_read_on_param_yields_both_key_and_index_error() {
    // `xs` is a parameter pinned to `Seq` by its own `.append(1)` usage; a caller can still pass
    // a mapping, so the may-set must not narrow to `IndexError` alone.
    let s = analyze("def f(xs, i):\n    xs.append(1)\n    return xs[i]\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"IndexError".to_string()));
    assert!(f.raises.implicit.contains(&"KeyError".to_string()));
}

#[test]
fn subscript_with_any_key_yields_implicit_type_error() {
    let s = analyze("def f(d, k):\n    return d[k]\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn subscript_with_any_base_yields_implicit_type_error() {
    // `xs`'s shape is never pinned by anything but the subscript itself, so the BASE — not
    // just the (here, literal) key — is a genuine `TypeError` candidate: `xs` could be any
    // non-subscriptable runtime value.
    let s = analyze("def g(xs):\n    return xs[0]\n");
    let g = sig(&s, "g");
    assert!(g.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn subscript_with_literal_index_on_pinned_param_base_still_yields_type_error() {
    // `xs` is a parameter, and a parameter's pinned shape (here, from its own `.append(1)`
    // usage) constrains no caller — a caller can still pass a non-subscriptable value, so the
    // BASE remains a `TypeError` candidate even though the literal index itself is not.
    let s = analyze("def g(xs):\n    xs.append(1)\n    return xs[0]\n");
    let g = sig(&s, "g");
    assert!(g.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn list_specific_evidence_on_a_param_still_infers_a_bare_seq() {
    // `.append` is list-specific evidence, not sequence-protocol evidence, so it must not widen
    // to admit `str` — appending to a string isn't valid Python.
    let s = analyze("def f(xs):\n    xs.append(1)\n");
    let f = sig(&s, "f");
    let xs = f.params.iter().find(|p| p.name == "xs").unwrap();
    assert_eq!(xs.shape, Shape::any_seq());
}

#[test]
fn sequence_protocol_evidence_on_a_param_widens_to_admit_str() {
    // `len(p)` and `for c in p` are both sequence-protocol evidence only — a real caller can
    // pass a `str` here just as validly as a `list`, so the inferred shape must admit both.
    let s = analyze("def g(p):\n    n = len(p)\n    for c in p:\n        pass\n    return n\n");
    let g = sig(&s, "g");
    let p = g.params.iter().find(|p| p.name == "p").unwrap();
    assert_eq!(p.shape, Shape::union_of([Shape::any_seq(), Shape::Str]));
}

#[test]
fn subscript_on_local_list_literal_does_not_gain_key_error() {
    // `xs` is a LOCAL built from a list literal, never a parameter — a caller cannot substitute
    // a mapping into it, so its pinned `Seq` shape may still narrow the may-set to `IndexError`
    // alone.
    let s = analyze("def f(i):\n    xs = [1, 2, 3]\n    return xs[i]\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"IndexError".to_string()));
    assert!(!f.raises.implicit.contains(&"KeyError".to_string()));
}

#[test]
fn local_aliasing_a_parameter_is_treated_as_a_parameter_for_type_error() {
    // `y = x` makes `y` alias the parameter `x`; comparing `y` must be treated exactly like
    // comparing `x` directly — the pinned shape still cannot suppress the may-raise.
    let s = analyze("def f(x):\n    y = x\n    if y > 0:\n        return 1\n    return 0\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn int_conversion_yields_implicit_value_error() {
    let s = analyze("def f(s):\n    return int(s)\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"ValueError".to_string()));
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

#[test]
fn nested_container_shape_is_inferred_from_loop_variable_usage() {
    let s = analyze(
        "def f(m):\n\
         \x20   for row in m:\n\
         \x20       s = sum(row)\n\
         \x20   return 0\n",
    );
    let f = sig(&s, "f");
    let m = f.params.iter().find(|p| p.name == "m").unwrap();
    // `m` is a parameter, so the iteration evidence widens to admit `str` too.
    assert_eq!(
        m.shape,
        Shape::union_of([Shape::Seq(Box::new(Shape::any_seq())), Shape::Str])
    );
}

#[test]
fn nested_numeric_container_shape_is_inferred_from_subscript_division() {
    let s = analyze(
        "def g(rows):\n\
         \x20   for r in rows:\n\
         \x20       for i in range(len(r)):\n\
         \x20           r[i] = r[i] / 2\n",
    );
    let s = sig(&s, "g");
    let rows = s.params.iter().find(|p| p.name == "rows").unwrap();
    // `rows` is a parameter, so the iteration evidence widens to admit `str` too.
    assert_eq!(
        rows.shape,
        Shape::union_of([Shape::Seq(Box::new(Shape::Seq(Box::new(Shape::Float)))), Shape::Str])
    );
}

#[test]
fn param_rebound_to_a_list_literal_stops_voting_as_a_sequence() {
    // `x` is reassigned to a brand-new list, unrelated to the caller's argument — the
    // subsequent `.append` describes the new object, not what the caller passed, so `x`'s
    // declared shape must not become `Seq`.
    let s = analyze("def f(x):\n    x = []\n    x.append(1)\n");
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert_eq!(x.shape, Shape::Any);
}

#[test]
fn param_rebound_to_a_constructor_call_stops_voting_as_an_instance() {
    let s = analyze(
        "class Box:\n    pass\n\ndef f(x):\n    x = Box()\n    x.open()\n",
    );
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert_eq!(x.shape, Shape::Any);
}

#[test]
fn param_rebound_to_an_unrelated_name_stops_voting() {
    let s = analyze("def f(x, other):\n    x = other\n    x.append(1)\n    return other\n");
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert_eq!(x.shape, Shape::Any);
}

#[test]
fn self_referential_param_rebind_still_votes() {
    // `x = x.strip()` reads the original `x` before rebinding — the read still describes the
    // caller's argument, so it may still vote (here, toward `Str`).
    let s = analyze("def f(x):\n    x = x.strip()\n    return x\n");
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert_eq!(x.shape, Shape::Str);
}

#[test]
fn param_rebound_to_a_constructor_call_still_resolves_the_post_rebind_method() {
    // The parameter's declared shape stays `Any` (the caller can pass anything — the rebind
    // must not narrow that), but the post-rebind LOCAL fact (`x` really is a `Box` after `x =
    // Box()`) is sound to resolve calls made through the name afterward, same as any other
    // local — flow-sensitive rebind (STATUS.md open work item 1). `Box` declares no `__init__`,
    // so its own constructor call stays unresolved (`resolve_unique` deliberately leaves an
    // inherited/absent `__init__` unresolved); `bump` is a real method on `Box` and must resolve,
    // dropping the `call_method_unknown` acknowledgment. The mutation `bump` performs on its
    // receiver (`self.n = 1`) is on a freshly constructed object, never the caller's — per the
    // `CallReceiver` caller-view taxonomy it stays untracked (no root), so it does not appear in
    // `mutations` either.
    let s = analyze(
        "class Box:\n    def bump(self):\n        self.n = 1\n\ndef f(x):\n    x = Box()\n    x.bump()\n    return x\n",
    );
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert_eq!(x.shape, Shape::Any);
    assert!(
        !f.unresolved_effects.iter().any(|u| u.reason == "call_method_unknown"),
        "bump() must resolve through the post-rebind local fact: {:?}",
        f.unresolved_effects
    );
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_unknown_callee" && u.callee.as_deref() == Some("Box")),
        "Box() itself stays unresolved (no declared __init__): {:?}",
        f.unresolved_effects
    );
    assert!(
        f.mutations.is_empty(),
        "a mutation of a fresh local is invisible to the caller: {:?}",
        f.mutations
    );
}

#[test]
fn pre_rebind_use_stays_frozen_even_though_the_same_name_is_later_rebound() {
    // Soundness regression caught after the flow-sensitive rebind fix shipped: the env is
    // flow-INSENSITIVE (one merged shape per name over the whole function), so a call BEFORE the
    // rebind must not see the post-rebind shape — `x` at `x.bump()` on line 1 could still be
    // anything the caller passed (e.g. `pre_rebind_call(3)`, which raises `AttributeError`).
    // Dominance gate: a call only sees the post-rebind evidence when it sits in a top-level
    // statement strictly AFTER the rebind's own top-level statement (`ShapeState::
    // frozen_dominance`). Mirrors `temp/probe_flow2.py`.
    let s = analyze(concat!(
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
    ));
    let f = sig(&s, "pre_rebind_call");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert_eq!(x.shape, Shape::Any);
    // Exactly one `call_method_unknown` acknowledgment must survive: the PRE-rebind call, which
    // must never resolve through the post-rebind `Box` shape.
    let unknown_method_calls =
        f.unresolved_effects.iter().filter(|u| u.reason == "call_method_unknown").count();
    assert_eq!(
        unknown_method_calls, 1,
        "expected exactly the pre-rebind bump() call to stay unresolved: {:?}",
        f.unresolved_effects
    );
    // The pre-rebind acknowledgment's `may_affect` must still cover the real parameter `x` —
    // dropping it (as the regression did) is what left the runtime `AttributeError` uncovered.
    let pre_rebind_ack = f
        .unresolved_effects
        .iter()
        .find(|u| u.reason == "call_method_unknown")
        .expect("one call_method_unknown acknowledgment");
    assert!(
        pre_rebind_ack.may_affect.contains(&MutationTarget::Param { name: "x".into() }),
        "expected the pre-rebind acknowledgment to cover the parameter: {:?}",
        pre_rebind_ack
    );
    // No mutation may be attributed to the parameter: `bump`'s `self.n` write belongs to the
    // fresh post-rebind `Box`, per the `CallReceiver` caller-view taxonomy — never fabricated
    // onto `x` (the regression reported a bogus `x.n` param mutation here).
    assert!(
        f.mutations.is_empty(),
        "expected no mutation attributed to the parameter: {:?}",
        f.mutations
    );
}

#[test]
fn rebind_inside_a_loop_body_never_qualifies_for_the_dominance_gate() {
    // A rebind nested inside ANY compound statement (here, a `for` loop) never qualifies for the
    // dominance shortcut — on iteration 1, the loop body's `x.bump()` executes BEFORE that same
    // iteration's `x = Box()`, so no top-level statement ordering can prove the rebind already
    // happened. `x` stays fully frozen for the whole function, exactly like before the
    // flow-sensitive rebind fix: `bump()` must never resolve, and the parameter's shape stays
    // `Any`.
    let s = analyze(concat!(
        "class Box:\n",
        "    def bump(self):\n",
        "        self.n = 1\n",
        "\n",
        "def g(x):\n",
        "    for _ in range(2):\n",
        "        x.bump()\n",
        "        x = Box()\n",
    ));
    let f = sig(&s, "g");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert_eq!(x.shape, Shape::Any);
    assert!(
        f.unresolved_effects.iter().any(|u| u.reason == "call_method_unknown"),
        "expected bump() to never resolve inside the loop: {:?}",
        f.unresolved_effects
    );
    assert!(
        f.mutations.is_empty(),
        "expected no mutation attributed to the parameter: {:?}",
        f.mutations
    );
}

#[test]
fn local_rebound_to_a_list_literal_still_infers_a_sequence() {
    // The rebind rule is about parameters, whose caller-supplied value the rebind severs from —
    // a plain local really is what it was last assigned, so `xs = []` still infers `Seq`: the
    // param model has no `shape` field to check directly, so this proves it indirectly the same
    // way `subscript_on_local_list_literal_does_not_gain_key_error` does — a `Seq`-shaped base
    // narrows the subscript's implicit raise to `IndexError` alone, never `KeyError`.
    let s = analyze("def f(i):\n    xs = []\n    xs.append(1)\n    return xs[i]\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"IndexError".to_string()));
    assert!(!f.raises.implicit.contains(&"KeyError".to_string()));
}

#[test]
fn local_helper_mutation_propagates_to_caller() {
    let s = analyze(
        "def helper(xs):\n\
         \x20   xs.append(1)\n\
         def caller(data):\n\
         \x20   helper(data)\n",
    );
    let caller = sig(&s, "caller");
    assert!(has_mutation(
        caller,
        &MutationTarget::Param { name: "data".into() },
        MutationKind::Method
    ));
    assert_eq!(caller.purity, Purity::Impure);
    assert!(caller.unresolved_effects.is_empty());
}

#[test]
fn local_helper_mutation_via_keyword_arg_propagates_to_caller() {
    let s = analyze(
        "def helper(xs):\n\
         \x20   xs.append(1)\n\
         def caller(data):\n\
         \x20   helper(xs=data)\n",
    );
    let caller = sig(&s, "caller");
    assert!(has_mutation(
        caller,
        &MutationTarget::Param { name: "data".into() },
        MutationKind::Method
    ));
    assert_eq!(caller.purity, Purity::Impure);
    assert!(caller.unresolved_effects.is_empty());
}

#[test]
fn star_unpacked_call_to_local_callee_is_acknowledged_not_pure() {
    // `helper(*lst)` hands helper an element of `lst` the positional mapping can't attribute,
    // and the unpack operation itself can raise TypeError — neither may be silently dropped.
    let s = analyze(
        "def helper(xs):\n\
         \x20   xs.append(1)\n\
         def caller(lst):\n\
         \x20   helper(*lst)\n",
    );
    let caller = sig(&s, "caller");
    let u = caller
        .unresolved_effects
        .iter()
        .find(|u| u.reason == "call_unpacked_args")
        .expect("expected a call_unpacked_args acknowledgment");
    assert_eq!(u.callee.as_deref(), Some("helper"));
    assert!(u.may_affect.contains(&MutationTarget::Param { name: "lst".into() }));
    assert!(caller.raises.implicit.iter().any(|r| r == "TypeError"));
    assert_ne!(caller.purity, Purity::Pure);
}

#[test]
fn double_star_unpacked_call_to_local_callee_is_acknowledged_not_pure() {
    let s = analyze(
        "def helper(xs):\n\
         \x20   xs.append(1)\n\
         def caller(d):\n\
         \x20   helper(**d)\n",
    );
    let caller = sig(&s, "caller");
    let u = caller
        .unresolved_effects
        .iter()
        .find(|u| u.reason == "call_unpacked_args")
        .expect("expected a call_unpacked_args acknowledgment");
    assert!(u.may_affect.contains(&MutationTarget::Param { name: "d".into() }));
    assert!(caller.raises.implicit.iter().any(|r| r == "TypeError"));
    assert_ne!(caller.purity, Purity::Pure);
}

#[test]
fn opaque_call_may_affect_includes_keyword_and_unpacked_roots() {
    // An imported callee is opaque: everything handed to it — positionally, by keyword, or
    // unpacked — may be mutated, so all trackable roots belong in the acknowledgment.
    let s = analyze(
        "from ext import sink\n\
         def caller(a, b, c):\n\
         \x20   sink(a, key=b, **c)\n",
    );
    let caller = sig(&s, "caller");
    let u = caller
        .unresolved_effects
        .iter()
        .find(|u| u.reason == "call_import")
        .expect("expected a call_import acknowledgment");
    for name in ["a", "b", "c"] {
        assert!(
            u.may_affect.contains(&MutationTarget::Param { name: name.into() }),
            "expected {name} in may_affect: {:?}",
            u.may_affect
        );
    }
}

#[test]
fn pure_local_helper_call_keeps_caller_pure() {
    let s = analyze(
        "def add_one(x):\n\
         \x20   return x + 1\n\
         def caller(y):\n\
         \x20   return add_one(y)\n",
    );
    let caller = sig(&s, "caller");
    assert!(caller.mutations.is_empty());
    assert!(caller.unresolved_effects.is_empty());
    assert_eq!(caller.purity, Purity::Pure);
}

#[test]
fn call_to_genuinely_unknown_callee_stays_unresolved() {
    let s = analyze("def f(data):\n    unknown_thing(data)\n");
    let f = sig(&s, "f");
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_unknown_callee" && u.callee.as_deref() == Some("unknown_thing"))
    );
    assert_eq!(f.purity, Purity::Unknown);
}

#[test]
fn direct_recursion_terminates_and_is_pure() {
    let s = analyze(
        "def fact(n):\n\
         \x20   if n <= 1:\n\
         \x20       return 1\n\
         \x20   return n * fact(n - 1)\n",
    );
    let f = sig(&s, "fact");
    assert!(f.mutations.is_empty());
    assert!(f.unresolved_effects.is_empty());
    assert_eq!(f.purity, Purity::Pure);
}

#[test]
fn self_method_call_propagates_receiver_mutation_to_caller() {
    let s = analyze(
        "class C:\n\
         \x20   def helper(self, x):\n\
         \x20       self.data.append(x)\n\
         \x20   def caller(self, y):\n\
         \x20       self.helper(y)\n",
    );
    let caller = sig(&s, "caller");
    assert!(has_mutation(
        caller,
        &MutationTarget::SelfAttr { name: "data".into() },
        MutationKind::Method
    ));
    assert_eq!(caller.purity, Purity::Impure);
}

#[test]
fn varargs_and_kwargs_are_marked_by_kind() {
    let s = analyze("def f(a, *args, **kw):\n    return a\n");
    let f = sig(&s, "f");
    let by_name = |n: &str| f.params.iter().find(|p| p.name == n).unwrap();
    assert_eq!(by_name("a").kind, ParamKind::Positional);
    assert_eq!(by_name("args").kind, ParamKind::VarPositional);
    assert_eq!(by_name("kw").kind, ParamKind::VarKeyword);
}

#[test]
fn declared_int_but_returns_str_is_flagged() {
    let s = analyze("def f(x) -> int:\n    return \"a\"\n");
    let f = sig(&s, "f");
    assert_eq!(f.type_mismatches.len(), 1);
    let m = &f.type_mismatches[0];
    assert_eq!(m.kind, "return");
    assert_eq!(m.declared, "int");
    assert_eq!(m.inferred, vec![ReturnKind::Str]);
}

#[test]
fn opaque_inferred_return_is_never_flagged() {
    let s = analyze("def f(x) -> int:\n    return x\n");
    let f = sig(&s, "f");
    assert!(f.type_mismatches.is_empty());
}

#[test]
fn matching_declared_and_inferred_return_is_not_flagged() {
    let s = analyze("def f(x) -> list:\n    return []\n");
    let f = sig(&s, "f");
    assert!(f.type_mismatches.is_empty());
}

#[test]
fn unannotated_function_is_never_flagged() {
    let s = analyze("def f(x):\n    return \"a\"\n");
    let f = sig(&s, "f");
    assert!(f.type_mismatches.is_empty());
}

#[test]
fn equality_guard_records_literal_sample() {
    let s = analyze("def f(x):\n    if x == 42:\n        return 1\n    return 0\n");
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert!(
        x.guard_samples.contains(&serde_json::json!(42)),
        "expected 42 among guard samples: {:?}",
        x.guard_samples
    );
}

#[test]
fn branch_local_rebind_of_a_param_drops_its_shape_rather_than_unioning() {
    // `x` is pinned `Int` via the `> 0` comparison, then rebound to `None` (an unrelated value)
    // on the else branch — the rebind isn't a caller-observed fact, so it must not be unioned
    // into the param's declared shape either; the whole hypothesis is dropped to `Any`. (PEP 604
    // union rendering itself is covered directly in `src/stub/mod.rs`'s
    // `union_shape_param_renders_pep604`.)
    let s = analyze("def f(x):\n    if x > 0:\n        pass\n    else:\n        x = None\n    return x\n");
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert_eq!(x.shape, Shape::Any);
}

#[test]
fn declared_str_param_but_inferred_int_is_flagged() {
    // `x - 1` pins `x`'s shape to `Int`, fully disjoint from the declared `str` annotation.
    let s = analyze("def f(x: str):\n    return x - 1\n");
    let f = sig(&s, "f");
    let m = f
        .type_mismatches
        .iter()
        .find(|m| m.kind == "param")
        .expect("expected a param type mismatch");
    assert_eq!(m.param.as_deref(), Some("x"));
    assert_eq!(m.declared, "str");
    assert_eq!(m.inferred_shape, Some(Shape::Int));
}

#[test]
fn compatible_declared_and_inferred_param_is_not_flagged() {
    let s = analyze("def f(x: int):\n    return x - 1\n");
    let f = sig(&s, "f");
    assert!(f.type_mismatches.iter().all(|m| m.kind != "param"));
}

#[test]
fn bool_inferred_against_declared_int_is_not_flagged() {
    // `bool` is an `int` subtype (PEP 484 numeric tower): rebinding an `int` param to a
    // comparison result, or returning a `bool` from an `-> int` function, is not a mismatch.
    let s = analyze("def f(n: int) -> int:\n    n = n > 0\n    return n\n");
    let f = sig(&s, "f");
    assert!(f.type_mismatches.is_empty());
}

#[test]
fn ordered_guard_records_boundary_neighbors() {
    let s = analyze("def f(x):\n    if x > 10:\n        return 1\n    return 0\n");
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert!(
        x.guard_samples.contains(&serde_json::json!(10)),
        "expected 10 among guard samples: {:?}",
        x.guard_samples
    );
    assert!(
        x.guard_samples.contains(&serde_json::json!(11)),
        "expected 11 among guard samples: {:?}",
        x.guard_samples
    );
}

#[test]
fn local_instance_method_call_resolves_but_reports_no_mutation() {
    // `account` is a fresh local, not a parameter: the caller can never observe its mutation
    // (exactly like a fresh local list/dict — see `local_collection_mutation_is_not_an_effect`),
    // so the resolved call must not fabricate a mutation onto a name that isn't a real parameter.
    let s = analyze(
        "class Account:\n\
         \x20   def __init__(self):\n\
         \x20       self.balance = 0\n\
         \x20   def deposit(self, amount):\n\
         \x20       self.balance += amount\n\
         def f(amount):\n\
         \x20   account = Account()\n\
         \x20   account.deposit(amount)\n",
    );
    let f = sig(&s, "f");
    // Not an opaque, unresolved call: the deposit method resolved, so no
    // `call_method_unknown` for it.
    assert!(!f.unresolved_effects.iter().any(|u| u.reason == "call_method_unknown"));
    assert!(f.mutations.is_empty(), "expected no mutations, got {:?}", f.mutations);
}

#[test]
fn constructor_call_to_same_module_class_is_not_an_unknown_callee() {
    let s = analyze(
        "class Account:\n\
         \x20   def __init__(self):\n\
         \x20       self.balance = 0\n\
         def f():\n\
         \x20   account = Account()\n\
         \x20   return account\n",
    );
    let f = sig(&s, "f");
    assert!(!f.unresolved_effects.iter().any(|u| u.reason == "call_unknown_callee"));
}

#[test]
fn raise_propagates_through_a_resolved_constructor_and_a_resolved_method_call() {
    let s = analyze(
        "class Account:\n\
         \x20   def __init__(self, amount):\n\
         \x20       if amount < 0:\n\
         \x20           raise ValueError('negative')\n\
         \x20       self.balance = amount\n\
         \x20   def deposit(self, amount):\n\
         \x20       if amount < 0:\n\
         \x20           raise ValueError('negative')\n\
         \x20       self.balance += amount\n\
         def f(amount):\n\
         \x20   account = Account(amount)\n\
         \x20   account.deposit(amount)\n",
    );
    let f = sig(&s, "f");
    // Both the constructor's and `deposit`'s `ValueError` propagate to the caller, though
    // neither call attributes a mutation (the receiver is always a fresh, caller-invisible
    // local) — the point of resolving these calls is the raise (and any io/global-write),
    // not the mutation, which genuinely can't reach the caller.
    assert!(f.raises.explicit.contains(&"ValueError".to_string())
        || f.raises.implicit.contains(&"ValueError".to_string()));
    assert!(f.mutations.is_empty());
    assert!(!f.unresolved_effects.iter().any(|u| u.reason == "call_unknown_callee"));
}

#[test]
fn union_of_two_instances_does_not_resolve_the_method_call() {
    let s = analyze(
        "class Account:\n\
         \x20   def label(self, tag):\n\
         \x20       return tag\n\
         class Basket:\n\
         \x20   def label(self, tag):\n\
         \x20       return tag\n\
         def f(flag, tag):\n\
         \x20   if flag:\n\
         \x20       holder = Account()\n\
         \x20   else:\n\
         \x20       holder = Basket()\n\
         \x20   holder.label(tag)\n",
    );
    let f = sig(&s, "f");
    // A union receiver must NOT resolve — it stays an unresolved, opaque call.
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_method_unknown" && u.callee.as_deref() == Some("label"))
    );
}

#[test]
fn inherited_method_does_not_resolve() {
    let s = analyze(
        "class Base:\n\
         \x20   def run(self):\n\
         \x20       return 1\n\
         class Sub(Base):\n\
         \x20   pass\n\
         def f():\n\
         \x20   obj = Sub()\n\
         \x20   obj.run()\n",
    );
    let f = sig(&s, "f");
    // `run` is declared on `Base`, not `Sub` — `resolve_unique(decls, Some("Sub"), "run")`
    // finds nothing, so the call stays unresolved rather than misattributing to `Base`.
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_method_unknown" && u.callee.as_deref() == Some("run"))
    );
}

#[test]
fn unbound_superclass_call_on_a_locally_declared_class_resolves() {
    let s = analyze(
        "class BaseError(Exception):\n\
         \x20   def __init__(self, code):\n\
         \x20       if code < 0:\n\
         \x20           raise ValueError('bad code')\n\
         \x20       self.code = code\n\
         class SpecificError(BaseError):\n\
         \x20   def __init__(self, code, msg):\n\
         \x20       BaseError.__init__(self, code)\n\
         \x20       self.msg = msg\n",
    );
    let sub_init = s
        .iter()
        .find(|f| f.name == "__init__" && f.owner.as_deref() == Some("SpecificError"))
        .expect("SpecificError.__init__ signature");
    // `BaseError.__init__(self, code)` is the unbound-superclass form: `BaseError` is a
    // class declared in this module and the receiver argument is the caller's own `self`, so
    // it resolves like `self.__init__(...)` would — `BaseError.__init__`'s `ValueError`
    // propagates, and the call leaves no `call_method_unknown` acknowledgment.
    assert!(sub_init.raises.implicit.contains(&"ValueError".to_string()));
    assert!(!sub_init.unresolved_effects.iter().any(|u| u.reason == "call_method_unknown"));
}

#[test]
fn unbound_call_on_an_undeclared_class_stays_unresolved() {
    let s = analyze(
        "class SpecificError(Exception):\n\
         \x20   def __init__(self, code):\n\
         \x20       Exception.__init__(self, code)\n",
    );
    // `Exception` is a builtin, not declared in this module, so the unbound-superclass form
    // must NOT resolve — it stays an opaque, acknowledged call.
    let init = sig(&s, "__init__");
    assert!(
        init.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_method_unknown" && u.callee.as_deref() == Some("__init__"))
    );
}

#[test]
fn unbound_call_with_a_non_receiver_first_arg_stays_unresolved() {
    let s = analyze(
        "class BaseError(Exception):\n\
         \x20   def __init__(self, code):\n\
         \x20       self.code = code\n\
         class SpecificError(BaseError):\n\
         \x20   def __init__(self, other, code):\n\
         \x20       BaseError.__init__(other, code)\n",
    );
    // The first positional argument is `other`, not this method's own receiver `self` — the
    // unbound-superclass form must not resolve on a mismatched receiver.
    let sub_init = s
        .iter()
        .find(|f| f.name == "__init__" && f.owner.as_deref() == Some("SpecificError"))
        .expect("SpecificError.__init__ signature");
    assert!(
        sub_init
            .unresolved_effects
            .iter()
            .any(|u| u.reason == "call_method_unknown" && u.callee.as_deref() == Some("__init__"))
    );
}

#[test]
fn imported_class_does_not_infer_an_instance_shape() {
    let s = analyze("from somewhere import Widget\ndef f():\n    w = Widget()\n    w.render()\n");
    let f = sig(&s, "f");
    // `Widget` isn't declared in this module (it's imported), so the constructor call is a
    // foreign effect, not a same-module class — its shape stays `Any`, so `w.render()` can never
    // resolve through `Shape::Instance` (it still reaches the generic `call_method_unknown`
    // acknowledgment for an unrecognized method, same as any other unresolved receiver).
    assert!(f.unresolved_effects.iter().any(|u| u.reason == "call_import" && u.callee.as_deref() == Some("Widget")));
    assert!(f.unresolved_effects.iter().any(|u| u.reason == "call_method_unknown"));
}

#[test]
fn modelled_stdlib_call_adds_raises_and_drops_the_unresolved_acknowledgment() {
    let s = analyze("import os.path\ndef f(a, b):\n    return os.path.join(a, b)\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
    assert!(f.raises.implicit.contains(&"AttributeError".to_string()));
    assert!(
        !f.unresolved_effects.iter().any(|u| u.reason == "call_import"),
        "a modelled call must not also leave a call_import acknowledgment: {:?}",
        f.unresolved_effects
    );
}

#[test]
fn aliased_stdlib_import_resolves_to_the_same_model_entry() {
    // `import os.path as p; p.join(...)` must find the `os.path.join` entry via the resolved
    // module path, not the `p.join` call-site text.
    let s = analyze("import os.path as p\ndef f(a, b):\n    return p.join(a, b)\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
    assert!(f.raises.implicit.contains(&"AttributeError".to_string()));
    assert!(
        !f.unresolved_effects.iter().any(|u| u.reason == "call_import"),
        "the aliased call must resolve to the os.path.join entry: {:?}",
        f.unresolved_effects
    );
}

#[test]
fn aliased_from_import_resolves_to_the_same_model_entry() {
    // `from os.path import join as j; j(...)` must find the `os.path.join` entry through the
    // original imported name, not the local alias `j` (which would look up the nonexistent
    // `os.path.j`) — see `ModuleAnalysis::import_names`.
    let s = analyze("from os.path import join as j\ndef f(a, b):\n    return j(a, b)\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
    assert!(f.raises.implicit.contains(&"AttributeError".to_string()));
    assert!(
        !f.unresolved_effects.iter().any(|u| u.reason == "call_import"),
        "the aliased from-import call must resolve to the os.path.join entry: {:?}",
        f.unresolved_effects
    );
    assert_eq!(f.purity, Purity::Pure);
}

#[test]
fn in_body_import_raises_import_error_and_module_not_found_error() {
    // An `import`/`from ... import ...` statement inside a function body only runs when the
    // function is called, so a missing module surfaces as `ImportError`/`ModuleNotFoundError`
    // at call time — unlike a module-level import, which fails the whole module at load time.
    let s = analyze("def f():\n    import matplotlib\n    return matplotlib.plot([])\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"ImportError".to_string()));
    assert!(f.raises.implicit.contains(&"ModuleNotFoundError".to_string()));
}

#[test]
fn in_body_from_import_raises_import_error_and_module_not_found_error() {
    let s = analyze("def f():\n    from yaml import safe_load\n    return safe_load('')\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"ImportError".to_string()));
    assert!(f.raises.implicit.contains(&"ModuleNotFoundError".to_string()));
}

#[test]
fn module_level_import_does_not_add_the_implicit_import_raise() {
    // A module-level import fails the whole module at load time (uncallable), not per-call — it
    // must not contribute to a function's raise may-set.
    let s = analyze("import os.path\ndef f(a, b):\n    return os.path.join(a, b)\n");
    let f = sig(&s, "f");
    assert!(!f.raises.implicit.contains(&"ImportError".to_string()));
    assert!(!f.raises.implicit.contains(&"ModuleNotFoundError".to_string()));
}

#[test]
fn unmodelled_member_of_a_modelled_namespace_stays_unresolved() {
    // `os.path.*` is modelled per-name (no namespace fallback), so a member absent from the
    // table (e.g. `samestat`) must stay an unresolved `call_import`, not silently pass through.
    let s = analyze("import os.path\ndef f(a, b):\n    return os.path.samestat(a, b)\n");
    let f = sig(&s, "f");
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_import" && u.callee.as_deref() == Some("os.path.samestat")),
        "expected samestat to remain unresolved: {:?}",
        f.unresolved_effects
    );
}

#[test]
fn do_not_model_list_entry_stays_unresolved() {
    // `sys.audit` invokes arbitrary hooks — explicitly excluded from the table.
    let s = analyze("import sys\ndef f():\n    sys.audit('event')\n");
    let f = sig(&s, "f");
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_import" && u.callee.as_deref() == Some("sys.audit")),
        "sys.audit must stay unresolved: {:?}",
        f.unresolved_effects
    );
}

#[test]
fn project_local_import_never_matches_a_stdlib_model() {
    // `util.*` ranks high in the stdlib corpus only because it's a relative import inside stdlib
    // packages — a project's own `util` module must never accidentally match a model entry.
    let s = analyze("import util\ndef f(x):\n    return util.helper(x)\n");
    let f = sig(&s, "f");
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_import" && u.callee.as_deref() == Some("util.helper")),
        "util.helper must stay unresolved: {:?}",
        f.unresolved_effects
    );
}

#[test]
fn param_named_url_passed_to_urlparse_gets_url_tag() {
    let s = analyze(
        "from urllib.parse import urlparse\ndef f(url):\n    return urlparse(url)\n",
    );
    let f = sig(&s, "f");
    let p = f.params.iter().find(|p| p.name == "url").unwrap();
    assert!(
        p.hints.iter().any(|h| h == "url"),
        "expected the url tag: {:?}",
        p.hints
    );
}

#[test]
fn param_with_no_evidence_gets_no_hints() {
    let s = analyze("def f(x):\n    return x + 1\n");
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert!(x.hints.is_empty(), "expected no hints: {:?}", x.hints);
}

#[test]
fn hints_never_change_purity_or_raises() {
    let with_hint = analyze("def f(s):\n    return int(s)\n");
    let without_hint = analyze("def f(z):\n    return int(z)\n");
    let f_hint = sig(&with_hint, "f");
    let f_plain = sig(&without_hint, "f");
    assert_eq!(f_hint.purity, f_plain.purity);
    assert_eq!(f_hint.raises, f_plain.raises);
    let s = f_hint.params.iter().find(|p| p.name == "s").unwrap();
    assert!(
        s.hints.iter().any(|h| h == "numeric_str"),
        "expected the numeric_str tag: {:?}",
        s.hints
    );
}

#[test]
fn hints_field_omitted_from_json_when_empty() {
    let s = analyze("def f(x):\n    return x + 1\n");
    let f = sig(&s, "f");
    let json = serde_json::to_value(f).unwrap();
    let param = &json["params"][0];
    assert!(
        param.get("hints").is_none(),
        "expected `hints` omitted from JSON when empty: {param}"
    );
}

#[test]
fn attribute_load_on_an_any_shaped_parameter_predicts_attribute_error() {
    let s = analyze("def f(fast):\n    return fast.next\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"AttributeError".to_string()));
}

#[test]
fn self_attribute_load_of_a_declared_attribute_is_proven_safe() {
    let s = analyze(
        "class Node:\n    def __init__(self, x):\n        self.x = x\n    def get(self):\n        return self.x\n",
    );
    let get = s.iter().find(|f| f.name == "get" && f.owner.as_deref() == Some("Node")).unwrap();
    assert!(
        !get.raises.implicit.contains(&"AttributeError".to_string()),
        "self.x is declared by Node's own __init__, so the load is proven: {:?}",
        get.raises.implicit
    );
}

#[test]
fn self_attribute_load_of_an_undeclared_attribute_predicts_attribute_error() {
    let s = analyze(
        "class Node:\n    def get(self):\n        return self.missing\n",
    );
    let get = s.iter().find(|f| f.name == "get" && f.owner.as_deref() == Some("Node")).unwrap();
    assert!(
        get.raises.implicit.contains(&"AttributeError".to_string()),
        "Node never declares `missing`: {:?}",
        get.raises.implicit
    );
}

#[test]
fn attribute_load_on_a_local_instance_with_the_declared_attribute_is_proven_safe() {
    let s = analyze(
        "class Box:\n    def __init__(self):\n        self.n = 0\n\ndef f():\n    b = Box()\n    return b.n\n",
    );
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"AttributeError".to_string()),
        "b is a local, proven Box() instance that declares n: {:?}",
        f.raises.implicit
    );
}

#[test]
fn attribute_load_on_a_parameter_with_an_instance_looking_shape_still_predicts_attribute_error() {
    // A parameter's inferred shape is a hypothesis built from how this function's own body
    // happens to use it, never a guarantee a caller is bound by — see the module doc's soundness
    // rule. `p`'s only evidence for `Shape::Instance("Box")` is this very constructor call inside
    // the function, so it must not be used to prove the load below it safe.
    let s = analyze(
        "class Box:\n    def __init__(self):\n        self.n = 0\n\ndef f(p):\n    p = Box()\n    return p.n\n",
    );
    let f = sig(&s, "f");
    assert!(
        f.raises.implicit.contains(&"AttributeError".to_string()),
        "a rebound parameter never proves the load safe: {:?}",
        f.raises.implicit
    );
}

#[test]
fn bare_len_call_predicts_type_error() {
    let s = analyze("def f(p):\n    return len(p)\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn self_attribute_assigned_outside_init_never_proves_the_load_safe() {
    // Mirrors `temp/probe_selfattr.py`: `Gadget.arm()` sets `self.v`, but a fresh instance can
    // reach `read()` without ever calling `arm()` first — that's a real `AttributeError`, so
    // "declared anywhere in the class's methods" was unsound. Only `__init__` proves an
    // attribute; `Safe.__init__` assigns `self.v`, so `Safe.read` stays proven.
    let s = analyze(
        "class Gadget:\n\
         \x20   def arm(self):\n\
         \x20       self.v = 1\n\
         \x20   def read(self):\n\
         \x20       return self.v\n\
         \n\
         class Safe:\n\
         \x20   def __init__(self):\n\
         \x20       self.v = 0\n\
         \x20   def read(self):\n\
         \x20       return self.v\n",
    );
    let gadget_read = s.iter().find(|f| f.name == "read" && f.owner.as_deref() == Some("Gadget")).unwrap();
    assert!(
        gadget_read.raises.implicit.contains(&"AttributeError".to_string()),
        "arm() assigning self.v proves nothing for read(): {:?}",
        gadget_read.raises.implicit
    );
    let safe_read = s.iter().find(|f| f.name == "read" && f.owner.as_deref() == Some("Safe")).unwrap();
    assert!(
        !safe_read.raises.implicit.contains(&"AttributeError".to_string()),
        "Safe.__init__ assigns self.v unconditionally: {:?}",
        safe_read.raises.implicit
    );
}

#[test]
fn a_local_len_shadow_suppresses_the_builtin_raise_prediction() {
    let s = analyze("def f(p):\n    def len(x):\n        return 0\n    return len(p)\n");
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"TypeError".to_string()),
        "a nested def len(...) shadows the builtin, so its raise profile no longer applies: {:?}",
        f.raises.implicit
    );
}

#[test]
fn tuple_unpack_of_a_parameter_predicts_value_error() {
    let s = analyze("def f(pair):\n    a, b = pair\n    return a\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"ValueError".to_string()));
}

#[test]
fn tuple_unpack_of_a_matching_literal_is_proven_safe() {
    let s = analyze("def f():\n    a, b = (1, 2)\n    return a\n");
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"ValueError".to_string()),
        "(1, 2) is a literal 2-tuple matching the 2-target pattern: {:?}",
        f.raises.implicit
    );
}

#[test]
fn for_loop_unpack_of_a_parameter_predicts_value_error() {
    let s = analyze("def f(pairs):\n    for a, b in pairs:\n        return a\n    return None\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"ValueError".to_string()));
}

#[test]
fn for_loop_unpack_of_matching_literal_tuples_is_proven_safe() {
    let s = analyze(
        "def f():\n    for a, b in [(1, 2), (3, 4)]:\n        return a\n    return None\n",
    );
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"ValueError".to_string()),
        "every literal item is a matching 2-tuple: {:?}",
        f.raises.implicit
    );
}

#[test]
fn for_loop_over_a_parameter_predicts_type_error() {
    let s = analyze("def f(xs):\n    for x in xs:\n        return x\n    return None\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn for_loop_over_a_literal_list_is_proven_iterable() {
    let s = analyze("def f():\n    for x in [1, 2, 3]:\n        return x\n    return None\n");
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"TypeError".to_string()),
        "a literal list display is always iterable: {:?}",
        f.raises.implicit
    );
}

#[test]
fn for_loop_over_a_seq_shaped_local_is_proven_iterable() {
    let s = analyze("def f():\n    xs = [1, 2, 3]\n    for x in xs:\n        return x\n    return None\n");
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"TypeError".to_string()),
        "xs is a local pinned to a Seq shape: {:?}",
        f.raises.implicit
    );
}

#[test]
fn left_shift_by_a_parameter_predicts_value_error() {
    let s = analyze("def f(n, i):\n    return n << i\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"ValueError".to_string()));
}

#[test]
fn left_shift_by_a_nonnegative_literal_is_proven_safe() {
    let s = analyze("def f(n):\n    return n << 3\n");
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"ValueError".to_string()),
        "3 is a proven non-negative literal shift count: {:?}",
        f.raises.implicit
    );
}

#[test]
fn right_shift_by_a_negative_literal_still_predicts_value_error() {
    // A negative literal is a `UnaryOp(USub, ...)` node, not a bare `NumberLiteral` — it must
    // NOT be mistaken for a proven non-negative shift count.
    let s = analyze("def f(n):\n    return n >> -1\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"ValueError".to_string()));
}

#[test]
fn readonly_method_call_with_a_parameter_argument_predicts_type_error() {
    let s = analyze("def f(s, x):\n    return s.count(x)\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn readonly_method_call_with_a_string_literal_argument_is_proven_safe() {
    let s = analyze("def f(s):\n    return s.count('a')\n");
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"TypeError".to_string()),
        "'a' is a proven str literal argument to count(): {:?}",
        f.raises.implicit
    );
}

#[test]
fn zero_argument_readonly_method_call_never_predicts_type_error() {
    let s = analyze("def f(s):\n    return s.lower()\n");
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"TypeError".to_string()),
        "lower() takes no arguments, so it has no argument contract: {:?}",
        f.raises.implicit
    );
}

#[test]
fn join_with_a_non_literal_argument_predicts_type_error() {
    let s = analyze("def f(sep, xs):\n    return sep.join(xs)\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn join_with_a_literal_list_of_string_literals_is_proven_safe() {
    let s = analyze("def f(sep):\n    return sep.join(['a', 'b'])\n");
    let f = sig(&s, "f");
    assert!(
        !f.raises.implicit.contains(&"TypeError".to_string()),
        "['a', 'b'] is a proven literal list of str literals: {:?}",
        f.raises.implicit
    );
}

fn has_relation(s: &EffectSignature, left: (&str, bool), right: (&str, bool), kind: RelationKind) -> bool {
    s.param_relations.iter().any(|r| {
        r.left == ParamRef { param: left.0.into(), element: left.1 }
            && r.right == ParamRef { param: right.0.into(), element: right.1 }
            && r.kind == kind
    })
}

#[test]
fn find_closest_element_relates_arr_element_to_target() {
    let s = analyze(
        "def find_closest_element(arr, target):\n\
         \x20   left, right = 0, len(arr) - 1\n\
         \x20   best = None\n\
         \x20   while left <= right:\n\
         \x20       mid = (left + right) // 2\n\
         \x20       if arr[mid] == target:\n\
         \x20           return arr[mid]\n\
         \x20       if best is None or abs(arr[mid] - target) < abs(best - target):\n\
         \x20           best = arr[mid]\n\
         \x20       if arr[mid] < target:\n\
         \x20           left = mid + 1\n\
         \x20       else:\n\
         \x20           right = mid - 1\n\
         \x20   return best\n",
    );
    let f = sig(&s, "find_closest_element");
    assert!(has_relation(f, ("arr", true), ("target", false), RelationKind::Eq));
    assert!(has_relation(f, ("arr", true), ("target", false), RelationKind::Order));
    assert!(has_relation(f, ("arr", true), ("target", false), RelationKind::Arith));
    assert!(
        !f.param_relations.iter().any(|r| r.left.param == r.right.param),
        "no relation should relate a parameter to itself: {:?}",
        f.param_relations
    );
}

#[test]
fn for_loop_over_bare_param_relates_element_to_other_param() {
    let s = analyze("def f(xs, t):\n    for x in xs:\n        if x > t: return x\n    return None\n");
    let f = sig(&s, "f");
    assert!(has_relation(f, ("xs", true), ("t", false), RelationKind::Order));
}

#[test]
fn comprehension_over_local_index_records_no_relation() {
    let s = analyze("def g(a, b, n):\n    return [(a + i, b) for i in range(n)]\n");
    let f = sig(&s, "g");
    assert!(
        f.param_relations.is_empty(),
        "a local loop index should not relate to a parameter: {:?}",
        f.param_relations
    );
}

#[test]
fn slice_comparison_records_no_relation() {
    let s = analyze("def h(a, b):\n    return a[1:] == b\n");
    let f = sig(&s, "h");
    assert!(f.param_relations.is_empty(), "a slice is not an element: {:?}", f.param_relations);
}

#[test]
fn param_relations_are_not_serialized() {
    let s = analyze(
        "def find_closest_element(arr, target):\n\
         \x20   if arr[0] == target:\n\
         \x20       return arr[0]\n\
         \x20   return None\n",
    );
    let f = sig(&s, "find_closest_element");
    assert!(!f.param_relations.is_empty());
    let value = serde_json::to_value(f).expect("serialize");
    assert!(
        value.as_object().unwrap().get("param_relations").is_none(),
        "param_relations must not appear in the JSON output: {value}"
    );
}
