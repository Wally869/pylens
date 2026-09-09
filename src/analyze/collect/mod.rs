//! Per-walk collectors driven by the single Effects-pass AST traversal (one traversal, not N).
//! Each collector is a swappable unit responsible for one concern.

pub(super) mod aliases;
pub(super) mod body_lines;
pub(super) mod branches;
pub(super) mod exceptions;
pub(super) mod guards;
pub(super) mod hints;
pub(super) mod mutations;
pub(super) mod relations;
pub(super) mod returns;
pub(super) mod shapes;
