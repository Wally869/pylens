//! Self-contained HTML report rendering. Renders `analyze` / `record` / `validate` results
//! (already assembled into the same `serde_json::Value` the JSON output would emit — see
//! `main.rs`) into a single `<!doctype html>` document with inline `<style>` only: no external
//! stylesheets, scripts, fonts, or images, so the page works fully offline and can be emailed,
//! archived, or opened straight from disk. Because the input is OUR schema (not arbitrary JSON),
//! the renderer reads known field names directly rather than doing generic JSON-to-HTML dumping.
//!
//! One renderer, both shapes: a project report (`{root, files, summary}`) is detected by the
//! presence of a top-level `files` array; a single-file report (`{functions, ...}`) is anything
//! else. `analyze`/`record`/`validate` all place their per-function entries under a top-level (or
//! per-file) `functions` array, so the same function-card renderer covers all three, branching
//! only on the fields each command's entries actually carry.
//!
//! Every piece of dynamic text (names, paths, messages) is passed through [`escape_html`] before
//! being written into the document — the source data comes from user Python source (function
//! names, decorator names, exception names, ...) and must never be interpreted as markup.

use std::fmt::Write as _;

use serde_json::Value;

/// Escape the five ASCII characters that are meaningful in HTML text/attribute context
/// (`& < > " '`). Applied to every dynamic string before it is written into the document, so
/// no untrusted content (function/parameter/exception names pulled from analyzed Python source)
/// can break out of its containing tag or attribute.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

const STYLE: &str = r#"
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

fn purity_counts(functions: &[Value]) -> (u64, u64, u64) {
    let mut pure = 0;
    let mut impure = 0;
    let mut unknown = 0;
    for f in functions {
        match f.get("purity").and_then(Value::as_str) {
            Some("pure") => pure += 1,
            Some("impure") => impure += 1,
            Some("unknown") => unknown += 1,
            _ => {}
        }
    }
    (pure, impure, unknown)
}

/// The summary panel for a single-file report: function count plus purity distribution
/// (`analyze`/`record`), or the hard/soft defect totals (`validate`, which carries a top-level
/// `summary` object instead of per-function purity).
fn single_summary_panel(command: &str, data: &Value) -> String {
    let empty = Vec::new();
    let functions = data.get("functions").and_then(Value::as_array).unwrap_or(&empty);
    let mut out = format!(
        "<section class=\"summary\">\n<h2>Summary</h2>\n<div class=\"stat-row\">\
         <span class=\"stat\">{} function(s)</span>",
        functions.len()
    );
    if command == "validate" {
        if let Some(summary) = data.get("summary") {
            let hard = summary.get("hard_defects").and_then(Value::as_u64).unwrap_or(0);
            let soft = summary.get("soft_defects").and_then(Value::as_u64).unwrap_or(0);
            write!(
                out,
                "<span class=\"stat{}\">{hard} hard defect(s)</span>\
                 <span class=\"stat\">{soft} soft defect(s)</span>",
                if hard > 0 { " hard" } else { "" },
            )
            .unwrap();
        }
    } else {
        let (pure, impure, unknown) = purity_counts(functions);
        write!(
            out,
            "<span class=\"stat purity-pure\">{pure} pure</span>\
             <span class=\"stat purity-impure\">{impure} impure</span>\
             <span class=\"stat purity-unknown\">{unknown} unknown</span>",
        )
        .unwrap();
    }
    out.push_str("</div>\n</section>\n");
    out
}

