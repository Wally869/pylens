use serde::{Deserialize, Serialize};

/// The recursive shape lattice: usage-inferred shapes for parameter generation.
/// All type definitions, impl blocks, and serialization logic are in [`shape`].
pub mod shape;
pub use shape::{Shape, is_zero, is_false, same_constructor};

/// The internal branch-point model for `record`'s per-branch-outcome accounting.
pub mod branch;
pub use branch::{BranchKind, BranchPoint, BranchPointOutcome, OutcomeEvidence};

/// Whether the analyzed definition is a free function or a method.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DefKind {
    Function,
    Method,
}

/// A coarse, inferred description of a returned value. Inferred from the body — never
/// trusted from annotations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReturnKind {
    /// `return`, `return None`, or fall-through off the end.
    None,
    Bool,
    Int,
    Float,
    Str,
    Bytes,
    /// list/tuple literal or constructor.
    Sequence,
    /// dict literal or constructor.
    Mapping,
    Set,
    /// A value whose type could not be statically inferred (e.g. a returned attribute, local,
    /// or unknown call). NOT the same as `none` — there *is* a value, its kind is just opaque.
    Opaque,
}

impl ReturnKind {
    /// The conservative [`Shape`] a value of this return kind carries. Used to feed a resolved
    /// stdlib model-table return kind into the Shapes pass — `Opaque` (and every container kind,
    /// since the element type is unknown) maps to the widest shape in its constructor rather
    /// than guessing an element type.
    pub fn to_shape(self) -> Shape {
        match self {
            ReturnKind::None => Shape::None,
            ReturnKind::Bool => Shape::Bool,
            ReturnKind::Int => Shape::Int,
            ReturnKind::Float => Shape::Float,
            ReturnKind::Str => Shape::Str,
            ReturnKind::Bytes => Shape::Bytes,
            ReturnKind::Sequence => Shape::any_seq(),
            ReturnKind::Mapping => Shape::any_map(),
            ReturnKind::Set => Shape::any_set(),
            ReturnKind::Opaque => Shape::Any,
        }
    }
}

/// A contradiction between an untrusted declared annotation and the statically inferred
/// may-set. See `analyze::passes::type_check` for the conservative flagging rule (only raised
/// on full disjointness, never on a mere subset mismatch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeMismatch {
    /// What was checked — `"return"` or `"param"`.
    pub kind: String,
    /// The parameter name, when `kind == "param"`. `None` for a `"return"` mismatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub param: Option<String>,
    /// The untrusted declared annotation name.
    pub declared: String,
    /// The inferred return may-set that contradicts it, when `kind == "return"`. Empty for a
    /// `"param"` mismatch — see `inferred_shape`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inferred: Vec<ReturnKind>,
    /// The inferred parameter shape that contradicts the declaration, when `kind == "param"`.
    /// `None` for a `"return"` mismatch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferred_shape: Option<Shape>,
}

/// The root object a mutation targets.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "root", rename_all = "snake_case")]
pub enum MutationTarget {
    Param { name: String },
    SelfAttr { name: String },
    Global { name: String },
    Nonlocal { name: String },
    /// Aliased to something we could not root.
    Unknown,
}

/// How a mutation is performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationKind {
    SubscriptSet,
    SubscriptDel,
    AttrSet,
    AttrDel,
    /// A (possibly) mutating method call; `Mutation::name` carries which method.
    Method,
    AugSubscript,
    AugAttr,
    /// `+=`/`-=`/... on a plain name (e.g. `p += [1]`) — a may-mutation since the operator's
    /// effect on the underlying object depends on its runtime type (list `+=` mutates in
    /// place, int `+=` does not).
    AugName,
}

/// A single may-mutation: some root, mutated some way.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Mutation {
    pub target: MutationTarget,
    pub via: MutationKind,
    /// Method or attribute name where relevant (e.g. "append", "cache").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Exceptions the function may raise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Raises {
    /// From `raise` statements — high confidence.
    pub explicit: Vec<String>,
    /// Operator-induced (ZeroDivisionError / KeyError / IndexError / TypeError / ...) —
    /// over-approximated or deferred to the dynamic layer.
    pub implicit: Vec<String>,
}

