//! Per-function **effect records**: the static signature plus concrete *cases* — generated
//! inputs run in the jail, with the effects actually observed (return, raises, argument and
//! `self` mutations, aliasing). This is the record for one function; it does no comparison and
//! computes no score — that belongs to whatever consumes these records.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::exec::{CallResult, HarnessError, NsjailPool, Sandbox};
use crate::generate::{GenInput, ValueDomain, gen_inputs, keyword_only_params, positional_params};
use crate::model::{DefKind, EffectSignature, Import, ReturnKind};
use crate::shrink::shrink_case;
use crate::validate::observable_io_kind;
use crate::{analyze_source, imports_of};

mod cover;
mod stability;

pub use cover::{BranchCoverage, BranchOutcomeReport, BranchReport};
pub use stability::DroppedCases;

/// One long-lived jailed worker, reused for the whole file (load probe + dependency probes +
/// every case). Recording is sequential, so a single fork-server worker amortizes interpreter
/// startup without idle jails; each request still runs in its own forked child.
const POOL_SIZE: usize = 1;

/// Bundles `--value-domain`, `--cover-branches`, and `--stability-runs` for
/// [`record_with_signatures_replay`] — keeps its argument count down alongside
/// `sandbox`/`src`/`imports`/`sigs`/`max_inputs`/`replay`.
#[derive(Clone, Copy)]
pub struct RecordFlags<'a> {
    pub domain: Option<&'a ValueDomain>,
    pub cover_branches: bool,
    /// `--stability-runs <N>`: re-execute every case (generated, cover-loop, and replayed alike)
    /// until it has run `N` times total, dropping any case whose runs disagree — see
    /// [`stabilize_cases`]. `None` (the default) leaves `record`'s output unchanged, including
    /// omitting `FunctionRecord::dropped_cases` entirely.
    pub stability_runs: Option<usize>,
}

/// Generation settings threaded through the recording of one function or method: the
/// `--inputs` budget (interpreted as the TOTAL per-function case budget once `cover_branches` is
/// set — see `cover::run_loop`), the `--value-domain` profile to enforce (if any), and whether
/// `--cover-branches` opted into the predicate-targeted coverage loop. Bundled to keep
/// `function_cases`/`method_record`'s argument counts down.
#[derive(Clone, Copy)]
pub(super) struct GenOptions<'a> {
    pub(super) max_inputs: usize,
    pub(super) domain: Option<&'a ValueDomain>,
    pub(super) cover_branches: bool,
}

/// External input tuples supplied via `--replay`: function name → list of positional-argument
/// tuples, each tuple a `Vec<Value>` ready to hand to the sandbox as-is (no shape-directed
/// generation, no shrinking). See [`parse_replay`].
pub type ReplayMap = HashMap<String, Vec<Vec<Value>>>;

/// Parse a `--replay` file: a JSON object mapping function name → an array of input tuples,
/// each tuple itself a JSON array of positional argument values. Malformed JSON, a non-object
/// top level, or a mapping whose value isn't an array of arrays, is an error — surfaced to the
/// caller rather than silently dropped.
pub fn parse_replay(text: &str) -> Result<ReplayMap, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("replay file: {e}"))?;
    let obj = value.as_object().ok_or_else(|| {
        "replay file: expected a JSON object mapping function name to input tuples".to_string()
    })?;
    let mut out = ReplayMap::new();
    for (name, tuples) in obj {
        let arr = tuples
            .as_array()
            .ok_or_else(|| format!("replay file: {name:?} must map to an array of input tuples"))?;
        let mut parsed = Vec::with_capacity(arr.len());
        for tuple in arr {
            let t = tuple
                .as_array()
                .ok_or_else(|| format!("replay file: each input for {name:?} must be an array"))?;
            parsed.push(t.clone());
        }
        out.insert(name.clone(), parsed);
    }
    Ok(out)
}

/// Where an executed case's input came from — see `Case::source`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseSource {
    /// Sampled by [`crate::generate::gen_inputs`] from the inferred shapes.
    Generated,
    /// Supplied externally via `--replay` — see [`parse_replay`].
    Replay,
}

/// A mutation observed by diffing a value before vs. after the call.
#[derive(Serialize)]
pub struct ObservedMutation {
    /// The mutated root: a parameter name, or `"self"` (the receiver).
    pub target: String,
    pub before: Value,
    pub after: Value,
}

