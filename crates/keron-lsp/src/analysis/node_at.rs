//! Locate the AST node under a byte offset.
//!
//! [`node_at`] is the front door for hover / go-to-definition /
//! signature help: given a parsed [`Program`] and a cursor offset it
//! returns the most specific *interesting* node — identifier
//! references, declaration names, type names, import items. Plain
//! literals and structural punctuation return `None`; there is nothing
//! to say about them.

use keron_lang::{
    Block, Expr, FnDecl, Param, Pattern, Program, Span, Spanned, Stmt, StructDecl, StructField,
    Type, TypeAliasDecl, UseDecl, ValDecl,
};

/// The node found under the cursor. Reference-shaped variants carry
/// what the handlers need to resolve them; declaration-shaped variants
/// carry the whole declaration so hover can render it directly.
#[derive(Debug)]
pub enum NodeRef<'a> {
    /// A call's callee or a struct literal's name — something invoked.
    Callee(&'a Spanned<String>),
    /// A `val`/param/binding reference (`Expr::Var` or a struct-literal
    /// shorthand field).
    Var {
        name: &'a str,
        span: Span,
    },
    /// The `field` of `receiver.field`.
    FieldAccess {
        receiver: &'a Spanned<Expr>,
        field: &'a Spanned<String>,
    },
    /// A named type inside a type annotation (struct, string union, or
    /// unresolved name), e.g. the `Point` of `List<Point>`.
    TypeName {
        name: &'a str,
        span: Span,
    },
    FnName(&'a FnDecl),
    ValName(&'a ValDecl),
    StructName(&'a StructDecl),
    TypeAliasName(&'a TypeAliasDecl),
    ParamName(&'a Param),
    StructFieldName(&'a StructField),
    /// The quoted path of a `from "…" use …` item.
    UsePath(&'a UseDecl),
    /// One imported name of a `from "…" use …` item.
    UseName {
        name: &'a Spanned<String>,
    },
}

const fn contains(span: &Span, offset: usize) -> bool {
    span.start <= offset && offset < span.end
}

/// Find the most specific interesting node at `offset`.
#[must_use]
pub fn node_at(program: &Program, offset: usize) -> Option<NodeRef<'_>> {
    program
        .items
        .iter()
        .filter(|item| contains(&item.span(), offset))
        .find_map(|item| item_at(item, offset))
}

fn item_at(item: &keron_lang::Item, offset: usize) -> Option<NodeRef<'_>> {
    use keron_lang::Item;
    match item {
        Item::Use(u) => use_at(u, offset),
        Item::Val(v) => val_at(v, offset),
        Item::Fn(f) => fn_at(f, offset),
        Item::Struct(s) => struct_at(s, offset),
        Item::TypeAlias(t) => contains(&t.name.span, offset).then_some(NodeRef::TypeAliasName(t)),
        Item::Reconcile(r) => r.chains.iter().flatten().find_map(|e| expr_at(e, offset)),
        Item::ExprStmt(e) => expr_at(e, offset),
    }
}

fn use_at(u: &UseDecl, offset: usize) -> Option<NodeRef<'_>> {
    if contains(&u.source.span, offset) {
        return Some(NodeRef::UsePath(u));
    }
    u.names
        .iter()
        .find(|n| contains(&n.span, offset))
        .map(|name| NodeRef::UseName { name })
}

fn val_at(v: &ValDecl, offset: usize) -> Option<NodeRef<'_>> {
    if contains(&v.name.span, offset) {
        return Some(NodeRef::ValName(v));
    }
    if let Some(ty) = &v.ty
        && let Some(found) = type_at(ty, offset)
    {
        return Some(found);
    }
    expr_at(&v.value, offset)
}

fn fn_at(f: &FnDecl, offset: usize) -> Option<NodeRef<'_>> {
    if contains(&f.name.span, offset) {
        return Some(NodeRef::FnName(f));
    }
    for p in &f.params {
        if contains(&p.name.span, offset) {
            return Some(NodeRef::ParamName(p));
        }
        if let Some(found) = type_at(&p.ty, offset) {
            return Some(found);
        }
        if let Some(d) = &p.default
            && let Some(found) = expr_at(d, offset)
        {
            return Some(found);
        }
    }
    if let Some(found) = type_at(&f.return_type, offset) {
        return Some(found);
    }
    block_at(&f.body, offset)
}

fn struct_at(s: &StructDecl, offset: usize) -> Option<NodeRef<'_>> {
    if contains(&s.name.span, offset) {
        return Some(NodeRef::StructName(s));
    }
    for field in &s.fields {
        if contains(&field.name.span, offset) {
            return Some(NodeRef::StructFieldName(field));
        }
        if let Some(found) = type_at(&field.ty, offset) {
            return Some(found);
        }
        if let Some(d) = &field.default
            && let Some(found) = expr_at(d, offset)
        {
            return Some(found);
        }
    }
    None
}

/// A `Spanned<Type>` covers the whole annotation (`List<Point>` is one
/// span), so report the *primary* named type inside it — the name a
/// user would want to jump to.
fn type_at(ty: &Spanned<Type>, offset: usize) -> Option<NodeRef<'_>> {
    if !contains(&ty.span, offset) {
        return None;
    }
    primary_named(&ty.node).map(|name| NodeRef::TypeName {
        name,
        span: ty.span.clone(),
    })
}

