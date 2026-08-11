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
fn comprehension_call_is_descended_into() {
    let s = analyze("def f(items):\n    return [helper(x) for x in items]\n");
    let f = sig(&s, "f");
    assert!(
        f.unresolved_effects
            .iter()
            .any(|u| u.reason == "call_unknown_callee" && u.callee.as_deref() == Some("helper"))
    );
    assert_eq!(f.purity, Purity::Unknown);
    // The comprehension's `for` iterable votes shape like an ordinary `for` loop's.
    assert!(f.params.iter().any(|p| p.name == "items" && p.shape == Shape::any_seq()));
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
    assert_eq!(m.shape, Shape::Seq(Box::new(Shape::any_seq())));
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
    assert_eq!(
        rows.shape,
        Shape::Seq(Box::new(Shape::Seq(Box::new(Shape::Float))))
    );
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
fn conflicting_param_shapes_join_to_union_and_render_pep604() {
    // `x` is pinned `Int` via the `> 0` comparison against a numeric literal, then rebound to
    // `None` on the else branch — two disjoint pieces of evidence for the same param, so the
    // fixpoint join must produce `Union([Int, None])` rather than collapsing to `Any`.
    let s = analyze("def f(x):\n    if x > 0:\n        pass\n    else:\n        x = None\n    return x\n");
    let f = sig(&s, "f");
    let x = f.params.iter().find(|p| p.name == "x").unwrap();
    assert_eq!(x.shape, Shape::Union(vec![Shape::Int, Shape::None]));

    let stub = pylens::stub::render_stub(&s);
    assert!(
        stub.contains("x: int | None"),
        "expected PEP 604 union rendering in stub: {stub}"
    );
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