/// A shrunk variant of a raised case's input that still raises the same exception type — see
/// [`crate::shrink::shrink_case`]. Present on a `Case` only when at least one argument was
/// successfully shrunk; this is a reporting aid and is never fed back into `validate`.
#[derive(Serialize)]
pub struct MinimizedInput {
    pub input: Vec<Value>,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub kwargs: Map<String, Value>,
}

/// One executed case: an input vector and what the function did with it.
#[derive(Serialize)]
pub struct Case {
    pub input: Vec<Value>,
    /// Keyword-only arguments passed this call, name → value.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub kwargs: Map<String, Value>,
    /// Constructor arguments used to build the receiver (methods only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ctor_args: Option<Vec<Value>>,
    /// `generated` (sampled from the inferred shapes) or `replay` (supplied via `--replay`) —
    /// see [`CaseSource`].
    pub source: CaseSource,
    /// `returned` | `raised` | `error`. **`raised` means the function itself raised a semantic
    /// exception** (part of its behavior/spec) — `raises` carries the exception type. A
    /// **resource kill** (out-of-memory, recursion limit, timeout — an artifact of the sandbox,
    /// not the function's semantics) is never `raised`: it is always `error`, with the
    /// structured `error.stage == "resource"` (see [`crate::exec::HarnessError::is_resource`]).
    /// Other harness/setup failures (bad source, missing function, timeout-unrelated crashes)
    /// are also `error`, with a different `stage`.
    pub outcome: String,
    #[serde(rename = "return", skip_serializing_if = "Option::is_none")]
    pub ret: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raises: Option<String>,
    /// Mutations observed this run (arguments and/or `self`).
    pub mutations: Vec<ObservedMutation>,
    /// Index of the argument the return value is identical to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub return_aliases_arg: Option<i64>,
    /// Captured stdout produced by the call, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    /// Captured stderr produced by the call, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<HarnessError>,
    /// Present only for `outcome == "raised"` cases where shrinking found a smaller input that
    /// still raises the same exception type. See [`crate::shrink::shrink_case`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minimized: Option<MinimizedInput>,
    /// Lines this case reached in the module under test — an input to [`coverage_for`]'s
    /// per-function aggregate, not something a consumer needs per case (noisy at N cases).
    #[serde(skip)]
    pub lines: Vec<u32>,
    /// Line-transition arcs this case traced — an input to [`branch_report_for`]'s per-function
    /// aggregate, not something a consumer needs per case (noisy at N cases).
    #[serde(skip)]
    pub arcs: Vec<(u32, u32)>,
}

/// A function's executed-line coverage, aggregated over all its cases: how many of its
/// `body_lines` (see [`EffectSignature::body_lines`]) were reached by *some* case, and which
/// ones never were. This is exactly the gap `docs/DESIGN.md` flags as unmeasured — generated
/// inputs are heuristic, so `validate` only checks the paths they happen to reach; `coverage`
/// makes that reach visible instead of leaving it implicit.
#[derive(Serialize)]
pub struct Coverage {
    pub executed: usize,
    pub total: usize,
    pub missed: Vec<u32>,
}

/// Aggregate `cases`' observed lines against `sig.body_lines`, intersecting so lines the trace
/// saw in some *other* function of the same module (the call reached past this function's own
/// body) don't inflate `executed`. `None` when there's nothing to measure: no `body_lines`, or no
/// cases to have measured them with.
fn coverage_for(sig: &EffectSignature, cases: &[Case]) -> Option<Coverage> {
    if cases.is_empty() || sig.body_lines.is_empty() {
        return None;
    }
    let body: HashSet<u32> = sig.body_lines.iter().copied().collect();
    let reached: HashSet<u32> = cases
        .iter()
        .flat_map(|c| c.lines.iter().copied())
        .filter(|l| body.contains(l))
        .collect();
    let mut missed: Vec<u32> = sig
        .body_lines
        .iter()
        .copied()
        .filter(|l| !reached.contains(l))
        .collect();
    missed.sort_unstable();
    Some(Coverage {
        executed: reached.len(),
        total: sig.body_lines.len(),
        missed,
    })
}

/// One `io` may-set entry (see [`EffectSignature::io`]) tagged with whether the sandbox has any
/// channel to observe it — see [`crate::validate::observable_io_kind`]. `record`'s and
/// `validate`'s honesty flag: a consumer must not read an unobservable `io` claim's absence from
/// `validate`'s defects as corroboration, since no execution could ever have disproved it.
#[derive(Serialize)]
pub struct IoObservability {
    pub kind: String,
    pub observable: bool,
}

