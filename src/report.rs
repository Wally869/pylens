//! Thin terminal-summary formatter — the `report` injection point named in the architecture
//! (json now, this + html later). Pure string formatting: no I/O, no jail, unit-testable in
//! isolation. Each `*_summary` function renders one command's result as a scannable,
//! human-readable block; the JSON path in `main.rs` is unaffected by anything here.

use serde_json::Value;

use crate::model::{DefKind, EffectSignature, Shape};
use crate::record::{Coverage, DepStatus, ModuleRecord};
use crate::validate::{Defect, Severity};

/// One function's executed-line coverage as a scannable line: `executed/total lines`, plus the
/// missed line numbers when any remain.
fn coverage_line(cov: &Coverage) -> String {
    if cov.missed.is_empty() {
        format!("coverage: {}/{} lines\n", cov.executed, cov.total)
    } else {
        format!(
            "coverage: {}/{} lines (missed: {})\n",
            cov.executed,
            cov.total,
            cov.missed.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
        )
    }
}

/// Sum `executed`/`total` over every present coverage, for an aggregate figure across the
/// functions that have one (uncallable/case-less functions carry no `coverage`).
fn aggregate_coverage<'a>(covs: impl Iterator<Item = &'a Coverage>) -> Option<(usize, usize)> {
    let mut executed = 0usize;
    let mut total = 0usize;
    let mut any = false;
    for cov in covs {
        any = true;
        executed += cov.executed;
        total += cov.total;
    }
    any.then_some((executed, total))
}

/// Render `shape` compactly: scalars as their tag, containers recursively
/// (`seq<seq<float>>`, `map<str,int>`, `set<any>`).
pub fn shape_to_string(shape: &Shape) -> String {
    match shape {
        Shape::Int => "int".to_string(),
        Shape::Float => "float".to_string(),
        Shape::Bool => "bool".to_string(),
        Shape::Str => "str".to_string(),
        Shape::Bytes => "bytes".to_string(),
        Shape::None => "none".to_string(),
        Shape::Any => "any".to_string(),
        Shape::Seq(elem) => format!("seq<{}>", shape_to_string(elem)),
        Shape::Set(elem) => format!("set<{}>", shape_to_string(elem)),
        Shape::Map(key, value) => {
            format!("map<{},{}>", shape_to_string(key), shape_to_string(value))
        }
        Shape::Union(members) => members
            .iter()
            .map(shape_to_string)
            .collect::<Vec<_>>()
            .join("|"),
        Shape::Instance(name) => name.clone(),
    }
}

fn qualified_name(sig: &EffectSignature) -> String {
    match &sig.owner {
        Some(owner) => format!("{owner}.{}", sig.name),
        None => sig.name.clone(),
    }
}

