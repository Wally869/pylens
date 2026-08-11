use std::fmt::Write as _;
use serde_json::Value;

use super::{escape_html, render_list_row, mutation_to_string, values_to_strings, shape_value_to_string};

/// Render one function/method entry as a card. Covers all three commands' entry shapes:
/// `analyze` (a full `EffectSignature`), `record` (the signature flattened plus `cases`/
/// `uncallable`), and `validate` (name/owner plus defect counts and a defect list) — each
/// section below is a no-op when its field isn't present on `entry`.
pub fn function_card(command: &str, entry: &Value) -> String {
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

    if let Some(cov) = entry.get("coverage").filter(|c| !c.is_null()) {
        let executed = cov.get("executed").and_then(Value::as_u64).unwrap_or(0);
        let total = cov.get("total").and_then(Value::as_u64).unwrap_or(0);
        let missed = cov
            .get("missed")
            .and_then(Value::as_array)
            .map(|a| values_to_strings(a))
            .unwrap_or_default();
        write!(out, "<div class=\"row coverage\">coverage: {executed}/{total} lines").unwrap();
        if !missed.is_empty() {
            write!(out, " (missed: {})", escape_html(&missed.join(", "))).unwrap();
        }
        out.push_str("</div>\n");
    }

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
