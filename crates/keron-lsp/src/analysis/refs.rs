//! Reference collection — the shared engine behind find-references,
//! rename, and document highlight.
//!
//! keron has one flat top-level namespace per module (the checker
//! rejects a `val` and a `fn` sharing a name), so a name maps to at
//! most one top-level declaration. The only shadowing is value-scoped:
//! params, `for` bindings, block vals, and match binds can shadow an
//! outer val. Accordingly, value references are verified through
//! [`find_local_def`] (does this occurrence resolve to the same
//! definition site?) while callee/type/use references match by name.

use keron_lang::{
    Block, Expr, ForPattern, Item, Pattern, Program, Span, Spanned, Stmt, StringPart,
};

use crate::analysis::symbols::find_local_def;

/// How a name occurrence participates in the program — this decides
/// both whether it belongs to a given rename and how the edit is
/// spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    /// The defining name of a decl / param / binding.
    Decl,
    /// A value reference (`Expr::Var`).
    VarRef,
    /// A callee or struct-literal name.
    CalleeRef,
    /// A name inside a type annotation (span already refined to the
    /// exact identifier).
    TypeRef,
    /// A name in a `from "…" use …` list.
    UseName,
    /// Shorthand struct-literal field (`P { x }`) or shorthand
    /// pattern field — the name is simultaneously the field and a
    /// value binding/reference, so a rename must expand it to
    /// `field: new_name` instead of replacing it.
    Shorthand,
}

#[derive(Debug, Clone)]
pub struct NameHit {
    pub span: Span,
    pub kind: HitKind,
}

/// Every occurrence of `name` in `program` (declarations, value refs,
/// callees, type positions, use lists, shorthand fields). Field-name
/// positions (struct field decls, `receiver.field`, named args,
/// non-shorthand literal/pattern fields) are deliberately excluded —
/// fields are a separate namespace.
#[must_use]
pub fn collect_name_hits(program: &Program, text: &str, name: &str) -> Vec<NameHit> {
    let mut c = Collector {
        name,
        text,
        hits: Vec::new(),
    };
    for item in &program.items {
        c.item(item);
    }
    c.hits.sort_by_key(|h| h.span.start);
    c.hits.dedup_by_key(|h| h.span.start);
    c.hits
}

/// Spans of named arguments `param = …` in every call to `fn_name` —
/// the extra edits a *param* rename needs.
#[must_use]
pub fn named_arg_spans(program: &Program, fn_name: &str, param: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    crate::analysis::node_at::walk_exprs(program, &mut |e| {
        if let Expr::Call { callee, args } = &e.node
            && callee.node == fn_name
        {
            for arg in args {
                if let Some(n) = &arg.name
                    && n.node == param
                {
                    spans.push(n.span.clone());
                }
            }
        }
    });
    spans
}

/// Does the value reference at `offset` resolve to the definition
/// whose name occupies `def_span`? Used to drop shadowed occurrences.
#[must_use]
pub fn resolves_to(program: &Program, name: &str, offset: usize, def_span: &Span) -> bool {
    find_local_def(program, name, offset).is_some_and(|d| d.name_span() == *def_span)
}

struct Collector<'a> {
    name: &'a str,
    text: &'a str,
    hits: Vec<NameHit>,
}

