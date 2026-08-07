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