/// An effect we could not resolve statically. The honest record of a blind spot — the
/// anti-reward-hacking surface. Never silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnresolvedEffect {
    /// e.g. "call_unknown_callee", "dynamic_setattr", "param_escapes_to_callee".
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub callee: Option<String>,
    /// Roots this unresolved effect may touch (e.g. params passed into an unknown call).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub may_affect: Vec<MutationTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purity {
    Pure,
    Impure,
    /// Has unresolved effects — purity cannot be determined.
    Unknown,
}

/// A name pulled in by `from module import name [as alias]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportedName {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// A dotted module reference, split into its top-level **package** and the remaining **path**.
/// `xml.etree.ElementTree` → package `xml`, path `etree.ElementTree`. A bare module (`os`) has
/// an empty path. A pure relative `from . import x` has an empty package (see `Import::level`).
/// The package is the unit that answers "is this library installed"; the path navigates within
/// it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleRef {
    pub package: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
}

/// Where an import statement lives — which decides *when* it executes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportScope {
    /// Runs at module load: top level, a class body, or top-level control flow. A failure here
    /// stops the whole module from loading.
    Module,
    /// Runs only when a function is called (an import inside a function body). A failure here
    /// surfaces per-call, not at load.
    Function,
}

/// A single import. `import a, b` is split into one entry per bound module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Import {
    /// `false` for `import x`, `true` for `from x import ...`.
    pub from: bool,
    /// The module imported from, split into package + path.
    pub module: ModuleRef,
    /// Leading-dot count for relative imports (0 = absolute).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub level: u32,
    /// For `import x as y`: the alias `y`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// For `from m import a, b as c`: the imported names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<ImportedName>,
    /// `from m import *`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub star: bool,
    /// Whether this import runs at module load or only when a function runs.
    pub scope: ImportScope,
}

/// A reference from a function to an import it uses: the bound name as referenced in the body,
/// plus the module that name resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportUse {
    pub binding: String,
    pub module: ModuleRef,
}


/// Whether a parameter binds one positional/keyword argument, or collects a variable number of
/// them (`*args` / `**kwargs`). A var-positional/var-keyword parameter never receives a single
/// generated value positionally — see `generate::gen_inputs`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    #[default]
    Positional,
    /// A parameter after a bare `*` or `*args` that only binds by keyword (`def f(a, *, b)`).
    /// Generated and passed as a named keyword argument — see `generate::gen_inputs`.
    KeywordOnly,
    /// `*args`.
    VarPositional,
    /// `**kwargs`.
    VarKeyword,
}

/// Which side of a [`ParamRelation`] one operand sits on: the parameter itself, or one element
/// of it (`p[i]`, or the loop variable of `for p_i in p:`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamRef {
    pub param: String,
    pub element: bool,
}

/// The kind of expression that put two [`ParamRef`]s together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    /// `<`, `<=`, `>`, `>=`.
    Order,
    /// `==`, `!=`.
    Eq,
    /// `+`, `-`, `*`, `/`, `//`, `%`, `**`.
    Arith,
}

/// A generation-only fact: two parameters (or one parameter's element and another parameter) meet
/// as the two operands of one comparison or arithmetic expression somewhere in the body. A
/// bounded heuristic, not a solver — over the two parameters' meeting alone it says nothing about
/// their VALUES, only that the sampler should stop picking each one independently: pairing a `str`
/// `arr` with an `int` `target` when the body only ever does `arr[mid] < target` wastes most of
/// the generation budget on a guaranteed `TypeError`. Like `guard_samples`/`hints`/
/// `default_literal`, this never narrows a shape, a may-set, or purity — a wrong relation only
/// wastes a generation slot. See `analyze::collect::relations`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamRelation {
    pub left: ParamRef,
    pub right: ParamRef,
    pub kind: RelationKind,
}

