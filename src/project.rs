//! Multi-file / directory ("project") analysis: walk a directory for `*.py` files, run
//! `analyze` / `record` / `validate` over each, and aggregate into one project-level JSON
//! report. A file that fails to parse or record becomes a `{path, error}` entry instead of
//! aborting the whole run — see `main.rs` for the CLI dispatch that picks this path over the
//! single-file one.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde_json::Value;

use crate::exec::{NsjailPool, Sandbox};
use crate::model::Purity;
use crate::record::record_with;
use crate::validate::{Severity, validate_function};

/// Directory names skipped anywhere in the tree, plus any directory whose name starts with
/// `.`. Honoring `.gitignore` is future work; this is a fixed list of well-known noise dirs.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "__pycache__",
    ".venv",
    "venv",
    "env",
    "node_modules",
    "build",
    "dist",
    "target",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
];

fn is_skipped_dir(name: &str) -> bool {
    name.starts_with('.') || SKIP_DIRS.contains(&name)
}

/// Recursively collect every `*.py` file under `root`, skipping [`SKIP_DIRS`] and dotfile
/// directories. Sorted by path for deterministic output. A directory that can't be read (e.g.
/// a permissions error partway through the tree) is silently skipped rather than aborting the
/// whole walk, consistent with "one bad file/dir must not abort the run".
pub fn collect_py_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skip = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(is_skipped_dir);
            if !skip {
                walk(&path, out);
            }
        } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
            out.push(path);
        }
    }
}

fn relative_path(root: &Path, path: &Path) -> String {
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
fn parallel_map<F>(items: &[PathBuf], f: F) -> Vec<FileEntry>
where
    F: Fn(&Path) -> FileEntry + Sync,
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
                let local: Vec<FileEntry> = chunk.iter().map(|p| f(p)).collect();
                results.lock().unwrap().extend(local);
            });
        }
    });
    results.into_inner().unwrap()
}

fn analyze_file_entry(root: &Path, path: &Path) -> FileEntry {
    let rel = relative_path(root, path);
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return error_entry(rel, format!("read error: {e}")),
    };
    match (crate::imports_of(&src), crate::analyze_source(&src)) {
        (Ok(imports), Ok(functions)) => {
            let purities = functions.iter().map(|f| f.purity).collect();
            let json = serde_json::json!({
                "path": rel,
                "imports": imports,
                "functions": functions,
            });
            FileEntry {
                path: rel,
                json,
                purities,
                hard_defects: 0,
                soft_defects: 0,
                functions_checked: 0,
                ok: true,
            }
        }
        (Err(e), _) | (_, Err(e)) => error_entry(rel, format!("parse error: {e}")),
    }
}

/// Static effect analysis over every `*.py` file under `root`. Pure and CPU-bound, so files
/// are analyzed in parallel across a small worker-thread pool; no jail is involved.
pub fn analyze_project(root: &Path) -> Value {
    let files = collect_py_files(root);
    let entries = parallel_map(&files, |path| analyze_file_entry(root, path));
    build_report(root, entries, false)
}

fn record_file_entry(sandbox: &dyn Sandbox, root: &Path, path: &Path, max_inputs: usize) -> FileEntry {
    let rel = relative_path(root, path);
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return error_entry(rel, format!("read error: {e}")),
    };
    match record_with(sandbox, &src, max_inputs) {
        Ok(record) => {
            let purities = record.functions.iter().map(|f| f.signature.purity).collect();
            let json = serde_json::json!({
                "path": rel,
                "dependencies": record.dependencies,
                "functions": record.functions,
            });
            FileEntry {
                path: rel,
                json,
                purities,
                hard_defects: 0,
                soft_defects: 0,
                functions_checked: 0,
                ok: true,
            }
        }
        Err(e) => error_entry(rel, format!("record error: {e}")),
    }
}

/// Record every `*.py` file under `root` against one shared jailed pool (created once, reused
/// for the whole run — spawning a pool per file would pay nsjail startup repeatedly). Sequential
/// across files for v1; parallelizing jailed record across a multi-worker pool is future work.
/// Fails the whole run only if the sandbox can't be provisioned at all; a per-file record
/// failure becomes a `{path, error}` entry instead.
pub fn record_project(root: &Path, max_inputs: usize) -> Result<Value, String> {
    let files = collect_py_files(root);
    let sandbox = NsjailPool::new(1)?;
    let entries: Vec<FileEntry> = files
        .iter()
        .map(|path| record_file_entry(&sandbox, root, path, max_inputs))
        .collect();
    Ok(build_report(root, entries, false))
}

fn validate_file_entry(sandbox: &dyn Sandbox, root: &Path, path: &Path, max_inputs: usize) -> FileEntry {
    let rel = relative_path(root, path);
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => return error_entry(rel, format!("read error: {e}")),
    };
    let record = match record_with(sandbox, &src, max_inputs) {
        Ok(r) => r,
        Err(e) => return error_entry(rel, format!("record error: {e}")),
    };

    let mut hard_total = 0usize;
    let mut soft_total = 0usize;
    let functions_json: Vec<Value> = record
        .functions
        .iter()
        .map(|f| {
            let defects = validate_function(f);
            let hard = defects.iter().filter(|d| d.severity == Severity::Hard).count();
            let soft = defects.len() - hard;
            hard_total += hard;
            soft_total += soft;
            serde_json::json!({
                "name": f.signature.name,
                "owner": f.signature.owner,
                "hard_defects": hard,
                "soft_defects": soft,
                "defects": defects,
            })
        })
        .collect();
    let purities = record.functions.iter().map(|f| f.signature.purity).collect();
    let functions_checked = record.functions.len();
    let json = serde_json::json!({
        "path": rel,
        "functions": functions_json,
        "summary": {
            "hard_defects": hard_total,
            "soft_defects": soft_total,
            "functions_checked": functions_checked,
        }
    });
    FileEntry {
        path: rel,
        json,
        purities,
        hard_defects: hard_total,
        soft_defects: soft_total,
        functions_checked,
        ok: true,
    }
}

/// Validate every `*.py` file under `root` (record + `observed ⊆ static` check), aggregating
/// hard/soft defect totals. Returns the report plus the aggregate hard-defect count so the
/// caller can set the same non-zero exit code contract as single-file `validate`.
pub fn validate_project(root: &Path, max_inputs: usize) -> Result<(Value, usize), String> {
    let files = collect_py_files(root);
    let sandbox = NsjailPool::new(1)?;
    let entries: Vec<FileEntry> = files
        .iter()
        .map(|path| validate_file_entry(&sandbox, root, path, max_inputs))
        .collect();
    let hard_total: usize = entries.iter().map(|e| e.hard_defects).sum();
    Ok((build_report(root, entries, true), hard_total))
}
