//! pylens CLI.
//!
//!   pylens analyze <file.py>                static effect signatures as JSON
//!   pylens record  <file.py> [--inputs N]   signatures + observed cases (runs the jail), JSON

use std::io::Read;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("analyze") => cmd_analyze(&args[2..]),
        Some("record") => cmd_record(&args[2..]),
        _ => {
            eprintln!(
                "usage:\n  pylens analyze <file.py>\n  \
                 pylens record <file.py> [--inputs <N>]"
            );
            std::process::exit(2);
        }
    }
}

fn cmd_analyze(args: &[String]) {
    let src = match args.first() {
        Some(path) => read_file(path),
        None => read_stdin(),
    };
    match (pylens::imports_of(&src), pylens::analyze_source(&src)) {
        (Ok(imports), Ok(functions)) => {
            let out = serde_json::json!({ "imports": imports, "functions": functions });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
        }
        (Err(e), _) | (_, Err(e)) => fail(&format!("parse error: {e}")),
    }
}

fn cmd_record(args: &[String]) {
    let path = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| fail("record needs a <file.py>"));
    let inputs: usize = flag(args, "--inputs")
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);

    let src = read_file(&path);
    match pylens::record::record_file(&src, inputs) {
        Ok(records) => println!("{}", serde_json::to_string_pretty(&records).unwrap()),
        Err(e) => fail(&format!("record error: {e}")),
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
