//! Parser boundary. All `ruff_*` parsing entry points are isolated here so the parser
//! stays swappable.

use ruff_python_ast::ModModule;
use ruff_python_parser::{ParseError, Parsed, parse_module};

/// Parse Python source into a ruff module syntax tree.
pub fn parse_source(src: &str) -> Result<Parsed<ModModule>, ParseError> {
    parse_module(src)
}