fn params_to_string(sig: &EffectSignature) -> String {
    sig.params
        .iter()
        .map(|p| format!("{}: {}", p.name, shape_to_string(&p.shape)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// One function's summary line: name, kind, purity, compact params, and effect counts.
fn function_line(sig: &EffectSignature) -> String {
    let kind = match sig.kind {
        DefKind::Function => "fn",
        DefKind::Method => "method",
    };
    let purity = format!("{:?}", sig.purity).to_lowercase();
    let raises = sig.raises.explicit.len() + sig.raises.implicit.len();
    format!(
        "{name} ({kind}, {purity}) params: [{params}] — #mutations={mutations}, \
         #raises={raises}, #io={io}, #unresolved={unresolved}",
        name = qualified_name(sig),
        params = params_to_string(sig),
        mutations = sig.mutations.len(),
        io = sig.io.len(),
        unresolved = sig.unresolved_effects.len(),
    )
}

/// Render the `analyze` result as a thin terminal summary: a header line with the file and
/// function count, then one block per function/method.
pub fn analyze_summary(file: &str, sigs: &[EffectSignature]) -> String {
    let mut out = format!("{file}: {n} function(s)\n", n = sigs.len());
    for sig in sigs {
        out.push_str("  ");
        out.push_str(&function_line(sig));
        out.push('\n');
    }
    out
}

/// Render the `record` result: the analyze summary per function plus a case tally and
/// `uncallable` reason, and a dependencies line.
pub fn record_summary(file: &str, record: &ModuleRecord) -> String {
    let mut out = format!("{file}: {n} function(s)\n", n = record.functions.len());

    let resolved = record
        .dependencies
        .iter()
        .filter(|d| matches!(d.status, DepStatus::Resolved))
        .count();
    let unresolved = record
        .dependencies
        .iter()
        .filter(|d| matches!(d.status, DepStatus::Unresolved))
        .count();
    let not_probed = record.dependencies.len() - resolved - unresolved;
    out.push_str(&format!(
        "  {k} imports: {resolved} resolved, {unresolved} unresolved, {not_probed} not_probed\n",
        k = record.dependencies.len(),
    ));

    for f in &record.functions {
        out.push_str("  ");
        out.push_str(&function_line(&f.signature));
        out.push('\n');
        if let Some(u) = &f.uncallable {
            out.push_str(&format!(
                "    uncallable: {} ({})\n",
                u.reason, u.error.kind
            ));
            continue;
        }
        let total = f.cases.len();
        let returned = f.cases.iter().filter(|c| c.outcome == "returned").count();
        let raised = f.cases.iter().filter(|c| c.outcome == "raised").count();
        let error = f.cases.iter().filter(|c| c.outcome == "error").count();
        out.push_str(&format!(
            "    {total} cases: {returned} returned, {raised} raised, {error} error\n"
        ));
        if let Some(cov) = &f.coverage {
            out.push_str("    ");
            out.push_str(&coverage_line(cov));
        }
    }
    if let Some((executed, total)) = aggregate_coverage(record.functions.iter().filter_map(|f| f.coverage.as_ref())) {
        out.push_str(&format!("  aggregate coverage: {executed}/{total} lines\n"));
    }
    out
}

/// A validated function's identity plus the defects found against it — the slice `report` needs
/// to render a `validate` summary without re-running the checker.
pub struct FunctionValidation<'a> {
    pub name: &'a str,
    pub owner: Option<&'a str>,
    pub defects: &'a [Defect],
    pub coverage: Option<&'a Coverage>,
}

/// Render the `validate` result: per function with any defects, hard/soft counts and one line
/// per defect, then the overall summary with the hard-defect total made prominent. A function
/// that never executed — uncallable, or callable but left with zero cases — produced no
/// observations at all — their "0 defects" is vacuous, so `unvalidated` is surfaced rather than
/// letting them pass silently (see `FunctionRecord::validated`, computed by the caller).
pub fn validate_summary(
    file: &str,
    functions_checked: usize,
    unvalidated: usize,
    hard_total: usize,
    soft_total: usize,
    results: &[FunctionValidation],
) -> String {
    let mut out = format!(
        "{file}: HARD DEFECTS: {hard_total} — {functions_checked} function(s) checked, \
         {soft_total} soft defect(s)\n"
    );
    if unvalidated > 0 {
        out.push_str(&format!(
            "  WARNING: {unvalidated} function(s) unvalidated — never executed, nothing validated\n"
        ));
    }
    if let Some((executed, total)) = aggregate_coverage(results.iter().filter_map(|r| r.coverage)) {
        out.push_str(&format!("  aggregate coverage: {executed}/{total} lines\n"));
    }
    for r in results {
        if r.defects.is_empty() {
            continue;
        }
        let hard = r
            .defects
            .iter()
            .filter(|d| d.severity == Severity::Hard)
            .count();
        let soft = r.defects.len() - hard;
        let name = match r.owner {
            Some(owner) => format!("{owner}.{}", r.name),
            None => r.name.to_string(),
        };
        out.push_str(&format!("  {name}: {hard} hard, {soft} soft\n"));
        for d in r.defects {
            out.push_str(&format!(
                "    [{:?}/{:?}] {}\n",
                d.dimension, d.severity, d.observed
            ));
        }
        if let Some(cov) = r.coverage {
            out.push_str("    ");
            out.push_str(&coverage_line(cov));
        }
    }
    out
}

/// Render a project-level report (`pylens::project::{analyze,record,validate}_project`) as a
/// thin terminal summary: a header line, the purity distribution and function total, the
/// hard/soft defect totals when present (validate), and one line per file.
pub fn project_summary(report: &Value) -> String {
    let root = report["root"].as_str().unwrap_or("?");
    let summary = &report["summary"];
    let files = summary["files"].as_u64().unwrap_or(0);
    let ok = summary["ok"].as_u64().unwrap_or(0);
    let errors = summary["errors"].as_u64().unwrap_or(0);
    let functions = summary["functions"].as_u64().unwrap_or(0);
    let purity = &summary["purity"];

    let mut out = format!("{root}: {files} files, {ok} ok, {errors} errors\n");
    out.push_str(&format!(
        "  functions: {functions} (pure={}, impure={}, unknown={})\n",
        purity["pure"], purity["impure"], purity["unknown"]
    ));
    if let Some(hard) = summary.get("hard_defects") {
        let soft = summary["soft_defects"].as_u64().unwrap_or(0);
        let checked = summary["functions_checked"].as_u64().unwrap_or(0);
        out.push_str(&format!(
            "  HARD DEFECTS: {hard} — {checked} function(s) checked, {soft} soft defect(s)\n"
        ));
        let unvalidated = summary["unvalidated"].as_u64().unwrap_or(0);
        if unvalidated > 0 {
            out.push_str(&format!(
                "  WARNING: {unvalidated} function(s) unvalidated — never executed, nothing validated\n"
            ));
        }
    }
    if let Some(coverage) = summary.get("coverage") {
        let executed = coverage["executed"].as_u64().unwrap_or(0);
        let total = coverage["total"].as_u64().unwrap_or(0);
        out.push_str(&format!("  aggregate coverage: {executed}/{total} lines\n"));
    }
    if let Some(files) = report["files"].as_array() {
        for f in files {
            let path = f["path"].as_str().unwrap_or("?");
            match f.get("error") {
                Some(e) => out.push_str(&format!("  {path}: ERROR {}\n", e.as_str().unwrap_or(""))),
                None => out.push_str(&format!("  {path}: ok\n")),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DefKind, Mutation, MutationKind, MutationTarget, ParamInfo, Purity, Raises};

    fn base_sig() -> EffectSignature {
        let mut sig = EffectSignature::new("normalize_rows", DefKind::Function);
        sig.params.push(ParamInfo {
            name: "matrix".to_string(),
            shape: Shape::Seq(Box::new(Shape::Seq(Box::new(Shape::Float)))),
            has_default: false,
            kind: Default::default(),
            declared: None,
            guard_samples: Vec::new(),
            default_literal: None,
            hints: Vec::new(),
        });
        sig.mutations.push(Mutation {
            target: MutationTarget::Param {
                name: "matrix".to_string(),
            },
            via: MutationKind::SubscriptSet,
            name: None,
        });
        sig.raises = Raises {
            explicit: vec!["ValueError".to_string()],
            implicit: vec!["ZeroDivisionError".to_string()],
        };
        sig.purity = Purity::Impure;
        sig
    }

    #[test]
    fn shape_renders_compactly() {
        let shape = Shape::Seq(Box::new(Shape::Seq(Box::new(Shape::Float))));
        assert_eq!(shape_to_string(&shape), "seq<seq<float>>");
        assert_eq!(shape_to_string(&Shape::any_map()), "map<any,any>");
    }

    #[test]
    fn analyze_summary_contains_purity_shape_and_counts() {
        let sig = base_sig();
        let out = analyze_summary("normalize.py", std::slice::from_ref(&sig));
        assert!(out.contains("normalize.py: 1 function(s)"));
        assert!(out.contains("normalize_rows (fn, impure)"));
        assert!(out.contains("matrix: seq<seq<float>>"));
        assert!(out.contains("#mutations=1"));
        assert!(out.contains("#raises=2"));
        assert!(out.contains("#io=0"));
        assert!(out.contains("#unresolved=0"));
    }

    #[test]
    fn analyze_summary_prefixes_methods_with_owner() {
        let mut sig = EffectSignature::new("add", DefKind::Method);
        sig.owner = Some("Inventory".to_string());
        let out = analyze_summary("inventory.py", std::slice::from_ref(&sig));
        assert!(out.contains("Inventory.add (method,"));
    }
}
