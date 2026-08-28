//! Static raise models for shadowless builtin calls: `len(x)`, `int(x)`, `sorted(xs)`, and the
//! like raise predictably on a bad argument even though the analyzer can't see inside CPython's
//! C implementation. Keyed on the **bare builtin name** (never a module path — builtins aren't
//! imported), and consulted only when a call's callee is a bare `Name` that (a) matches this
//! table and (b) isn't shadowed by a parameter, a local rebind, an import, a nested `def`/
//! `class`, or a module-level `def`/`class` of the same name — see
//! `passes::effects::calls::visit_call`'s bare-`Name` arm, the sole caller, and
//! `FunctionFacts::builtin_shadowed`.
//!
//! Every entry over-approximates on purpose, same as `models.rs`: `len(x)` predicts `TypeError`
//! even when `x`'s inferred shape already proves it's a sized container (a parameter's inferred
//! shape is a hypothesis, never a guarantee — see the module doc's soundness rule), and
//! `max`/`min`/`sorted`/`sum` predict `ValueError` even on a call the analyzer could show always
//! passes a non-empty literal. This tolerable slack is the same trade the stdlib model table
//! makes; the alternative (trying to disprove each case) risks the one mistake that matters far
//! more — predicting *nothing* for a real raise.

/// The implicit raise names a call to the shadowless builtin `name` may contribute, if any.
/// `&[]` means `name` either isn't modelled or is one of `builtins.rs`'s known-pure entries this
/// table deliberately leaves alone (`bool`, `hash`, `isinstance`, `type`, ...).
pub(in crate::analyze) fn lookup(name: &str) -> &'static [&'static str] {
    match name {
        "len" => &["TypeError"],
        "int" | "float" => &["TypeError", "ValueError"],
        "ord" | "chr" => &["TypeError", "ValueError"],
        "sorted" | "max" | "min" => &["TypeError", "ValueError"],
        "sum" => &["TypeError"],
        "abs" | "round" => &["TypeError"],
        "range" => &["TypeError", "ValueError"],
        "divmod" => &["TypeError", "ZeroDivisionError"],
        // `pow(base, exp)` raises `ZeroDivisionError` for a negative `exp` on a zero `base`; the
        // 3-argument form additionally raises `ValueError` for a zero modulus or a base that
        // isn't invertible modulo it.
        "pow" => &["TypeError", "ZeroDivisionError", "ValueError"],
        "next" => &["StopIteration", "TypeError"],
        "str" | "repr" | "list" | "tuple" | "set" | "dict" => &["TypeError"],
        "zip" | "enumerate" => &["TypeError"],
        "getattr" | "setattr" => &["AttributeError", "TypeError"],
        _ => &[],
    }
}