fn primary_named(ty: &Type) -> Option<&str> {
    match ty {
        Type::Struct { name, .. } | Type::StringUnion { name, .. } | Type::Named(name) => {
            Some(name)
        }
        Type::List(inner) | Type::Nullable(inner) => primary_named(inner),
        Type::Map(k, v) => primary_named(v).or_else(|| primary_named(k)),
        _ => None,
    }
}

fn block_at(b: &Block, offset: usize) -> Option<NodeRef<'_>> {
    if !contains(&b.span, offset) {
        return None;
    }
    for stmt in &b.stmts {
        let found = match stmt {
            Stmt::Val(v) => val_at(v, offset),
            Stmt::Reconcile(r) => r.chains.iter().flatten().find_map(|e| expr_at(e, offset)),
            Stmt::Expr(e) => expr_at(e, offset),
        };
        if found.is_some() {
            return found;
        }
    }
    b.trailing.as_ref().and_then(|t| expr_at(t, offset))
}

fn expr_at(e: &Spanned<Expr>, offset: usize) -> Option<NodeRef<'_>> {
    if !contains(&e.span, offset) {
        return None;
    }
    match &e.node {
        Expr::Literal(_) => None,
        Expr::Unary { operand, .. } => expr_at(operand, offset),
        Expr::Binary { lhs, rhs, .. } => expr_at(lhs, offset).or_else(|| expr_at(rhs, offset)),
        Expr::Interpolation(parts) => parts.iter().find_map(|p| match p {
            keron_lang::StringPart::Expr { expr, .. } => expr_at(expr, offset),
            keron_lang::StringPart::Text(_) => None,
        }),
        Expr::List(items) => items.iter().find_map(|x| expr_at(x, offset)),
        Expr::Map(entries) => entries
            .iter()
            .find_map(|en| expr_at(&en.key, offset).or_else(|| expr_at(&en.value, offset))),
        Expr::Var(name) => Some(NodeRef::Var {
            name,
            span: e.span.clone(),
        }),
        Expr::Call { callee, args } => {
            if contains(&callee.span, offset) {
                return Some(NodeRef::Callee(callee));
            }
            args.iter().find_map(|a| expr_at(&a.value, offset))
        }
        Expr::StructLiteral { name, fields } => {
            if contains(&name.span, offset) {
                return Some(NodeRef::Callee(name));
            }
            fields.iter().find_map(|f| {
                f.value.as_ref().map_or_else(
                    // Shorthand `Name { field }` — the field name
                    // doubles as a val reference.
                    || {
                        contains(&f.name.span, offset).then(|| NodeRef::Var {
                            name: &f.name.node,
                            span: f.name.span.clone(),
                        })
                    },
                    |v| expr_at(v, offset),
                )
            })
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => expr_at(cond, offset)
            .or_else(|| block_at(then_branch, offset))
            .or_else(|| block_at(else_branch, offset)),
        Expr::For {
            iter_expr, body, ..
        } => expr_at(iter_expr, offset).or_else(|| block_at(body, offset)),
        Expr::Field { receiver, field } => {
            if let Some(found) = expr_at(receiver, offset) {
                return Some(found);
            }
            contains(&field.span, offset).then_some(NodeRef::FieldAccess { receiver, field })
        }
        Expr::Match { scrutinee, arms } => expr_at(scrutinee, offset).or_else(|| {
            arms.iter().find_map(|arm| {
                pattern_at(&arm.pattern, offset)
                    .or_else(|| arm.guard.as_ref().and_then(|g| expr_at(g, offset)))
                    .or_else(|| expr_at(&arm.body, offset))
            })
        }),
    }
}

fn pattern_at(p: &Spanned<Pattern>, offset: usize) -> Option<NodeRef<'_>> {
    if !contains(&p.span, offset) {
        return None;
    }
    match &p.node {
        Pattern::Struct { name, fields } => {
            if contains(&name.span, offset) {
                return Some(NodeRef::TypeName {
                    name: &name.node,
                    span: name.span.clone(),
                });
            }
            fields
                .iter()
                .find_map(|f| f.pattern.as_ref().and_then(|sub| pattern_at(sub, offset)))
        }
        Pattern::Lit(_) | Pattern::Wildcard | Pattern::Bind(_) => None,
    }
}

