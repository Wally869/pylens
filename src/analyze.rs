//! Static effect analysis: walk a function/method body and accumulate an [`EffectSignature`]
//! using may-set (over-approximating) semantics. See DESIGN.md.

use std::collections::{HashMap, HashSet};

use ruff_python_ast as ast;

use crate::model::*;

/// Analyze every function and method defined at module top level (functions) or directly in
/// a class body (methods). Nested functions are not descended into yet.
pub fn analyze_module(module: &ast::ModModule) -> Vec<EffectSignature> {
    // The import table lets each function be linked to the imports it references, and lets
    // calls through an imported name (`np.mean(...)`) be recorded as foreign effects instead
    // of silently passing as pure.
    let imports = collect_imports(module);
    let mut bindings: HashMap<String, ModuleRef> = HashMap::new();
    let mut has_star = false;
    for imp in &imports {
        if imp.star {
            has_star = true;
        }
        for b in imp.bindings() {
            bindings.entry(b).or_insert_with(|| imp.module.clone());
        }
    }

    let mut out = Vec::new();
    for stmt in &module.body {
        match stmt {
            ast::Stmt::FunctionDef(def) => {
                out.push(analyze_function(def, DefKind::Function, &bindings, has_star));
            }
            ast::Stmt::ClassDef(class) => {
                for member in &class.body {
                    if let ast::Stmt::FunctionDef(def) = member {
                        let mut sig = analyze_function(def, DefKind::Method, &bindings, has_star);
                        sig.owner = Some(class.name.as_str().to_string());
                        out.push(sig);
                    }
                }
            }
            _ => {}
        }
    }
    out
}

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

/// Analyze a single function definition. `imports` maps each in-scope import binding to its
/// module; `has_star` flags a `from m import *` in scope.
pub fn analyze_function(
    def: &ast::StmtFunctionDef,
    kind: DefKind,
    imports: &HashMap<String, ModuleRef>,
    has_star: bool,
) -> EffectSignature {
    let mut a = Analyzer::new(def, kind, imports, has_star);
    a.run();
    a.finish()
}

fn is_mutating_method(name: &str) -> bool {
    matches!(
        name,
        // list
        "append" | "extend" | "insert" | "remove" | "pop" | "clear" | "sort" | "reverse"
        // dict
        | "update" | "setdefault" | "popitem"
        // set
        | "add" | "discard" | "intersection_update" | "difference_update"
        | "symmetric_difference_update"
    )
}

struct Analyzer<'a> {
    self_param: Option<String>,
    /// Local name -> the parameter name it currently aliases (params seed this with identity).
    aliases: HashMap<String, String>,
    globals: HashSet<String>,
    nonlocals: HashSet<String>,
    /// Generatable parameters (excludes the method receiver), in declaration order.
    param_defs: Vec<ParamInfo>,
    /// Param name -> strongest usage-inferred shape vote so far.
    shapes: HashMap<String, ParamShape>,
    /// In-scope import binding -> the module it names (e.g. `np` -> numpy).
    imports: &'a HashMap<String, ModuleRef>,
    /// A `from m import *` is in scope.
    has_star: bool,
    /// Import bindings referenced in the body, in first-seen order.
    used_imports: Vec<String>,
    /// Set when an unresolved free callee is seen while a star import is in scope.
    may_use_star: bool,
    sig: EffectSignature,
    _def: &'a ast::StmtFunctionDef,
}