fn io_observability(io: &[String]) -> Vec<IoObservability> {
    io.iter()
        .map(|kind| IoObservability {
            kind: kind.clone(),
            observable: observable_io_kind(kind),
        })
        .collect()
}

/// Why a function couldn't be executed at all — recorded once, instead of as N identical
/// per-case failures.
#[derive(Serialize)]
pub struct Uncallable {
    /// `module_not_loadable` (a module-scope import failed) | `constructor_failed`.
    pub reason: String,
    pub error: HarnessError,
}

/// The full record of one function or method: its static signature fields, flattened, plus the
/// observed `cases`. If the function couldn't be executed, `uncallable` says why and `cases` is
/// empty.
#[derive(Serialize)]
pub struct FunctionRecord {
    #[serde(flatten)]
    pub signature: EffectSignature,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncallable: Option<Uncallable>,
    pub cases: Vec<Case>,
    /// Executed-line coverage over `body_lines`, aggregated over `cases`. Omitted when there's
    /// nothing to measure — see [`coverage_for`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<Coverage>,
    /// Per-branch-outcome accounting over `cases` — see [`cover::branch_report_for`]. Omitted
    /// when there's nothing to measure (no branch points, or no cases), same as `coverage`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branches: Option<Vec<BranchReport>>,
    /// The rollup over every outcome in `branches`. Present exactly when `branches` is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_coverage: Option<BranchCoverage>,
    /// The closed count of cases `--stability-runs` dropped — see [`DroppedCases`]. Omitted
    /// entirely when `--stability-runs` wasn't passed, so plain `record` output is unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_cases: Option<DroppedCases>,
    /// Per-`io`-entry observability — see [`IoObservability`]. Parallel to, and never a
    /// replacement for, the flattened `io: Vec<String>` may-set carried by `signature`.
    pub io_observability: Vec<IoObservability>,
    /// Whether every return kind in `signature.returns` was observed in some surviving case's
    /// return value AND every `return` statement's line was executed by some surviving case —
    /// see [`output_type_coverage_for`]. `None` when nothing was observed at all (uncallable, or
    /// zero cases): `validated: false` in `validate` output already carries that story, and
    /// there is nothing here to be full or partial about.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_type_coverage: Option<OutputTypeCoverage>,
    /// Present exactly when `output_type_coverage == Some(Partial)`: the static return kinds no
    /// surviving case's return value matched, and/or the `return` statement lines no surviving
    /// case ever executed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unobserved_returns: Option<UnobservedReturns>,
}

/// `"full"` when [`output_type_coverage_for`]'s two conditions both hold; `"partial"` otherwise,
/// with the gap detailed in [`UnobservedReturns`].
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputTypeCoverage {
    Full,
    Partial,
}

/// What kept `output_type_coverage` from being `full` — see [`output_type_coverage_for`].
#[derive(Serialize)]
pub struct UnobservedReturns {
    /// Static return kinds (from `signature.returns`) no surviving case's return value matched.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<ReturnKind>,
    /// `return` statement lines (from the function's body) no surviving case executed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<u32>,
}

/// Whether `cases` observed every static return kind and executed every `return` statement's
/// line, for one function. `None` when there's nothing to measure: no cases at all. Reuses
/// [`crate::validate::classify_return`] for the observed-return classification so `record` and
/// `validate` never disagree on what an observed return value's kind is.
fn output_type_coverage_for(
    sig: &EffectSignature,
    cases: &[Case],
) -> Option<(OutputTypeCoverage, Option<UnobservedReturns>)> {
    if cases.is_empty() {
        return None;
    }
    let observed_kinds: HashSet<ReturnKind> = cases
        .iter()
        .filter(|c| c.outcome == "returned")
        .filter_map(|c| c.ret.as_ref())
        .map(crate::validate::classify_return)
        .collect();
    let mut missing_kinds = Vec::new();
    for kind in &sig.returns {
        if !observed_kinds.contains(kind) && !missing_kinds.contains(kind) {
            missing_kinds.push(*kind);
        }
    }

    let reached_lines: HashSet<u32> = cases.iter().flat_map(|c| c.lines.iter().copied()).collect();
    let mut missing_lines: Vec<u32> = sig
        .return_lines
        .iter()
        .copied()
        .filter(|l| !reached_lines.contains(l))
        .collect();
    missing_lines.sort_unstable();

    if missing_kinds.is_empty() && missing_lines.is_empty() {
        Some((OutputTypeCoverage::Full, None))
    } else {
        Some((
            OutputTypeCoverage::Partial,
            Some(UnobservedReturns { kinds: missing_kinds, lines: missing_lines }),
        ))
    }
}

