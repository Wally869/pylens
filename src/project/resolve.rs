//! Static project-local import resolution: does an import (absolute or relative) point at
//! another file in this same project? Purely static — no jail, no execution — so it applies
//! equally to `analyze` (which has no jail) and `record`/`validate` (which do, but shouldn't
//! need one just to answer "is this a project file").
//!
//! ## Module index
//! [`ModuleIndex`] maps every project file's importable dotted path to that file, relative to
//! the project root. `<root>/a/b/c.py` provides `a.b.c`; `<root>/a/b/__init__.py` provides the
//! *package* `a.b` (its `__init__` component is dropped, since importing `a.b` runs the
//! `__init__.py`, not a module literally named `__init__`).
//!
//! ## Absolute resolution
//! `import a.b` / `from a.b import name` look up `a.b` directly in the index.
//!
//! ## Relative resolution
//! A relative import climbs from the *importing file's own package directory* (its parent
//! directory, whether or not that file is an `__init__.py`): `level == 1` stays in that
//! directory, `level == 2` goes up one, etc. The import's module (if any) is then appended to
//! that base directory and the result is looked up in the same index. A relative import that
//! climbs above the project root, or that lands on a dotted path the index doesn't have, is
//! [`Resolution::UnresolvedRelative`].
//!
//! Anything that isn't a hit in the index is [`Resolution::External`] (for absolute imports —
//! third-party/stdlib) or [`Resolution::UnresolvedRelative`] (for relative ones). Jail-based
//! external-module resolution (`record::probe_dependencies`) is unchanged by this module.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::model::{Import, ModuleRef};

/// How one import resolved against the project's own files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Points at another file in this project; see `ImportResolution::project_target`.
    ProjectLocal,
    /// A non-relative import that isn't any project file (stdlib/third-party).
    External,
    /// A relative import (`level >= 1`) that doesn't land on a project file.
    UnresolvedRelative,
}

/// One import's resolution outcome: the category, plus the target file (relative to the
/// project root, forward-slash separated) when it resolved project-local.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportResolution {
    pub resolution: Resolution,
    pub project_target: Option<String>,
}

/// dotted module path -> project file that provides it (relative to the project root,
/// forward-slash separated). Built once per project and reused for every file's imports.
pub struct ModuleIndex(HashMap<String, String>);

impl ModuleIndex {
    /// Index every file in `files` (as returned by `project::collect_py_files`) under its
    /// importable dotted path relative to `root`.
    pub fn build(root: &Path, files: &[PathBuf]) -> Self {
        let mut map = HashMap::new();
        for file in files {
            let rel = super::relative_path(root, file);
            let dotted = module_dotted_path(&rel);
            map.insert(dotted, rel);
        }
        Self(map)
    }

    fn lookup(&self, dotted: &str) -> Option<&str> {
        self.0.get(dotted).map(String::as_str)
    }
}

/// The dotted module path a file provides: strip `.py`, split on `/`, join with `.`; an
/// `__init__.py` provides its *containing* package, so its own component is dropped.
fn module_dotted_path(rel: &str) -> String {
    let no_ext = rel.strip_suffix(".py").unwrap_or(rel);
    let parts: Vec<&str> = no_ext.split('/').collect();
    if parts.last() == Some(&"__init__") {
        parts[..parts.len() - 1].join(".")
    } else {
        parts.join(".")
    }
}

/// The package directory a file belongs to, as path components relative to the project root:
/// its parent directory, whether or not the file itself is `__init__.py`.
fn package_dir_components(rel: &str) -> Vec<&str> {
    let no_ext = rel.strip_suffix(".py").unwrap_or(rel);
    let mut parts: Vec<&str> = no_ext.split('/').collect();
    parts.pop();
    parts
}

/// Resolve one import from `file_rel` (the importing file's path, relative to the project
/// root, forward-slash separated) against the project's module index.
pub fn resolve_import(index: &ModuleIndex, file_rel: &str, import: &Import) -> ImportResolution {
    if import.level > 0 {
        resolve_relative(index, file_rel, import)
    } else {
        resolve_absolute(index, &import.module)
    }
}

fn resolve_absolute(index: &ModuleIndex, module: &ModuleRef) -> ImportResolution {
    if module.is_empty() {
        return external();
    }
    match index.lookup(&module.dotted()) {
        Some(target) => project_local(target),
        None => external(),
    }
}

fn resolve_relative(index: &ModuleIndex, file_rel: &str, import: &Import) -> ImportResolution {
    let pkg_dir = package_dir_components(file_rel);
    let up = (import.level - 1) as usize;
    if up > pkg_dir.len() {
        return unresolved_relative();
    }
    let base = &pkg_dir[..pkg_dir.len() - up];
    let dotted = match (base.is_empty(), import.module.is_empty()) {
        (_, true) => base.join("."),
        (true, false) => import.module.dotted(),
        (false, false) => format!("{}.{}", base.join("."), import.module.dotted()),
    };
    match index.lookup(&dotted) {
        Some(target) => project_local(target),
        None => unresolved_relative(),
    }
}

fn project_local(target: &str) -> ImportResolution {
    ImportResolution {
        resolution: Resolution::ProjectLocal,
        project_target: Some(target.to_string()),
    }
}