impl<'a> Analyzer<'a> {
    fn new(
        def: &'a ast::StmtFunctionDef,
        kind: DefKind,
        imports: &'a HashMap<String, ModuleRef>,
        has_star: bool,
    ) -> Self {
        let params = collect_param_names(&def.parameters);
        let self_param = match kind {
            DefKind::Method if !is_static(def) => params.first().cloned(),
            _ => None,
        };
        let mut aliases = HashMap::new();
        for p in &params {
            aliases.insert(p.clone(), p.clone());
        }
        let param_defs = collect_param_defs(&def.parameters, self_param.as_deref());
        let mut sig = EffectSignature::new(def.name.as_str(), kind);
        sig.declared_return = annotation_name(def.returns.as_deref());
        Self {
            self_param,
            aliases,
            globals: HashSet::new(),
            nonlocals: HashSet::new(),
            param_defs,
            shapes: HashMap::new(),
            imports,
            has_star,
            used_imports: Vec::new(),
            may_use_star: false,
            sig,
            _def: def,
        }
    }

    /// Note that `name` was referenced; if it's an import binding, record it as used.
    fn note_name(&mut self, name: &str) {
        if self.imports.contains_key(name) && !self.used_imports.iter().any(|u| u == name) {
            self.used_imports.push(name.to_string());
        }
    }

    /// Resolve an expression's leftmost name to a generatable parameter root (excludes the
    /// receiver and non-parameters).
    fn param_root(&self, expr: &ast::Expr) -> Option<String> {
        let name = leftmost_name(expr)?;
        if Some(name) == self.self_param.as_deref() {
            return None;
        }
        self.aliases.get(name).cloned()
    }

    /// Record a usage-inferred shape vote for the parameter that `expr` resolves to.
    fn vote(&mut self, expr: &ast::Expr, shape: ParamShape) {
        if let Some(p) = self.param_root(expr) {
            let cur = self.shapes.get(&p).copied().unwrap_or(ParamShape::Any);
            if shape_priority(shape) > shape_priority(cur) {
                self.shapes.insert(p, shape);
            }
        }
    }

    fn run(&mut self) {
        // Two notes on the may-set model:
        // - a body that can fall off the end contributes a `None` return.
        // - returns/raises are unioned over all exits.
        self.visit_body(&self._def.body);
        if can_fall_through(&self._def.body) {
            self.add_return(ReturnKind::None);
        }
    }

    fn finish(mut self) -> EffectSignature {
        dedup(&mut self.sig.returns);
        dedup(&mut self.sig.raises.explicit);
        dedup(&mut self.sig.raises.implicit);
        dedup(&mut self.sig.global_writes);
        dedup(&mut self.sig.io);
        dedup_mutations(&mut self.sig.mutations);
        let params: Vec<ParamInfo> = self
            .param_defs
            .iter()
            .map(|pi| ParamInfo {
                name: pi.name.clone(),
                shape: self.shapes.get(&pi.name).copied().unwrap_or(ParamShape::Any),
                has_default: pi.has_default,
            })
            .collect();
        self.sig.params = params;
        self.sig.purity = if !self.sig.unresolved_effects.is_empty() {
            Purity::Unknown
        } else if self.sig.mutations.is_empty()
            && self.sig.global_writes.is_empty()
            && self.sig.io.is_empty()
            && !self.sig.is_generator
        {
            Purity::Pure
        } else {
            Purity::Impure
        };
        self.sig.uses = self
            .used_imports
            .iter()
            .filter_map(|b| {
                self.imports.get(b).map(|m| ImportUse {
                    binding: b.clone(),
                    module: m.clone(),
                })
            })
            .collect();
        self.sig.may_use_star = self.may_use_star;
        self.sig
    }

    fn visit_body(&mut self, body: &[ast::Stmt]) {
        for stmt in body {
            self.visit_stmt(stmt);
        }
    }

