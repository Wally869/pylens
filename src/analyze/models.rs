//! Static effect models for a slice of the CPython standard library: known raises/io profiles
//! for library calls the Effects pass would otherwise leave as `call_import` unresolved effects.
//! Keyed on the **resolved module path** (`os.path.join`, `sys.stdout.write`, ...), never on the
//! text at the call site, so an aliased import (`import os.path as p; p.join(...)`) still finds
//! the same entry — see `passes::effects::calls::resolve_stdlib_call`.
//!
//! Every entry here removes the `unresolved_effects` acknowledgment from the callers that use
//! it: a missed effect that was a soft `validate` defect before the entry becomes a hard one
//! after. So every entry over-approximates on purpose — see `temp/effect_models.md` for the
//! measurement and the full per-entry rationale.
//!
//! [`return_kind`] is a separate, independent table (rather than a field on [`ModelEntry`])
//! because the raises/io entries above are grouped by shared profile, not by shared return
//! type — `os.path.isabs` and `os.path.join` share [`OS_PATH_PURE`]'s raises/io but return a
//! `bool` and a `str` respectively. It's queried by the Shapes pass through
//! [`resolve_stdlib_return`], which shares [`resolve_dotted`] with [`resolve_stdlib_call`] so
//! both passes resolve a call to the same stdlib path.

/// One modelled call's known raises/io profile.
pub(in crate::analyze) struct ModelEntry {
    pub raises: &'static [&'static str],
    pub io: &'static [&'static str],
}

const fn entry(raises: &'static [&'static str], io: &'static [&'static str]) -> ModelEntry {
    ModelEntry { raises, io }
}

/// Look up the effect model for a resolved dotted stdlib path (e.g. `"os.path.join"`). `None`
/// means the call stays unresolved, either because the path isn't modelled at all or because it's
/// on the explicit do-not-model list (see `temp/effect_models.md`).
pub(in crate::analyze) fn lookup(resolved: &str) -> Option<&'static ModelEntry> {
    overrides(resolved).or_else(|| namespace_default(resolved))
}

