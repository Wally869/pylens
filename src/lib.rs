//! pylens — static effect analysis of Python functions, plus jailed execution to record the
//! effects a function actually has on generated inputs. See docs/DESIGN.md.

pub mod analyze;
pub mod exec;
pub mod generate;
pub mod html;
pub mod model;
pub mod parse;
pub mod project;
pub mod record;
pub mod report;
pub mod shrink;
pub mod stub;
pub mod validate;

/// The version of the top-level JSON contract emitted by `analyze` / `record` / `validate`.
/// Downstream adapters key off this to detect breaking schema changes. Pre-release: stays at
/// `0.1` and absorbs all contract changes until a first release ships.
pub const SCHEMA_VERSION: &str = "0.1";

/// Strip a leading UTF-8 byte-order mark from Python source. CPython accepts BOM-prefixed
/// source files, so pylens must too — but a U+FEFF reaching `compile()` as text (in the jailed
/// worker) is a `SyntaxError`, and it is invisible noise to the parser besides. Applied at
/// every source-ingestion point (file read, stdin) so the analyzer and the jail always see
/// identical, BOM-less source.
pub fn strip_bom(src: &str) -> &str {
    src.strip_prefix('\u{feff}').unwrap_or(src)
}

/// Parse Python source and return the effect signature of every top-level function and
/// method.
pub fn analyze_source(
    src: &str,
) -> Result<Vec<model::EffectSignature>, ruff_python_parser::ParseError> {
    let parsed = parse::parse_source(src)?;
    Ok(analyze::analyze_module(parsed.syntax()))
}

/// Parse Python source and catalog its imports (all styles, including nested).
pub fn imports_of(src: &str) -> Result<Vec<model::Import>, ruff_python_parser::ParseError> {
    let parsed = parse::parse_source(src)?;
    Ok(analyze::collect_imports(parsed.syntax()))
}
