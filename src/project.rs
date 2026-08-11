//! Multi-file / directory ("project") analysis: walk a directory for `*.py` files, run
//! `analyze` / `record` / `validate` over each, and aggregate into one project-level JSON
//! report. A file that fails to parse or record becomes a `{path, error}` entry instead of
//! aborting the whole run — see `main.rs` for the CLI dispatch that picks this path over the
//! single-file one.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ignore::WalkBuilder;
use serde_json::Value;

use crate::analyze::{analyze_module_with_call_sites, collect_imports};
use crate::exec::{NsjailPool, Sandbox};
use crate::model::{EffectSignature, Import, Purity};
use crate::record::record_with_signatures;
use crate::validate::{Severity, validate_function};

pub mod interproc;
pub mod resolve;
use interproc::{FileUnit, propagate};
use resolve::{ModuleIndex, annotate_with_resolution, resolve_import};

/// Directory names skipped anywhere in the tree, on top of whatever `.gitignore`/`.ignore`
/// already exclude. Covers well-known noise dirs that don't start with `.` (so aren't caught by
/// the walker's hidden-file skipping) and that a project may not have gitignored at all.
const SKIP_DIRS: &[&str] = &["__pycache__", "venv", "env", "node_modules", "build", "dist", "target"];

fn is_skipped_dir(name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|n| SKIP_DIRS.contains(&n))
}

/// Recursively collect every `*.py` file under `root` using the `ignore` crate's walker: it
/// honors `.gitignore`/`.ignore` hierarchically (even outside a git repository — `require_git`
/// is disabled) and skips hidden files/dirs by default, on top of the explicit [`SKIP_DIRS`]
/// list for common noise dirs a project may not have gitignored. Sorted by path for
/// deterministic output. A directory that can't be read (e.g. a permissions error partway
/// through the tree) is silently skipped rather than aborting the whole walk, consistent with
/// "one bad file/dir must not abort the run".
pub fn collect_py_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let walker = WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|entry| !entry.file_type().is_some_and(|ft| ft.is_dir()) || !is_skipped_dir(entry.file_name()))
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if entry.file_type().is_some_and(|ft| ft.is_file())
            && path.extension().and_then(|e| e.to_str()) == Some("py")
        {
            out.push(path.to_path_buf());
        }
    }
    out.sort();
    out
}

pub(crate) fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// One file's contribution to a project report: its JSON body (ok result or `{path, error}`)
/// plus the per-file counts needed to build the aggregate `summary` without re-parsing JSON.
struct FileEntry {
    path: String,
    json: Value,
    purities: Vec<Purity>,
    hard_defects: usize,
    soft_defects: usize,
    functions_checked: usize,
    /// Functions that never executed (`uncallable`: module didn't load, constructor failed) —
    /// validate observed nothing for them, so their zero defects must not read as "validated".
    uncallable: usize,
    /// Sum of `coverage.executed` over this file's functions that carry a `coverage` (see
    /// `record::Coverage`) — the numerator half of the project-wide aggregate.
    coverage_executed: usize,
    /// Sum of `coverage.total` over the same functions — the denominator half.
    coverage_total: usize,
    ok: bool,
}

fn error_entry(path: String, message: String) -> FileEntry {
    FileEntry {
        json: serde_json::json!({ "path": path, "error": message }),
        path,
        purities: Vec::new(),
        hard_defects: 0,
        soft_defects: 0,
        functions_checked: 0,
        uncallable: 0,
        coverage_executed: 0,
        coverage_total: 0,
        ok: false,
    }
}

