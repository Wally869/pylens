use std::fmt::Write as _;
use serde_json::Value;

use super::{escape_html, project_summary_panel, file_section, single_summary_panel, function_card, STYLE};

/// Render `command` (`"analyze"` | `"record"` | `"validate"`) result `data` — the same JSON
/// value the `--format json` path would emit — as a full, self-contained HTML document.
pub fn render(command: &str, data: &Value) -> String {
    let mut out = String::new();
    let cmd_esc = escape_html(command);
    write!(
        out,
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>pylens {cmd_esc}</title>\n<style>{STYLE}</style>\n</head>\n<body>\n"
    )
    .unwrap();

    let schema_version = data.get("schema_version").and_then(Value::as_str).unwrap_or("?");
    let is_project = data.get("files").is_some();
    let source = if is_project {
        data.get("root").and_then(Value::as_str).unwrap_or("?").to_string()
    } else {
        data.get("source").and_then(Value::as_str).unwrap_or("(stdin)").to_string()
    };

    write!(
        out,
        "<header><h1>pylens {cmd_esc}</h1><div class=\"meta\">source: <code>{}</code> · schema \
         {}</div></header>\n<main class=\"container\">\n",
        escape_html(&source),
        escape_html(schema_version),
    )
    .unwrap();

    if is_project {
        out.push_str(&project_summary_panel(data));
        if let Some(files) = data.get("files").and_then(Value::as_array) {
            for f in files {
                out.push_str(&file_section(command, f));
            }
        }
    } else {
        out.push_str(&single_summary_panel(command, data));
        if let Some(functions) = data.get("functions").and_then(Value::as_array) {
            out.push_str("<section class=\"functions\">\n");
            for func in functions {
                out.push_str(&function_card(command, func));
            }
            out.push_str("</section>\n");
        }
    }

    out.push_str("</main>\n</body>\n</html>\n");
    out
}

/// Append `<div class="row">{label}: item, item, ...</div>` for a non-empty JSON array field,
/// rendering each element with `render_item` and HTML-escaping the whole line. No-op when the
/// field is absent or empty (nothing to show).
pub fn render_list_row(out: &mut String, label: &str, items: Option<&Vec<Value>>, render_item: impl Fn(&Value) -> String) {
    let Some(items) = items else { return };
    if items.is_empty() {
        return;
    }
    let rendered: Vec<String> = items.iter().map(|i| escape_html(&render_item(i))).collect();
    writeln!(out, "<div class=\"row\">{}: {}</div>", escape_html(label), rendered.join(", ")).unwrap();
}
