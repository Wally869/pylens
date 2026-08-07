use std::fmt::Write as _;
use serde_json::Value;

use super::{escape_html, function_card};

/// One project file: a collapsible `<details>` block headed by its path, containing that
/// file's function cards, or its `error` message when the file failed to analyze/record.
pub fn file_section(command: &str, f: &Value) -> String {
    let path = f.get("path").and_then(Value::as_str).unwrap_or("?");
    let mut out = format!(
        "<details class=\"file\" open>\n<summary>{}</summary>\n",
        escape_html(path)
    );
    if let Some(err) = f.get("error").and_then(Value::as_str) {
        writeln!(out, "<div class=\"row error\">error: {}</div>", escape_html(err)).unwrap();
        out.push_str("</details>\n");
        return out;
    }
    if let Some(functions) = f.get("functions").and_then(Value::as_array) {
        for func in functions {
            out.push_str(&function_card(command, func));
        }
    }
    out.push_str("</details>\n");
    out
}
