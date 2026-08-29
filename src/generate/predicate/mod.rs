//! Predicate-targeted synthesis for `record --cover-branches`: shared types and public surface.
//! Extraction of handled branch-test forms lives in [`extraction`], value construction in
//! [`synthesis`]. Sound by construction: synthesized inputs run through the sandbox like any
//! other case, so a wrong guess only wastes budget.

use std::collections::HashMap;

/// How a comparison's derived value relates to the named parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Derivation {
    /// The parameter's own value.
    Direct,
    /// `len(p)`.
    Len,
    /// `p[i]`, `i` a literal integer index.
    Index(i64),
    /// `p % k`, `k` a literal integer modulus.
    Mod(i64),
    /// A loop-bound element of the parameter (`for x in p:`, or a tuple unpack of that whole
    /// element, `for t in p: a, b = t` / `for a, b in p:`). `field` is `None` for a plain,
    /// un-unpacked loop target and `Some(i)` for field `i` of an `arity`-wide tuple unpack;
    /// `arity` is 1 for a plain target. Synthesis builds a one-element list around the field's
    /// value (see [`element_value`]) — never the empty list, so the `for` body actually runs.
    Element { field: Option<usize>, arity: usize },
    /// `p.split(sep)` (`sep` a string literal) or `p.split()` (`None`, whitespace) — the whole
    /// result list, bound to a local by `parts = p.split(sep)`.
    Split(Option<String>),
    /// `len(parts)` where `parts` is [`Derivation::Split`]-bound — the part count.
    SplitLen(Option<String>),
    /// A loop element of a [`Derivation::Split`]-bound local (`for part in parts:`) — the
    /// two-level `param -> split -> element` chain; nesting stops here.
    SplitElement(Option<String>),
    /// `len(part)` where `part` is [`Derivation::SplitElement`]-bound.
    SplitElementLen(Option<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Str(String),
}

/// A no-argument or single-string-literal-argument string method called on a parameter — the
/// method forms this module recognizes as a branch predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrMethod {
    StartsWith,
    EndsWith,
    IsDigit,
    IsAlpha,
    IsUpper,
    IsLower,
    IsSpace,
    IsAlnum,
}

/// One handled predicate, extracted from a branch test. Every variant but
/// [`Predicate::ParamCompare`] names exactly one parameter (see [`Predicate::param`]);
/// `ParamCompare` coordinates two, and is synthesized separately by
/// [`synthesize_pair`] since satisfying it means overriding both parameters' slots together in
/// one input, not choosing one value in isolation.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Compare { param: String, deriv: Derivation, op: CmpOp, literal: Literal },
    /// A bare-name truthiness test (`if p:`), including a local assigned directly or via `len`
    /// from a parameter.
    Truthy { param: String },
    /// `p in C` / `p not in C` against a literal container.
    Membership { param: String, negated: bool, items: Vec<Literal> },
    /// `v in p` / `v not in p` — `p` is the container (a parameter), `v` a literal tested for
    /// membership.
    ContainerMembership { param: String, negated: bool, literal: Literal },
    /// A [`StrMethod`] call on a parameter, or on a name derived from it — `deriv` is
    /// [`Derivation::Direct`] for `p.startswith("x")`, [`Derivation::Element`]/
    /// [`Derivation::SplitElement`] for a method called on a loop element (`part.isdigit()`).
    StrMethod { param: String, deriv: Derivation, method: StrMethod, arg: Option<String> },
    /// `for p2 in p:` — iterating a parameter directly ([`Derivation::Direct`]) or a
    /// [`Derivation::Split`]-bound local (`for part in parts:`).
    ForIter { param: String, deriv: Derivation },
    /// `not <inner>` — negates the inner predicate's outcome polarity at synthesis time.
    Not(Box<Predicate>),
    /// `a <op> b`, both operands resolving to a parameter (or a derivation of one) — a
    /// coordinated pair, synthesized by [`synthesize_pair`], not [`synthesize`].
    ParamCompare { param_a: String, deriv_a: Derivation, op: CmpOp, param_b: String, deriv_b: Derivation },
}

/// The predicates found at one branch-point line: either a test expression's decomposed
/// predicates (`If`/`While`), or a `for` loop's iterated-parameter predicate.
#[derive(Debug, Clone, PartialEq)]
pub enum LinePredicates {
    Test(Vec<Predicate>),
    ForIter(Predicate),
}

/// Local-name → derivation aliases discovered so far in a sequential walk of a function body: a
/// local assigned directly from one of the recognized derivations of a parameter (`n = len(s)`,
/// `y = s`) resolves through this map exactly as if the parameter's own name had been written —
/// see the module doc's "one piece of cross-variable reasoning".
type Aliases = HashMap<String, (String, Derivation)>;

/// Local-name → literal-bool value, for the pending half of a loop-state-flag candidate: a name
/// most recently assigned `True`/`False` directly (not through any derivation).
type BoolInits = HashMap<String, bool>;

/// Flag name → the fully polarity-resolved predicate that explains its value after a qualifying
/// loop (see `extraction::detect_loop_flag`) — consulted by [`extraction::extract`] exactly like
/// [`Aliases`], but never cleared when the loop's own scope ends, since the flag's meaning holds
/// for the rest of the function.
pub(super) type FlagPreds = HashMap<String, Predicate>;

/// A sentinel value overwhelmingly unlikely to appear as a substring/element of an ordinarily
/// generated container — used as the "definitely excludes `literal`" side of
/// [`container_membership_value`], the same role `not_in_value`'s `z`-padding plays for
/// [`Predicate::Membership`].
const EXCLUSION_SENTINEL: &str = "\u{1}\u{2}\u{3}pylens_no_match\u{1}\u{2}\u{3}";

impl StrMethod {
    fn parse(name: &str) -> Option<StrMethod> {
        match name {
            "startswith" => Some(StrMethod::StartsWith),
            "endswith" => Some(StrMethod::EndsWith),
            "isdigit" => Some(StrMethod::IsDigit),
            "isalpha" => Some(StrMethod::IsAlpha),
            "isupper" => Some(StrMethod::IsUpper),
            "islower" => Some(StrMethod::IsLower),
            "isspace" => Some(StrMethod::IsSpace),
            "isalnum" => Some(StrMethod::IsAlnum),
            _ => None,
        }
    }

    /// Whether this method takes the one string-literal argument this module can extract
    /// (`startswith`/`endswith`) rather than none (the `is*` predicates).
    fn takes_str_arg(self) -> bool {
        matches!(self, StrMethod::StartsWith | StrMethod::EndsWith)
    }
}

impl Predicate {
    /// The predicate's one named parameter. For [`Predicate::ParamCompare`] — which names two —
    /// this returns `param_a`; callers that need both must match the variant directly (see
    /// `record::cover::override_set`).
    pub fn param(&self) -> &str {
        match self {
            Predicate::Compare { param, .. }
            | Predicate::Truthy { param }
            | Predicate::Membership { param, .. }
            | Predicate::ContainerMembership { param, .. }
            | Predicate::StrMethod { param, .. }
            | Predicate::ForIter { param, .. } => param,
            Predicate::Not(inner) => inner.param(),
            Predicate::ParamCompare { param_a, .. } => param_a,
        }
    }
}

mod extraction;
mod synthesis;

pub use extraction::{collect_predicates, find_function_body};
pub use synthesis::{synthesize, synthesize_pair, synthesize_variant};

#[cfg(test)]
mod tests;
