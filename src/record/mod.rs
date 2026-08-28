//! Per-function **effect records**: the static signature plus concrete *cases* — generated
//! inputs run in the jail, with the effects actually observed (return, raises, argument and
//! `self` mutations, aliasing). This is the record for one function; it does no comparison and
//! computes no score — that belongs to whatever consumes these records.

use std::collections::HashMap;

use serde_json::Value;

use crate::exec::{CallResult, HarnessError, NsjailPool, Sandbox};
use crate::generate::{GenInput, ValueDomain, gen_inputs};
use crate::model::{DefKind, EffectSignature, Import};
use crate::shrink::shrink_case;
use crate::{analyze_source, imports_of};

mod case;
mod cover;
mod stability;

pub use case::{
    Case, CaseSource, Coverage, DepStatus, Dependency, FunctionRecord, IoObservability,
    MinimizedInput, ModuleRecord, ObservedMutation, OutputTypeCoverage, Uncallable, UnobservedReturns,
};
pub use cover::{BranchCoverage, BranchOutcomeReport, BranchReport};
pub use stability::DroppedCases;

use case::{build_case, coverage_for, io_observability, output_type_coverage_for};

/// One long-lived jailed worker, reused for the whole file (load probe + dependency probes +
/// every case). Recording is sequential, so a single fork-server worker amortizes interpreter
/// startup without idle jails; each request still runs in its own forked child.
const POOL_SIZE: usize = 1;

/// Bundles `--value-domain`, `--cover-branches`, `--stability-runs`, and `--time-budget` for
/// [`record_with_signatures`] — keeps its argument count down alongside
/// `sandbox`/`src`/`imports`/`sigs`/`max_inputs`/`replay`. `RecordFlags::default()` is the plain
/// path: no domain restriction, no branch coverage loop, no stability re-runs, no time budget.
#[derive(Clone, Copy, Default)]
pub struct RecordFlags<'a> {
    pub domain: Option<&'a ValueDomain>,
    pub cover_branches: bool,
    /// `--stability-runs <N>`: re-execute every case (generated, cover-loop, and replayed alike)
    /// until it has run `N` times total, dropping any case whose runs disagree — see
    /// [`stabilize_cases`]. `None` (the default) leaves `record`'s output unchanged, including
    /// omitting `FunctionRecord::dropped_cases` entirely.
    pub stability_runs: Option<usize>,
    /// `--time-budget <seconds>`: a soft per-function wall cap covering generated-case
    /// execution, the `--cover-branches` loop, and `--stability-runs` re-runs of generated
    /// cases. Replayed cases (see [`ReplayMap`]) always execute in full regardless of the
    /// budget — external evidence must not silently vanish. `None` (the default) leaves
    /// `record`'s behavior and timing unchanged.
    pub time_budget: Option<std::time::Duration>,
}

/// Generation settings threaded through the recording of one function or method: the
/// `--inputs` budget (interpreted as the TOTAL per-function case budget once `cover_branches` is
/// set — see `cover::run_loop`), the `--value-domain` profile to enforce (if any), whether
/// `--cover-branches` opted into the predicate-targeted coverage loop, and the per-function
/// `--time-budget` deadline (if any). Bundled to keep `function_cases`/`method_record`'s
/// argument counts down.
#[derive(Clone, Copy)]
pub(super) struct GenOptions<'a> {
    pub(super) max_inputs: usize,
    pub(super) domain: Option<&'a ValueDomain>,
    pub(super) cover_branches: bool,
    /// Once `std::time::Instant::now() >= deadline`, generation loops stop starting new
    /// generated work — see [`RecordFlags::time_budget`].
    pub(super) deadline: Option<std::time::Instant>,
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

/// External input tuples supplied via project-mode `--replay`: file path (relative to the
/// project root, forward slashes, exactly as project output's `path` field renders it) → that
/// file's [`ReplayMap`]. See [`parse_project_replay`].
pub type ProjectReplayMap = HashMap<String, ReplayMap>;