fn build_report(root: &Path, mut entries: Vec<FileEntry>, include_defects: bool) -> Value {
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let files = entries.len();
    let ok = entries.iter().filter(|e| e.ok).count();
    let errors = files - ok;
    let mut functions = 0usize;
    let (mut pure, mut impure, mut unknown) = (0usize, 0usize, 0usize);
    let mut hard_total = 0usize;
    let mut soft_total = 0usize;
    let mut checked_total = 0usize;
    let mut uncallable_total = 0usize;
    let mut coverage_executed_total = 0usize;
    let mut coverage_total_total = 0usize;
    for e in &entries {
        functions += e.purities.len();
        for p in &e.purities {
            match p {
                Purity::Pure => pure += 1,
                Purity::Impure => impure += 1,
                Purity::Unknown => unknown += 1,
            }
        }
        hard_total += e.hard_defects;
        soft_total += e.soft_defects;
        checked_total += e.functions_checked;
        uncallable_total += e.uncallable;
        coverage_executed_total += e.coverage_executed;
        coverage_total_total += e.coverage_total;
    }

    let mut summary = serde_json::json!({
        "files": files,
        "ok": ok,
        "errors": errors,
        "functions": functions,
        "purity": { "pure": pure, "impure": impure, "unknown": unknown },
    });
    if include_defects {
        summary["hard_defects"] = serde_json::json!(hard_total);
        summary["soft_defects"] = serde_json::json!(soft_total);
        summary["functions_checked"] = serde_json::json!(checked_total);
        summary["uncallable"] = serde_json::json!(uncallable_total);
    }
    if coverage_total_total > 0 {
        summary["coverage"] = serde_json::json!({
            "executed": coverage_executed_total,
            "total": coverage_total_total,
        });
    }

    let files_json: Vec<Value> = entries.into_iter().map(|e| e.json).collect();
    serde_json::json!({
        "schema_version": crate::SCHEMA_VERSION,
        "root": root.display().to_string(),
        "files": files_json,
        "summary": summary,
    })
}

/// Spread `f` over `items` across a small pool of worker threads (`std::thread::available_
/// parallelism`, capped at `items.len()`), then return the results in whatever order the
/// threads finished. Used only for the CPU-bound, jail-free `analyze` path: `record`/`validate`
/// go through the jail and stay sequential (see [`record_project`]).
fn parallel_map<T, F>(items: &[PathBuf], f: F) -> Vec<T>
where
    T: Send,
    F: Fn(&Path) -> T + Sync,
{
    if items.is_empty() {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
        .min(items.len());
    let chunk_size = items.len().div_ceil(workers).max(1);
    let results = Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|scope| {
        for chunk in items.chunks(chunk_size) {
            let f = &f;
            let results = &results;
            scope.spawn(move || {
                let local: Vec<T> = chunk.iter().map(|p| f(p)).collect();
                results.lock().unwrap().extend(local);
            });
        }
    });
    results.into_inner().unwrap()
}

/// Cap on jailed worker threads for project-mode `record`/`validate`. Each worker owns its own
/// `NsjailPool`, i.e. its own WSL/nsjail fork-server process — unlike `parallel_map`'s CPU-bound
/// workers, spawning one per available core is wasteful (and on Windows, one `wsl` process per
/// thread is real overhead), so this is capped independently of `available_parallelism`.
const JAIL_WORKER_CAP: usize = 4;

/// Run `process` over every item in `items` using a fixed set of worker threads, each owning its
/// own single-worker [`NsjailPool`] (files are independent, so no pool is ever shared across
/// threads — each stays a private request/response channel). Worker count is
/// `available_parallelism` capped at [`JAIL_WORKER_CAP`] and at `items.len()`. If any worker
/// fails to start its pool (e.g. the sandbox isn't provisioned), that failure is propagated as
/// the whole run's error, same as today's single-pool startup failure. Result order does not
/// need to match `items`' order — callers (`build_report`) sort by path before emitting.
fn record_files_parallel<T, F>(items: Vec<T>, process: F) -> Result<Vec<FileEntry>, String>
where
    T: Send,
    F: Fn(&NsjailPool, T) -> FileEntry + Sync,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let workers = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1)
        .min(JAIL_WORKER_CAP)
        .min(items.len());

    let queue: Mutex<VecDeque<T>> = Mutex::new(items.into_iter().collect());
    let results: Mutex<Vec<FileEntry>> = Mutex::new(Vec::new());
    let process = &process;
    let queue = &queue;
    let results_ref = &results;

    std::thread::scope(|scope| -> Result<(), String> {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(move || -> Result<(), String> {
                let pool = NsjailPool::new(1)?;
                loop {
                    let item = queue.lock().unwrap().pop_front();
                    let Some(item) = item else { break };
                    let entry = process(&pool, item);
                    results_ref.lock().unwrap().push(entry);
                }
                Ok(())
            }));
        }
        for handle in handles {
            handle.join().unwrap()?;
        }
        Ok(())
    })?;

    Ok(results.into_inner().unwrap())
}

/// One file's raw (not yet cross-file-propagated) static analysis: its imports, the Effects
/// pass's signatures, and the imported call sites cross-file propagation needs — see
/// `interproc::FileUnit`.
struct AnalyzedFile {
    path: String,
    src: String,
    imports: Vec<Import>,
    signatures: Vec<EffectSignature>,
    import_call_sites: Vec<Vec<crate::analyze::ImportCallSite>>,
}

