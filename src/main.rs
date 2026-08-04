//! pylens CLI.
//!
//!   pylens analyze  <file.py|dir> [--format json|summary|pyi]          static effect signatures
//!   pylens record   <file.py|dir> [--inputs N] [--format json|summary] signatures + observed
//!                                                                       cases (runs the jail)
//!   pylens validate <file.py|dir> [--inputs N] [--format json|summary] observed ⊆ static
//!                                                                       soundness-defect report
//!                                                                       (runs the jail); exits
//!                                                                       non-zero on any hard
//!                                                                       defect
//!
//! A directory argument recurses over its `*.py` files and produces an aggregated project
//! report instead of a single-file one (see `pylens::project`); a single-file/stdin argument is
//! unchanged.
//!
//! `--format` defaults to `json`. `--format summary` renders a thin terminal summary instead
//! (see `pylens::report`). `analyze --format pyi` renders inferred `.pyi` type-hint stubs
//! instead (see `pylens::stub`); it is not available on `record`/`validate`.

use std::io::Read;

use pylens::report::{self, FunctionValidation};
use pylens::stub;
use pylens::validate::{Severity, validate_function};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("analyze") => cmd_analyze(&args[2..]),
        Some("record") => cmd_record(&args[2..]),
        Some("validate") => cmd_validate(&args[2..]),
        _ => {
            eprintln!(
                "usage:\n  pylens analyze <file.py|dir> [--format json|summary|pyi]\n  \
                 pylens record <file.py|dir> [--inputs <N>] [--format json|summary]\n  \
                 pylens validate <file.py|dir> [--inputs <N>] [--format json|summary]"
            );
            std::process::exit(2);
        }
    }
}

fn cmd_analyze(args: &[String]) {
    let format = format_of(args);
    let path = args.iter().find(|a| !a.starts_with("--"));
    if let Some(p) = path
        && std::path::Path::new(p.as_str()).is_dir()
    {
        let root = std::path::Path::new(p.as_str());
        if format == Format::Pyi {
            print!("{}", analyze_project_pyi(root));
            return;
        }
        let report = pylens::project::analyze_project(root);
        if format == Format::Summary {
            print!("{}", report::project_summary(&report));
        } else {
            println!("{}", serde_json::to_string_pretty(&report).unwrap());
        }
        return;
    }
    let src = match path {
        Some(p) => read_file(p),
        None => read_stdin(),
    };
    let label = path.map(String::as_str).unwrap_or("<stdin>");
    match (pylens::imports_of(&src), pylens::analyze_source(&src)) {
        (Ok(imports), Ok(functions)) => {
            match format {
                Format::Summary => {
                    print!("{}", report::analyze_summary(label, &functions));
                }
                Format::Pyi => {
                    print!("{}", stub::render_stub(&functions));
                }
                Format::Json => {
                    let out = serde_json::json!({
                        "schema_version": pylens::SCHEMA_VERSION,
                        "imports": imports,
                        "functions": functions,
                    });
                    println!("{}", serde_json::to_string_pretty(&out).unwrap());
                }
            }
        }
        (Err(e), _) | (_, Err(e)) => fail(&format!("parse error: {e}")),
    }
}

/// Render `.pyi` stubs for every `*.py` file under `root`, each preceded by a `# <relative
/// path>` comment line. A file that fails to read or parse is skipped with an inline error
/// comment instead of aborting the whole run, consistent with the JSON project-report behavior.
fn analyze_project_pyi(root: &std::path::Path) -> String {
    let files = pylens::project::collect_py_files(root);
    let mut out = String::new();
    for path in files {
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        out.push_str(&format!("# {rel}\n"));
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                out.push_str(&format!("# read error: {e}\n\n"));
                continue;
            }
        };
        match pylens::analyze_source(&src) {
            Ok(functions) => out.push_str(&stub::render_stub(&functions)),
            Err(e) => out.push_str(&format!("# parse error: {e}\n")),
        }
        out.push('\n');
    }
    out
}