/// The summary panel for a project report: files/ok/errors, function total, purity
/// distribution, and (when present — `validate`) the hard/soft defect totals.
fn project_summary_panel(data: &Value) -> String {
    let summary = data.get("summary").cloned().unwrap_or_default();
    let files = summary.get("files").and_then(Value::as_u64).unwrap_or(0);
    let ok = summary.get("ok").and_then(Value::as_u64).unwrap_or(0);
    let errors = summary.get("errors").and_then(Value::as_u64).unwrap_or(0);
    let functions = summary.get("functions").and_then(Value::as_u64).unwrap_or(0);
    let purity = summary.get("purity").cloned().unwrap_or_default();
    let pure = purity.get("pure").and_then(Value::as_u64).unwrap_or(0);
    let impure = purity.get("impure").and_then(Value::as_u64).unwrap_or(0);
    let unknown = purity.get("unknown").and_then(Value::as_u64).unwrap_or(0);

    let mut out = format!(
        "<section class=\"summary\">\n<h2>Summary</h2>\n<div class=\"stat-row\">\
         <span class=\"stat\">{files} file(s)</span>\
         <span class=\"stat\">{ok} ok</span>\
         <span class=\"stat{}\">{errors} error(s)</span>\
         <span class=\"stat\">{functions} function(s)</span>\
         <span class=\"stat purity-pure\">{pure} pure</span>\
         <span class=\"stat purity-impure\">{impure} impure</span>\
         <span class=\"stat purity-unknown\">{unknown} unknown</span>",
        if errors > 0 { " hard" } else { "" },
    );
    if let Some(hard) = summary.get("hard_defects").and_then(Value::as_u64) {
        let soft = summary.get("soft_defects").and_then(Value::as_u64).unwrap_or(0);
        write!(
            out,
            "<span class=\"stat{}\">{hard} hard defect(s)</span>\
             <span class=\"stat\">{soft} soft defect(s)</span>",
            if hard > 0 { " hard" } else { "" },
        )
        .unwrap();
    }
    out.push_str("</div>\n</section>\n");
    out
}

/// One project file: a collapsible `<details>` block headed by its path, containing that
/// file's function cards, or its `error` message when the file failed to analyze/record.
fn file_section(command: &str, f: &Value) -> String {
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

/// Render a `Shape` value (as it appears in JSON: a scalar tag string, or a tagged
/// `{"seq"|"set"|"map": ...}` object) compactly — mirrors `report::shape_to_string`.
fn shape_value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Object(map) => {
            if let Some(elem) = map.get("seq") {
                format!("seq<{}>", shape_value_to_string(elem))
            } else if let Some(elem) = map.get("set") {
                format!("set<{}>", shape_value_to_string(elem))
            } else if let Some(kv) = map.get("map") {
                let key = kv.get("key").map(shape_value_to_string).unwrap_or_else(|| "any".to_string());
                let value = kv.get("value").map(shape_value_to_string).unwrap_or_else(|| "any".to_string());
                format!("map<{key},{value}>")
            } else {
                "any".to_string()
            }
        }
        _ => "any".to_string(),
    }
}

fn values_to_strings(arr: &[Value]) -> Vec<String> {
    arr.iter().filter_map(Value::as_str).map(str::to_string).collect()
}

/// Append `<div class="row">{label}: item, item, ...</div>` for a non-empty JSON array field,
/// rendering each element with `render_item` and HTML-escaping the whole line. No-op when the
/// field is absent or empty (nothing to show).
fn render_list_row(out: &mut String, label: &str, items: Option<&Vec<Value>>, render_item: impl Fn(&Value) -> String) {
    let Some(items) = items else { return };
    if items.is_empty() {
        return;
    }
    let rendered: Vec<String> = items.iter().map(|i| escape_html(&render_item(i))).collect();
    writeln!(out, "<div class=\"row\">{}: {}</div>", escape_html(label), rendered.join(", ")).unwrap();
}

fn mutation_target_string(t: &Value) -> String {
    let root = t.get("root").and_then(Value::as_str).unwrap_or("?");
    match t.get("name").and_then(Value::as_str) {
        Some(n) => format!("{root}:{n}"),
        None => root.to_string(),
    }
}

fn mutation_to_string(m: &Value) -> String {
    let target = m.get("target").map(mutation_target_string).unwrap_or_else(|| "?".to_string());
    let via = m.get("via").and_then(Value::as_str).unwrap_or("?");
    match m.get("name").and_then(Value::as_str) {
        Some(n) => format!("{target} via {via}({n})"),
        None => format!("{target} via {via}"),
    }
}

