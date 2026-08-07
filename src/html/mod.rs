

pub const STYLE: &str = r#"
:root { color-scheme: light dark; }
* { box-sizing: border-box; }
body {
  margin: 0;
  font-family: -apple-system, "Segoe UI", Roboto, Helvetica, Arial, sans-serif;
  background: #f6f7f9;
  color: #1a1c20;
  line-height: 1.4;
}
header {
  padding: 1rem 1.5rem;
  background: #20232a;
  color: #f0f2f5;
}
header h1 { margin: 0 0 0.25rem 0; font-size: 1.25rem; text-transform: capitalize; }
header .meta { font-size: 0.85rem; color: #b9bec8; }
.container { max-width: 960px; margin: 0 auto; padding: 1.5rem; }
section.summary {
  background: #fff;
  border: 1px solid #dde1e7;
  border-radius: 8px;
  padding: 1rem 1.25rem;
  margin-bottom: 1.5rem;
}
section.summary h2 { margin: 0 0 0.5rem 0; font-size: 1rem; }
.stat-row { display: flex; flex-wrap: wrap; gap: 0.5rem; }
.stat {
  background: #eef1f5;
  border-radius: 6px;
  padding: 0.2rem 0.6rem;
  font-size: 0.85rem;
  white-space: nowrap;
}
.stat.hard { background: #fbdada; color: #7a1414; font-weight: 600; }
details.file {
  background: #fff;
  border: 1px solid #dde1e7;
  border-radius: 8px;
  margin-bottom: 1rem;
  padding: 0.25rem 1rem 0.75rem 1rem;
}
details.file > summary {
  cursor: pointer;
  padding: 0.5rem 0;
  font-weight: 600;
  font-family: ui-monospace, Consolas, monospace;
}
.card {
  border: 1px solid #e2e5eb;
  border-radius: 6px;
  padding: 0.75rem 1rem;
  margin: 0.75rem 0;
  background: #fbfcfe;
  overflow-wrap: anywhere;
}
.card-head { display: flex; align-items: center; gap: 0.6rem; margin-bottom: 0.4rem; }
.fn-name { font-family: ui-monospace, Consolas, monospace; font-weight: 600; }
.badge {
  display: inline-block;
  border-radius: 999px;
  padding: 0.1rem 0.6rem;
  font-size: 0.75rem;
  font-weight: 600;
}
.badge.purity-pure { background: #d5f0dc; color: #14602c; }
.badge.purity-impure { background: #fde3cf; color: #8a3d07; }
.badge.purity-unknown { background: #e4e6ea; color: #45494f; }
.badge.hard { background: #f6c6c6; color: #7a1414; }
.badge.soft { background: #fbe9b8; color: #7a5a05; }
.badge.defect-ok { background: #d5f0dc; color: #14602c; }
.row { font-size: 0.85rem; margin: 0.2rem 0; color: #3a3d43; }
.row.error { color: #8a1414; font-weight: 600; }
.row.uncallable { color: #8a3d07; font-weight: 600; }
ul.defects { margin: 0.3rem 0 0 0; padding-left: 1.1rem; font-size: 0.85rem; }
li.defect-hard { color: #8a1414; font-weight: 600; }
li.defect-soft { color: #7a5a05; }
@media (prefers-color-scheme: dark) {
  body { background: #14161a; color: #e6e8eb; }
  header { background: #0c0d10; color: #e6e8eb; }
  header .meta { color: #8b909c; }
  section.summary, details.file, .card {
    background: #1c1f26;
    border-color: #2c303a;
  }
  .stat { background: #262b34; color: #dfe2e7; }
  .stat.hard { background: #4d1a1a; color: #ff9d9d; }
  .badge.purity-pure { background: #133b21; color: #7fe0a0; }
  .badge.purity-impure { background: #4a2a0d; color: #ffb877; }
  .badge.purity-unknown { background: #33373f; color: #c3c7cf; }
  .badge.hard { background: #4d1a1a; color: #ff9d9d; }
  .badge.soft { background: #453405; color: #ffd479; }
  .badge.defect-ok { background: #133b21; color: #7fe0a0; }
  .row { color: #c4c8ce; }
  .row.error, li.defect-hard { color: #ff9d9d; }
  .row.uncallable, li.defect-soft { color: #ffd479; }
}
"#;


/// HTML escaping utility.
mod escaping;

/// Core render functions and list row rendering.
mod rendering;

/// Purity count analysis helper.
mod purity;

/// Summary panel rendering (single and project).
mod summary;

/// File-level section rendering.
mod file_section;

/// String conversion for shapes, mutations, and values.
mod formatting;

/// Function card rendering.
mod function_card;

pub use escaping::*;
pub use rendering::*;
pub use purity::*;
pub use summary::*;
pub use file_section::*;
pub use formatting::*;
pub use function_card::*;

#[cfg(test)]
mod tests;