/// Exact per-name entries. Takes priority over `namespace_default`.
fn overrides(resolved: &str) -> Option<&'static ModelEntry> {
    static OS_PATH_PURE: ModelEntry = entry(&["TypeError", "AttributeError"], &[]);
    static OS_PATH_IO_TV: ModelEntry = entry(&["TypeError", "ValueError"], &["filesystem"]);
    static OS_PATH_IO_T: ModelEntry = entry(&["TypeError"], &["filesystem"]);
    static OS_PATH_IO_OT: ModelEntry = entry(&["OSError", "TypeError"], &["filesystem"]);
    static OS_IO: ModelEntry = entry(&["OSError", "TypeError"], &["filesystem"]);
    static OS_FSPATH: ModelEntry = entry(&["TypeError"], &[]);
    static OS_GETPID: ModelEntry = entry(&[], &[]);
    static OS_GETCWD: ModelEntry = entry(&[], &["filesystem"]);
    static SYS_EXIT: ModelEntry = entry(&["SystemExit"], &[]);
    static SYS_STDOUT_WRITE: ModelEntry = entry(&[], &["stdout"]);
    static SYS_STDERR_WRITE: ModelEntry = entry(&[], &["stderr"]);
    static SYS_PURE_NORAISE: ModelEntry = entry(&[], &[]);
    static SYS_GETFRAME: ModelEntry = entry(&["ValueError"], &[]);
    static TIME_PURE: ModelEntry = entry(&[], &[]);
    static TIME_SLEEP: ModelEntry = entry(&["ValueError", "TypeError"], &[]);
    static TIME_STRFTIME: ModelEntry = entry(&["ValueError", "TypeError"], &[]);
    static JSON_DUMPS: ModelEntry = entry(&["TypeError", "ValueError"], &[]);
    static JSON_LOADS: ModelEntry = entry(&["ValueError", "TypeError"], &[]);
    static JSON_DUMP: ModelEntry = entry(&["TypeError", "ValueError"], &["filesystem"]);
    static JSON_LOAD: ModelEntry = entry(&["ValueError", "TypeError"], &["filesystem"]);

    match resolved {
        "os.path.join" | "os.path.basename" | "os.path.dirname" | "os.path.normcase"
        | "os.path.normpath" | "os.path.splitext" | "os.path.split" | "os.path.splitdrive"
        | "os.path.isabs" | "os.path.relpath" | "os.path.commonprefix" => Some(&OS_PATH_PURE),

        "os.path.exists" | "os.path.lexists" | "os.path.isdir" | "os.path.isfile"
        | "os.path.islink" => Some(&OS_PATH_IO_TV),

        "os.path.abspath" | "os.path.realpath" | "os.path.expanduser" | "os.path.expandvars" => {
            Some(&OS_PATH_IO_T)
        }

        "os.path.getmtime" | "os.path.getatime" | "os.path.getctime" | "os.path.getsize"
        | "os.path.samefile" => Some(&OS_PATH_IO_OT),

        "os.close" | "os.read" | "os.write" | "os.open" | "os.listdir" | "os.stat"
        | "os.unlink" | "os.remove" | "os.rename" | "os.replace" | "os.mkdir" | "os.makedirs"
        | "os.rmdir" | "os.chmod" | "os.scandir" | "os.walk" => Some(&OS_IO),

        "os.fspath" => Some(&OS_FSPATH),
        "os.getpid" => Some(&OS_GETPID),
        "os.getcwd" => Some(&OS_GETCWD),

        "sys.exit" => Some(&SYS_EXIT),
        "sys.stdout.write" => Some(&SYS_STDOUT_WRITE),
        "sys.stderr.write" => Some(&SYS_STDERR_WRITE),
        "sys.getsizeof" | "sys.intern" | "sys.getrecursionlimit" => Some(&SYS_PURE_NORAISE),
        "sys._getframe" => Some(&SYS_GETFRAME),
        "sys.modules.get" => Some(&SYS_PURE_NORAISE),

        "time.time" | "time.monotonic" | "time.perf_counter" => Some(&TIME_PURE),
        "time.sleep" => Some(&TIME_SLEEP),
        "time.strftime" => Some(&TIME_STRFTIME),

        "json.dumps" => Some(&JSON_DUMPS),
        "json.loads" => Some(&JSON_LOADS),
        "json.dump" => Some(&JSON_DUMP),
        "json.load" => Some(&JSON_LOAD),

        _ => None,
    }
}

/// Namespace-wide entries: any dotted name directly under one of these modules that isn't
/// covered by `overrides` (and isn't on the explicit exclusion list below) gets this profile.
/// Only used for namespaces whose members genuinely share one raises/io profile — `os` and `sys`
/// deliberately have no default here, see `temp/effect_models.md`.
fn namespace_default(resolved: &str) -> Option<&'static ModelEntry> {
    static RE: ModelEntry = entry(&["error", "TypeError"], &[]);
    static MATH: ModelEntry = entry(&["ValueError", "TypeError", "OverflowError"], &[]);
    static ITERTOOLS: ModelEntry = entry(&["TypeError", "ValueError"], &[]);
    static STRUCT: ModelEntry = entry(&["error", "TypeError"], &[]);

    // `struct.pack_into` mutates its buffer argument, which this table doesn't model — leaving
    // it out (rather than folding it into the pure `struct.*` default) keeps the entry honest.
    if resolved == "struct.pack_into" {
        return None;
    }

    let namespace = resolved.rsplit_once('.')?.0;
    match namespace {
        "re" => Some(&RE),
        "math" => Some(&MATH),
        "itertools" => Some(&ITERTOOLS),
        "struct" => Some(&STRUCT),
        _ => None,
    }
}