    fn visit_stmt(&mut self, stmt: &ast::Stmt) {
        use ast::Stmt;
        match stmt {
            Stmt::Return(ret) => match ret.value.as_deref() {
                Some(expr) => {
                    let kind = classify_return(expr);
                    self.add_return(kind);
                    self.visit_expr(expr);
                }
                None => self.add_return(ReturnKind::None),
            },
            Stmt::Raise(raise) => {
                if let Some(exc) = raise.exc.as_deref() {
                    if let Some(name) = exception_name(exc) {
                        self.sig.raises.explicit.push(name);
                    }
                    // The exception *constructor* is not an effect — visit only its arguments
                    // so real effects there are still seen, without flagging the exception
                    // type itself as an unknown mutating callee.
                    match exc {
                        ast::Expr::Call(call) => {
                            for arg in call.arguments.args.iter() {
                                self.visit_expr(arg);
                            }
                            for kw in call.arguments.keywords.iter() {
                                self.visit_expr(&kw.value);
                            }
                        }
                        _ => self.visit_expr(exc),
                    }
                }
            }
            Stmt::Global(g) => {
                for name in &g.names {
                    self.globals.insert(name.as_str().to_string());
                }
            }
            Stmt::Nonlocal(n) => {
                for name in &n.names {
                    self.nonlocals.insert(name.as_str().to_string());
                }
            }
            Stmt::Assign(assign) => {
                self.visit_expr(&assign.value);
                for target in &assign.targets {
                    self.handle_assign_target(target, &assign.value);
                }
            }
            Stmt::AugAssign(aug) => {
                self.visit_expr(&aug.value);
                self.handle_aug_target(&aug.target);
            }
            Stmt::AnnAssign(ann) => {
                if let Some(value) = ann.value.as_deref() {
                    self.visit_expr(value);
                    self.handle_assign_target(&ann.target, value);
                }
            }
            Stmt::Delete(del) => {
                for target in &del.targets {
                    self.handle_delete_target(target);
                }
            }
            Stmt::Expr(e) => self.visit_expr(&e.value),
            Stmt::If(if_stmt) => {
                self.visit_expr(&if_stmt.test);
                self.visit_body(&if_stmt.body);
                for clause in &if_stmt.elif_else_clauses {
                    if let Some(test) = &clause.test {
                        self.visit_expr(test);
                    }
                    self.visit_body(&clause.body);
                }
            }
            Stmt::For(for_stmt) => {
                self.visit_expr(&for_stmt.iter);
                self.vote(&for_stmt.iter, ParamShape::Sequence);
                self.visit_body(&for_stmt.body);
                self.visit_body(&for_stmt.orelse);
            }
            Stmt::While(while_stmt) => {
                self.visit_expr(&while_stmt.test);
                self.visit_body(&while_stmt.body);
                self.visit_body(&while_stmt.orelse);
            }
            Stmt::With(with_stmt) => {
                for item in &with_stmt.items {
                    self.visit_expr(&item.context_expr);
                }
                self.visit_body(&with_stmt.body);
            }
            Stmt::Try(try_stmt) => {
                self.visit_body(&try_stmt.body);
                for handler in &try_stmt.handlers {
                    let ast::ExceptHandler::ExceptHandler(h) = handler;
                    self.visit_body(&h.body);
                }
                self.visit_body(&try_stmt.orelse);
                self.visit_body(&try_stmt.finalbody);
            }
            Stmt::Match(match_stmt) => {
                self.visit_expr(&match_stmt.subject);
                for case in &match_stmt.cases {
                    self.visit_body(&case.body);
                }
            }
            // Nested defs are separate scopes; not descended into yet.
            _ => {}
        }
    }

