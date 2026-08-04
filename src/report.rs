//! Thin terminal-summary formatter — the `report` injection point named in the architecture
//! (json now, this + html later). Pure string formatting: no I/O, no jail, unit-testable in
//! isolation. Each `*_summary` function renders one command's result as a scannable,
//! human-readable block; the JSON path in `main.rs` is unaffected by anything here.

use crate::model::{DefKind, EffectSignature, Shape};
use crate::record::{DepStatus, ModuleRecord};
use crate::validate::{Defect, Severity};

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
    }
    out
}

/// A validated function's identity plus the defects found against it — the slice `report` needs
/// to render a `validate` summary without re-running the checker.
pub struct FunctionValidation<'a> {
    pub name: &'a str,
    pub owner: Option<&'a str>,
    pub defects: &'a [Defect],
}

/// Render the `validate` result: per function with any defects, hard/soft counts and one line
/// per defect, then the overall summary with the hard-defect total made prominent.
pub fn validate_summary(
    file: &str,
    functions_checked: usize,
    hard_total: usize,
    soft_total: usize,
    results: &[FunctionValidation],
) -> String {
    let mut out = format!(
        "{file}: HARD DEFECTS: {hard_total} — {functions_checked} function(s) checked, \
         {soft_total} soft defect(s)\n"
    );
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
            guard_samples: Vec::new(),
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