/// Resolve an attribute-chain call's (`base.suffix...(...)`) callee text to its true dotted
/// stdlib path: `module` is the binding's already-resolved [`crate::model::ModuleRef`], `base`
/// the bound name as it appears at the call site, `full` the full dotted callee text
/// (`dotted_attr` of the call expression, e.g. `"p.join"` or `"os.path.join"`). Shared by
/// [`resolve_stdlib_call`] and [`resolve_stdlib_return`] so every pass that resolves a stdlib
/// call agrees on which function it names.
///
/// `base == module.package` means `base` is written as the real top-level package name, not an
/// alias — e.g. `import os.path; os.path.join(...)` binds `"os"` to `ModuleRef{os, path}` (the
/// *most specific* module the statement names), but the call-site text `os.path.join` is already
/// the true dotted path, so it's used as-is rather than re-prefixed with `module.dotted()`
/// (which would double up the `path` segment). Only a genuine alias (`import os.path as p`,
/// `import os as o`) needs the substitution: `base` then differs from `module.package`, and the
/// call-site text after `base.` is appended to `module.dotted()` instead.
fn resolve_dotted(module: &crate::model::ModuleRef, base: &str, full: &str) -> Option<String> {
    if base == module.package {
        return Some(full.to_string());
    }
    let suffix = full.strip_prefix(base)?.strip_prefix('.')?;
    Some(format!("{}.{}", module.dotted(), suffix))
}

/// Resolve an attribute-chain call to the model entry for its resolved module path — see
/// [`resolve_dotted`].
pub(in crate::analyze) fn resolve_stdlib_call(
    module: &crate::model::ModuleRef,
    base: &str,
    full: &str,
) -> Option<&'static ModelEntry> {
    lookup(&resolve_dotted(module, base, full)?)
}

/// Resolve an attribute-chain call to its modelled return kind — see [`resolve_dotted`]. Used
/// only by the Shapes pass; `None` means either the call isn't resolvable or its return type
/// isn't modelled, and the caller must fall back to `Shape::Any`.
pub(in crate::analyze) fn resolve_stdlib_return(
    module: &crate::model::ModuleRef,
    base: &str,
    full: &str,
) -> Option<crate::model::ReturnKind> {
    return_kind(&resolve_dotted(module, base, full)?)
}

/// The modelled return kind for a resolved dotted stdlib path, e.g. `"os.path.join"` ->
/// [`crate::model::ReturnKind::Str`]. Deliberately conservative and far smaller than the
/// raises/io table above: only entries whose return type is unambiguous across the whole group
/// are listed, everything else stays `None` (⇒ `Shape::Any`, never a guess).
pub(in crate::analyze) fn return_kind(resolved: &str) -> Option<crate::model::ReturnKind> {
    use crate::model::ReturnKind;
    match resolved {
        // `os.fspath` is deliberately excluded: it returns whatever `__fspath__` returns (`str`
        // or `bytes` depending on the input), so a single return kind would be a guess.
        "os.path.join" | "os.path.basename" | "os.path.dirname" | "os.path.normcase"
        | "os.path.normpath" | "os.path.relpath" | "os.path.commonprefix"
        | "os.path.abspath" | "os.path.realpath" | "os.path.expanduser" | "os.path.expandvars"
        | "os.getcwd" => Some(ReturnKind::Str),

        "os.path.splitext" | "os.path.split" | "os.path.splitdrive" => Some(ReturnKind::Sequence),

        "os.path.isabs" | "os.path.exists" | "os.path.lexists" | "os.path.isdir"
        | "os.path.isfile" | "os.path.islink" | "os.path.samefile" => Some(ReturnKind::Bool),

        "os.path.getmtime" | "os.path.getatime" | "os.path.getctime" => Some(ReturnKind::Float),
        "os.path.getsize" | "os.getpid" => Some(ReturnKind::Int),

        "sys.getsizeof" | "sys.getrecursionlimit" => Some(ReturnKind::Int),
        "sys.intern" => Some(ReturnKind::Str),

        "time.time" | "time.monotonic" | "time.perf_counter" => Some(ReturnKind::Float),
        "time.strftime" => Some(ReturnKind::Str),
        "time.sleep" => Some(ReturnKind::None),

        "json.dumps" => Some(ReturnKind::Str),

        _ => None,
    }
}