    fn handle_assign_target(&mut self, target: &ast::Expr, value: &ast::Expr) {
        match target {
            ast::Expr::Name(name) => {
                let n = name.id.as_str();
                // Alias tracking: `x = y` where y aliases a param.
                if let ast::Expr::Name(rhs) = value
                    && let Some(root) = self.aliases.get(rhs.id.as_str()).cloned()
                {
                    self.aliases.insert(n.to_string(), root);
                    return;
                }
                // Rebinding: x no longer aliases its original parameter object.
                self.aliases.remove(n);
                // Writing a module-level global declared in this scope.
                if self.globals.contains(n) {
                    self.sig.global_writes.push(n.to_string());
                }
            }
            ast::Expr::Subscript(sub) => {
                if let Some(t) = self.resolve_target(&sub.value, None) {
                    self.add_mutation(t, MutationKind::SubscriptSet, None);
                }
            }
            ast::Expr::Attribute(attr) => {
                if let Some(t) = self.resolve_target(&attr.value, Some(attr.attr.as_str())) {
                    self.add_mutation(t, MutationKind::AttrSet, Some(attr.attr.as_str()));
                }
            }
            ast::Expr::Tuple(tuple) => {
                for el in &tuple.elts {
                    self.handle_assign_target(el, value);
                }
            }
            ast::Expr::List(list) => {
                for el in &list.elts {
                    self.handle_assign_target(el, value);
                }
            }
            _ => {}
        }
    }

    fn handle_aug_target(&mut self, target: &ast::Expr) {
        match target {
            ast::Expr::Subscript(sub) => {
                if let Some(t) = self.resolve_target(&sub.value, None) {
                    self.add_mutation(t, MutationKind::AugSubscript, None);
                }
            }
            ast::Expr::Attribute(attr) => {
                if let Some(t) = self.resolve_target(&attr.value, Some(attr.attr.as_str())) {
                    self.add_mutation(t, MutationKind::AugAttr, Some(attr.attr.as_str()));
                }
            }
            ast::Expr::Name(name) => {
                let n = name.id.as_str();
                if self.globals.contains(n) {
                    self.sig.global_writes.push(n.to_string());
                }
            }
            _ => {}
        }
    }

    fn handle_delete_target(&mut self, target: &ast::Expr) {
        match target {
            ast::Expr::Subscript(sub) => {
                if let Some(t) = self.resolve_target(&sub.value, None) {
                    self.add_mutation(t, MutationKind::SubscriptDel, None);
                }
            }
            ast::Expr::Attribute(attr) => {
                if let Some(t) = self.resolve_target(&attr.value, Some(attr.attr.as_str())) {
                    self.add_mutation(t, MutationKind::AttrDel, Some(attr.attr.as_str()));
                }
            }
            _ => {}
        }
    }

    fn visit_expr(&mut self, expr: &ast::Expr) {
        use ast::Expr;
        match expr {
            Expr::Yield(y) => {
                self.sig.is_generator = true;
                if let Some(v) = y.value.as_deref() {
                    self.visit_expr(v);
                }
            }
            Expr::YieldFrom(y) => {
                self.sig.is_generator = true;
                self.visit_expr(&y.value);
            }
            Expr::Await(a) => self.visit_expr(&a.value),
            Expr::Call(call) => self.visit_call(call),
            Expr::Attribute(a) => self.visit_expr(&a.value),
            Expr::Subscript(s) => {
                self.vote(&s.value, ParamShape::Sequence);
                self.visit_expr(&s.value);
                self.visit_expr(&s.slice);
            }
            Expr::BinOp(b) => {
                self.visit_expr(&b.left);
                self.visit_expr(&b.right);
                let num = match b.op {
                    ast::Operator::Div => Some(ParamShape::Float),
                    ast::Operator::Sub
                    | ast::Operator::Mod
                    | ast::Operator::Pow
                    | ast::Operator::FloorDiv => Some(ParamShape::Int),
                    _ => None,
                };
                if let Some(sh) = num {
                    self.vote(&b.left, sh);
                    self.vote(&b.right, sh);
                }
            }
            Expr::BoolOp(b) => {
                for v in &b.values {
                    self.visit_expr(v);
                }
            }
            Expr::UnaryOp(u) => self.visit_expr(&u.operand),
            Expr::Compare(c) => {
                self.visit_expr(&c.left);
                for v in &c.comparators {
                    self.visit_expr(v);
                }
                // Comparing a parameter against a numeric literal ⇒ it's numeric.
                let operands: Vec<&ast::Expr> =
                    std::iter::once(c.left.as_ref()).chain(c.comparators.iter()).collect();
                for (i, op) in operands.iter().enumerate() {
                    if let Some(sh) = numeric_literal_shape(op) {
                        for (j, other) in operands.iter().enumerate() {
                            if j != i {
                                self.vote(other, sh);
                            }
                        }
                    }
                }
            }
            Expr::If(i) => {
                self.visit_expr(&i.test);
                self.visit_expr(&i.body);
                self.visit_expr(&i.orelse);
            }
            Expr::Name(n) => self.note_name(n.id.as_str()),
            Expr::Named(n) => self.visit_expr(&n.value),
            Expr::Starred(s) => self.visit_expr(&s.value),
            Expr::List(l) => l.elts.iter().for_each(|e| self.visit_expr(e)),
            Expr::Tuple(t) => t.elts.iter().for_each(|e| self.visit_expr(e)),
            Expr::Set(s) => s.elts.iter().for_each(|e| self.visit_expr(e)),
            Expr::Dict(d) => {
                for item in &d.items {
                    if let Some(k) = &item.key {
                        self.visit_expr(k);
                    }
                    self.visit_expr(&item.value);
                }
            }
            _ => {}
        }
    }