/// The innermost call whose argument list contains `offset` — the
/// inputs signature help needs.
#[derive(Debug)]
pub struct CallCtx<'a> {
    pub callee: &'a Spanned<String>,
    pub args: &'a [keron_lang::CallArg],
    /// Zero-based index of the argument the cursor sits in (equals
    /// `args.len()` when the cursor is past the last argument).
    pub active: usize,
}

/// Find the innermost call whose argument-list region contains
/// `offset`.
#[must_use]
pub fn enclosing_call(program: &Program, offset: usize) -> Option<CallCtx<'_>> {
    let mut best: Option<(CallCtx<'_>, usize)> = None;
    walk_exprs(program, &mut |e| {
        if let Expr::Call { callee, args } = &e.node
            && contains(&e.span, offset)
            && offset >= callee.span.end
        {
            let width = e.span.end - e.span.start;
            if best.as_ref().is_none_or(|&(_, w)| width <= w) {
                let active = args.iter().take_while(|a| offset > a.span.end).count();
                best = Some((
                    CallCtx {
                        callee,
                        args,
                        active,
                    },
                    width,
                ));
            }
        }
    });
    best.map(|(ctx, _)| ctx)
}

/// Visit every expression node in the program, depth-first.
pub fn walk_exprs<'a>(program: &'a Program, f: &mut impl FnMut(&'a Spanned<Expr>)) {
    use keron_lang::Item;
    for item in &program.items {
        match item {
            Item::Val(v) => walk_expr(&v.value, f),
            Item::Fn(fun) => {
                for p in &fun.params {
                    if let Some(d) = &p.default {
                        walk_expr(d, f);
                    }
                }
                walk_block(&fun.body, f);
            }
            Item::Struct(s) => {
                for field in &s.fields {
                    if let Some(d) = &field.default {
                        walk_expr(d, f);
                    }
                }
            }
            Item::Reconcile(r) => {
                for e in r.chains.iter().flatten() {
                    walk_expr(e, f);
                }
            }
            Item::ExprStmt(e) => walk_expr(e, f),
            Item::Use(_) | Item::TypeAlias(_) => {}
        }
    }
}

fn walk_block<'a>(b: &'a Block, f: &mut impl FnMut(&'a Spanned<Expr>)) {
    for stmt in &b.stmts {
        match stmt {
            Stmt::Val(v) => walk_expr(&v.value, f),
            Stmt::Reconcile(r) => {
                for e in r.chains.iter().flatten() {
                    walk_expr(e, f);
                }
            }
            Stmt::Expr(e) => walk_expr(e, f),
        }
    }
    if let Some(t) = &b.trailing {
        walk_expr(t, f);
    }
}

