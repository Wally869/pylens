//! Per-parameter content-domain hint inference: `url`, `email`, `path`, `json`, `date`,
//! `numeric_str`, `regex`, `html`. **Advisory only** — feeds `generate::seeds::hint_candidates`
//! so generation is more likely to reach a function's real body instead of raising on the first
//! parse of a placeholder like `"hello world"`. Never folds into `Shape`, `Purity`, or `Raises`:
//! a wrong tag just wastes a generation slot on a value that turns out no more (or less) useful
//! than the generic spread, exactly like a wrong structural guess already can. That's the whole
//! justification the heuristics below need — cheap, common evidence, not provably correct
//! evidence.
//!
//! Ranked by trust, strongest first:
//! 1. the root passed as an argument to a well-known stdlib call (`json.loads(s)`, `open(p)`, …);
//! 2. a method called directly on the root, refined by a string-literal argument's own content
//!    (`s.startswith("http")` is a much stronger URL signal than `s.startswith` alone);
//! 3. the parameter's own name — nearly free, and the weakest signal, so it never overrides
//!    stronger evidence, only adds to it (a parameter may carry several tags).

use std::collections::HashMap;

use ruff_python_ast as ast;
use ruff_python_ast::visitor::{self, Visitor};

use super::aliases::{dotted_attr, leftmost_name};

/// Infer content-domain tags for every name in `param_names`, from a single walk of `body` plus
/// each name's own spelling. A parameter with no evidence at all gets no entry.
pub(in crate::analyze) fn infer_hints(
    body: &[ast::Stmt],
    param_names: &[String],
) -> HashMap<String, Vec<String>> {
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for name in param_names {
        for tag in name_hints(name) {
            add(&mut out, name, tag);
        }
    }
    let mut collector = Collector { params: param_names, out: &mut out };
    for stmt in body {
        collector.visit_stmt(stmt);
    }
    out
}

fn add(out: &mut HashMap<String, Vec<String>>, root: &str, tag: &'static str) {
    let entry = out.entry(root.to_string()).or_default();
    if !entry.iter().any(|t| t == tag) {
        entry.push(tag.to_string());
    }
}

/// Weak, near-free evidence from the parameter's own spelling. Case-insensitive, exact-name
/// match only — no substring matching, to keep the false-positive rate low on this cheapest
/// tier.
fn name_hints(name: &str) -> Vec<&'static str> {
    let lower = name.to_lowercase();
    let mut tags = Vec::new();
    if matches!(lower.as_str(), "url" | "uri" | "link" | "href") {
        tags.push("url");
    }
    if matches!(lower.as_str(), "email" | "mail") {
        tags.push("email");
    }
    if matches!(lower.as_str(), "path" | "filename" | "filepath" | "dirname") {
        tags.push("path");
    }
    if matches!(lower.as_str(), "date" | "timestamp") {
        tags.push("date");
    }
    if matches!(lower.as_str(), "json" | "payload") {
        tags.push("json");
    }
    if matches!(lower.as_str(), "html" | "markup") {
        tags.push("html");
    }
    if matches!(lower.as_str(), "pattern" | "regex") {
        tags.push("regex");
    }
    tags
}

/// String methods whose literal argument is worth inspecting for domain markers (tier 2).
const SIGNAL_METHODS: &[&str] = &[
    "startswith", "endswith", "split", "lower", "strip", "encode", "decode", "replace",
    "splitlines",
];

/// Substring markers in a string-literal method argument that suggest a domain, cheapest
/// heuristic in the collector — a bounded set of obvious markers, not a classifier.
fn literal_signal(s: &str) -> Vec<&'static str> {
    let lower = s.to_lowercase();
    let mut tags = Vec::new();
    if lower.contains("http://") || lower.contains("https://") || lower.contains("www.") {
        tags.push("url");
    }
    if s.contains('@') {
        tags.push("email");
    }
    if s.contains('/') || s.contains('\\') {
        tags.push("path");
    }
    if s.contains('<') && s.contains('>') {
        tags.push("html");
    }
    tags
}

struct Collector<'a> {
    params: &'a [String],
    out: &'a mut HashMap<String, Vec<String>>,
}

impl Collector<'_> {
    fn param_root<'e>(&self, expr: &'e ast::Expr) -> Option<&'e str> {
        let name = leftmost_name(expr)?;
        self.params.iter().find(|p| p.as_str() == name).map(|_| name)
    }

    fn tag_arg(&mut self, arg: Option<&ast::Expr>, tag: &'static str) {
        if let Some(arg) = arg
            && let Some(root) = self.param_root(arg)
        {
            add(self.out, root, tag);
        }
    }

    /// Tier 1: the root passed as an argument to a well-known stdlib call, identified by its
    /// fully dotted callee name (`os.path.exists`, `re.compile`, ...).
    fn handle_dotted_call(&mut self, dotted: &str, arguments: &ast::Arguments) {
        match dotted {
            "json.loads" => self.tag_arg(arguments.args.first(), "json"),
            "os.path.exists" => self.tag_arg(arguments.args.first(), "path"),
            "os.path.join" => {
                for arg in arguments.args.iter() {
                    self.tag_arg(Some(arg), "path");
                }
            }
            "re.match" | "re.compile" => self.tag_arg(arguments.args.first(), "regex"),
            "datetime.strptime" => self.tag_arg(arguments.args.first(), "date"),
            _ => {}
        }
    }

    /// Tier 1: the root passed to a well-known bare-name callee (`open`, `int`, `float`,
    /// `urlparse`).
    fn handle_name_call(&mut self, name: &str, arguments: &ast::Arguments) {
        match name {
            "open" => self.tag_arg(arguments.args.first(), "path"),
            "int" | "float" => self.tag_arg(arguments.args.first(), "numeric_str"),
            "urlparse" => self.tag_arg(arguments.args.first(), "url"),
            _ => {}
        }
    }

    /// Tier 2: a method called directly on the root, with a string-literal argument scanned for
    /// domain markers.
    fn handle_method(&mut self, root: &str, method: &str, arguments: &ast::Arguments) {
        if !SIGNAL_METHODS.contains(&method) {
            return;
        }
        for arg in arguments.args.iter() {
            if let ast::Expr::StringLiteral(lit) = arg {
                let tags = literal_signal(lit.value.to_str());
                for tag in tags {
                    add(self.out, root, tag);
                }
            }
        }
    }

    fn handle_call(&mut self, call: &ast::ExprCall) {
        match call.func.as_ref() {
            ast::Expr::Attribute(attr) => {
                if let Some(dotted) = dotted_attr(&call.func) {
                    self.handle_dotted_call(&dotted, &call.arguments);
                }
                if let Some(root) = self.param_root(&attr.value) {
                    let root = root.to_string();
                    self.handle_method(&root, attr.attr.as_str(), &call.arguments);
                }
            }
            ast::Expr::Name(name) => {
                self.handle_name_call(name.id.as_str(), &call.arguments);
            }
            _ => {}
        }
    }
}

impl<'ast> Visitor<'ast> for Collector<'_> {
    fn visit_expr(&mut self, expr: &'ast ast::Expr) {
        if let ast::Expr::Call(call) = expr {
            self.handle_call(call);
        }
        visitor::walk_expr(self, expr);
    }
}
