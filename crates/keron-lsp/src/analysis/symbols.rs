//! Name resolution helpers over a parsed module: find the local
//! declaration a name refers to at a given offset, respecting keron's
//! scoping rules (top-level fns/structs/aliases are order-independent;
//! vals must precede their reference; params, `for` bindings, and
//! match binds shadow outer names within their construct).

use std::path::Path;

use keron_lang::{
    Block, Expr, FnDecl, ForPattern, Item, Param, Pattern, Program, Span, Spanned, Stmt,
    StructDecl, TypeAliasDecl, ValDecl,
};
use keron_modules::{CheckedModule, ModuleId, Resolution};

/// A local (same-module) definition site for a name.
#[derive(Debug)]
pub enum LocalDef<'a> {
    Fn(&'a FnDecl),
    Val(&'a ValDecl),
    Struct(&'a StructDecl),
    TypeAlias(&'a TypeAliasDecl),
    Param(&'a Param),
    /// A `for x in …` / `for (k, v) in …` binding or a match-pattern
    /// bind. Only the name's span is available.
    Binding {
        name: String,
        span: Span,
    },
}

impl LocalDef<'_> {
    /// Span of the defining name — the go-to-definition target.
    #[must_use]
    pub fn name_span(&self) -> Span {
        match self {
            Self::Fn(f) => f.name.span.clone(),
            Self::Val(v) => v.name.span.clone(),
            Self::Struct(s) => s.name.span.clone(),
            Self::TypeAlias(t) => t.name.span.clone(),
            Self::Param(p) => p.name.span.clone(),
            Self::Binding { span, .. } => span.clone(),
        }
    }
}

const fn contains(span: &Span, offset: usize) -> bool {
    span.start <= offset && offset < span.end
}

/// Find what `name`, referenced at `offset`, resolves to within this
/// module. Inner scopes win over top-level declarations.
#[must_use]
pub fn find_local_def<'a>(program: &'a Program, name: &str, offset: usize) -> Option<LocalDef<'a>> {
    for item in &program.items {
        if contains(&item.span(), offset)
            && let Some(def) = def_in_item(item, name, offset)
        {
            return Some(def);
        }
    }
    top_level_def(program, name, offset)
}

/// Top-level declaration lookup. `offset` gates vals only: forward
/// val references are check errors, so a val declared after the
/// cursor is not a candidate.
fn top_level_def<'a>(program: &'a Program, name: &str, offset: usize) -> Option<LocalDef<'a>> {
    program.items.iter().find_map(|item| match item {
        Item::Fn(f) if f.name.node == name => Some(LocalDef::Fn(f)),
        Item::Struct(s) if s.name.node == name => Some(LocalDef::Struct(s)),
        Item::TypeAlias(t) if t.name.node == name => Some(LocalDef::TypeAlias(t)),
        Item::Val(v) if v.name.node == name && v.span.start <= offset => Some(LocalDef::Val(v)),
        _ => None,
    })
}

fn def_in_item<'a>(item: &'a Item, name: &str, offset: usize) -> Option<LocalDef<'a>> {
    match item {
        Item::Fn(f) => def_in_block(&f.body, name, offset).or_else(|| {
            f.params
                .iter()
                .find(|p| p.name.node == name)
                .map(LocalDef::Param)
        }),
        Item::Val(v) => def_in_expr(&v.value, name, offset),
        Item::ExprStmt(e) => def_in_expr(e, name, offset),
        Item::Reconcile(r) => r
            .chains
            .iter()
            .flatten()
            .find_map(|e| def_in_expr(e, name, offset)),
        Item::Struct(s) => s
            .fields
            .iter()
            .filter_map(|f| f.default.as_ref())
            .find_map(|d| def_in_expr(d, name, offset)),
        Item::Use(_) | Item::TypeAlias(_) => None,
    }
}

fn def_in_block<'a>(b: &'a Block, name: &str, offset: usize) -> Option<LocalDef<'a>> {
    if !contains(&b.span, offset) {
        return None;
    }
    // Inner constructs first (their bindings shadow this block's vals).
    for stmt in &b.stmts {
        let found = match stmt {
            Stmt::Val(v) => def_in_expr(&v.value, name, offset),
            Stmt::Reconcile(r) => r
                .chains
                .iter()
                .flatten()
                .find_map(|e| def_in_expr(e, name, offset)),
            Stmt::Expr(e) => def_in_expr(e, name, offset),
        };
        if found.is_some() {
            return found;
        }
    }
    if let Some(t) = &b.trailing
        && let Some(found) = def_in_expr(t, name, offset)
    {
        return Some(found);
    }
    // Then this block's own vals, declaration-before-use.
    for stmt in &b.stmts {
        if let Stmt::Val(v) = stmt
            && v.name.node == name
            && v.span.start <= offset
        {
            return Some(LocalDef::Val(v));
        }
    }
    None
}

