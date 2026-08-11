//! Per-function **effect records**: the static signature plus concrete *cases* — generated
//! inputs run in the jail, with the effects actually observed (return, raises, argument and
//! `self` mutations, aliasing). This is the record for one function; it does no comparison and
//! computes no score — that belongs to whatever consumes these records.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use serde_json::{Map, Value};

use crate::exec::{CallResult, HarnessError, NsjailPool, Sandbox};
use crate::generate::{GenInput, gen_inputs, keyword_only_params, positional_params};
use crate::model::{DefKind, EffectSignature, Import};
use crate::shrink::shrink_case;
use crate::{analyze_source, imports_of};

/// One long-lived jailed worker, reused for the whole file (load probe + dependency probes +
/// every case). Recording is sequential, so a single fork-server worker amortizes interpreter
/// startup without idle jails; each request still runs in its own forked child.
const POOL_SIZE: usize = 1;

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
    record_with(&sandbox, src, max_inputs)
}

/// Record a whole file's functions against an already-provisioned sandbox. Lets a caller
/// processing many files (see `project.rs`) share one jail pool instead of paying nsjail
/// startup per file; behavior is otherwise identical to [`record_file`].
pub fn record_with(sandbox: &dyn Sandbox, src: &str, max_inputs: usize) -> Result<ModuleRecord, String> {
    let imports = imports_of(src).map_err(|e| e.to_string())?;
    let sigs = analyze_source(src).map_err(|e| e.to_string())?;
    record_with_signatures(sandbox, src, imports, sigs, max_inputs)
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
) -> Result<ModuleRecord, String> {
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
                signature: sig.clone(),
                uncallable: Some(Uncallable {
                    reason: "module_not_loadable".to_string(),
                    error: err.clone(),
                }),
                cases: Vec::new(),
                coverage: None,
            });
            continue;
        }
        let (uncallable, cases) = match sig.kind {
            DefKind::Function => (None, function_cases(sandbox, src, sig, max_inputs)?),
            DefKind::Method => method_record(sandbox, src, sig, &sigs, max_inputs, &mut ctor_cache)?,
        };
        let coverage = if uncallable.is_some() {
            None
        } else {
            coverage_for(sig, &cases)
        };
        functions.push(FunctionRecord {
            signature: sig.clone(),
            uncallable,
            cases,
            coverage,
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
    max_inputs: usize,
) -> Result<Vec<Case>, String> {
    let mut cases = Vec::new();
    for input in gen_inputs(sig, max_inputs) {
        let result = sandbox.call(src, &sig.name, &input.positional, &input.kwargs)?;
        let mut case = build_case(sig, &input, None, &result);
        if case.outcome == "raised" {
            case.minimized = minimize_raised(&case, &input, |pos, kw| {
                sandbox.call(src, &sig.name, pos, kw)
            })?;
        }
        cases.push(case);
    }
    Ok(cases)
}

/// Shrink a `raised` case's input, re-executing via `call` (the same call shape — free function
/// or method — the case itself ran on). Returns `None` when nothing shrank.
fn minimize_raised(
    case: &Case,
    input: &GenInput,
    call: impl FnMut(&[Value], &[(String, Value)]) -> Result<CallResult, String>,
) -> Result<Option<MinimizedInput>, String> {
    let exc = case
        .raises
        .as_deref()
        .expect("a raised case always carries an exception type");
    let shrunk = shrink_case(exc, &input.positional, &input.kwargs, call)?;
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
    max_inputs: usize,
    ctor_cache: &mut HashMap<String, Option<HarnessError>>,
) -> Result<(Option<Uncallable>, Vec<Case>), String> {
    let class = sig
        .owner
        .as_deref()
        .ok_or_else(|| format!("method {:?} has no owning class", sig.name))?;
    let ctor_args = constructor_args(all, class);

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
        ));
    }

    let mut cases = Vec::new();
    for input in gen_inputs(sig, max_inputs) {
        let result =
            sandbox.call_method(src, class, &ctor_args, &sig.name, &input.positional, &input.kwargs)?;
        let mut case = build_case(sig, &input, Some(ctor_args.clone()), &result);
        if case.outcome == "raised" {
            case.minimized = minimize_raised(&case, &input, |pos, kw| {
                sandbox.call_method(src, class, &ctor_args, &sig.name, pos, kw)
            })?;
        }
        cases.push(case);
    }
    Ok((None, cases))
}

/// Constructor arguments for `class`: empty when `__init__` is absent or fully defaulted
/// (so a no-arg receiver is valid), else the first generated vector's positional arguments.
fn constructor_args(all: &[EffectSignature], class: &str) -> Vec<Value> {
    let Some(init) = all
        .iter()
        .find(|s| s.name == "__init__" && s.owner.as_deref() == Some(class))
    else {
        return Vec::new();
    };
    if init.params.iter().all(|p| p.has_default) {
        return Vec::new();
    }
    gen_inputs(init, 1)
        .into_iter()
        .next()
        .map(|g| g.positional)
        .unwrap_or_default()
}

fn build_case(
    sig: &EffectSignature,
    input: &GenInput,
    ctor_args: Option<Vec<Value>>,
    r: &CallResult,
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
        };
    }
    if r.ok {
        Case {
            input: input.positional.clone(),
            kwargs,
            ctor_args,
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
        }
    } else {
        Case {
            input: input.positional.clone(),
            kwargs,
            ctor_args,
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
        }
    }
}

/// Structural equality with float tolerance; sets/dict items are pre-sorted by the worker, so
/// positional array comparison is order-insensitive for them.
fn value_eq(a: &Value, b: &Value) -> bool {
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