/// Parse a project-mode `--replay` file: a JSON object mapping a file path to a nested object of
/// function name → input tuples (the single-file [`parse_replay`] shape, one level deeper).
/// Malformed JSON, a non-object top level, or a per-file value that isn't an object of
/// function-name → array-of-arrays, is an error surfaced to the caller. Does not check paths
/// against the project's actual files — that requires knowing which files were analyzed, so it's
/// the caller's job (see `project::record_project`).
pub fn parse_project_replay(text: &str) -> Result<ProjectReplayMap, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("replay file: {e}"))?;
    let obj = value.as_object().ok_or_else(|| {
        "replay file: expected a JSON object mapping file path to per-function input tuples".to_string()
    })?;
    let mut out = ProjectReplayMap::new();
    for (path, per_fn) in obj {
        let fn_obj = per_fn.as_object().ok_or_else(|| {
            format!("replay file: {path:?} must map to an object of function name to input tuples")
        })?;
        let mut replay = ReplayMap::new();
        for (name, tuples) in fn_obj {
            let arr = tuples.as_array().ok_or_else(|| {
                format!("replay file: {path:?}.{name:?} must map to an array of input tuples")
            })?;
            let mut parsed = Vec::with_capacity(arr.len());
            for tuple in arr {
                let t = tuple.as_array().ok_or_else(|| {
                    format!("replay file: each input for {path:?}.{name:?} must be an array")
                })?;
                parsed.push(t.clone());
            }
            replay.insert(name.clone(), parsed);
        }
        out.insert(path.clone(), replay);
    }
    Ok(out)
}

/// Record a whole file: catalog + probe its imports, then record every function and method
/// with `max_inputs` generated cases each. If the module won't load (a module-scope import is
/// unresolved), every function is marked `uncallable` once rather than producing identical
/// per-case setup errors. `replay` supplies externally-provided `--replay` input tuples executed
/// in addition to the generated ones (empty for none — see [`parse_replay`]); `flags` bundles
/// `--value-domain`/`--cover-branches`/`--stability-runs`/`--time-budget`
/// (`RecordFlags::default()` for none of them).
pub fn record_file(
    src: &str,
    max_inputs: usize,
    replay: &ReplayMap,
    flags: RecordFlags,
) -> Result<ModuleRecord, String> {
    let sandbox = NsjailPool::new(POOL_SIZE)?;
    let imports = imports_of(src).map_err(|e| e.to_string())?;
    let sigs = analyze_source(src).map_err(|e| e.to_string())?;
    record_with_signatures(&sandbox, src, imports, sigs, max_inputs, replay, flags)
}

/// Record a whole file's functions against an already-provisioned sandbox, using precomputed
/// imports and effect signatures rather than deriving them from `src` with `analyze_source`.
/// Lets a caller that has already produced (and, in project mode, cross-file-propagated)
/// signatures — see `project::interproc` — record and validate against those enriched
/// signatures instead of the plain per-file ones. Behavior is otherwise identical to
/// [`record_file`], including how `sig.name == "__init__"` is skipped and how
/// `positional_params(sig)`/`gen_inputs(sig, ...)` read `sigs`. Each `replay` tuple is executed
/// exactly like a generated one (same sandbox call, same mutation-diff machinery), tagged
/// `CaseSource::Replay`, and never shrunk. A replay key that names no function in `sigs` is an
/// error — the caller (see `pylens::main`) surfaces it and exits non-zero rather than silently
/// dropping unmatched replay data.
pub fn record_with_signatures(
    sandbox: &dyn Sandbox,
    src: &str,
    imports: Vec<Import>,
    sigs: Vec<EffectSignature>,
    max_inputs: usize,
    replay: &ReplayMap,
    flags: RecordFlags,
) -> Result<ModuleRecord, String> {
    let RecordFlags { domain, cover_branches, stability_runs, time_budget } = flags;
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
                time_budget_hit: None,
            });
            continue;
        }
        let replay_inputs: &[Vec<Value>] = replay.get(&sig.name).map(Vec::as_slice).unwrap_or(&[]);
        let deadline = time_budget.map(|d| std::time::Instant::now() + d);
        let opts = GenOptions { max_inputs, domain, cover_branches, deadline };
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
                    stability::stabilize_cases(sandbox, src, sig, std::mem::take(&mut cases), runs, deadline)?;
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
        let time_budget_hit = deadline.is_some_and(|dl| std::time::Instant::now() >= dl);
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
            time_budget_hit: time_budget_hit.then_some(true),
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

/// Whether `opts.deadline` (the `--time-budget` cap) has already passed — generation loops stop
/// starting new *generated* work once this is true, but replayed cases (see [`ReplayMap`]) never
/// check it: external evidence must not silently vanish.
pub(super) fn deadline_passed(deadline: Option<std::time::Instant>) -> bool {
    deadline.is_some_and(|dl| std::time::Instant::now() >= dl)
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
        if deadline_passed(opts.deadline) {
            break;
        }
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
        if deadline_passed(opts.deadline) {
            break;
        }
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
