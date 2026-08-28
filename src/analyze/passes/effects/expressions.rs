use ruff_python_ast as ast;
use super::super::super::collect::exceptions::{
    binop_implicit_exception, is_ordered_compare, is_proven_nonnegative_int_literal,
    subscript_read_exceptions,
};
use super::Walker;

impl Walker < '_ , '_ > {
        pub fn visit_expr(&mut self, expr: &ast::Expr) {
            use ast::Expr;
            match expr {
                Expr::Yield(y) => {
                    self.facts.sig.is_generator = true;
                    if let Some(v) = y.value.as_deref() {
                        self.visit_expr(v);
                    }
                }
                Expr::YieldFrom(y) => {
                    self.facts.sig.is_generator = true;
                    self.visit_expr(&y.value);
                }
                Expr::Await(a) => self.visit_expr(&a.value),
                Expr::Call(call) => self.visit_call(call),
                Expr::Attribute(a) => {
                    // An attribute LOAD (this arm is reached in load position only — a call's
                    // own callee attribute is excluded by `visit_call`'s own final visit of
                    // `call.func`, see its doc). Unless the base is *proven* — see
                    // `FunctionFacts::attribute_load_proven` — a may-set over-approximation:
                    // most `any`-shaped bases genuinely can raise `AttributeError`.
                    if !self.facts.attribute_load_proven(&a.value, a.attr.as_str()) {
                        self.facts.sig.raises.implicit.push("AttributeError".to_string());
                    }
                    self.visit_expr(&a.value);
                }
                Expr::Subscript(s) => {
                    // A subscript READ (this arm is only reached in value position — assignment
                    // and delete targets are handled separately and never call `visit_expr`) may
                    // raise, over-approximated by the base's shape known so far in this forward
                    // walk: mapping ⇒ `KeyError`, sequence/str ⇒ `IndexError`, else both.
                    if let Some(shape) = self.facts.env_shape(&s.value) {
                        let is_param_root = self.facts.param_root(&s.value).is_some();
                        for exc in subscript_read_exceptions(&shape, is_param_root) {
                            self.facts.sig.raises.implicit.push((*exc).to_string());
                        }
                    }
                    // The key/index itself may also raise `TypeError` if its type is unknown to
                    // the analyzer (e.g. an unhashable value used as a dict key) — see
                    // `collect::exceptions` doc. Composes with the base-driven KeyError/IndexError
                    // above: a subscript can contribute both.
                    self.facts.note_type_error_candidate(&s.slice);
                    // The BASE itself may not be subscriptable at all (an `Any`-typed base's
                    // runtime value could be anything, including a non-container) — a further
                    // `TypeError` candidate, additive with the base-shape-driven Key/IndexError
                    // above (a subscript on a genuinely unknown base can raise either).
                    self.facts.note_type_error_candidate(&s.value);
                    self.visit_expr(&s.value);
                    self.visit_expr(&s.slice);
                }
                Expr::BinOp(b) => {
                    self.visit_expr(&b.left);
                    self.visit_expr(&b.right);
                    if let Some(exc) = binop_implicit_exception(b.op) {
                        self.facts.sig.raises.implicit.push(exc.to_string());
                    }
                    // Arithmetic on an operand whose type the analyzer never pinned may raise
                    // `TypeError` — see `collect::exceptions` doc.
                    for operand in [b.left.as_ref(), b.right.as_ref()] {
                        self.facts.note_type_error_candidate(operand);
                    }
                    // `<<`/`>>` additionally raise `ValueError` for a negative shift count,
                    // proven safe only when the right operand is a literal non-negative int (no
                    // shape lookup — see `collect::exceptions::is_proven_nonnegative_int_literal`).
                    if matches!(b.op, ast::Operator::LShift | ast::Operator::RShift)
                        && !is_proven_nonnegative_int_literal(&b.right)
                    {
                        self.facts.sig.raises.implicit.push("ValueError".to_string());
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
                    // An ordered comparison (`<`/`<=`/`>`/`>=`) on an operand whose type the
                    // analyzer never pinned may raise `TypeError` — see `collect::exceptions` doc.
                    if c.ops.iter().any(|op| is_ordered_compare(*op)) {
                        let operands =
                            std::iter::once(c.left.as_ref()).chain(c.comparators.iter());
                        for operand in operands {
                            self.facts.note_type_error_candidate(operand);
                        }
                    }
                    // Membership (`in`/`not in`) hashes (or otherwise scans) its LEFT operand
                    // against the right-hand container; an `Any`-typed left operand may genuinely
                    // be unhashable (or otherwise unsupported) at runtime — `TypeError`. Unlike the
                    // ordered-compare rule, only the left operand is checked: the right-hand
                    // container's own shape is covered separately by the base-driven subscript
                    // rules where relevant, and membership never mistypes on the container itself.
                    if c.ops.iter().any(|op| matches!(op, ast::CmpOp::In | ast::CmpOp::NotIn)) {
                        self.facts.note_type_error_candidate(&c.left);
                    }
                }
                Expr::If(i) => {
                    self.note_guard_test(&i.test);
                    self.visit_expr(&i.test);
                    self.visit_expr(&i.body);
                    self.visit_expr(&i.orelse);
                }
                Expr::Name(n) => self.facts.note_name(n.id.as_str()),
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
                Expr::ListComp(c) => {
                    self.visit_comprehensions(&c.generators);
                    self.visit_expr(&c.elt);
                }
                Expr::SetComp(c) => {
                    self.visit_comprehensions(&c.generators);
                    self.visit_expr(&c.elt);
                }
                Expr::DictComp(c) => {
                    self.visit_comprehensions(&c.generators);
                    if let Some(k) = &c.key {
                        self.visit_expr(k);
                    }
                    self.visit_expr(&c.value);
                }
                Expr::Generator(c) => {
                    self.visit_comprehensions(&c.generators);
                    self.visit_expr(&c.elt);
                }
                Expr::Lambda(l) => self.visit_expr(&l.body),
                _ => {}
            }
        }

}