fn def_in_expr<'a>(e: &'a Spanned<Expr>, name: &str, offset: usize) -> Option<LocalDef<'a>> {
    if !contains(&e.span, offset) {
        return None;
    }
    match &e.node {
        Expr::Unary { operand, .. } => def_in_expr(operand, name, offset),
        Expr::Binary { lhs, rhs, .. } => {
            def_in_expr(lhs, name, offset).or_else(|| def_in_expr(rhs, name, offset))
        }
        Expr::Interpolation(parts) => parts.iter().find_map(|p| match p {
            keron_lang::StringPart::Expr { expr, .. } => def_in_expr(expr, name, offset),
            keron_lang::StringPart::Text(_) => None,
        }),
        Expr::List(items) => items.iter().find_map(|x| def_in_expr(x, name, offset)),
        Expr::Map(entries) => entries.iter().find_map(|en| {
            def_in_expr(&en.key, name, offset).or_else(|| def_in_expr(&en.value, name, offset))
        }),
        Expr::Call { args, .. } => args
            .iter()
            .find_map(|a| def_in_expr(&a.value, name, offset)),
        Expr::StructLiteral { fields, .. } => fields
            .iter()
            .filter_map(|f| f.value.as_ref())
            .find_map(|v| def_in_expr(v, name, offset)),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => def_in_expr(cond, name, offset)
            .or_else(|| def_in_block(then_branch, name, offset))
            .or_else(|| def_in_block(else_branch, name, offset)),
        Expr::For {
            pattern,
            iter_expr,
            body,
        } => {
            if let Some(found) = def_in_expr(iter_expr, name, offset) {
                return Some(found);
            }
            if !contains(&body.span, offset) {
                return None;
            }
            def_in_block(body, name, offset).or_else(|| for_binding(pattern, name))
        }
        Expr::Field { receiver, .. } => def_in_expr(receiver, name, offset),
        Expr::Match { scrutinee, arms } => {
            def_in_expr(scrutinee, name, offset).or_else(|| {
                arms.iter().find_map(|arm| {
                    let in_arm_body = arm
                        .guard
                        .as_ref()
                        .and_then(|g| def_in_expr(g, name, offset))
                        .or_else(|| def_in_expr(&arm.body, name, offset));
                    // Pattern binds are only in scope for this arm.
                    if in_arm_body.is_some() {
                        return in_arm_body;
                    }
                    let arm_scope = contains(&arm.span, offset) && offset >= arm.pattern.span.start;
                    arm_scope
                        .then(|| pattern_binding(&arm.pattern, name))
                        .flatten()
                })
            })
        }
        Expr::Literal(_) | Expr::Var(_) => None,
    }
}

fn for_binding<'a>(pattern: &ForPattern, name: &str) -> Option<LocalDef<'a>> {
    let found = match pattern {
        ForPattern::Elem(n) => (n.node == name).then_some(n),
        ForPattern::Entry { key, value } => (key.node == name)
            .then_some(key)
            .or_else(|| (value.node == name).then_some(value)),
    }?;
    Some(LocalDef::Binding {
        name: found.node.clone(),
        span: found.span.clone(),
    })
}

fn pattern_binding<'a>(p: &Spanned<Pattern>, name: &str) -> Option<LocalDef<'a>> {
    match &p.node {
        Pattern::Bind(n) if n == name => Some(LocalDef::Binding {
            name: n.clone(),
            span: p.span.clone(),
        }),
        Pattern::Struct { fields, .. } => fields.iter().find_map(|f| {
            f.pattern.as_ref().map_or_else(
                // Shorthand `Name { field }` binds the field's value
                // to its own name.
                || {
                    (f.name.node == name).then(|| LocalDef::Binding {
                        name: f.name.node.clone(),
                        span: f.name.span.clone(),
                    })
                },
                |sub| pattern_binding(sub, name),
            )
        }),
        Pattern::Bind(_) | Pattern::Lit(_) | Pattern::Wildcard => None,
    }
}

/// The checked module backing `path` in the latest resolution, if it
/// resolved at all.
#[must_use]
pub fn module_for<'r>(resolution: &'r Resolution, path: &Path) -> Option<&'r CheckedModule> {
    resolution.graph.modules.get(&ModuleId(path.to_path_buf()))
}

/// Struct fields of the variable `name` as referenced at `offset`,
/// resolvable without type inference: the var must be a `val` with a
/// struct annotation, a struct-typed param, or an imported val.
/// Powers `receiver.field` hover and `.`-triggered completion.
#[must_use]
pub fn var_struct_fields(
    program: &Program,
    name: &str,
    offset: usize,
    imported: &keron_lang::ImportedSymbols,
) -> Option<Vec<(String, keron_lang::Type)>> {
    let ty = match find_local_def(program, name, offset) {
        Some(LocalDef::Val(v)) => v.ty.as_ref().map(|t| t.node.clone()),
        Some(LocalDef::Param(p)) => Some(p.ty.node.clone()),
        _ => None,
    }
    .or_else(|| imported.vals.get(name).cloned())?;
    match ty {
        keron_lang::Type::Struct { fields, .. } => Some(fields),
        // A freshly parsed program (the last-good snapshot) has NOT
        // been through the module loader's type-name resolution, so a
        // user-struct annotation is still `Named` — resolve it against
        // the module's own decls and the imported/builtin types.
        keron_lang::Type::Named(type_name) => struct_fields_named(program, &type_name, imported),
        _ => None,
    }
}