impl Collector<'_> {
    fn hit(&mut self, span: Span, kind: HitKind) {
        self.hits.push(NameHit { span, kind });
    }

    fn named(&mut self, spanned: &Spanned<String>, kind: HitKind) {
        if spanned.node == self.name {
            self.hit(spanned.span.clone(), kind);
        }
    }

    /// A `Spanned<Type>` covers the whole annotation (`List<Point>`),
    /// so find the exact identifier occurrences textually. Type
    /// annotations only contain type names and punctuation, making a
    /// word-boundary scan exact.
    fn ty(&mut self, span: &Span) {
        let is_ident = |c: char| c.is_alphanumeric() || c == '_';
        let hay = &self.text[span.start.min(self.text.len())..span.end.min(self.text.len())];
        let mut from = 0;
        while let Some(i) = hay[from..].find(self.name) {
            let start = from + i;
            let end = start + self.name.len();
            let before_ok = start == 0 || !hay[..start].chars().next_back().is_some_and(is_ident);
            let after_ok = !hay[end..].chars().next().is_some_and(is_ident);
            if before_ok && after_ok {
                self.hit(span.start + start..span.start + end, HitKind::TypeRef);
            }
            from = end;
        }
    }

    fn item(&mut self, item: &Item) {
        match item {
            Item::Use(u) => {
                for n in &u.names {
                    self.named(n, HitKind::UseName);
                }
            }
            Item::Val(v) => {
                self.named(&v.name, HitKind::Decl);
                if let Some(ty) = &v.ty {
                    self.ty(&ty.span);
                }
                self.expr(&v.value);
            }
            Item::Fn(f) => {
                self.named(&f.name, HitKind::Decl);
                for p in &f.params {
                    self.named(&p.name, HitKind::Decl);
                    self.ty(&p.ty.span);
                    if let Some(d) = &p.default {
                        self.expr(d);
                    }
                }
                self.ty(&f.return_type.span);
                self.block(&f.body);
            }
            Item::Struct(s) => {
                self.named(&s.name, HitKind::Decl);
                for field in &s.fields {
                    self.ty(&field.ty.span);
                    if let Some(d) = &field.default {
                        self.expr(d);
                    }
                }
            }
            Item::TypeAlias(t) => self.named(&t.name, HitKind::Decl),
            Item::Reconcile(r) => {
                for e in r.chains.iter().flatten() {
                    self.expr(e);
                }
            }
            Item::ExprStmt(e) => self.expr(e),
        }
    }

    fn block(&mut self, b: &Block) {
        for stmt in &b.stmts {
            match stmt {
                Stmt::Val(v) => {
                    self.named(&v.name, HitKind::Decl);
                    if let Some(ty) = &v.ty {
                        self.ty(&ty.span);
                    }
                    self.expr(&v.value);
                }
                Stmt::Reconcile(r) => {
                    for e in r.chains.iter().flatten() {
                        self.expr(e);
                    }
                }
                Stmt::Expr(e) => self.expr(e),
            }
        }
        if let Some(t) = &b.trailing {
            self.expr(t);
        }
    }

    fn expr(&mut self, e: &Spanned<Expr>) {
        match &e.node {
            Expr::Var(n) => {
                if n == self.name {
                    self.hit(e.span.clone(), HitKind::VarRef);
                }
            }
            Expr::Literal(_) => {}
            Expr::Unary { operand, .. } => self.expr(operand),
            Expr::Binary { lhs, rhs, .. } => {
                self.expr(lhs);
                self.expr(rhs);
            }
            Expr::Interpolation(parts) => {
                for p in parts {
                    if let StringPart::Expr { expr, .. } = p {
                        self.expr(expr);
                    }
                }
            }
            Expr::List(items) => {
                for x in items {
                    self.expr(x);
                }
            }
            Expr::Map(entries) => {
                for en in entries {
                    self.expr(&en.key);
                    self.expr(&en.value);
                }
            }
            Expr::Call { callee, args } => {
                self.named(callee, HitKind::CalleeRef);
                for a in args {
                    self.expr(&a.value);
                }
            }
            Expr::StructLiteral { name, fields } => {
                self.named(name, HitKind::CalleeRef);
                for f in fields {
                    match &f.value {
                        Some(v) => self.expr(v),
                        None => self.named(&f.name, HitKind::Shorthand),
                    }
                }
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                self.expr(cond);
                self.block(then_branch);
                self.block(else_branch);
            }
            Expr::For {
                pattern,
                iter_expr,
                body,
            } => {
                match pattern {
                    ForPattern::Elem(n) => self.named(n, HitKind::Decl),
                    ForPattern::Entry { key, value } => {
                        self.named(key, HitKind::Decl);
                        self.named(value, HitKind::Decl);
                    }
                }
                self.expr(iter_expr);
                self.block(body);
            }
            Expr::Field { receiver, .. } => self.expr(receiver),
            Expr::Match { scrutinee, arms } => {
                self.expr(scrutinee);
                for arm in arms {
                    self.pattern(&arm.pattern);
                    if let Some(g) = &arm.guard {
                        self.expr(g);
                    }
                    self.expr(&arm.body);
                }
            }
        }
    }

    fn pattern(&mut self, p: &Spanned<Pattern>) {
        match &p.node {
            Pattern::Bind(n) => {
                if n == self.name {
                    self.hit(p.span.clone(), HitKind::Decl);
                }
            }
            Pattern::Struct { name, fields } => {
                self.named(name, HitKind::CalleeRef);
                for f in fields {
                    match &f.pattern {
                        Some(sub) => self.pattern(sub),
                        None => self.named(&f.name, HitKind::Shorthand),
                    }
                }
            }
            Pattern::Lit(_) | Pattern::Wildcard => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keron_lang::parse;

    fn hits(src: &str, name: &str) -> Vec<(HitKind, String)> {
        let program = parse(src).expect("fixture parses");
        collect_name_hits(&program, src, name)
            .into_iter()
            .map(|h| (h.kind, src[h.span].to_string()))
            .collect()
    }

    #[test]
    fn val_decl_and_refs_including_interpolation() {
        let src = "val name: String = \"x\"\nval msg: String = \"hi ${name}\"\nval again: String = name\n";
        let got = hits(src, "name");
        assert_eq!(
            got,
            vec![
                (HitKind::Decl, "name".to_string()),
                (HitKind::VarRef, "name".to_string()),
                (HitKind::VarRef, "name".to_string()),
            ]
        );
    }

    #[test]
    fn struct_hits_cover_decl_literal_annotation_and_pattern() {
        let src = "struct Point { x: Int }\n\
                   val p: Point = Point { x: 1 }\n\
                   fn f(q: List<Point>): Int { match p { Point { x } => x } }\n";
        let got = hits(src, "Point");
        let kinds: Vec<HitKind> = got.iter().map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            vec![
                HitKind::Decl,
                HitKind::TypeRef,
                HitKind::CalleeRef,
                HitKind::TypeRef,
                HitKind::CalleeRef,
            ],
            "got: {got:?}"
        );
    }

    #[test]
    fn type_annotation_refinement_is_word_boundary_exact() {
        let src =
            "struct P { x: Int }\nstruct PP { y: Int }\nfn f(a: Map<PP, List<P>>): Int { 1 }\n";
        let got = hits(src, "P");
        // Decl + exactly one occurrence inside the Map annotation —
        // the `PP` must not match.
        assert_eq!(got.len(), 2, "got: {got:?}");
        assert!(got.iter().all(|(_, s)| s == "P"));
    }

    #[test]
    fn shorthand_literal_field_is_flagged() {
        let src = "struct P { x: Int }\nval x: Int = 1\nval p: P = P { x }\n";
        let got = hits(src, "x");
        assert_eq!(
            got.last().map(|(k, _)| *k),
            Some(HitKind::Shorthand),
            "got: {got:?}"
        );
    }

    #[test]
    fn field_positions_are_excluded() {
        // `x` as struct field decl, named arg, and field access must
        // NOT appear; only the val decl and its var ref do.
        let src = "struct S { x: Int }\n\
                   val x: Int = 1\n\
                   val s: S = S { x: x }\n\
                   val n: Int = s.x\n";
        let got = hits(src, "x");
        assert_eq!(
            got,
            vec![
                (HitKind::Decl, "x".to_string()),
                (HitKind::VarRef, "x".to_string()),
            ]
        );
    }

    #[test]
    fn named_arg_spans_find_param_uses() {
        let src = "fn f(count: Int): Int { count }\nval a: Int = f(count = 1)\nval b: Int = f(2)\n";
        let program = parse(src).expect("parses");
        let spans = named_arg_spans(&program, "f", "count");
        assert_eq!(spans.len(), 1);
        assert_eq!(&src[spans[0].clone()], "count");
    }

    #[test]
    fn resolves_to_rejects_shadowed_occurrences() {
        let src = "val x: Int = 1\nfn f(x: Int): Int { x }\nval y: Int = x\n";
        let program = parse(src).expect("parses");
        let top_val_span = src.find('x').unwrap()..src.find('x').unwrap() + 1;
        let body_ref = src.find("{ x }").unwrap() + 2;
        let tail_ref = src.rfind('x').unwrap();
        assert!(
            !resolves_to(&program, "x", body_ref, &top_val_span),
            "param-shadowed ref must not resolve to the top-level val"
        );
        assert!(resolves_to(&program, "x", tail_ref, &top_val_span));
    }
}