/// Whether a dependency's module resolves in the jail.
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DepStatus {
    /// `import <module>` succeeded.
    Resolved,
    /// `import <module>` failed (e.g. the package isn't installed).
    Unresolved,
    /// Not attempted. Relative imports need a package context that a standalone file doesn't
    /// supply — they aren't failures, just unprobed here.
    NotProbed,
}

/// A catalogued import plus whether its module resolves in the jail.
#[derive(Serialize)]
pub struct Dependency {
    #[serde(flatten)]
    pub import: Import,
    pub status: DepStatus,
    /// Why it didn't resolve (structured: stage/kind/message/module).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<HarnessError>,
}

/// The record for a whole file: its dependencies (with resolution status) and per-function
/// records.
#[derive(Serialize)]
pub struct ModuleRecord {
    pub dependencies: Vec<Dependency>,
    pub functions: Vec<FunctionRecord>,
}

/// Record a whole file: catalog + probe its imports, then record every function and method
/// with `max_inputs` generated cases each. If the module won't load (a module-scope import is
/// unresolved), every function is marked `uncallable` once rather than producing identical
/// per-case setup errors.
pub fn record_file(src: &str, max_inputs: usize) -> Result<ModuleRecord, String> {
    let sandbox = NsjailPool::new(POOL_SIZE)?;
    record_with(&sandbox, src, max_inputs, None)
}

/// Like [`record_file`], with externally supplied `--replay` input tuples executed in addition
/// to the generated ones (see [`parse_replay`], [`ReplayMap`]).
pub fn record_file_with_replay(
    src: &str,
    max_inputs: usize,
    replay: &ReplayMap,
) -> Result<ModuleRecord, String> {
    let sandbox = NsjailPool::new(POOL_SIZE)?;
    record_with_replay(&sandbox, src, max_inputs, replay, None, false, None)
}

/// Like [`record_file`], with generation restricted to a `--value-domain` profile (see
/// [`ValueDomain`]). `--replay` inputs bypass the filter — see [`record_file_with_options`].
pub fn record_file_with_domain(
    src: &str,
    max_inputs: usize,
    domain: Option<&ValueDomain>,
) -> Result<ModuleRecord, String> {
    let sandbox = NsjailPool::new(POOL_SIZE)?;
    record_with(&sandbox, src, max_inputs, domain)
}

/// The general entry point combining `--replay`, `--value-domain`, and `--cover-branches`:
/// replayed tuples are executed as-is (never filtered), generated ones are restricted to `domain`
/// when given, and `cover_branches` opts into `cover::run_loop`'s predicate-targeted coverage
/// loop (see the module doc) once the initial generated batch is done.
pub fn record_file_with_options(
    src: &str,
    max_inputs: usize,
    replay: &ReplayMap,
    domain: Option<&ValueDomain>,
    cover_branches: bool,
) -> Result<ModuleRecord, String> {
    record_file_with_options_and_stability(src, max_inputs, replay, domain, cover_branches, None)
}

/// Like [`record_file_with_options`], also opting into `--stability-runs` — see
/// [`RecordFlags::stability_runs`].
pub fn record_file_with_options_and_stability(
    src: &str,
    max_inputs: usize,
    replay: &ReplayMap,
    domain: Option<&ValueDomain>,
    cover_branches: bool,
    stability_runs: Option<usize>,
) -> Result<ModuleRecord, String> {
    let sandbox = NsjailPool::new(POOL_SIZE)?;
    record_with_replay(&sandbox, src, max_inputs, replay, domain, cover_branches, stability_runs)
}

/// Record a whole file's functions against an already-provisioned sandbox. Lets a caller
/// processing many files (see `project.rs`) share one jail pool instead of paying nsjail
/// startup per file; behavior is otherwise identical to [`record_file`].
pub fn record_with(
    sandbox: &dyn Sandbox,
    src: &str,
    max_inputs: usize,
    domain: Option<&ValueDomain>,
) -> Result<ModuleRecord, String> {
    record_with_replay(sandbox, src, max_inputs, &ReplayMap::new(), domain, false, None)
}