/// A parameter and its inferred shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamInfo {
    pub name: String,
    pub shape: Shape,
    pub has_default: bool,
    #[serde(default, skip_serializing_if = "is_positional")]
    pub kind: ParamKind,
    /// From annotation — UNTRUSTED. Kept only for declared-vs-inferred mismatch detection (see
    /// `analyze::passes::type_check`), mirroring `EffectSignature::declared_return`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<String>,
    /// Literal values pulled from guard conditions (`if`/`while`/`assert` tests, ternary
    /// conditions) that compare, contain, or identity-test this parameter — e.g. `x == 42`
    /// records `42`. Feeds `generate::candidates_for` so generated inputs are more likely to
    /// exercise both sides of a guarded branch. A bounded heuristic, not a solver: direct
    /// per-parameter literal extraction only. See `analyze::collect::guards`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guard_samples: Vec<serde_json::Value>,
    /// The parameter's default value, when it's a literal (number/string/bool/`None`) — the
    /// function's own source, not an annotation, so it's trustworthy evidence of intent (unlike
    /// `declared`). Feeds `generate::candidates_for`/`generate::generation_shape`: a generation
    /// input only, never folded into `shape` — it can't affect the may-set or purity.
    #[serde(skip)]
    pub default_literal: Option<serde_json::Value>,
    /// Inferred content-domain tags (`"url"`, `"email"`, `"path"`, `"json"`, `"date"`,
    /// `"numeric_str"`, `"regex"`, `"html"`) — see `analyze::collect::hints`. **Advisory only**,
    /// exactly like `type_mismatches`: never folded into `shape`, never affects `purity` or
    /// `raises`. Feeds `generate::seeds::hint_candidates` so generation is more likely to reach
    /// a function's real body instead of raising on the first parse of a placeholder string.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
    /// A concrete [`Shape`] read off `declared`'s annotation for the simple forms (`int`, `list`,
    /// `Optional[int]`, `int | None`, ...) — same UNTRUSTED source as `declared`, kept separate
    /// from it and never folded into `shape` or the may-set. Used only as a ranking hint by
    /// `generate::candidates_for`, which prepends candidates matching this shape so the sampler's
    /// early budget lands on inputs the author's own annotation expects. `None` for an annotation
    /// that isn't one of the recognized simple forms.
    #[serde(skip)]
    pub declared_shape_hint: Option<Shape>,
}

