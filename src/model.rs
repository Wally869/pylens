//! Effect-signature data model. Parser-independent and serde-serializable.
//!
//! Uses **may-set** (over-approximation) semantics: the static sets should be a superset of
//! whatever any execution actually does, so the soundness invariant `observed ⊆ static`
//! holds. See DESIGN.md.

use serde::de::{self, MapAccess, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

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

/// A contradiction between an untrusted declared annotation and the statically inferred
/// may-set. See `analyze::passes::type_check` for the conservative flagging rule (only raised
/// on full disjointness, never on a mere subset mismatch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeMismatch {
    /// What was checked — `"return"` today; param-annotation mismatch is future work (params
    /// aren't captured yet).
    pub kind: String,
    /// The untrusted declared annotation name.
    pub declared: String,
    /// The inferred may-set that contradicts it.
    pub inferred: Vec<ReturnKind>,
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

fn is_zero(n: &u32) -> bool {
    *n == 0
}
fn is_false(b: &bool) -> bool {
    !*b
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

    /// Reconstruct the dotted module string (for probing/display; not serialized).
    pub fn dotted(&self) -> String {
        if self.path.is_empty() {
            self.package.clone()
        } else {
            format!("{}.{}", self.package, self.path)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.package.is_empty() && self.path.is_empty()
    }
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

/// A reference from a function to an import it uses: the bound name as referenced in the body,
/// plus the module that name resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportUse {
    pub binding: String,
    pub module: ModuleRef,
}

/// A usage-inferred shape for a parameter, used to direct input generation. Inferred from how
/// the parameter is used in the body (not from annotations). Recursive: a container shape
/// carries the shape of its elements/keys/values, which in turn may be `Any` (unknown) or
/// another container. Scalars serialize as a lowercase string (e.g. `"int"`, `"any"`);
/// containers serialize as a tagged object (`{"seq": <elem>}`, `{"map": {"key": ..., "value":
/// ...}}`, `{"set": <elem>}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    Int,
    Float,
    Bool,
    Str,
    Bytes,
    None,
    /// list/tuple-like: indexed, iterated, `len()`-ed, or list-mutated. Carries the element
    /// shape.
    Seq(Box<Shape>),
    /// dict-like: `.keys`/`.get`/`.items`/`.update`. Carries the key and value shapes.
    Map(Box<Shape>, Box<Shape>),
    /// Carries the element shape.
    Set(Box<Shape>),
    /// No discriminating usage observed.
    Any,
}

impl Shape {
    /// `Seq(Any)` — a sequence with no discriminating element usage observed.
    pub fn any_seq() -> Self {
        Shape::Seq(Box::new(Shape::Any))
    }

    /// `Map(Any, Any)` — a mapping with no discriminating key/value usage observed.
    pub fn any_map() -> Self {
        Shape::Map(Box::new(Shape::Any), Box::new(Shape::Any))
    }

    /// `Set(Any)` — a set with no discriminating element usage observed.
    pub fn any_set() -> Self {
        Shape::Set(Box::new(Shape::Any))
    }

    /// Merge two pieces of shape evidence for the same root. Matching constructors recurse into
    /// their children; `Any` defers to the other side; conflicting constructors (e.g. `Int` vs
    /// `Seq`) resolve to `Any` since the evidence disagrees and the analyzer must over-approximate
    /// rather than pick arbitrarily.
    pub fn join(a: Shape, b: Shape) -> Shape {
        match (a, b) {
            (Shape::Any, other) | (other, Shape::Any) => other,
            (Shape::Seq(e1), Shape::Seq(e2)) => Shape::Seq(Box::new(Shape::join(*e1, *e2))),
            (Shape::Set(e1), Shape::Set(e2)) => Shape::Set(Box::new(Shape::join(*e1, *e2))),
            (Shape::Map(k1, v1), Shape::Map(k2, v2)) => Shape::Map(
                Box::new(Shape::join(*k1, *k2)),
                Box::new(Shape::join(*v1, *v2)),
            ),
            (a, b) if a == b => a,
            _ => Shape::Any,
        }
    }
}

/// Scalars serialize as a lowercase tag string; containers serialize as a single-key tagged
/// object (`{"seq": <elem>}`, `{"set": <elem>}`, `{"map": {"key": ..., "value": ...}}`).
impl Serialize for Shape {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Shape::Int => serializer.serialize_str("int"),
            Shape::Float => serializer.serialize_str("float"),
            Shape::Bool => serializer.serialize_str("bool"),
            Shape::Str => serializer.serialize_str("str"),
            Shape::Bytes => serializer.serialize_str("bytes"),
            Shape::None => serializer.serialize_str("none"),
            Shape::Any => serializer.serialize_str("any"),
            Shape::Seq(elem) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("seq", elem)?;
                map.end()
            }
            Shape::Set(elem) => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("set", elem)?;
                map.end()
            }
            Shape::Map(key, value) => {
                #[derive(Serialize)]
                struct MapFields<'a> {
                    key: &'a Shape,
                    value: &'a Shape,
                }
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("map", &MapFields { key, value })?;
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for Shape {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ShapeVisitor;

        impl<'de> Visitor<'de> for ShapeVisitor {
            type Value = Shape;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a shape tag string or a tagged shape object")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Shape, E> {
                match v {
                    "int" => Ok(Shape::Int),
                    "float" => Ok(Shape::Float),
                    "bool" => Ok(Shape::Bool),
                    "str" => Ok(Shape::Str),
                    "bytes" => Ok(Shape::Bytes),
                    "none" => Ok(Shape::None),
                    "any" => Ok(Shape::Any),
                    other => Err(de::Error::unknown_variant(
                        other,
                        &["int", "float", "bool", "str", "bytes", "none", "any"],
                    )),
                }
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Shape, A::Error> {
                let tag: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("expected a shape tag key"))?;
                match tag.as_str() {
                    "seq" => Ok(Shape::Seq(Box::new(map.next_value()?))),
                    "set" => Ok(Shape::Set(Box::new(map.next_value()?))),
                    "map" => {
                        #[derive(Deserialize)]
                        struct MapFields {
                            key: Shape,
                            value: Shape,
                        }
                        let fields: MapFields = map.next_value()?;
                        Ok(Shape::Map(Box::new(fields.key), Box::new(fields.value)))
                    }
                    other => Err(de::Error::unknown_variant(other, &["seq", "set", "map"])),
                }
            }
        }

        deserializer.deserialize_any(ShapeVisitor)
    }
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

fn is_positional(k: &ParamKind) -> bool {
    matches!(k, ParamKind::Positional)
}

/// A parameter and its inferred shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamInfo {
    pub name: String,
    pub shape: Shape,
    pub has_default: bool,
    #[serde(default, skip_serializing_if = "is_positional")]
    pub kind: ParamKind,
    /// Literal values pulled from guard conditions (`if`/`while`/`assert` tests, ternary
    /// conditions) that compare, contain, or identity-test this parameter — e.g. `x == 42`
    /// records `42`. Feeds `generate::candidates_for` so generated inputs are more likely to
    /// exercise both sides of a guarded branch. A bounded heuristic, not a solver: direct
    /// per-parameter literal extraction only. See `analyze::collect::guards`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guard_samples: Vec<serde_json::Value>,
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
}

impl EffectSignature {
    /// An empty signature for `name`/`kind` with no detected effects yet.
    pub fn new(name: impl Into<String>, kind: DefKind) -> Self {
        Self {
            name: name.into(),
            kind,
            owner: None,
            params: Vec::new(),
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
        }
    }
}