/// Like [`record_with`], with externally supplied `--replay` input tuples, the
/// `--cover-branches` opt-in, and the `--stability-runs` opt-in — see
/// [`record_file_with_replay`], [`record_file_with_options`], [`RecordFlags::stability_runs`].
pub fn record_with_replay(
    sandbox: &dyn Sandbox,
    src: &str,
    max_inputs: usize,
    replay: &ReplayMap,
    domain: Option<&ValueDomain>,
    cover_branches: bool,
    stability_runs: Option<usize>,
) -> Result<ModuleRecord, String> {
    let imports = imports_of(src).map_err(|e| e.to_string())?;
    let sigs = analyze_source(src).map_err(|e| e.to_string())?;
    record_with_signatures_replay(
        sandbox,
        src,
        imports,
        sigs,
        max_inputs,
        replay,
        RecordFlags { domain, cover_branches, stability_runs },
    )
}

/// Record a whole file's functions against an already-provisioned sandbox, using precomputed
/// imports and effect signatures rather than deriving them from `src` with `analyze_source`.
/// Lets a caller that has already produced (and, in project mode, cross-file-propagated)
/// signatures — see `project::interproc` — record and validate against those enriched
/// signatures instead of the plain per-file ones. Behavior is otherwise identical to
/// [`record_with`], including how `sig.name == "__init__"` is skipped and how `positional_
/// params(sig)`/`gen_inputs(sig, ...)` read `sigs`.
pub fn record_with_signatures(
    sandbox: &dyn Sandbox,
    src: &str,
    imports: Vec<Import>,
    sigs: Vec<EffectSignature>,
    max_inputs: usize,
    domain: Option<&ValueDomain>,
    cover_branches: bool,
) -> Result<ModuleRecord, String> {
    record_with_signatures_flags(
        sandbox,
        src,
        imports,
        sigs,
        max_inputs,
        RecordFlags { domain, cover_branches, stability_runs: None },
    )
}

/// Like [`record_with_signatures`], taking a [`RecordFlags`] bundle so `--stability-runs` (see
/// [`RecordFlags::stability_runs`]) can be threaded through without growing the argument count —
/// used by `project::record_project` for project mode.
pub fn record_with_signatures_flags(
    sandbox: &dyn Sandbox,
    src: &str,
    imports: Vec<Import>,
    sigs: Vec<EffectSignature>,
    max_inputs: usize,
    flags: RecordFlags,
) -> Result<ModuleRecord, String> {
    record_with_signatures_replay(sandbox, src, imports, sigs, max_inputs, &ReplayMap::new(), flags)
}