fn cmd_record(args: &[String]) {
    let format = format_of(args);
    reject_pyi_format(&format);
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| fail("record needs a <file.py>"));
    let inputs: usize = flag(args, "--inputs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);

    if std::path::Path::new(&path).is_dir() {
        match pylens::project::record_project(std::path::Path::new(&path), inputs) {
            Ok(report) => {
                if format == Format::Summary {
                    print!("{}", report::project_summary(&report));
                } else {
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                }
            }
            Err(e) => fail(&format!("record error: {e}")),
        }
        return;
    }

    let src = read_file(&path);
    match pylens::record::record_file(&src, inputs) {
        Ok(record) => {
            if format == Format::Summary {
                print!("{}", report::record_summary(&path, &record));
                return;
            }
            let out = serde_json::json!({
                "schema_version": pylens::SCHEMA_VERSION,
                "dependencies": record.dependencies,
                "functions": record.functions,
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        }
        Err(e) => fail(&format!("record error: {e}")),
    }
}

fn cmd_validate(args: &[String]) {
    let format = format_of(args);
    reject_pyi_format(&format);
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| fail("validate needs a <file.py>"));
    let inputs: usize = flag(args, "--inputs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);

    if std::path::Path::new(&path).is_dir() {
        match pylens::project::validate_project(std::path::Path::new(&path), inputs) {
            Ok((report, hard_total)) => {
                if format == Format::Summary {
                    print!("{}", report::project_summary(&report));
                } else {
                    println!("{}", serde_json::to_string_pretty(&report).unwrap());
                }
                if hard_total > 0 {
                    std::process::exit(1);
                }
            }
            Err(e) => fail(&format!("record error: {e}")),
        }
        return;
    }

    let src = read_file(&path);
    let record = match pylens::record::record_file(&src, inputs) {
        Ok(r) => r,
        Err(e) => fail(&format!("record error: {e}")),
    };

    let mut hard_total = 0usize;
    let mut soft_total = 0usize;
    let per_function: Vec<_> = record
        .functions
        .iter()
        .map(|f| {
            let defects = validate_function(f);
            let hard = defects
                .iter()
                .filter(|d| d.severity == Severity::Hard)
                .count();
            let soft = defects.len() - hard;
            hard_total += hard;
            soft_total += soft;
            (f, defects)
        })
        .collect();

    if format == Format::Summary {
        let results: Vec<FunctionValidation> = per_function
            .iter()
            .map(|(f, defects)| FunctionValidation {
                name: &f.signature.name,
                owner: f.signature.owner.as_deref(),
                defects,
            })
            .collect();
        print!(
            "{}",
            report::validate_summary(&path, record.functions.len(), hard_total, soft_total, &results)
        );
    } else {
        let functions: Vec<serde_json::Value> = per_function
            .iter()
            .map(|(f, defects)| {
                let hard = defects
                    .iter()
                    .filter(|d| d.severity == Severity::Hard)
                    .count();
                let soft = defects.len() - hard;
                serde_json::json!({
                    "name": f.signature.name,
                    "owner": f.signature.owner,
                    "hard_defects": hard,
                    "soft_defects": soft,
                    "defects": defects,
                })
            })
            .collect();

        let out = serde_json::json!({
            "schema_version": pylens::SCHEMA_VERSION,
            "functions": functions,
            "summary": {
                "hard_defects": hard_total,
                "soft_defects": soft_total,
                "functions_checked": record.functions.len(),
            }
        });
        println!("{}", serde_json::to_string_pretty(&out).unwrap());
    }
    if hard_total > 0 {
        std::process::exit(1);
    }
}

#[derive(PartialEq, Eq)]
enum Format {
    Json,
    Summary,
    /// `.pyi` type-hint stub output — `analyze` only (see `cmd_analyze`).
    Pyi,
}

fn format_of(args: &[String]) -> Format {
    match flag(args, "--format").as_deref() {
        Some("summary") => Format::Summary,
        Some("pyi") => Format::Pyi,
        Some("json") | None => Format::Json,
        Some(other) => fail(&format!(
            "unknown --format '{other}' (expected json|summary|pyi)"
        )),
    }
}

fn reject_pyi_format(format: &Format) {
    if *format == Format::Pyi {
        fail("--format pyi is only supported by 'analyze'");
    }
}

fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn read_file(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| fail(&format!("error reading {path}: {e}")))
}

fn read_stdin() -> String {
    let mut s = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut s) {
        fail(&format!("error reading stdin: {e}"));
    }
    s
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}