fn struct_fields_named(
    program: &Program,
    type_name: &str,
    imported: &keron_lang::ImportedSymbols,
) -> Option<Vec<(String, keron_lang::Type)>> {
    for item in &program.items {
        if let Item::Struct(s) = item
            && s.name.node == type_name
        {
            return Some(
                s.fields
                    .iter()
                    .map(|f| (f.name.node.clone(), f.ty.node.clone()))
                    .collect(),
            );
        }
    }
    match imported.types.get(type_name) {
        Some(keron_lang::Type::Struct { fields, .. }) => Some(fields.clone()),
        _ => None,
    }
}

/// The span of top-level declaration `name` in `program` — where an
/// import of that name should jump to.
#[must_use]
pub fn top_level_decl_span(program: &Program, name: &str) -> Option<Span> {
    program.items.iter().find_map(|item| match item {
        Item::Fn(f) if f.name.node == name => Some(f.name.span.clone()),
        Item::Val(v) if v.name.node == name => Some(v.name.span.clone()),
        Item::Struct(s) if s.name.node == name => Some(s.name.span.clone()),
        Item::TypeAlias(t) if t.name.node == name => Some(t.name.span.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use keron_lang::parse;

    fn program(src: &str) -> Program {
        parse(src).expect("fixture parses")
    }

    #[test]
    fn param_shadows_top_level_val() {
        let src = "val x: Int = 1\nfn f(x: Int): Int { x }\n";
        let p = program(src);
        let body_ref = src.rfind('x').unwrap();
        match find_local_def(&p, "x", body_ref) {
            Some(LocalDef::Param(param)) => assert_eq!(param.name.node, "x"),
            other => panic!("expected Param, got {other:?}"),
        }
    }

    #[test]
    fn top_level_val_must_precede_reference() {
        let src = "val a: Int = b\nval b: Int = 2\n";
        let p = program(src);
        let a_ref = src.find("= b").unwrap() + 2;
        assert!(
            find_local_def(&p, "b", a_ref).is_none(),
            "forward val reference must not resolve"
        );
        let src2 = "val b: Int = 2\nval a: Int = b\n";
        let p2 = program(src2);
        let b_ref = src2.rfind('b').unwrap();
        match find_local_def(&p2, "b", b_ref) {
            Some(LocalDef::Val(v)) => assert_eq!(v.name.node, "b"),
            other => panic!("expected Val, got {other:?}"),
        }
    }

    #[test]
    fn fn_is_found_regardless_of_order() {
        let src = "val n: Int = later()\nfn later(): Int { 1 }\n";
        let p = program(src);
        match find_local_def(&p, "later", src.find("later()").unwrap()) {
            Some(LocalDef::Fn(f)) => assert_eq!(f.name.node, "later"),
            other => panic!("expected Fn, got {other:?}"),
        }
    }

    #[test]
    fn for_binding_resolves_inside_body_only() {
        let src = "fn f(xs: List<Int>): Void { for x in xs { reconcile_nothing(x) } }\n";
        // `reconcile_nothing` isn't real; parse only cares about shape.
        let p = program(src);
        let body_ref = src.rfind("(x)").unwrap() + 1;
        match find_local_def(&p, "x", body_ref) {
            Some(LocalDef::Binding { name, .. }) => assert_eq!(name, "x"),
            other => panic!("expected Binding, got {other:?}"),
        }
    }

    #[test]
    fn block_local_val_shadows_param() {
        let src = "fn f(x: Int): Int { val y = x + 1\n y }\n";
        let p = program(src);
        let y_ref = src.rfind('y').unwrap();
        match find_local_def(&p, "y", y_ref) {
            Some(LocalDef::Val(v)) => assert_eq!(v.name.node, "y"),
            other => panic!("expected block-local Val, got {other:?}"),
        }
    }

    #[test]
    fn match_bind_resolves_in_arm_body() {
        let src = "fn f(s: String): String { match s { v => v } }\n";
        let p = program(src);
        let v_ref = src.rfind('v').unwrap();
        match find_local_def(&p, "v", v_ref) {
            Some(LocalDef::Binding { name, .. }) => assert_eq!(name, "v"),
            other => panic!("expected match Binding, got {other:?}"),
        }
    }

    #[test]
    fn top_level_decl_span_finds_each_kind() {
        let src = "fn f(): Int { 1 }\nval v: Int = 1\nstruct S { x: Int }\ntype T = \"a\"\n";
        let p = program(src);
        for name in ["f", "v", "S", "T"] {
            let span = top_level_decl_span(&p, name).expect(name);
            assert_eq!(&src[span], name);
        }
        assert!(top_level_decl_span(&p, "missing").is_none());
    }
}