/// Like [`record_with_signatures`], with externally supplied `--replay` input tuples: each
/// tuple is executed exactly like a generated one (same sandbox call, same mutation-diff
/// machinery), tagged `CaseSource::Replay`, and never shrunk. A replay key that names no
/// function in `sigs` is an error — the caller (see `pylens::main`) surfaces it and exits
/// non-zero rather than silently dropping unmatched replay data.
pub fn record_with_signatures_replay(
    sandbox: &dyn Sandbox,
    src: &str,
    imports: Vec<Import>,
    sigs: Vec<EffectSignature>,
    max_inputs: usize,
    replay: &ReplayMap,
    flags: RecordFlags,
) -> Result<ModuleRecord, String> {
    let RecordFlags { domain, cover_branches, stability_runs } = flags;
    if let Some(runs) = stability_runs {
        assert!(runs >= 2, "stability_runs must be >= 2 (checked by the CLI)");
    }
    for name in replay.keys() {
        if !sigs.iter().any(|s| &s.name == name) {
            return Err(format!("replay: no function named {name:?} in this module"));
        }
    }
    let dependencies = probe_dependencies(sandbox, imports)?;

    // Ground truth for "can anything in this file run": exec the real source once. This
    // respects guards (e.g. `try: import numpy except ImportError: ...`) that per-import
    // probing can't see, and yields the exact blocking module via the structured error.
    let load = sandbox.probe_load(src, None, None)?;
    let module_error: Option<HarnessError> = if load.ok { None } else { load.error };

    let mut functions = Vec::new();
    let mut ctor_cache: HashMap<String, Option<HarnessError>> = HashMap::new();
    for sig in &sigs {
        if sig.name == "__init__" {
            continue; // the constructor is plumbing; it runs as part of every method case
        }
        if let Some(err) = &module_error {
            functions.push(FunctionRecord {
                io_observability: io_observability(&sig.io),
                signature: sig.clone(),
                uncallable: Some(Uncallable {
                    reason: "module_not_loadable".to_string(),
                    error: err.clone(),
                }),
                cases: Vec::new(),
                coverage: None,
                branches: None,
                branch_coverage: None,
                dropped_cases: stability_runs.map(|_| DroppedCases::default()),
                output_type_coverage: None,
                unobserved_returns: None,
            });
            continue;
        }
        let replay_inputs: &[Vec<Value>] = replay.get(&sig.name).map(Vec::as_slice).unwrap_or(&[]);
        let opts = GenOptions { max_inputs, domain, cover_branches };
        let (uncallable, mut cases, cover_ctx) = match sig.kind {
            DefKind::Function => {
                let (cases, ctx) = function_cases(sandbox, src, sig, opts, replay_inputs)?;
                (None, cases, ctx)
            }
            DefKind::Method => {
                method_record(sandbox, src, sig, &sigs, opts, &mut ctor_cache, replay_inputs)?
            }
        };
        let dropped_cases = match stability_runs {
            Some(runs) if uncallable.is_none() => {
                let (kept, dropped) =
                    stability::stabilize_cases(sandbox, src, sig, std::mem::take(&mut cases), runs)?;
                cases = kept;
                Some(dropped)
            }
            Some(_) => Some(DroppedCases::default()),
            None => None,
        };
        let coverage = if uncallable.is_some() {
            None
        } else {
            coverage_for(sig, &cases)
        };
        let (branches, branch_coverage) = if uncallable.is_some() {
            (None, None)
        } else {
            match cover::branch_report_for(sig, &cases, &cover_ctx) {
                Some((b, c)) => (Some(b), Some(c)),
                None => (None, None),
            }
        };
        let (output_type_coverage, unobserved_returns) = if uncallable.is_some() {
            (None, None)
        } else {
            match output_type_coverage_for(sig, &cases) {
                Some((otc, unobserved)) => (Some(otc), unobserved),
                None => (None, None),
            }
        };
        functions.push(FunctionRecord {
            io_observability: io_observability(&sig.io),
            signature: sig.clone(),
            uncallable,
            cases,
            coverage,
            branches,
            branch_coverage,
            dropped_cases,
            output_type_coverage,
            unobserved_returns,
        });
    }
    Ok(ModuleRecord {
        dependencies,
        functions,
    })
}

/// For each import, decide whether its module resolves — probing each distinct absolute module
/// once in the jail. Relative imports can't be resolved standalone and are left `NotProbed`.
fn probe_dependencies(
    sandbox: &dyn Sandbox,
    imports: Vec<Import>,
) -> Result<Vec<Dependency>, String> {
    let mut probed: HashMap<String, Option<HarnessError>> = HashMap::new();
    let mut out = Vec::new();
    for import in imports {
        let (status, error) = if import.level > 0 || import.module.is_empty() {
            (DepStatus::NotProbed, None)
        } else {
            let dotted = import.module.dotted();
            if !probed.contains_key(&dotted) {
                let result = probe_import(sandbox, &dotted)?;
                probed.insert(dotted.clone(), result);
            }
            match &probed[&dotted] {
                None => (DepStatus::Resolved, None),
                Some(e) => (DepStatus::Unresolved, Some(e.clone())),
            }
        };
        out.push(Dependency {
            import,
            status,
            error,
        });
    }
    Ok(out)
}

/// Try `import <module>` in the jail. Returns `None` if it resolves, else the structured error.
fn probe_import(sandbox: &dyn Sandbox, module: &str) -> Result<Option<HarnessError>, String> {
    let src = format!("import {module}\n");
    let r = sandbox.probe_load(&src, None, None)?;
    if r.ok {
        Ok(None)
    } else {
        Ok(Some(r.error.unwrap_or_else(|| HarnessError {
            stage: "setup".to_string(),
            kind: "import_failed".to_string(),
            message: format!("import {module} failed"),
            module: None,
        })))
    }
}