fn analyze_file_raw(root: &Path, path: &Path) -> Result<AnalyzedFile, Box<FileEntry>> {
    let rel = relative_path(root, path);
    let src = match std::fs::read_to_string(path) {
        Ok(s) => crate::strip_bom(&s).to_string(),
        Err(e) => return Err(Box::new(error_entry(rel, format!("read error: {e}")))),
    };
    let parsed = match crate::parse::parse_source(&src) {
        Ok(p) => p,
        Err(e) => return Err(Box::new(error_entry(rel, format!("parse error: {e}")))),
    };
    let imports = collect_imports(parsed.syntax());
    let result = analyze_module_with_call_sites(parsed.syntax(), &src);
    Ok(AnalyzedFile {
        path: rel,
        src,
        imports,
        signatures: result.signatures,
        import_call_sites: result.import_call_sites,
    })
}

/// One project file's static analysis, after cross-file effect propagation has settled — the
/// enriched inputs `record`/`validate` (as well as `analyze`'s own report) build on.
struct EnrichedFile {
    path: String,
    src: String,
    imports: Vec<Import>,
    signatures: Vec<EffectSignature>,
}

/// Analyze every project file (in parallel — pure and CPU-bound, no jail involved), then run
/// cross-file effect propagation (`interproc::propagate`) once over all of them together. Shared
/// by `analyze_project`, `record_project`, and `validate_project` so every project-mode entry
/// point works from the same cross-file-propagated signatures. A file that fails to read or
/// parse becomes an error [`FileEntry`] instead, kept out of propagation.
fn analyze_and_propagate(root: &Path, files: &[PathBuf]) -> (Vec<EnrichedFile>, Vec<FileEntry>) {
    let raw: Vec<Result<AnalyzedFile, Box<FileEntry>>> = parallel_map(files, |path| analyze_file_raw(root, path));

    let mut srcs = Vec::new();
    let mut units = Vec::new();
    let mut errors = Vec::new();
    for r in raw {
        match r {
            Ok(af) => {
                srcs.push(af.src);
                units.push(FileUnit {
                    path: af.path,
                    signatures: af.signatures,
                    import_call_sites: af.import_call_sites,
                    imports: af.imports,
                });
            }
            Err(e) => errors.push(*e),
        }
    }
    propagate(&mut units, root);

    let enriched = units
        .into_iter()
        .zip(srcs)
        .map(|(fu, src)| EnrichedFile { path: fu.path, src, imports: fu.imports, signatures: fu.signatures })
        .collect();
    (enriched, errors)
}

fn build_analyze_entry(ef: EnrichedFile, index: &ModuleIndex) -> FileEntry {
    let purities = ef.signatures.iter().map(|f| f.purity).collect();
    let imports_json: Vec<Value> = ef
        .imports
        .iter()
        .map(|imp| annotate_with_resolution(imp, &resolve_import(index, &ef.path, imp)))
        .collect();
    let json = serde_json::json!({
        "path": ef.path,
        "imports": imports_json,
        "functions": ef.signatures,
    });
    FileEntry {
        path: ef.path,
        json,
        purities,
        hard_defects: 0,
        soft_defects: 0,
        functions_checked: 0,
        uncallable: 0,
        coverage_executed: 0,
        coverage_total: 0,
        ok: true,
    }
}

/// Static effect analysis over every `*.py` file under `root`. Each file's imports are also
/// statically resolved against the other files in the project (see `resolve::resolve_import`).
/// Once every file's raw signatures are in, cross-file effect propagation
/// (`interproc::propagate`) runs once over all of them together before the report is built, so
/// the reported `functions` reflect cross-file-propagated effects.
pub fn analyze_project(root: &Path) -> Value {
    let files = collect_py_files(root);
    let (enriched, errors) = analyze_and_propagate(root, &files);
    let index = ModuleIndex::build(root, &files);
    let mut entries: Vec<FileEntry> = enriched.into_iter().map(|ef| build_analyze_entry(ef, &index)).collect();
    entries.extend(errors);
    build_report(root, entries, false)
}