fn walk_expr<'a>(e: &'a Spanned<Expr>, f: &mut impl FnMut(&'a Spanned<Expr>)) {
    f(e);
    match &e.node {
        Expr::Literal(_) | Expr::Var(_) => {}
        Expr::Unary { operand, .. } => walk_expr(operand, f),
        Expr::Binary { lhs, rhs, .. } => {
            walk_expr(lhs, f);
            walk_expr(rhs, f);
        }
        Expr::Interpolation(parts) => {
            for p in parts {
                if let keron_lang::StringPart::Expr { expr, .. } = p {
                    walk_expr(expr, f);
                }
            }
        }
        Expr::List(items) => {
            for x in items {
                walk_expr(x, f);
            }
        }
        Expr::Map(entries) => {
            for en in entries {
                walk_expr(&en.key, f);
                walk_expr(&en.value, f);
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                walk_expr(&a.value, f);
            }
        }
        Expr::StructLiteral { fields, .. } => {
            for field in fields {
                if let Some(v) = &field.value {
                    walk_expr(v, f);
                }
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            walk_expr(cond, f);
            walk_block(then_branch, f);
            walk_block(else_branch, f);
        }
        Expr::For {
            iter_expr, body, ..
        } => {
            walk_expr(iter_expr, f);
            walk_block(body, f);
        }
        Expr::Field { receiver, .. } => walk_expr(receiver, f),
        Expr::Match { scrutinee, arms } => {
            walk_expr(scrutinee, f);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    walk_expr(g, f);
                }
                walk_expr(&arm.body, f);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keron_lang::parse;

    fn program(src: &str) -> Program {
        parse(src).expect("test fixture parses")
    }

    /// Byte offset of the first occurrence of `needle` in `src`, plus
    /// `add` to land inside the token.
    fn at(src: &str, needle: &str, add: usize) -> usize {
        src.find(needle).expect("needle present") + add
    }

    #[test]
    fn finds_val_name_and_var_reference() {
        let src = "val name: String = \"x\"\nval other: String = name\n";
        let p = program(src);
        match node_at(&p, at(src, "name", 1)) {
            Some(NodeRef::ValName(v)) => assert_eq!(v.name.node, "name"),
            other => panic!("expected ValName, got {other:?}"),
        }
        let ref_offset = src.rfind("name").unwrap() + 1;
        match node_at(&p, ref_offset) {
            Some(NodeRef::Var { name, .. }) => assert_eq!(name, "name"),
            other => panic!("expected Var, got {other:?}"),
        }
    }

    #[test]
    fn finds_callee_inside_call() {
        let src = "val s: Symlink = symlink(source = \"a\", target = \"b\")\n";
        let p = program(src);
        match node_at(&p, at(src, "symlink(", 3)) {
            Some(NodeRef::Callee(c)) => assert_eq!(c.node, "symlink"),
            other => panic!("expected Callee, got {other:?}"),
        }
    }

    #[test]
    fn finds_fn_name_param_and_var_in_body() {
        let src = "fn greet(who: String): String { \"hi \" + who }\n";
        let p = program(src);
        match node_at(&p, at(src, "greet", 2)) {
            Some(NodeRef::FnName(f)) => assert_eq!(f.name.node, "greet"),
            other => panic!("expected FnName, got {other:?}"),
        }
        match node_at(&p, at(src, "who:", 1)) {
            Some(NodeRef::ParamName(param)) => assert_eq!(param.name.node, "who"),
            other => panic!("expected ParamName, got {other:?}"),
        }
        let body_ref = src.rfind("who").unwrap() + 1;
        match node_at(&p, body_ref) {
            Some(NodeRef::Var { name, .. }) => assert_eq!(name, "who"),
            other => panic!("expected Var, got {other:?}"),
        }
    }

    #[test]
    fn finds_use_path_and_use_name() {
        let src = "from \"./lib.keron\" use helper\n";
        let p = program(src);
        match node_at(&p, at(src, "./lib", 1)) {
            Some(NodeRef::UsePath(u)) => assert_eq!(u.source.node, "./lib.keron"),
            other => panic!("expected UsePath, got {other:?}"),
        }
        match node_at(&p, at(src, "helper", 2)) {
            Some(NodeRef::UseName { name, .. }) => assert_eq!(name.node, "helper"),
            other => panic!("expected UseName, got {other:?}"),
        }
    }

    #[test]
    fn finds_named_type_inside_list_annotation() {
        let src = "struct Point { x: Int, y: Int }\nfn f(ps: List<Point>): Int { 1 }\n";
        let p = program(src);
        match node_at(&p, at(src, "List<Point>", 6)) {
            Some(NodeRef::TypeName { name, .. }) => assert_eq!(name, "Point"),
            other => panic!("expected TypeName, got {other:?}"),
        }
    }

    #[test]
    fn finds_field_access() {
        let src = "struct P { x: Int }\nval p: P = P { x: 1 }\nval n: Int = p.x\n";
        let p = program(src);
        let offset = src.rfind(".x").unwrap() + 1;
        match node_at(&p, offset) {
            Some(NodeRef::FieldAccess { field, .. }) => assert_eq!(field.node, "x"),
            other => panic!("expected FieldAccess, got {other:?}"),
        }
    }

    #[test]
    fn struct_literal_name_is_callee() {
        let src = "struct P { x: Int }\nval p: P = P { x: 1 }\n";
        let p = program(src);
        let offset = src.find("P { x: 1").unwrap();
        match node_at(&p, offset) {
            Some(NodeRef::Callee(c)) => assert_eq!(c.node, "P"),
            other => panic!("expected Callee, got {other:?}"),
        }
    }

    #[test]
    fn whitespace_between_items_finds_nothing() {
        let src = "val a: Int = 1\n\nval b: Int = 2\n";
        let p = program(src);
        assert!(node_at(&p, src.find("\n\n").unwrap() + 1).is_none());
    }

    #[test]
    fn enclosing_call_tracks_argument_index() {
        let src = "fn two(a: Int, b: Int): Int { a + b }\nval n: Int = two(1, 2)\n";
        let p = program(src);
        let open = src.rfind('(').unwrap();
        let ctx = enclosing_call(&p, open + 1).expect("inside call");
        assert_eq!(ctx.callee.node, "two");
        assert_eq!(ctx.active, 0);
        assert_eq!(ctx.args.len(), 2);
        let comma = src.rfind(',').unwrap();
        let ctx = enclosing_call(&p, comma + 1).expect("after comma");
        assert_eq!(ctx.active, 1);
    }

    #[test]
    fn enclosing_call_prefers_the_inner_call() {
        let src = "fn f(a: Int): Int { a }\nval n: Int = f(f(1))\n";
        let p = program(src);
        let inner = src.rfind("(1").unwrap();
        let ctx = enclosing_call(&p, inner + 1).expect("inner call");
        assert_eq!(ctx.callee.node, "f");
        assert_eq!(ctx.active, 0);
    }
}