fn external() -> ImportResolution {
    ImportResolution {
        resolution: Resolution::External,
        project_target: None,
    }
}

fn unresolved_relative() -> ImportResolution {
    ImportResolution {
        resolution: Resolution::UnresolvedRelative,
        project_target: None,
    }
}

/// Serialize `value` (an `Import` or a `record::Dependency`) to a JSON object and merge in its
/// project resolution: `resolution` always, `project_target` only when it resolved
/// project-local. Used to extend `analyze`'s `imports` and `record`'s `dependencies` in project
/// mode only; single-file output is untouched.
pub fn annotate_with_resolution<T: Serialize>(value: &T, resolution: &ImportResolution) -> Value {
    let mut v = serde_json::to_value(value).expect("Import/Dependency always serialize");
    if let Value::Object(map) = &mut v {
        map.insert(
            "resolution".to_string(),
            serde_json::to_value(resolution.resolution).expect("Resolution always serializes"),
        );
        if let Some(target) = &resolution.project_target {
            map.insert("project_target".to_string(), Value::String(target.clone()));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Import, ImportScope};

    fn files(root: &Path, rels: &[&str]) -> Vec<PathBuf> {
        rels.iter().map(|r| root.join(r)).collect()
    }

    fn abs_import(dotted: &str) -> Import {
        Import {
            from: true,
            module: ModuleRef::parse(dotted),
            level: 0,
            alias: None,
            names: Vec::new(),
            star: false,
            scope: ImportScope::Module,
        }
    }

    fn rel_import(level: u32, dotted: &str) -> Import {
        Import {
            from: true,
            module: ModuleRef::parse(dotted),
            level,
            alias: None,
            names: Vec::new(),
            star: false,
            scope: ImportScope::Module,
        }
    }

    fn bare_relative(level: u32) -> Import {
        Import {
            from: true,
            module: ModuleRef {
                package: String::new(),
                path: String::new(),
            },
            level,
            alias: None,
            names: Vec::new(),
            star: false,
            scope: ImportScope::Module,
        }
    }

    fn index() -> (PathBuf, ModuleIndex) {
        let root = PathBuf::from("/proj");
        let idx = ModuleIndex::build(
            &root,
            &files(
                &root,
                &[
                    "pkg/__init__.py",
                    "pkg/main.py",
                    "pkg/util.py",
                    "pkg/sub/__init__.py",
                    "pkg/sub/helper.py",
                ],
            ),
        );
        (root, idx)
    }

    #[test]
    fn absolute_import_resolves_to_project_file() {
        let (_, idx) = index();
        let r = resolve_import(&idx, "pkg/main.py", &abs_import("pkg.util"));
        assert_eq!(r.resolution, Resolution::ProjectLocal);
        assert_eq!(r.project_target.as_deref(), Some("pkg/util.py"));
    }

    #[test]
    fn absolute_import_of_stdlib_is_external() {
        let (_, idx) = index();
        let r = resolve_import(&idx, "pkg/main.py", &abs_import("os"));
        assert_eq!(r.resolution, Resolution::External);
        assert_eq!(r.project_target, None);
    }

    #[test]
    fn relative_import_of_sibling_module_resolves() {
        let (_, idx) = index();
        let r = resolve_import(&idx, "pkg/main.py", &rel_import(1, "util"));
        assert_eq!(r.resolution, Resolution::ProjectLocal);
        assert_eq!(r.project_target.as_deref(), Some("pkg/util.py"));
    }

    #[test]
    fn relative_import_of_nested_package_module_resolves() {
        let (_, idx) = index();
        let r = resolve_import(&idx, "pkg/main.py", &rel_import(1, "sub.helper"));
        assert_eq!(r.resolution, Resolution::ProjectLocal);
        assert_eq!(r.project_target.as_deref(), Some("pkg/sub/helper.py"));
    }

    #[test]
    fn bare_relative_import_resolves_to_own_package_init() {
        let (_, idx) = index();
        let r = resolve_import(&idx, "pkg/sub/helper.py", &bare_relative(1));
        assert_eq!(r.resolution, Resolution::ProjectLocal);
        assert_eq!(r.project_target.as_deref(), Some("pkg/sub/__init__.py"));
    }

    #[test]
    fn double_dot_relative_import_climbs_to_parent_package() {
        let (_, idx) = index();
        let r = resolve_import(&idx, "pkg/sub/helper.py", &rel_import(2, "util"));
        assert_eq!(r.resolution, Resolution::ProjectLocal);
        assert_eq!(r.project_target.as_deref(), Some("pkg/util.py"));
    }

    #[test]
    fn relative_import_past_the_root_is_unresolved() {
        let (_, idx) = index();
        let r = resolve_import(&idx, "pkg/main.py", &rel_import(3, "util"));
        assert_eq!(r.resolution, Resolution::UnresolvedRelative);
        assert_eq!(r.project_target, None);
    }

    #[test]
    fn relative_import_of_missing_module_is_unresolved() {
        let (_, idx) = index();
        let r = resolve_import(&idx, "pkg/main.py", &rel_import(1, "nope"));
        assert_eq!(r.resolution, Resolution::UnresolvedRelative);
        assert_eq!(r.project_target, None);
    }
}