/// Render one function/method entry as a card. Covers all three commands' entry shapes:
/// `analyze` (a full `EffectSignature`), `record` (the signature flattened plus `cases`/
/// `uncallable`), and `validate` (name/owner plus defect counts and a defect list) — each
/// section below is a no-op when its field isn't present on `entry`.
fn function_card(command: &str, entry: &Value) -> String {
    let name = entry.get("name").and_then(Value::as_str).unwrap_or("?");
    let owner = entry.get("owner").and_then(Value::as_str);
    let qualified = match owner {
        Some(o) => format!("{o}.{name}"),
        None => name.to_string(),
    };

    let mut out = format!(
        "<div class=\"card\">\n<div class=\"card-head\"><span class=\"fn-name\">{}</span>",
        escape_html(&qualified)
    );
    if let Some(purity) = entry.get("purity").and_then(Value::as_str) {
        write!(
            out,
            "<span class=\"badge purity-{p}\">{p}</span>",
            p = escape_html(purity)
        )
        .unwrap();
    }
    out.push_str("</div>\n");

    if command == "validate" {
        let hard = entry.get("hard_defects").and_then(Value::as_u64).unwrap_or(0);
        let soft = entry.get("soft_defects").and_then(Value::as_u64).unwrap_or(0);
        writeln!(
            out,
            "<div class=\"row\"><span class=\"badge{}\">{hard} hard</span> \
             <span class=\"badge soft\">{soft} soft</span></div>",
            if hard > 0 { " hard" } else { " defect-ok" },
        )
        .unwrap();
        if let Some(defects) = entry.get("defects").and_then(Value::as_array)
            && !defects.is_empty()
        {
            out.push_str("<ul class=\"defects\">\n");
            for d in defects {
                let dim = d.get("dimension").and_then(Value::as_str).unwrap_or("?");
                let sev = d.get("severity").and_then(Value::as_str).unwrap_or("?");
                let observed = d.get("observed").and_then(Value::as_str).unwrap_or("");
                let cls = if sev == "hard" { "defect-hard" } else { "defect-soft" };
                writeln!(
                    out,
                    "<li class=\"{cls}\">[{}/{}] {}</li>",
                    escape_html(dim),
                    escape_html(sev),
                    escape_html(observed),
                )
                .unwrap();
            }
            out.push_str("</ul>\n");
        }
        out.push_str("</div>\n");
        return out;
    }

    if let Some(params) = entry.get("params").and_then(Value::as_array)
        && !params.is_empty()
    {
        let rendered: Vec<String> = params
            .iter()
            .map(|p| {
                let pname = p.get("name").and_then(Value::as_str).unwrap_or("?");
                let shape = p.get("shape").map(shape_value_to_string).unwrap_or_else(|| "any".to_string());
                format!("{}: {}", escape_html(pname), escape_html(&shape))
            })
            .collect();
        writeln!(out, "<div class=\"row params\">params: {}</div>", rendered.join(", ")).unwrap();
    }

    if let Some(mutations) = entry.get("mutations").and_then(Value::as_array) {
        render_list_row(&mut out, "mutations", Some(mutations), mutation_to_string);
    }

    if let Some(raises) = entry.get("raises") {
        let explicit = raises
            .get("explicit")
            .and_then(Value::as_array)
            .map(|a| values_to_strings(a))
            .unwrap_or_default();
        let implicit = raises
            .get("implicit")
            .and_then(Value::as_array)
            .map(|a| values_to_strings(a))
            .unwrap_or_default();
        if !explicit.is_empty() || !implicit.is_empty() {
            writeln!(
                out,
                "<div class=\"row\">raises: {}</div>",
                escape_html(&format!(
                    "explicit=[{}], implicit=[{}]",
                    explicit.join(", "),
                    implicit.join(", ")
                ))
            )
            .unwrap();
        }
    }

    if let Some(io) = entry.get("io").and_then(Value::as_array) {
        render_list_row(&mut out, "io", Some(io), |v| v.as_str().unwrap_or("").to_string());
    }

    if let Some(unresolved) = entry.get("unresolved_effects").and_then(Value::as_array) {
        render_list_row(&mut out, "unresolved effects", Some(unresolved), |u| {
            let reason = u.get("reason").and_then(Value::as_str).unwrap_or("?");
            match u.get("callee").and_then(Value::as_str) {
                Some(c) => format!("{reason} ({c})"),
                None => reason.to_string(),
            }
        });
    }

    if let Some(decorators) = entry.get("decorators").and_then(Value::as_array) {
        render_list_row(&mut out, "decorators", Some(decorators), |v| v.as_str().unwrap_or("").to_string());
    }

    if let Some(mismatches) = entry.get("type_mismatches").and_then(Value::as_array) {
        render_list_row(&mut out, "type mismatches", Some(mismatches), |t| {
            let kind = t.get("kind").and_then(Value::as_str).unwrap_or("?");
            let declared = t.get("declared").and_then(Value::as_str).unwrap_or("?");
            if kind == "param" {
                let param = t.get("param").and_then(Value::as_str).unwrap_or("?");
                let inferred_shape = t.get("inferred_shape").map(|v| v.to_string()).unwrap_or_default();
                return format!("param {param}: declared {declared}, inferred {inferred_shape}");
            }
            let inferred = t
                .get("inferred")
                .and_then(Value::as_array)
                .map(|a| values_to_strings(a))
                .unwrap_or_default();
            format!("{kind}: declared {declared}, inferred [{}]", inferred.join(", "))
        });
    }

    if command == "record" {
        if let Some(uncallable) = entry.get("uncallable") {
            let reason = uncallable.get("reason").and_then(Value::as_str).unwrap_or("?");
            writeln!(out, "<div class=\"row uncallable\">uncallable: {}</div>", escape_html(reason)).unwrap();
        } else if let Some(cases) = entry.get("cases").and_then(Value::as_array) {
            let total = cases.len();
            let outcome_is = |c: &&Value, want: &str| c.get("outcome").and_then(Value::as_str) == Some(want);
            let returned = cases.iter().filter(|c| outcome_is(c, "returned")).count();
            let raised = cases.iter().filter(|c| outcome_is(c, "raised")).count();
            let error = cases.iter().filter(|c| outcome_is(c, "error")).count();
            writeln!(
                out,
                "<div class=\"row cases\">{total} cases: {returned} returned / {raised} raised / {error} error</div>"
            )
            .unwrap();
        }
    }

    out.push_str("</div>\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn escape_html_neutralizes_markup() {
        assert_eq!(
            escape_html("<script>alert('x')</script> & \"quoted\""),
            "&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;quoted&quot;"
        );
    }

    #[test]
    fn analyze_document_has_name_badge_and_shape() {
        let data = json!({
            "schema_version": "1.0",
            "source": "normalize.py",
            "imports": [],
            "functions": [{
                "name": "normalize_rows",
                "kind": "function",
                "params": [{
                    "name": "matrix",
                    "shape": {"seq": {"seq": "float"}},
                    "has_default": false,
                }],
                "declared_return": null,
                "is_generator": false,
                "returns": ["sequence"],
                "raises": {"explicit": [], "implicit": []},
                "mutations": [],
                "global_writes": [],
                "io": [],
                "unresolved_effects": [],
                "purity": "pure",
                "uses": [],
                "may_use_star": false,
                "decorators": [],
                "type_mismatches": [],
            }],
        });
        let out = render("analyze", &data);
        assert!(out.starts_with("<!doctype html>"));
        assert!(out.contains("normalize_rows"));
        assert!(out.contains("badge purity-pure"));
        assert!(out.contains("seq&lt;seq&lt;float&gt;&gt;") || out.contains("seq<seq<float>>"));
    }

    #[test]
    fn validate_document_highlights_hard_defect() {
        let data = json!({
            "schema_version": "1.0",
            "source": "f.py",
            "functions": [{
                "name": "f",
                "hard_defects": 1,
                "soft_defects": 0,
                "defects": [{
                    "case_index": 0,
                    "dimension": "raise",
                    "observed": "ValueError",
                    "expected": "'ValueError' not predicted",
                    "severity": "hard",
                }],
            }],
            "summary": {"hard_defects": 1, "soft_defects": 0, "functions_checked": 1},
        });
        let out = render("validate", &data);
        assert!(out.contains("defect-hard"));
        assert!(out.contains("ValueError"));
        assert!(out.contains("1 hard defect(s)"));
    }

    #[test]
    fn dynamic_text_is_escaped_not_raw() {
        let data = json!({
            "schema_version": "1.0",
            "source": "<script>evil()</script>",
            "functions": [{
                "name": "<script>alert(1)</script>",
                "kind": "function",
                "params": [],
                "declared_return": null,
                "is_generator": false,
                "returns": [],
                "raises": {"explicit": [], "implicit": []},
                "mutations": [],
                "global_writes": [],
                "io": [],
                "unresolved_effects": [],
                "purity": "unknown",
                "uses": [],
                "may_use_star": false,
                "decorators": [],
                "type_mismatches": [],
            }],
        });
        let out = render("analyze", &data);
        assert!(!out.contains("<script>alert(1)</script>"));
        assert!(!out.contains("<script>evil()</script>"));
        assert!(out.contains("&lt;script&gt;"));
    }
}