fn record_file_entry(sandbox: &dyn Sandbox, index: &ModuleIndex, ef: EnrichedFile, max_inputs: usize) -> FileEntry {
    let EnrichedFile { path, src, imports, signatures } = ef;
    match record_with_signatures(sandbox, &src, imports, signatures, max_inputs) {
        Ok(record) => {
            let purities = record.functions.iter().map(|f| f.signature.purity).collect();
            let (coverage_executed, coverage_total) = record
                .functions
                .iter()
                .filter_map(|f| f.coverage.as_ref())
                .fold((0, 0), |(e, t), c| (e + c.executed, t + c.total));
            let deps_json: Vec<Value> = record
                .dependencies
                .iter()
                .map(|dep| annotate_with_resolution(dep, &resolve_import(index, &path, &dep.import)))
                .collect();
            let json = serde_json::json!({
                "path": path,
                "dependencies": deps_json,
                "functions": record.functions,
            });
            FileEntry {
                path,
                json,
                purities,
                hard_defects: 0,
                soft_defects: 0,
                functions_checked: 0,
                uncallable: 0,
                coverage_executed,
                coverage_total,
                ok: true,
            }
        }
        Err(e) => error_entry(path, format!("record error: {e}")),
    }
}

/// Record every `*.py` file under `root`, spread across a fixed pool of worker threads (each
/// owning its own jailed [`NsjailPool`]) via [`record_files_parallel`] — files are independent,
/// so this parallelizes cleanly. Fails the whole run only if a worker's sandbox can't be
/// provisioned at all; a per-file record failure becomes a `{path, error}` entry instead.
/// Records against cross-file-propagated signatures, same as `analyze_project`.
pub fn record_project(root: &Path, max_inputs: usize) -> Result<Value, String> {
    let files = collect_py_files(root);
    let index = &ModuleIndex::build(root, &files);
    let (enriched, error_entries) = analyze_and_propagate(root, &files);
    let mut entries =
        record_files_parallel(enriched, |pool, ef| record_file_entry(pool, index, ef, max_inputs))?;
    entries.extend(error_entries);
    Ok(build_report(root, entries, false))
}

fn validate_file_entry(sandbox: &dyn Sandbox, ef: EnrichedFile, max_inputs: usize) -> FileEntry {
    let EnrichedFile { path, src, imports, signatures } = ef;
    let record = match record_with_signatures(sandbox, &src, imports, signatures, max_inputs) {
        Ok(r) => r,
        Err(e) => return error_entry(path, format!("record error: {e}")),
    };

    let mut hard_total = 0usize;
    let mut soft_total = 0usize;
    let mut coverage_executed = 0usize;
    let mut coverage_total = 0usize;
    let functions_json: Vec<Value> = record
        .functions
        .iter()
        .map(|f| {
            let defects = validate_function(f);
            let hard = defects.iter().filter(|d| d.severity == Severity::Hard).count();
            let soft = defects.len() - hard;
            hard_total += hard;
            soft_total += soft;
            if let Some(cov) = &f.coverage {
                coverage_executed += cov.executed;
                coverage_total += cov.total;
            }
            serde_json::json!({
                "name": f.signature.name,
                "owner": f.signature.owner,
                "hard_defects": hard,
                "soft_defects": soft,
                "defects": defects,
                "coverage": f.coverage,
            })
        })
        .collect();
    let purities = record.functions.iter().map(|f| f.signature.purity).collect();
    let functions_checked = record.functions.len();
    let uncallable = record.functions.iter().filter(|f| f.uncallable.is_some()).count();
    let mut summary = serde_json::json!({
        "hard_defects": hard_total,
        "soft_defects": soft_total,
        "functions_checked": functions_checked,
        "uncallable": uncallable,
    });
    if coverage_total > 0 {
        summary["coverage"] = serde_json::json!({ "executed": coverage_executed, "total": coverage_total });
    }
    let json = serde_json::json!({
        "path": path,
        "functions": functions_json,
        "summary": summary,
    });
    FileEntry {
        path,
        json,
        purities,
        hard_defects: hard_total,
        soft_defects: soft_total,
        functions_checked,
        uncallable,
        coverage_executed,
        coverage_total,
        ok: true,
    }
}

/// Validate every `*.py` file under `root` (record + `observed ⊆ static` check), aggregating
/// hard/soft defect totals. Returns the report plus the aggregate hard-defect count so the
/// caller can set the same non-zero exit code contract as single-file `validate`. Validates
/// against cross-file-propagated signatures, same as `analyze_project`.
pub fn validate_project(root: &Path, max_inputs: usize) -> Result<(Value, usize), String> {
    let files = collect_py_files(root);
    let (enriched, error_entries) = analyze_and_propagate(root, &files);
    let mut entries = record_files_parallel(enriched, |pool, ef| validate_file_entry(pool, ef, max_inputs))?;
    entries.extend(error_entries);
    let hard_total: usize = entries.iter().map(|e| e.hard_defects).sum();
    Ok((build_report(root, entries, true), hard_total))
}