fn function_cases(
    sandbox: &dyn Sandbox,
    src: &str,
    sig: &EffectSignature,
    opts: GenOptions,
    replay_inputs: &[Vec<Value>],
) -> Result<(Vec<Case>, cover::CoverContext), String> {
    let mut cases = Vec::new();
    for input in gen_inputs(sig, opts.max_inputs, opts.domain) {
        let result = sandbox.call(src, &sig.name, &input.positional, &input.kwargs)?;
        let mut case = build_case(sig, &input, None, &result, CaseSource::Generated);
        if case.outcome == "raised" {
            case.minimized = minimize_raised(&case, &input, opts.domain, |pos, kw| {
                sandbox.call(src, &sig.name, pos, kw)
            })?;
        }
        cases.push(case);
    }
    for tuple in replay_inputs {
        let result = sandbox.call(src, &sig.name, tuple, &[])?;
        let input = GenInput {
            positional: tuple.clone(),
            kwargs: Vec::new(),
        };
        cases.push(build_case(sig, &input, None, &result, CaseSource::Replay));
    }
    let ctx = cover::run_loop(sandbox, src, sig, cover::CallTarget::Function, opts, &mut cases)?;
    Ok((cases, ctx))
}

/// Shrink a `raised` case's input, re-executing via `call` (the same call shape — free function
/// or method — the case itself ran on). Returns `None` when nothing shrank.
pub(super) fn minimize_raised(
    case: &Case,
    input: &GenInput,
    domain: Option<&ValueDomain>,
    call: impl FnMut(&[Value], &[(String, Value)]) -> Result<CallResult, String>,
) -> Result<Option<MinimizedInput>, String> {
    let exc = case
        .raises
        .as_deref()
        .expect("a raised case always carries an exception type");
    let shrunk = shrink_case(exc, &input.positional, &input.kwargs, domain, call)?;
    Ok(shrunk.map(|(pos, kw)| MinimizedInput {
        input: pos,
        kwargs: kw.into_iter().collect(),
    }))
}

/// Record a method: probe its constructor once per class (cached), and only generate cases if
/// the receiver can be built. A constructor that can't be satisfied is reported once, not as N
/// identical per-case ctor errors.
fn method_record(
    sandbox: &dyn Sandbox,
    src: &str,
    sig: &EffectSignature,
    all: &[EffectSignature],
    opts: GenOptions,
    ctor_cache: &mut HashMap<String, Option<HarnessError>>,
    replay_inputs: &[Vec<Value>],
) -> Result<(Option<Uncallable>, Vec<Case>, cover::CoverContext), String> {
    let class = sig
        .owner
        .as_deref()
        .ok_or_else(|| format!("method {:?} has no owning class", sig.name))?;
    let ctor_args = constructor_args(all, class, opts.domain);

    if !ctor_cache.contains_key(class) {
        let probe = sandbox.probe_load(src, Some(class), Some(&ctor_args))?;
        let err = if probe.ok { None } else { probe.error };
        ctor_cache.insert(class.to_string(), err);
    }
    if let Some(err) = &ctor_cache[class] {
        return Ok((
            Some(Uncallable {
                reason: "constructor_failed".to_string(),
                error: err.clone(),
            }),
            Vec::new(),
            cover::CoverContext::not_run(),
        ));
    }

    let mut cases = Vec::new();
    for input in gen_inputs(sig, opts.max_inputs, opts.domain) {
        let result =
            sandbox.call_method(src, class, &ctor_args, &sig.name, &input.positional, &input.kwargs)?;
        let mut case = build_case(sig, &input, Some(ctor_args.clone()), &result, CaseSource::Generated);
        if case.outcome == "raised" {
            case.minimized = minimize_raised(&case, &input, opts.domain, |pos, kw| {
                sandbox.call_method(src, class, &ctor_args, &sig.name, pos, kw)
            })?;
        }
        cases.push(case);
    }
    for tuple in replay_inputs {
        let result = sandbox.call_method(src, class, &ctor_args, &sig.name, tuple, &[])?;
        let input = GenInput {
            positional: tuple.clone(),
            kwargs: Vec::new(),
        };
        cases.push(build_case(
            sig,
            &input,
            Some(ctor_args.clone()),
            &result,
            CaseSource::Replay,
        ));
    }
    let ctx = cover::run_loop(
        sandbox,
        src,
        sig,
        cover::CallTarget::Method { class, ctor_args: &ctor_args },
        opts,
        &mut cases,
    )?;
    Ok((None, cases, ctx))
}