/// The full effect signature of one function/method.
///
/// A *set of behaviors*, not one flat signature: `returns` and `raises` are unioned over all
/// exits; `mutations` is a may-set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectSignature {
    pub name: String,
    pub kind: DefKind,
    /// For methods, the class the method is defined in; `None` for free functions. Needed to
    /// construct a receiver when executing the method.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Parameters with usage-inferred shapes (drives input generation).
    pub params: Vec<ParamInfo>,
    /// Cross-parameter relations found in the body (two parameters, or a parameter's element and
    /// another parameter, meeting as the operands of one comparison/arithmetic expression) — see
    /// [`ParamRelation`]. A generation-only hint, not part of the JSON contract: never folded
    /// into `params`, the may-set, or `purity`.
    #[serde(skip)]
    pub param_relations: Vec<ParamRelation>,
    /// From annotation — UNTRUSTED. Kept only for declared-vs-inferred mismatch detection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub declared_return: Option<String>,
    pub is_generator: bool,
    /// Union of return kinds over all return exits (including `None` / fall-through).
    pub returns: Vec<ReturnKind>,
    pub raises: Raises,
    pub mutations: Vec<Mutation>,
    pub global_writes: Vec<String>,
    pub io: Vec<String>,
    pub unresolved_effects: Vec<UnresolvedEffect>,
    pub purity: Purity,
    /// Imports this function references (the per-function ← dependency edge).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<ImportUse>,
    /// A `from m import *` is in scope and this function calls a name we could not otherwise
    /// resolve — so that name *may* come from the star import.
    #[serde(default, skip_serializing_if = "is_false")]
    pub may_use_star: bool,
    /// Dotted decorator names applied to this def, in source order. A decorator outside the
    /// recognized-transparent set can replace the function entirely, so it downgrades purity —
    /// see the Purity pass.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<String>,
    /// Declared-vs-inferred contradictions found by the TypeCheck pass. Empty unless the
    /// declared annotation and the inferred may-set are fully disjoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub type_mismatches: Vec<TypeMismatch>,
    /// The 1-based line numbers of every statement in this function's body (nested statements
    /// included, nested `def`/`class` bodies excluded — see `analyze::collect::body_lines`), used
    /// as the coverage denominator in `record.rs`. An internal input, not part of the JSON
    /// contract that downstream consumers key off.
    #[serde(skip)]
    pub body_lines: Vec<u32>,
    /// The enumerated branch points of this function's body (see `analyze::collect::branches`),
    /// used by `record.rs` to build the per-branch-outcome accounting. An internal input, not
    /// part of the JSON contract that downstream consumers key off.
    #[serde(skip)]
    pub branch_points: Vec<BranchPoint>,
    /// The 1-based line numbers of every explicit `return` statement in this function's body
    /// (see `analyze::collect::returns::collect_return_lines`), used by `record.rs` to compute
    /// `FunctionRecord::output_type_coverage`. An internal input, not part of the JSON contract.
    #[serde(skip)]
    pub return_lines: Vec<u32>,
}


/// Serde helper: checks if a ParamKind is Positional.
fn is_positional(k: &ParamKind) -> bool {
    matches!(k, ParamKind::Positional)
}

impl ModuleRef {
    /// Split a dotted module string at its first component.
    pub fn parse(dotted: &str) -> Self {
        match dotted.split_once('.') {
            Some((package, path)) => ModuleRef {
                package: package.to_string(),
                path: path.to_string(),
            },
            None => ModuleRef {
                package: dotted.to_string(),
                path: String::new(),
            },
        }
    }

    /// Whether both package and path are empty.
    pub fn is_empty(&self) -> bool {
        self.package.is_empty() && self.path.is_empty()
    }

    /// Reconstruct the dotted module string (for probing/display; not serialized).
    pub fn dotted(&self) -> String {
        if self.path.is_empty() {
            self.package.clone()
        } else {
            format!("{}.{}", self.package, self.path)
        }
    }
}

impl Import {
    /// The names this import introduces into the enclosing namespace. Empty for
    /// `from m import *` (a wildcard — any free name might come from it; see `star`).
    pub fn bindings(&self) -> Vec<String> {
        if self.star {
            Vec::new()
        } else if self.from {
            self.names
                .iter()
                .map(|n| n.alias.clone().unwrap_or_else(|| n.name.clone()))
                .collect()
        } else if let Some(alias) = &self.alias {
            vec![alias.clone()]
        } else {
            // `import a.b.c` binds the top-level package name `a`.
            vec![self.module.package.clone()]
        }
    }
}

impl EffectSignature {
    /// An empty signature for `name`/`kind` with no detected effects yet.
    pub fn new(name: impl Into<String>, kind: DefKind) -> Self {
        Self {
            name: name.into(),
            kind,
            owner: None,
            params: Vec::new(),
            param_relations: Vec::new(),
            declared_return: None,
            is_generator: false,
            returns: Vec::new(),
            raises: Raises::default(),
            mutations: Vec::new(),
            global_writes: Vec::new(),
            io: Vec::new(),
            unresolved_effects: Vec::new(),
            purity: Purity::Unknown,
            uses: Vec::new(),
            may_use_star: false,
            decorators: Vec::new(),
            type_mismatches: Vec::new(),
            body_lines: Vec::new(),
            branch_points: Vec::new(),
            return_lines: Vec::new(),
        }
    }
}
