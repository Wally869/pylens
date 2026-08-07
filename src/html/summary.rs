use std::fmt::Write as _;
use serde_json::Value;

use super::purity_counts;

/// The summary panel for a single-file report: function count plus purity distribution
/// (`analyze`/`record`), or the hard/soft defect totals (`validate`, which carries a top-level
/// `summary` object instead of per-function purity).
pub fn single_summary_panel(command: &str, data: &Value) -> String {
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
pub fn project_summary_panel(data: &Value) -> String {
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
