//! pylens — static effect analysis of Python functions, plus jailed execution to record the
//! effects a function actually has on generated inputs. See DESIGN.md.

pub mod analyze;
pub mod exec;
pub mod generate;
pub mod model;
pub mod parse;
pub mod record;

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
