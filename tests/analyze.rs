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
fn ordered_compare_on_pinned_shape_param_yields_no_type_error() {
    let s = analyze("def f(a):\n    if a > 0:\n        return 1\n    return 0\n");
    let f = sig(&s, "f");
    assert!(!f.raises.implicit.contains(&"TypeError".to_string()));
}

#[test]
fn mapping_shaped_subscript_read_yields_key_error() {
    let s = analyze("def f(d, k):\n    d.get(k)\n    return d[k]\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"KeyError".to_string()));
    assert!(!f.raises.implicit.contains(&"IndexError".to_string()));
}

#[test]
fn sequence_shaped_subscript_read_yields_index_error() {
    let s = analyze("def f(xs, i):\n    xs.append(1)\n    return xs[i]\n");
    let f = sig(&s, "f");
    assert!(f.raises.implicit.contains(&"IndexError".to_string()));
    assert!(!f.raises.implicit.contains(&"KeyError".to_string()));
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
fn subscript_with_literal_index_on_pinned_base_yields_no_type_error() {
    let s = analyze("def g(xs):\n    xs.append(1)\n    return xs[0]\n");
    let g = sig(&s, "g");
    assert!(!g.raises.implicit.contains(&"TypeError".to_string()));
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