    fn visit_call(&mut self, call: &ast::ExprCall) {
        match call.func.as_ref() {
            // Method call: `base.method(...)`.
            ast::Expr::Attribute(attr) => {
                // A call through an imported name (`np.mean(...)`, `os.path.join(...)`) is a
                // foreign effect we can't see through — record it (so the function isn't
                // mistaken for pure) rather than treating it as a value method.
                if let Some(base) = leftmost_name(&attr.value)
                    && self.imports.contains_key(base)
                {
                    self.sig.unresolved_effects.push(UnresolvedEffect {
                        reason: "call_import".to_string(),
                        callee: dotted_attr(&call.func),
                        may_affect: self.args_targets(&call.arguments),
                    });
                } else {
                    let method = attr.attr.as_str();
                    if is_mutating_method(method)
                        && let Some(t) = self.resolve_target(&attr.value, None)
                    {
                        self.add_mutation(t, MutationKind::Method, Some(method));
                    }
                    if let Some(sh) = shape_for_method(method) {
                        self.vote(&attr.value, sh);
                    }
                }
            }
            // Plain function call: `name(...)`.
            ast::Expr::Name(name) => {
                let n = name.id.as_str();
                if self.imports.contains_key(n) {
                    // A directly-imported callable (`from json import dumps; dumps(x)`).
                    self.sig.unresolved_effects.push(UnresolvedEffect {
                        reason: "call_import".to_string(),
                        callee: Some(n.to_string()),
                        may_affect: self.args_targets(&call.arguments),
                    });
                } else {
                    match n {
                        "print" => self.sig.io.push("stdout".to_string()),
                        "open" => self.sig.io.push("filesystem".to_string()),
                        "input" => self.sig.io.push("stdin".to_string()),
                        "len" | "sum" | "sorted" | "reversed" | "enumerate" | "min" | "max" => {
                            if let Some(arg) = call.arguments.args.first() {
                                self.vote(arg, ParamShape::Sequence);
                            }
                        }
                        "setattr" | "delattr" | "exec" | "eval" => {
                            self.sig.unresolved_effects.push(UnresolvedEffect {
                                reason: format!("dynamic_{n}"),
                                callee: Some(n.to_string()),
                                may_affect: self.args_targets(&call.arguments),
                            });
                        }
                        _ if !is_known_pure_builtin(n) => {
                            let may = self.args_targets(&call.arguments);
                            if !may.is_empty() {
                                self.sig.unresolved_effects.push(UnresolvedEffect {
                                    reason: "call_unknown_callee".to_string(),
                                    callee: Some(n.to_string()),
                                    may_affect: may,
                                });
                            }
                            // The callee isn't a builtin or a known import; if a star import is
                            // in scope, it may have come from there.
                            if self.has_star {
                                self.may_use_star = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        self.visit_expr(&call.func);
        for arg in call.arguments.args.iter() {
            self.visit_expr(arg);
        }
        for kw in call.arguments.keywords.iter() {
            self.visit_expr(&kw.value);
        }
    }

    /// Targets among `arguments` that resolve to a tracked root (params passed into a call
    /// may be mutated by it).
    fn args_targets(&self, arguments: &ast::Arguments) -> Vec<MutationTarget> {
        let mut out = Vec::new();
        for arg in arguments.args.iter() {
            if let Some(t) = self.resolve_target(arg, None) {
                out.push(t);
            }
        }
        out
    }

    /// Resolve the base of a mutation/argument expression to a [`MutationTarget`] root.
    /// `attr` is the attribute name when the mutation is an attribute write on this base.
    fn resolve_target(&self, base: &ast::Expr, attr: Option<&str>) -> Option<MutationTarget> {
        let name = leftmost_name(base)?;
        if let (Some(self_p), Some(attr)) = (&self.self_param, attr)
            && name == self_p
        {
            return Some(MutationTarget::SelfAttr {
                name: attr.to_string(),
            });
        }
        if let Some(root) = self.aliases.get(name) {
            if self.globals.contains(root) {
                return Some(MutationTarget::Global { name: root.clone() });
            }
            return Some(MutationTarget::Param { name: root.clone() });
        }
        if self.globals.contains(name) {
            return Some(MutationTarget::Global {
                name: name.to_string(),
            });
        }
        if self.nonlocals.contains(name) {
            return Some(MutationTarget::Nonlocal {
                name: name.to_string(),
            });
        }
        None
    }

    fn add_return(&mut self, kind: ReturnKind) {
        self.sig.returns.push(kind);
    }

    fn add_mutation(&mut self, target: MutationTarget, via: MutationKind, name: Option<&str>) {
        self.sig.mutations.push(Mutation {
            target,
            via,
            name: name.map(str::to_string),
        });
    }
}

/// Reconstruct a dotted attribute/name access as a string, e.g. `os.path.join`. `None` if the
/// base isn't a plain name chain (e.g. a subscript or call sits in the way).
fn dotted_attr(expr: &ast::Expr) -> Option<String> {
    match expr {
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Attribute(a) => Some(format!("{}.{}", dotted_attr(&a.value)?, a.attr)),
        _ => None,
    }
}

/// The leftmost `Name` reached by peeling `.attr` and `[...]` off an expression.
fn leftmost_name(expr: &ast::Expr) -> Option<&str> {
    match expr {
        ast::Expr::Name(n) => Some(n.id.as_str()),
        ast::Expr::Attribute(a) => leftmost_name(&a.value),
        ast::Expr::Subscript(s) => leftmost_name(&s.value),
        _ => None,
    }
}

fn collect_param_names(params: &ast::Parameters) -> Vec<String> {
    let mut out = Vec::new();
    for p in &params.posonlyargs {
        out.push(p.parameter.name.as_str().to_string());
    }
    for p in &params.args {
        out.push(p.parameter.name.as_str().to_string());
    }
    if let Some(v) = &params.vararg {
        out.push(v.name.as_str().to_string());
    }
    for p in &params.kwonlyargs {
        out.push(p.parameter.name.as_str().to_string());
    }
    if let Some(k) = &params.kwarg {
        out.push(k.name.as_str().to_string());
    }
    out
}

fn collect_param_defs(params: &ast::Parameters, skip: Option<&str>) -> Vec<ParamInfo> {
    let mut raw: Vec<(String, bool)> = Vec::new();
    for p in &params.posonlyargs {
        raw.push((p.parameter.name.as_str().to_string(), p.default.is_some()));
    }
    for p in &params.args {
        raw.push((p.parameter.name.as_str().to_string(), p.default.is_some()));
    }
    if let Some(v) = &params.vararg {
        raw.push((v.name.as_str().to_string(), true));
    }
    for p in &params.kwonlyargs {
        raw.push((p.parameter.name.as_str().to_string(), p.default.is_some()));
    }
    if let Some(k) = &params.kwarg {
        raw.push((k.name.as_str().to_string(), true));
    }
    raw.into_iter()
        .filter(|(n, _)| Some(n.as_str()) != skip)
        .map(|(name, has_default)| ParamInfo {
            name,
            shape: ParamShape::Any,
            has_default,
        })
        .collect()
}

fn shape_priority(s: ParamShape) -> u8 {
    match s {
        ParamShape::Str | ParamShape::Mapping | ParamShape::Set => 6,
        ParamShape::Sequence => 5,
        ParamShape::Float => 4,
        ParamShape::Int => 3,
        ParamShape::Bool => 2,
        ParamShape::Any => 0,
    }
}

fn shape_for_method(m: &str) -> Option<ParamShape> {
    match m {
        "split" | "rsplit" | "strip" | "lstrip" | "rstrip" | "upper" | "lower" | "title"
        | "capitalize" | "replace" | "startswith" | "endswith" | "join" | "encode"
        | "splitlines" | "format" | "isdigit" | "isalpha" | "zfill" => Some(ParamShape::Str),
        "keys" | "values" | "items" | "get" | "setdefault" | "popitem" => {
            Some(ParamShape::Mapping)
        }
        "add" | "discard" | "union" | "intersection" | "difference" | "issubset"
        | "issuperset" | "symmetric_difference" => Some(ParamShape::Set),
        "append" | "extend" | "insert" | "sort" | "reverse" => Some(ParamShape::Sequence),
        _ => None,
    }
}

fn is_static(def: &ast::StmtFunctionDef) -> bool {
    def.decorator_list.iter().any(|d| {
        matches!(&d.expression, ast::Expr::Name(n)
            if n.id.as_str() == "staticmethod")
    })
}

fn annotation_name(ann: Option<&ast::Expr>) -> Option<String> {
    match ann? {
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Subscript(s) => Some(leftmost_name(&s.value)?.to_string()),
        _ => None,
    }
}

fn exception_name(exc: &ast::Expr) -> Option<String> {
    match exc {
        ast::Expr::Call(call) => exception_name(&call.func),
        ast::Expr::Name(n) => Some(n.id.as_str().to_string()),
        ast::Expr::Attribute(a) => Some(a.attr.as_str().to_string()),
        _ => None,
    }
}

/// If `expr` is a numeric literal (optionally unary-signed), the corresponding param shape.
fn numeric_literal_shape(expr: &ast::Expr) -> Option<ParamShape> {
    match expr {
        ast::Expr::NumberLiteral(n) => match n.value {
            ast::Number::Int(_) => Some(ParamShape::Int),
            ast::Number::Float(_) => Some(ParamShape::Float),
            ast::Number::Complex { .. } => None,
        },
        ast::Expr::UnaryOp(u) => match u.op {
            ast::UnaryOp::USub | ast::UnaryOp::UAdd => numeric_literal_shape(&u.operand),
            _ => None,
        },
        _ => None,
    }
}

fn classify_return(expr: &ast::Expr) -> ReturnKind {
    use ast::Expr;
    match expr {
        Expr::NoneLiteral(_) => ReturnKind::None,
        Expr::BooleanLiteral(_) => ReturnKind::Bool,
        Expr::NumberLiteral(n) => match n.value {
            ast::Number::Int(_) => ReturnKind::Int,
            ast::Number::Float(_) => ReturnKind::Float,
            ast::Number::Complex { .. } => ReturnKind::Opaque,
        },
        Expr::StringLiteral(_) | Expr::FString(_) => ReturnKind::Str,
        Expr::BytesLiteral(_) => ReturnKind::Bytes,
        Expr::List(_) | Expr::ListComp(_) | Expr::Tuple(_) => ReturnKind::Sequence,
        Expr::Dict(_) | Expr::DictComp(_) => ReturnKind::Mapping,
        Expr::Set(_) | Expr::SetComp(_) => ReturnKind::Set,
        Expr::Compare(_) => ReturnKind::Bool,
        // `-1`, `+x`, `~n` parse as a unary op over the literal; `not x` is a bool.
        Expr::UnaryOp(u) => match u.op {
            ast::UnaryOp::Not => ReturnKind::Bool,
            ast::UnaryOp::USub | ast::UnaryOp::UAdd | ast::UnaryOp::Invert => {
                classify_return(&u.operand)
            }
        },
        Expr::Call(call) => match &*call.func {
            Expr::Name(n) => match n.id.as_str() {
                "bool" => ReturnKind::Bool,
                "int" | "len" | "ord" | "hash" => ReturnKind::Int,
                "float" => ReturnKind::Float,
                "str" | "repr" | "chr" => ReturnKind::Str,
                "bytes" => ReturnKind::Bytes,
                "list" | "tuple" | "sorted" => ReturnKind::Sequence,
                "dict" => ReturnKind::Mapping,
                "set" | "frozenset" => ReturnKind::Set,
                _ => ReturnKind::Opaque,
            },
            _ => ReturnKind::Opaque,
        },
        _ => ReturnKind::Opaque,
    }
}

/// Whether control can reach the end of `body` without an explicit return/raise — in which
/// case the function contributes an implicit `None` return.
fn can_fall_through(body: &[ast::Stmt]) -> bool {
    !body_terminates(body)
}

/// Whether a block always exits via return/raise (its last statement terminates).
fn body_terminates(body: &[ast::Stmt]) -> bool {
    body.last().is_some_and(stmt_terminates)
}

/// Whether `stmt` exits the enclosing block on every path. Conservative: constructs we can't
/// prove exhaustive (`match`, `try`, loops) are treated as able to fall through.
fn stmt_terminates(stmt: &ast::Stmt) -> bool {
    match stmt {
        ast::Stmt::Return(_) | ast::Stmt::Raise(_) => true,
        // An `if` terminates only with an `else` where every branch terminates.
        ast::Stmt::If(s) => {
            let mut has_else = false;
            let mut all = body_terminates(&s.body);
            for clause in &s.elif_else_clauses {
                if clause.test.is_none() {
                    has_else = true;
                }
                all = all && body_terminates(&clause.body);
            }
            has_else && all
        }
        // A `with` terminates iff its body does.
        ast::Stmt::With(s) => body_terminates(&s.body),
        _ => false,
    }
}

fn is_known_pure_builtin(name: &str) -> bool {
    matches!(
        name,
        "len" | "range" | "enumerate" | "zip" | "map" | "filter" | "sorted" | "reversed"
            | "int" | "float" | "str" | "bool" | "bytes" | "list" | "dict" | "set"
            | "tuple" | "frozenset" | "abs" | "min" | "max" | "sum" | "round" | "ord"
            | "chr" | "repr" | "hash" | "isinstance" | "issubclass" | "type" | "all"
            | "any" | "divmod" | "pow" | "hex" | "oct" | "bin" | "format"
    )
}

fn dedup<T: Clone + PartialEq>(v: &mut Vec<T>) {
    let mut seen: Vec<T> = Vec::new();
    v.retain(|x| {
        if seen.contains(x) {
            false
        } else {
            seen.push(x.clone());
            true
        }
    });
}

fn dedup_mutations(v: &mut Vec<Mutation>) {
    let mut seen: Vec<Mutation> = Vec::new();
    v.retain(|x| {
        if seen.contains(x) {
            false
        } else {
            seen.push(x.clone());
            true
        }
    });
}
