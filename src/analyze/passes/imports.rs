//! Imports pass: catalogs every import in the module — the public API also exposed standalone
//! as `pylens::imports_of` — then builds the binding table (`name` -> [`ModuleRef`], `has_star`)
//! that later passes use to recognize calls through an imported name.

use std::collections::HashMap;

use ruff_python_ast as ast;

use crate::model::{Import, ImportScope, ImportedName, ModuleRef};

use super::super::context::ModuleAnalysis;
use super::super::pass::Pass;

/// Catalog every import in the module, including those nested inside functions, classes, and
/// blocks. `import a, b` and `from m import a, b` are each captured as one entry.
pub fn collect_imports(module: &ast::ModModule) -> Vec<Import> {
    let mut out = Vec::new();
    collect_imports_in(&module.body, ImportScope::Module, &mut out);
    out
}

/// `scope` is the scope of the statements in `body`: `Module` until we descend into a function
/// body, then `Function` (an import there runs only when the function is called). Class bodies
/// and top-level control flow keep `Module` — they execute at load time.
fn collect_imports_in(body: &[ast::Stmt], scope: ImportScope, out: &mut Vec<Import>) {
    for stmt in body {
        match stmt {
            ast::Stmt::Import(imp) => {
                for alias in &imp.names {
                    out.push(Import {
                        from: false,
                        module: ModuleRef::parse(alias.name.as_str()),
                        level: 0,
                        alias: alias.asname.as_ref().map(|a| a.to_string()),
                        names: Vec::new(),
                        star: false,
                        scope,
                    });
                }
            }
            ast::Stmt::ImportFrom(imp) => {
                let module = imp.module.as_ref().map(|m| m.to_string()).unwrap_or_default();
                let mut names = Vec::new();
                let mut star = false;
                for alias in &imp.names {
                    if alias.name.as_str() == "*" {
                        star = true;
                    } else {
                        names.push(ImportedName {
                            name: alias.name.to_string(),
                            alias: alias.asname.as_ref().map(|a| a.to_string()),
                        });
                    }
                }
                out.push(Import {
                    from: true,
                    module: ModuleRef::parse(&module),
                    level: imp.level,
                    alias: None,
                    names,
                    star,
                    scope,
                });
            }
            ast::Stmt::FunctionDef(f) => collect_imports_in(&f.body, ImportScope::Function, out),
            ast::Stmt::ClassDef(c) => collect_imports_in(&c.body, scope, out),
            ast::Stmt::If(s) => {
                collect_imports_in(&s.body, scope, out);
                for clause in &s.elif_else_clauses {
                    collect_imports_in(&clause.body, scope, out);
                }
            }
            ast::Stmt::For(s) => {
                collect_imports_in(&s.body, scope, out);
                collect_imports_in(&s.orelse, scope, out);
            }
            ast::Stmt::While(s) => {
                collect_imports_in(&s.body, scope, out);
                collect_imports_in(&s.orelse, scope, out);
            }
            ast::Stmt::With(s) => collect_imports_in(&s.body, scope, out),
            ast::Stmt::Try(s) => {
                collect_imports_in(&s.body, scope, out);
                for handler in &s.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    collect_imports_in(&h.body, scope, out);
                }
                collect_imports_in(&s.orelse, scope, out);
                collect_imports_in(&s.finalbody, scope, out);
            }
            _ => {}
        }
    }
}

/// Builds `ModuleAnalysis::imports`/`bindings`/`has_star` for the rest of the pipeline.
pub(in crate::analyze) struct ImportsPass;

impl Pass for ImportsPass {
    fn run(&self, module: &ast::ModModule, ctx: &mut ModuleAnalysis) {
        let imports = collect_imports(module);
        let mut bindings: HashMap<String, ModuleRef> = HashMap::new();
        let mut import_names: HashMap<String, String> = HashMap::new();
        let mut has_star = false;
        for imp in &imports {
            if imp.star {
                has_star = true;
            }
            for b in imp.bindings() {
                bindings.entry(b).or_insert_with(|| imp.module.clone());
            }
            if imp.from {
                for n in &imp.names {
                    let local = n.alias.clone().unwrap_or_else(|| n.name.clone());
                    import_names.entry(local).or_insert_with(|| n.name.clone());
                }
            }
        }
        ctx.imports = imports;
        ctx.bindings = bindings;
        ctx.import_names = import_names;
        ctx.has_star = has_star;
    }
}