/// Constructor arguments for `class`: empty when `__init__` is absent or fully defaulted
/// (so a no-arg receiver is valid), else the first generated vector's positional arguments.
fn constructor_args(all: &[EffectSignature], class: &str, domain: Option<&ValueDomain>) -> Vec<Value> {
    let Some(init) = all
        .iter()
        .find(|s| s.name == "__init__" && s.owner.as_deref() == Some(class))
    else {
        return Vec::new();
    };
    if init.params.iter().all(|p| p.has_default) {
        return Vec::new();
    }
    gen_inputs(init, 1, domain)
        .into_iter()
        .next()
        .map(|g| g.positional)
        .unwrap_or_default()
}

pub(super) fn build_case(
    sig: &EffectSignature,
    input: &GenInput,
    ctor_args: Option<Vec<Value>>,
    r: &CallResult,
    source: CaseSource,
) -> Case {
    let mut mutations = Vec::new();

    // Positional argument mutations: diff the pre-call snapshot against the post-call state.
    // Both come from the worker in the same tagged encoding, so equal values compare equal.
    // `positional_params(sig)` is the same filter used to build `input.positional`, so the
    // index alignment holds.
    if let (Some(pre), Some(post)) = (&r.args_pre, &r.args_post) {
        for (i, p) in positional_params(sig).into_iter().enumerate() {
            if let (Some(before), Some(after)) = (pre.get(i), post.get(i))
                && !value_eq(before, after)
            {
                mutations.push(ObservedMutation {
                    target: p.name.clone(),
                    before: before.clone(),
                    after: after.clone(),
                });
            }
        }
    }
    // Keyword-only argument mutations: diff by name, symmetric to the positional case above.
    if let (Some(pre), Some(post)) = (&r.kwargs_pre, &r.kwargs_post) {
        for p in keyword_only_params(sig) {
            if let (Some(before), Some(after)) = (pre.get(&p.name), post.get(&p.name))
                && !value_eq(before, after)
            {
                mutations.push(ObservedMutation {
                    target: p.name.clone(),
                    before: before.clone(),
                    after: after.clone(),
                });
            }
        }
    }
    // Receiver mutation: diff self_pre against self_post.
    if let (Some(pre), Some(post)) = (&r.self_pre, &r.self_post)
        && !value_eq(pre, post)
    {
        mutations.push(ObservedMutation {
            target: "self".to_string(),
            before: pre.clone(),
            after: post.clone(),
        });
    }

    let stdout = r.stdout.clone();
    let stderr = r.stderr.clone();
    let kwargs: Map<String, Value> = input.kwargs.iter().cloned().collect();

    if let Some(err) = &r.error {
        return Case {
            input: input.positional.clone(),
            kwargs,
            ctor_args,
            source,
            outcome: "error".to_string(),
            ret: None,
            raises: None,
            mutations,
            return_aliases_arg: None,
            stdout,
            stderr,
            error: Some(err.clone()),
            minimized: None,
            lines: r.lines.clone(),
            arcs: r.arcs.clone(),
        };
    }
    if r.ok {
        Case {
            input: input.positional.clone(),
            kwargs,
            ctor_args,
            source,
            outcome: "returned".to_string(),
            ret: Some(r.ret.clone()),
            raises: None,
            mutations,
            return_aliases_arg: r.return_aliases_arg,
            stdout,
            stderr,
            error: None,
            minimized: None,
            lines: r.lines.clone(),
            arcs: r.arcs.clone(),
        }
    } else {
        Case {
            input: input.positional.clone(),
            kwargs,
            ctor_args,
            source,
            outcome: "raised".to_string(),
            ret: None,
            raises: r.exception.as_ref().map(|e| e.ty.clone()),
            mutations,
            return_aliases_arg: None,
            stdout,
            stderr,
            error: None,
            minimized: None,
            lines: r.lines.clone(),
            arcs: r.arcs.clone(),
        }
    }
}

/// Structural equality with float tolerance; sets/dict items are pre-sorted by the worker, so
/// positional array comparison is order-insensitive for them.
pub(super) fn value_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(xa), Some(yb)) => (xa - yb).abs() <= 1e-9 * (1.0 + xa.abs().max(yb.abs())),
            _ => x == y,
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| value_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| value_eq(v, w)))
        }
        _ => a == b,
    }
}
