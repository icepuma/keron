//! Type checker for keron AST.
//!
//! Bidirectional: when a `val` carries an annotation (or a fn body has
//! a declared return type), the expected type is pushed down via
//! [`check_expr`]; otherwise [`expr_type`] synthesises bottom-up. Two
//! constructs use the expected type non-trivially: list literals (so
//! `[]` or `[e1, …]` can be checked against a `List<T>` annotation
//! without first knowing `T`) and the `++` operator. Every other node
//! falls back to synth-then-equality.
//!
//! Arithmetic operators (`- * / **` and unary `-`) require numeric
//! operands. `+` is overloaded: numeric like the others, plus
//! `String + String → String`. Mixed `Int`/`Double` operands promote
//! to `Double`. Val annotations are strict.
//!
//! Lists are strictly homogeneous (no `Int`→`Double` promotion within
//! a list). `++` is list concat with strict element-type equality.
//!
//! **Functions** live in their own namespace. The checker runs in two
//! passes: pass 1 collects every top-level name (rejecting duplicates
//! across the val/fn namespaces) and builds an `fn_env` of validated
//! signatures. Pass 2 walks items in source order and type-checks
//! each one. Inside a fn body, the val scope inherits outer vals at
//! the fn's source position, then params, then body-local `val`s.
//! Mixed shadowing: params may shadow outer vals; body-locals may not
//! shadow anything (param, outer val, or earlier body-local).
//!
//! **Imports** are pre-resolved by the module loader (`keron-modules`)
//! and arrive as [`ImportedSymbols`]: a flat namespace of imported
//! function signatures and val types that participates in pass 1's
//! duplicate-name check exactly like a local declaration. The checker
//! itself is module-agnostic — it only sees the local AST plus this
//! imported symbol set.

use std::collections::{HashMap, HashSet};

use crate::{
    ast::{
        BinOp, Block, CallArg, Expr, FnDecl, ForPattern, Item, Literal, MapEntry, Param, Program,
        ReconcileDecl, Span, Spanned, Stmt, StringPart, StructDecl, Type, TypeAliasDecl, UnaryOp,
        ValDecl,
    },
    diagnostic::Diagnostic,
};

mod match_check;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingKind {
    OuterVal,
    Param,
    BodyLocal,
}

#[derive(Debug, Default, Clone)]
struct Env {
    bindings: HashMap<String, (Type, BindingKind)>,
}

impl Env {
    fn lookup(&self, name: &str) -> Option<&Type> {
        self.bindings.get(name).map(|(ty, _)| ty)
    }

    fn lookup_kind(&self, name: &str) -> Option<BindingKind> {
        self.bindings.get(name).map(|(_, k)| *k)
    }

    fn bind(&mut self, name: String, ty: Type, kind: BindingKind) {
        self.bindings.insert(name, (ty, kind));
    }
}

#[derive(Debug, Clone)]
pub struct ParamSig {
    pub name: String,
    pub ty: Type,
    pub has_default: bool,
}

#[derive(Debug, Clone)]
pub struct FnSig {
    pub params: Vec<ParamSig>,
    pub return_type: Type,
}

pub type FnEnv = HashMap<String, FnSig>;

#[derive(Debug, Clone)]
pub struct StructSig {
    /// Field name and type, in declaration order. Order matters
    /// because struct construction accepts positional args.
    pub fields: Vec<(String, Type)>,
}

pub type StructEnv = HashMap<String, StructSig>;

/// Symbols imported into a module from elsewhere.
///
/// Resolved by the module loader (user `from … use …` items plus the
/// always-implicit stdlib registry) before the checker runs and merged
/// into the module's own scope; collisions with local declarations
/// surface as ordinary duplicate-name errors.
#[derive(Debug, Default, Clone)]
pub struct ImportedSymbols {
    pub fns: FnEnv,
    pub vals: HashMap<String, Type>,
    /// Named types in scope. Maps the imported name (e.g. `Symlink`)
    /// to the canonical [`Type`] variant. The module loader rewrites
    /// every `Type::Named(name)` in the program against this map
    /// before invoking the checker.
    pub types: HashMap<String, Type>,
    /// Subset of [`Self::fns`] / [`Self::types`] keys that come from
    /// the implicit stdlib registry rather than a user `from … use …`
    /// item. The duplicate-name check uses this to emit a clearer
    /// "is a builtin and cannot be redefined" diagnostic when a user
    /// declaration shadows a stdlib name.
    pub builtins: HashSet<String>,
}

#[derive(Clone, Copy)]
enum ItemKind {
    Val,
    Fn,
}

/// Rewrite every `Type::Named(name)` in `program` to its canonical
/// variant, in place.
///
/// Resolution sees both `imported.types` and the program's own
/// `struct` / `type` declarations; locally-declared names take
/// precedence in case of a shadowing collision (the duplicate-name
/// check in pass 1 will reject the shadow separately).
///
/// # Errors
/// Returns one [`Diagnostic`] per unresolved type name. The program
/// is left in a partially-resolved state on error — callers should
/// not run further passes against it.
pub fn resolve_type_names(
    program: &mut Program,
    imported: &ImportedSymbols,
) -> Result<(), Vec<Diagnostic>> {
    let mut diags = Vec::new();
    let local_types = collect_local_types(program);
    let scope = TypeResolutionScope {
        local: &local_types,
        imported,
    };
    for item in &mut program.items {
        match item {
            Item::Val(v) => {
                if let Some(annot) = &mut v.ty {
                    resolve_type_in_place(
                        &mut annot.node,
                        &annot.span,
                        &scope,
                        &mut diags,
                        &mut Vec::new(),
                    );
                }
                resolve_expr_types(&mut v.value.node, &scope, &mut diags);
            }
            Item::Fn(f) => {
                for p in &mut f.params {
                    resolve_type_in_place(
                        &mut p.ty.node,
                        &p.ty.span,
                        &scope,
                        &mut diags,
                        &mut Vec::new(),
                    );
                    if let Some(default) = &mut p.default {
                        resolve_expr_types(&mut default.node, &scope, &mut diags);
                    }
                }
                resolve_type_in_place(
                    &mut f.return_type.node,
                    &f.return_type.span,
                    &scope,
                    &mut diags,
                    &mut Vec::new(),
                );
                resolve_block_types(&mut f.body, &scope, &mut diags);
            }
            Item::Struct(s) => {
                for field in &mut s.fields {
                    resolve_type_in_place(
                        &mut field.ty.node,
                        &field.ty.span,
                        &scope,
                        &mut diags,
                        &mut Vec::new(),
                    );
                }
            }
            Item::Reconcile(r) => resolve_reconcile_types(r, &scope, &mut diags),
            Item::ExprStmt(expr) => resolve_expr_types(&mut expr.node, &scope, &mut diags),
            Item::TypeAlias(_) | Item::Use(_) => {}
        }
    }
    if diags.is_empty() { Ok(()) } else { Err(diags) }
}

/// Locally-declared type names from `program.items`. Used during
/// type-name resolution alongside [`ImportedSymbols::types`].
fn collect_local_types(program: &Program) -> HashMap<String, Type> {
    let mut out: HashMap<String, Type> = HashMap::new();
    for item in &program.items {
        match item {
            Item::Struct(s) => {
                let fields = s
                    .fields
                    .iter()
                    .map(|f| (f.name.node.clone(), f.ty.node.clone()))
                    .collect();
                out.insert(
                    s.name.node.clone(),
                    Type::Struct {
                        name: s.name.node.clone(),
                        fields,
                    },
                );
            }
            Item::TypeAlias(t) => {
                let variants = t.variants.iter().map(|v| v.node.clone()).collect();
                out.insert(
                    t.name.node.clone(),
                    Type::StringUnion {
                        name: t.name.node.clone(),
                        variants,
                    },
                );
            }
            _ => {}
        }
    }
    out
}

struct TypeResolutionScope<'a> {
    local: &'a HashMap<String, Type>,
    imported: &'a ImportedSymbols,
}

impl TypeResolutionScope<'_> {
    fn lookup(&self, name: &str) -> Option<&Type> {
        // Locals take precedence: when a user shadows an imported
        // name, the duplicate-name check in pass 1 reports it; here
        // we still resolve to the local definition to keep the rest
        // of the diagnostics coherent.
        self.local
            .get(name)
            .or_else(|| self.imported.types.get(name))
    }
}

fn resolve_block_types(
    block: &mut Block,
    scope: &TypeResolutionScope<'_>,
    diags: &mut Vec<Diagnostic>,
) {
    for stmt in &mut block.stmts {
        match stmt {
            Stmt::Val(v) => {
                if let Some(annot) = &mut v.ty {
                    resolve_type_in_place(
                        &mut annot.node,
                        &annot.span,
                        scope,
                        diags,
                        &mut Vec::new(),
                    );
                }
                resolve_expr_types(&mut v.value.node, scope, diags);
            }
            Stmt::Reconcile(r) => resolve_reconcile_types(r, scope, diags),
        }
    }
    if let Some(trailing) = &mut block.trailing {
        resolve_expr_types(&mut trailing.node, scope, diags);
    }
}

fn resolve_reconcile_types(
    reconcile: &mut ReconcileDecl,
    scope: &TypeResolutionScope<'_>,
    diags: &mut Vec<Diagnostic>,
) {
    for expr in reconcile.chains.iter_mut().flatten() {
        resolve_expr_types(&mut expr.node, scope, diags);
    }
}

fn resolve_expr_types(
    expr: &mut Expr,
    scope: &TypeResolutionScope<'_>,
    diags: &mut Vec<Diagnostic>,
) {
    match expr {
        Expr::Unary { operand, .. } => {
            resolve_expr_types(&mut operand.node, scope, diags);
        }
        Expr::Binary { lhs, rhs, .. } => {
            resolve_expr_types(&mut lhs.node, scope, diags);
            resolve_expr_types(&mut rhs.node, scope, diags);
        }
        Expr::Interpolation(parts) => {
            for part in parts {
                if let StringPart::Expr(expr) = part {
                    resolve_expr_types(&mut expr.node, scope, diags);
                }
            }
        }
        Expr::List(items) => {
            for item in items {
                resolve_expr_types(&mut item.node, scope, diags);
            }
        }
        Expr::Map(entries) => {
            for entry in entries {
                resolve_expr_types(&mut entry.key.node, scope, diags);
                resolve_expr_types(&mut entry.value.node, scope, diags);
            }
        }
        Expr::Call { args, .. } => {
            for arg in args {
                resolve_expr_types(&mut arg.value.node, scope, diags);
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            resolve_expr_types(&mut cond.node, scope, diags);
            resolve_block_types(then_branch, scope, diags);
            resolve_block_types(else_branch, scope, diags);
        }
        Expr::For {
            iter_expr, body, ..
        } => {
            resolve_expr_types(&mut iter_expr.node, scope, diags);
            resolve_block_types(body, scope, diags);
        }
        Expr::Field { receiver, .. } => {
            resolve_expr_types(&mut receiver.node, scope, diags);
        }
        Expr::Match { scrutinee, arms } => {
            resolve_expr_types(&mut scrutinee.node, scope, diags);
            for arm in arms {
                resolve_expr_types(&mut arm.body.node, scope, diags);
            }
        }
        Expr::Literal(_) | Expr::Var(_) => {}
    }
}

fn resolve_type_in_place(
    ty: &mut Type,
    span: &Span,
    scope: &TypeResolutionScope<'_>,
    diags: &mut Vec<Diagnostic>,
    stack: &mut Vec<String>,
) {
    match ty {
        Type::Named(name) => match scope.lookup(name) {
            Some(canonical) => {
                if stack.iter().any(|n| n == name) {
                    diags.push(Diagnostic::new(
                        span.clone(),
                        format!("recursive type `{name}` is not supported"),
                    ));
                    return;
                }
                // Replace, then recurse: a struct payload pulled from
                // `local_types` may carry `Type::Named` placeholders
                // inside its field types (the local-type map is built
                // from the raw AST before we've walked anything).
                // Resolving those eagerly here keeps the val
                // annotation's payload structurally identical to the
                // one synthesised for the struct's constructor.
                stack.push(name.clone());
                *ty = canonical.clone();
                resolve_type_in_place(ty, span, scope, diags, stack);
                stack.pop();
            }
            None => diags.push(Diagnostic::new(
                span.clone(),
                format!("unknown type `{name}`"),
            )),
        },
        Type::List(inner) | Type::Nullable(inner) => {
            resolve_type_in_place(inner, span, scope, diags, stack);
        }
        Type::Map(k, v) => {
            resolve_type_in_place(k, span, scope, diags, stack);
            resolve_type_in_place(v, span, scope, diags, stack);
        }
        Type::Struct { fields, .. } => {
            for (_, fty) in fields {
                resolve_type_in_place(fty, span, scope, diags, stack);
            }
        }
        Type::StringUnion { .. }
        | Type::String
        | Type::Int
        | Type::Boolean
        | Type::Double
        | Type::Symlink
        | Type::Template
        | Type::Resource
        | Type::Secret
        | Type::Package
        | Type::Void
        | Type::Null => {}
    }
}

/// Type-check a program with no imported symbols.
///
/// Convenience wrapper around [`check_module`]; useful for
/// parser/checker unit tests where no module loader is involved.
/// Programs that contain `use` items will fail with "unknown
/// function/name" errors when called through this entry point —
/// production callers should go through the module loader and use
/// [`check_module`] instead.
///
/// # Errors
/// Returns one [`Diagnostic`] per type problem; a sub-expression
/// error short-circuits the rest of *that* declaration but sibling
/// items are still checked.
pub fn check(program: &Program) -> Result<(), Vec<Diagnostic>> {
    check_module(program, &ImportedSymbols::default())
}

/// Type-check a single module against pre-resolved imported symbols.
///
/// The loader populates `imported` from this module's `use` items;
/// the checker does not look at `Item::Use` itself beyond skipping
/// it in both passes.
///
/// # Errors
/// See [`check`].
pub fn check_module(program: &Program, imported: &ImportedSymbols) -> Result<(), Vec<Diagnostic>> {
    let mut diags = Vec::new();

    let mut top_names: HashMap<String, ItemKind> = HashMap::new();
    let mut fn_env: FnEnv = imported.fns.clone();
    for name in fn_env.keys() {
        top_names.insert(name.clone(), ItemKind::Fn);
    }
    for name in imported.vals.keys() {
        // Imported vals collide with imported fns of the same name.
        // The loader can also detect this earlier, but the checker
        // remains correct standalone.
        if top_names.contains_key(name) {
            // Best-effort: no span available here. Report against the
            // first local item that collides, or skip — keep silent
            // and rely on local collision below.
            continue;
        }
        top_names.insert(name.clone(), ItemKind::Val);
    }
    for item in &program.items {
        match item {
            Item::Val(v) => {
                if top_names.contains_key(&v.name.node) {
                    diags.push(Diagnostic::new(
                        v.name.span.clone(),
                        redefine_message(&v.name.node, imported),
                    ));
                } else {
                    top_names.insert(v.name.node.clone(), ItemKind::Val);
                }
            }
            Item::Fn(f) => {
                if top_names.contains_key(&f.name.node) {
                    diags.push(Diagnostic::new(
                        f.name.span.clone(),
                        redefine_message(&f.name.node, imported),
                    ));
                    continue;
                }
                if let Some(sig) = build_sig(f, &mut diags) {
                    top_names.insert(f.name.node.clone(), ItemKind::Fn);
                    fn_env.insert(f.name.node.clone(), sig);
                }
            }
            Item::Struct(s) => {
                if top_names.contains_key(&s.name.node) {
                    diags.push(Diagnostic::new(
                        s.name.span.clone(),
                        redefine_message(&s.name.node, imported),
                    ));
                    continue;
                }
                if let Some(sig) = build_struct_sig(s, &mut diags) {
                    top_names.insert(s.name.node.clone(), ItemKind::Fn);
                    fn_env.insert(s.name.node.clone(), sig);
                }
            }
            Item::TypeAlias(t) => {
                if top_names.contains_key(&t.name.node) {
                    diags.push(Diagnostic::new(
                        t.name.span.clone(),
                        redefine_message(&t.name.node, imported),
                    ));
                    continue;
                }
                validate_type_alias(t, &mut diags);
                top_names.insert(t.name.node.clone(), ItemKind::Val);
            }
            Item::Use(_) | Item::Reconcile(_) | Item::ExprStmt(_) => {}
        }
    }

    let mut val_env = Env::default();
    for (name, ty) in &imported.vals {
        val_env.bind(name.clone(), ty.clone(), BindingKind::OuterVal);
    }
    for item in &program.items {
        match item {
            Item::Use(_) | Item::Struct(_) | Item::TypeAlias(_) => {}
            Item::Val(v) => check_val_decl(v, &mut val_env, &fn_env, &mut diags),
            Item::Fn(f) => check_fn_decl(f, &val_env, &fn_env, &mut diags),
            Item::Reconcile(r) => check_reconcile_decl(r, &val_env, &fn_env, &mut diags),
            Item::ExprStmt(e) => check_top_expr_stmt(e, &val_env, &fn_env, &mut diags),
        }
    }

    if diags.is_empty() { Ok(()) } else { Err(diags) }
}

fn redefine_message(name: &str, imported: &ImportedSymbols) -> String {
    if imported.builtins.contains(name) {
        format!("`{name}` is a builtin and cannot be redefined")
    } else {
        format!("`{name}` is already defined")
    }
}

/// Synthesise a [`FnSig`] for a struct's implicit constructor: each
/// field becomes a positional parameter (no defaults) in declared
/// order, returning `Type::Struct{...}`. Returns `None` when the
/// struct has duplicate field names — that's reported via `diags`.
fn build_struct_sig(s: &StructDecl, diags: &mut Vec<Diagnostic>) -> Option<FnSig> {
    let mut params = Vec::with_capacity(s.fields.len());
    let mut field_pairs: Vec<(String, Type)> = Vec::with_capacity(s.fields.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut ok = true;
    for field in &s.fields {
        validate_type_annotation(&field.ty, diags);
        if !seen.insert(field.name.node.clone()) {
            diags.push(Diagnostic::new(
                field.name.span.clone(),
                format!("duplicate field `{}`", field.name.node),
            ));
            ok = false;
        }
        params.push(ParamSig {
            name: field.name.node.clone(),
            ty: field.ty.node.clone(),
            has_default: false,
        });
        field_pairs.push((field.name.node.clone(), field.ty.node.clone()));
    }
    if ok {
        Some(FnSig {
            params,
            return_type: Type::Struct {
                name: s.name.node.clone(),
                fields: field_pairs,
            },
        })
    } else {
        None
    }
}

/// Reject a string-union alias that has no variants or carries
/// duplicates. Empty / duplicate variants would silently accept any /
/// fewer strings than the user intended.
fn validate_type_alias(t: &TypeAliasDecl, diags: &mut Vec<Diagnostic>) {
    if t.variants.is_empty() {
        diags.push(Diagnostic::new(
            t.span.clone(),
            format!(
                "type alias `{}` must have at least one variant",
                t.name.node
            ),
        ));
        return;
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for v in &t.variants {
        if !seen.insert(v.node.as_str()) {
            diags.push(Diagnostic::new(
                v.span.clone(),
                format!("duplicate variant `\"{}\"`", v.node),
            ));
        }
    }
}

fn build_sig(f: &FnDecl, diags: &mut Vec<Diagnostic>) -> Option<FnSig> {
    let mut params = Vec::with_capacity(f.params.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut seen_default = false;
    let mut ok = true;
    for p in &f.params {
        validate_type_annotation(&p.ty, diags);
        if !seen.insert(p.name.node.clone()) {
            diags.push(Diagnostic::new(
                p.name.span.clone(),
                format!("duplicate parameter `{}`", p.name.node),
            ));
            ok = false;
        }
        let has_default = p.default.is_some();
        if !has_default && seen_default {
            diags.push(Diagnostic::new(
                p.span.clone(),
                "required parameters must come before defaulted parameters",
            ));
            ok = false;
        }
        if has_default {
            seen_default = true;
        }
        params.push(ParamSig {
            name: p.name.node.clone(),
            ty: p.ty.node.clone(),
            has_default,
        });
    }
    validate_type_annotation(&f.return_type, diags);
    if ok {
        Some(FnSig {
            params,
            return_type: f.return_type.node.clone(),
        })
    } else {
        None
    }
}

fn check_val_decl(v: &ValDecl, env: &mut Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    if let Some(annot) = &v.ty {
        validate_type_annotation(annot, diags);
    }
    let bind_ty: Option<Type> = match &v.ty {
        Some(annot) => {
            if let Err(d) = check_expr(&v.value, &annot.node, env, fns) {
                diags.push(d);
            }
            Some(annot.node.clone())
        }
        None => match expr_type(&v.value, env, fns) {
            Ok(t) => Some(t),
            Err(d) => {
                diags.push(d);
                None
            }
        },
    };
    if let Some(t) = bind_ty {
        env.bind(v.name.node.clone(), t, BindingKind::OuterVal);
    }
}

fn check_fn_decl(f: &FnDecl, outer_env: &Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    // Intrinsics have no body — their signature was validated in
    // pass 1 and the evaluator dispatches on the intrinsic tag, so
    // there's nothing further to check here.
    if f.intrinsic.is_some() {
        return;
    }
    let mut scope = Env::default();
    for (name, (ty, _)) in &outer_env.bindings {
        scope.bind(name.clone(), ty.clone(), BindingKind::OuterVal);
    }

    let mut seen_param: HashSet<String> = HashSet::new();
    for p in &f.params {
        check_param_default(p, &scope, fns, diags);
        // Reject only intra-param duplicates here; sig-pass already
        // reported, but we still want to skip rebinding to keep the
        // first param's type authoritative.
        if seen_param.insert(p.name.node.clone()) {
            scope.bind(p.name.node.clone(), p.ty.node.clone(), BindingKind::Param);
        }
    }

    check_top_block(&f.body, &f.return_type.node, scope, fns, diags);
}

fn check_param_default(p: &Param, env: &Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    if let Some(default) = &p.default
        && let Err(d) = check_expr(default, &p.ty.node, env, fns)
    {
        diags.push(d);
    }
}

/// Top-level block check, used for fn bodies and (with `Type::Void`)
/// for top-level expression statements that span a block. Processes
/// every statement in source order — collecting diagnostics rather
/// than short-circuiting — then bidirectionally checks the trailing
/// expression against `expected`. When the trailing is absent, the
/// block has type `Void`; if `expected` is anything else, that's an
/// error.
fn check_top_block(
    body: &Block,
    expected: &Type,
    mut env: Env,
    fns: &FnEnv,
    diags: &mut Vec<Diagnostic>,
) {
    process_block_stmts_collecting(&body.stmts, &mut env, fns, diags);
    check_block_trailing(body, expected, &env, fns, diags);
}

fn check_block_trailing(
    body: &Block,
    expected: &Type,
    env: &Env,
    fns: &FnEnv,
    diags: &mut Vec<Diagnostic>,
) {
    match &body.trailing {
        Some(expr) => {
            if let Err(d) = check_expr(expr, expected, env, fns) {
                diags.push(d);
            }
        }
        None => {
            if !matches!(expected, Type::Void) {
                diags.push(Diagnostic::new(
                    body.span.clone(),
                    format!(
                        "expected `{expected}`, found block with no trailing expression (type `Void`)"
                    ),
                ));
            }
        }
    }
}

/// Process a block's statements, mutating `env` with each new local
/// binding, and pushing one diagnostic per problem encountered. Used
/// where we want to keep checking later statements even when an
/// earlier one fails (top-level fn bodies and top-level expression
/// statements that wrap blocks).
fn process_block_stmts_collecting(
    stmts: &[Stmt],
    env: &mut Env,
    fns: &FnEnv,
    diags: &mut Vec<Diagnostic>,
) {
    for stmt in stmts {
        match stmt {
            Stmt::Val(v) => check_local_val_collecting(v, env, fns, diags),
            Stmt::Reconcile(r) => check_reconcile_decl(r, env, fns, diags),
        }
    }
}

fn check_local_val_collecting(
    binding: &ValDecl,
    env: &mut Env,
    fns: &FnEnv,
    diags: &mut Vec<Diagnostic>,
) {
    if let Some(kind) = env.lookup_kind(&binding.name.node) {
        let what = match kind {
            BindingKind::Param => "parameter",
            BindingKind::OuterVal => "outer `val`",
            BindingKind::BodyLocal => "previous body `val`",
        };
        diags.push(Diagnostic::new(
            binding.name.span.clone(),
            format!(
                "`{}` is already defined as a {what} in this scope",
                binding.name.node
            ),
        ));
        return;
    }
    if let Some(annot) = &binding.ty {
        validate_type_annotation(annot, diags);
    }
    let bind_ty: Option<Type> = match &binding.ty {
        Some(annot) => {
            if let Err(d) = check_expr(&binding.value, &annot.node, env, fns) {
                diags.push(d);
            }
            Some(annot.node.clone())
        }
        None => match expr_type(&binding.value, env, fns) {
            Ok(t) => Some(t),
            Err(d) => {
                diags.push(d);
                None
            }
        },
    };
    if let Some(t) = bind_ty {
        env.bind(binding.name.node.clone(), t, BindingKind::BodyLocal);
    }
}

/// Single-error block check used inside expression-typing recursion
/// (for `if`-branches embedded inside other expressions). Mirrors the
/// "first error wins" behavior of [`expr_type`] / [`check_expr`].
fn check_block(block: &Block, expected: &Type, env: &Env, fns: &FnEnv) -> Result<(), Diagnostic> {
    let mut local = env.clone();
    process_block_stmts_strict(&block.stmts, &mut local, fns)?;
    block.trailing.as_ref().map_or_else(
        || {
            if matches!(expected, Type::Void) {
                Ok(())
            } else {
                Err(Diagnostic::new(
                    block.span.clone(),
                    format!(
                        "expected `{expected}`, found block with no trailing expression (type `Void`)"
                    ),
                ))
            }
        },
        |expr| check_expr(expr, expected, &local, fns),
    )
}

/// Single-error block synthesis used in the same recursive contexts.
fn block_type(block: &Block, env: &Env, fns: &FnEnv) -> Result<Type, Diagnostic> {
    let mut local = env.clone();
    process_block_stmts_strict(&block.stmts, &mut local, fns)?;
    block
        .trailing
        .as_ref()
        .map_or(Ok(Type::Void), |expr| expr_type(expr, &local, fns))
}

fn process_block_stmts_strict(
    stmts: &[Stmt],
    env: &mut Env,
    fns: &FnEnv,
) -> Result<(), Diagnostic> {
    for stmt in stmts {
        match stmt {
            Stmt::Val(v) => check_local_val_strict(v, env, fns)?,
            Stmt::Reconcile(r) => {
                for step in r.chains.iter().flatten() {
                    let ty = expr_type(step, env, fns)?;
                    if !is_reconcilable(&ty) {
                        return Err(Diagnostic::new(
                            step.span.clone(),
                            format!(
                                "`reconcile` expects a resource or list of resources, found `{ty}`"
                            ),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn check_local_val_strict(binding: &ValDecl, env: &mut Env, fns: &FnEnv) -> Result<(), Diagnostic> {
    if let Some(kind) = env.lookup_kind(&binding.name.node) {
        let what = match kind {
            BindingKind::Param => "parameter",
            BindingKind::OuterVal => "outer `val`",
            BindingKind::BodyLocal => "previous body `val`",
        };
        return Err(Diagnostic::new(
            binding.name.span.clone(),
            format!(
                "`{}` is already defined as a {what} in this scope",
                binding.name.node
            ),
        ));
    }
    let ty = match &binding.ty {
        Some(annot) => {
            check_expr(&binding.value, &annot.node, env, fns)?;
            annot.node.clone()
        }
        None => expr_type(&binding.value, env, fns)?,
    };
    env.bind(binding.name.node.clone(), ty, BindingKind::BodyLocal);
    Ok(())
}

/// Top-level expression statement: must have type `Void`.
fn check_top_expr_stmt(e: &Spanned<Expr>, env: &Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    if let Err(d) = check_expr(e, &Type::Void, env, fns) {
        diags.push(d);
    }
}

/// Checking-mode judgment: verify `e` has type `expected`.
fn check_expr(
    e: &Spanned<Expr>,
    expected: &Type,
    env: &Env,
    fns: &FnEnv,
) -> Result<(), Diagnostic> {
    // String literal targeted at a `StringUnion` slot: admit only
    // when the literal is in the variant set. The reverse — assigning
    // a `String`-typed variable to a union slot — is rejected by
    // `is_subtype` (no auto-narrowing).
    if let (
        Expr::Literal(Literal::String(s)),
        Type::StringUnion {
            name: union_name,
            variants,
        },
    ) = (&e.node, expected)
    {
        return if variants.iter().any(|v| v == s) {
            Ok(())
        } else {
            Err(Diagnostic::new(
                e.span.clone(),
                format!(
                    "`\"{s}\"` is not a variant of `{union_name}` (expected one of {})",
                    format_variants(variants)
                ),
            ))
        };
    }
    match &e.node {
        Expr::List(items) => match expected {
            Type::List(elem_ty) => {
                for item in items {
                    check_expr(item, elem_ty, env, fns)?;
                }
                Ok(())
            }
            _ if items.is_empty() => Err(Diagnostic::new(
                e.span.clone(),
                format!("type mismatch: expected `{expected}`, found empty list"),
            )),
            _ => switch_to_synth(e, expected, env, fns),
        },
        Expr::Map(entries) => match expected {
            Type::Map(key_ty, value_ty) => {
                for entry in entries {
                    check_expr(&entry.key, key_ty, env, fns)?;
                    check_expr(&entry.value, value_ty, env, fns)?;
                }
                reject_duplicate_static_map_keys(entries)?;
                Ok(())
            }
            _ if entries.is_empty() => Err(Diagnostic::new(
                e.span.clone(),
                format!("type mismatch: expected `{expected}`, found empty map"),
            )),
            _ => switch_to_synth(e, expected, env, fns),
        },
        Expr::Binary {
            op: BinOp::Concat,
            lhs,
            rhs,
        } if matches!(expected, Type::List(_)) => {
            check_expr(lhs, expected, env, fns)?;
            check_expr(rhs, expected, env, fns)?;
            Ok(())
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            check_expr(cond, &Type::Boolean, env, fns)?;
            check_block(then_branch, expected, env, fns)?;
            check_block(else_branch, expected, env, fns)?;
            Ok(())
        }
        _ => switch_to_synth(e, expected, env, fns),
    }
}

fn switch_to_synth(
    e: &Spanned<Expr>,
    expected: &Type,
    env: &Env,
    fns: &FnEnv,
) -> Result<(), Diagnostic> {
    let got = expr_type(e, env, fns)?;
    if is_subtype(&got, expected) {
        Ok(())
    } else {
        Err(Diagnostic::new(
            e.span.clone(),
            format!("type mismatch: expected `{expected}`, found `{got}`"),
        ))
    }
}

/// Subtyping judgment used wherever a synthesised type meets an
/// expected type. Reflexive on every kind. The only non-trivial rule
/// is one-way: `Symlink|Template|Package <: Resource`. List subtyping
/// is covariant in the element type so `List<Symlink> <: List<Resource>`.
/// `Map` stays invariant — keys and values are matched exactly. There
/// is **no auto-narrowing** — a `Resource`-typed value does not
/// satisfy a `Symlink`/`Template`/`Package` slot. Going from `Resource`
/// back to a specific kind would require an explicit construct
/// (pattern match or cast); none exists today, by design.
// The recursive arms below have structurally identical bodies after
// alpha-renaming, but each pair of bound names is a distinct semantic
// rule (resource widening vs list covariance vs the two nullable
// rules) and merging them would obscure that. Pinning `_ => is_subtype(...)`
// to a single arm would also disable the type-checked exhaustiveness
// of new `Type` variants when they're added.
#[allow(clippy::match_same_arms)]
fn is_subtype(child: &Type, parent: &Type) -> bool {
    if child == parent {
        return true;
    }
    match (child, parent) {
        // Resource singletons widen to `Resource`. String-union
        // literal sets are themselves nominal subsets of `String`.
        // The reverse direction (auto-narrowing `String` to a union
        // slot) is intentionally not allowed; assignments from a
        // string literal are admitted in `check_expr` only when the
        // literal is in the variant set. `Null` flows into any
        // nullable slot (`Null <: T?`), and reflexively into itself.
        (Type::Symlink | Type::Template | Type::Package, Type::Resource)
        | (Type::StringUnion { .. }, Type::String)
        | (Type::Null, Type::Nullable(_)) => true,
        (Type::List(c), Type::List(p)) => is_subtype(c, p),
        // Nullability: `T <: T?` (any non-nullable value fits a
        // nullable slot), and `T? <: U?` iff `T <: U`. One arm
        // handles both cases by peeling at most one `Nullable`
        // wrapper off the LHS before recursing — `T <: U` for plain
        // `T`, `is_subtype(c, p)` for `Nullable(c) <: Nullable(p)`.
        // Going the other way (using a `T?` where `T` is required)
        // must pass through `match` to extract the inhabitant and is
        // intentionally *not* expressible here.
        (other, Type::Nullable(inner)) => {
            let lhs = match other {
                Type::Nullable(c) => c.as_ref(),
                x => x,
            };
            is_subtype(lhs, inner)
        }
        _ => false,
    }
}

const fn is_resource_singleton(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Symlink | Type::Template | Type::Package | Type::Resource
    )
}

fn expr_type(e: &Spanned<Expr>, env: &Env, fns: &FnEnv) -> Result<Type, Diagnostic> {
    match &e.node {
        Expr::Literal(lit) => Ok(lit.type_of()),
        Expr::Var(name) => env
            .lookup(name)
            .cloned()
            .ok_or_else(|| Diagnostic::new(e.span.clone(), format!("unknown variable `{name}`"))),
        Expr::Unary { op, operand } => {
            let inner = expr_type(operand, env, fns)?;
            match (op, &inner) {
                (UnaryOp::Neg, Type::Int | Type::Double) => Ok(inner),
                (UnaryOp::Neg, t) => Err(Diagnostic::new(
                    e.span.clone(),
                    format!(
                        "unary `{}` requires `Int` or `Double`, found `{t}`",
                        op.symbol()
                    ),
                )),
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let lt = expr_type(lhs, env, fns)?;
            let rt = expr_type(rhs, env, fns)?;
            binop_result(*op, &lt, &rt)
                .ok_or_else(|| Diagnostic::new(e.span.clone(), binop_error(*op, &lt, &rt)))
        }
        Expr::Interpolation(parts) => {
            for part in parts {
                if let StringPart::Expr(inner) = part {
                    let ty = expr_type(inner, env, fns)?;
                    if matches!(ty, Type::Nullable(_)) {
                        return Err(Diagnostic::new(
                            inner.span.clone(),
                            format!(
                                "cannot interpolate a `{ty}` directly; `match` it to extract the inhabitant first",
                            ),
                        ));
                    }
                    if matches!(ty, Type::Secret) {
                        return Err(Diagnostic::new(
                            inner.span.clone(),
                            "cannot interpolate a `Secret` directly; call `unwrap_secret(...)` to opt into a String first"
                                .to_string(),
                        ));
                    }
                }
            }
            Ok(Type::String)
        }
        Expr::List(items) => list_type(e.span.clone(), items, env, fns),
        Expr::Map(entries) => map_type(e.span.clone(), entries, env, fns),
        Expr::Call { callee, args } => check_call(e.span.clone(), callee, args, env, fns),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => if_type(cond, then_branch, else_branch, env, fns),
        Expr::For {
            pattern,
            iter_expr,
            body,
        } => for_type(pattern, iter_expr, body, env, fns),
        Expr::Field { receiver, field } => field_type(receiver, field, env, fns),
        Expr::Match { scrutinee, arms } => match_check::match_type(scrutinee, arms, env, fns),
    }
}

/// Field-access typing: the receiver must synthesise a struct type
/// and the named field must exist on it.
fn field_type(
    receiver: &Spanned<Expr>,
    field: &Spanned<String>,
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    let recv_ty = expr_type(receiver, env, fns)?;
    match &recv_ty {
        Type::Struct {
            name: struct_name,
            fields,
        } => fields
            .iter()
            .find(|(n, _)| n == &field.node)
            .map(|(_, t)| t.clone())
            .ok_or_else(|| {
                Diagnostic::new(
                    field.span.clone(),
                    format!("unknown field `{}` on struct `{struct_name}`", field.node),
                )
            }),
        _ => Err(Diagnostic::new(
            field.span.clone(),
            format!(
                "field access requires a struct, found `{recv_ty}` for `.{}`",
                field.node
            ),
        )),
    }
}

/// Render a `StringUnion`'s variants as a backticked, comma-separated
/// list (`` `"a"`, `"b"`, `"c"` ``). Used in diagnostics that point
/// at a literal that doesn't match the variant set.
fn format_variants(variants: &[String]) -> String {
    variants
        .iter()
        .map(|v| format!("`\"{v}\"`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Type-check a `for` expression. Always synthesises [`Type::Void`].
///
/// Strict pattern↔iterable matching: list iterables only accept the
/// single-bind form, map iterables only accept the pair form. The
/// loop variable(s) bind in a fresh body scope as
/// [`BindingKind::BodyLocal`], so they may not collide with outer
/// vals, params, or other body locals (the standard "already
/// defined" error fires). The body is checked against `Void`, so any
/// value-producing trailing expression is rejected with the existing
/// block-trailing diagnostic.
fn for_type(
    pattern: &ForPattern,
    iter_expr: &Spanned<Expr>,
    body: &Block,
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    let iter_ty = expr_type(iter_expr, env, fns)?;
    let mut scope = env.clone();
    match (pattern, &iter_ty) {
        (ForPattern::Elem(name), Type::List(elem)) => {
            bind_loop_var(&mut scope, name, (**elem).clone())?;
        }
        (ForPattern::Entry { key, value }, Type::Map(k, v)) => {
            if key.node == value.node {
                return Err(Diagnostic::new(
                    value.span.clone(),
                    format!(
                        "duplicate binding `{}` in `for` pattern: key and value must be distinct",
                        value.node
                    ),
                ));
            }
            bind_loop_var(&mut scope, key, (**k).clone())?;
            bind_loop_var(&mut scope, value, (**v).clone())?;
        }
        (ForPattern::Elem(_), Type::Map(_, _)) => {
            return Err(Diagnostic::new(
                iter_expr.span.clone(),
                format!(
                    "`for x in …` expects `List<T>`; use `for (k, v) in …` to iterate `{iter_ty}`"
                ),
            ));
        }
        (ForPattern::Entry { .. }, Type::List(_)) => {
            return Err(Diagnostic::new(
                iter_expr.span.clone(),
                format!(
                    "`for (k, v) in …` expects `Map<K, V>`; use `for x in …` to iterate `{iter_ty}`"
                ),
            ));
        }
        _ => {
            return Err(Diagnostic::new(
                iter_expr.span.clone(),
                format!("`for` expects `List<T>` or `Map<K, V>`, found `{iter_ty}`"),
            ));
        }
    }
    check_block(body, &Type::Void, &scope, fns)?;
    Ok(Type::Void)
}

fn bind_loop_var(env: &mut Env, name: &Spanned<String>, ty: Type) -> Result<(), Diagnostic> {
    if let Some(kind) = env.lookup_kind(&name.node) {
        let what = match kind {
            BindingKind::Param => "parameter",
            BindingKind::OuterVal => "outer `val`",
            BindingKind::BodyLocal => "previous body `val`",
        };
        return Err(Diagnostic::new(
            name.span.clone(),
            format!(
                "`{}` is already defined as a {what} in this scope",
                name.node
            ),
        ));
    }
    env.bind(name.node.clone(), ty, BindingKind::BodyLocal);
    Ok(())
}

fn if_type(
    cond: &Spanned<Expr>,
    then_branch: &Block,
    else_branch: &Block,
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    check_expr(cond, &Type::Boolean, env, fns)?;
    let then_ty = block_type(then_branch, env, fns)?;
    let else_ty = block_type(else_branch, env, fns)?;
    if then_ty == else_ty {
        Ok(then_ty)
    } else {
        // The branch we point at depends on which side is the "implicit
        // empty Void block" (an omitted `else`); pointing at the
        // non-trailing else with span at the closing `}` is more
        // legible than at the missing token.
        let span = if else_branch.trailing.is_none() && else_branch.stmts.is_empty() {
            then_branch.span.clone()
        } else {
            else_branch.span.clone()
        };
        Err(Diagnostic::new(
            span,
            format!(
                "`if` branches have mismatched types: `then` is `{then_ty}`, `else` is `{else_ty}`"
            ),
        ))
    }
}

fn check_call(
    call_span: Span,
    callee: &Spanned<String>,
    args: &[CallArg],
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    let sig = fns.get(&callee.node).ok_or_else(|| {
        Diagnostic::new(
            callee.span.clone(),
            format!("unknown function `{}`", callee.node),
        )
    })?;

    let mut seen_named = false;
    for arg in args {
        if arg.name.is_some() {
            seen_named = true;
        } else if seen_named {
            return Err(Diagnostic::new(
                arg.span.clone(),
                "positional arguments must come before named arguments",
            ));
        }
    }

    let mut matched: Vec<Option<&CallArg>> = std::iter::repeat_n(None, sig.params.len()).collect();
    let mut pos_idx = 0usize;
    for arg in args {
        match &arg.name {
            None => {
                if pos_idx >= sig.params.len() {
                    return Err(Diagnostic::new(
                        arg.span.clone(),
                        format!(
                            "too many arguments to `{}`: expected {}",
                            callee.node,
                            sig.params.len()
                        ),
                    ));
                }
                matched[pos_idx] = Some(arg);
                pos_idx += 1;
            }
            Some(name) => {
                let pos = sig.params.iter().position(|p| p.name == name.node);
                let pos = pos.ok_or_else(|| {
                    Diagnostic::new(
                        name.span.clone(),
                        format!("`{}` has no parameter `{}`", callee.node, name.node),
                    )
                })?;
                if matched[pos].is_some() {
                    return Err(Diagnostic::new(
                        arg.span.clone(),
                        format!("parameter `{}` is already supplied", name.node),
                    ));
                }
                matched[pos] = Some(arg);
            }
        }
    }

    for (i, param) in sig.params.iter().enumerate() {
        match matched[i] {
            Some(arg) => check_expr(&arg.value, &param.ty, env, fns)?,
            None => {
                if !param.has_default {
                    return Err(Diagnostic::new(
                        call_span,
                        format!(
                            "missing required argument `{}` for `{}`",
                            param.name, callee.node
                        ),
                    ));
                }
            }
        }
    }

    Ok(sig.return_type.clone())
}

fn map_type(
    map_span: Span,
    entries: &[MapEntry],
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    let Some((first, rest)) = entries.split_first() else {
        return Err(Diagnostic::new(
            map_span,
            "cannot infer type of empty map; add a `Map<K, V>` annotation",
        ));
    };
    let key_ty = expr_type(&first.key, env, fns)?;
    if !is_valid_map_key(&key_ty) {
        return Err(Diagnostic::new(
            first.key.span.clone(),
            format!(
                "`{key_ty}` is not a valid `Map` key type; allowed keys are `String` and `Int`"
            ),
        ));
    }
    let value_ty = expr_type(&first.value, env, fns)?;
    for entry in rest {
        let k = expr_type(&entry.key, env, fns)?;
        if k != key_ty {
            return Err(Diagnostic::new(
                entry.key.span.clone(),
                format!("map key type mismatch: expected `{key_ty}`, found `{k}`"),
            ));
        }
        let v = expr_type(&entry.value, env, fns)?;
        if v != value_ty {
            return Err(Diagnostic::new(
                entry.value.span.clone(),
                format!("map value type mismatch: expected `{value_ty}`, found `{v}`"),
            ));
        }
    }
    reject_duplicate_static_map_keys(entries)?;
    Ok(Type::Map(Box::new(key_ty), Box::new(value_ty)))
}

const fn is_valid_map_key(ty: &Type) -> bool {
    matches!(ty, Type::String | Type::Int)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum StaticMapKey {
    String(String),
    Int(i64),
}

fn reject_duplicate_static_map_keys(entries: &[MapEntry]) -> Result<(), Diagnostic> {
    let mut seen: HashSet<StaticMapKey> = HashSet::new();
    for entry in entries {
        let Some(key) = static_map_key(&entry.key.node) else {
            continue;
        };
        if !seen.insert(key) {
            return Err(Diagnostic::new(
                entry.key.span.clone(),
                "duplicate static map key",
            ));
        }
    }
    Ok(())
}

fn static_map_key(expr: &Expr) -> Option<StaticMapKey> {
    match expr {
        Expr::Literal(Literal::String(s)) => Some(StaticMapKey::String(s.clone())),
        Expr::Literal(Literal::Int(n)) => Some(StaticMapKey::Int(*n)),
        _ => None,
    }
}

/// Recursively check that every `Map<K, V>` occurrence in a declared
/// type annotation has a valid key type. Errors are reported at the
/// annotation's outer span.
fn validate_type_annotation(ty: &Spanned<Type>, diags: &mut Vec<Diagnostic>) {
    walk_type(&ty.node, &ty.span, diags);
}

fn walk_type(ty: &Type, span: &Span, diags: &mut Vec<Diagnostic>) {
    match ty {
        Type::String
        | Type::Int
        | Type::Boolean
        | Type::Double
        | Type::Symlink
        | Type::Template
        | Type::Resource
        | Type::Secret
        | Type::Package
        | Type::Void
        | Type::Null
        | Type::StringUnion { .. } => {}
        Type::List(inner) | Type::Nullable(inner) => walk_type(inner, span, diags),
        Type::Map(k, v) => {
            if !is_valid_map_key(k) {
                diags.push(Diagnostic::new(
                    span.clone(),
                    format!(
                        "`{k}` is not a valid `Map` key type; allowed keys are `String` and `Int`"
                    ),
                ));
            }
            walk_type(k, span, diags);
            walk_type(v, span, diags);
        }
        // Field types were validated when the struct was built; we
        // recurse here so that a struct field used as a `Map` key
        // (e.g. `Map<MyStruct, ...>`) still surfaces its own keying
        // problems via the parent walk.
        Type::Struct { fields, .. } => {
            for (_, fty) in fields {
                walk_type(fty, span, diags);
            }
        }
        // `Named` should be resolved away by [`resolve_type_names`]
        // before the checker runs; if one slips through, surface it
        // rather than silently accepting an opaque type.
        Type::Named(name) => diags.push(Diagnostic::new(
            span.clone(),
            format!("unknown type `{name}`"),
        )),
    }
}

fn check_reconcile_decl(r: &ReconcileDecl, env: &Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    for step in r.chains.iter().flatten() {
        match expr_type(step, env, fns) {
            Ok(ty) => {
                if !is_reconcilable(&ty) {
                    diags.push(Diagnostic::new(
                        step.span.clone(),
                        format!(
                            "`reconcile` expects a resource or list of resources, found `{ty}`"
                        ),
                    ));
                }
            }
            Err(d) => diags.push(d),
        }
    }
}

const fn is_reconcilable(ty: &Type) -> bool {
    match ty {
        Type::Symlink | Type::Template | Type::Package | Type::Resource => true,
        Type::List(inner) => matches!(
            **inner,
            Type::Symlink | Type::Template | Type::Package | Type::Resource
        ),
        _ => false,
    }
}

fn list_type(
    list_span: Span,
    items: &[Spanned<Expr>],
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    let Some((first, rest)) = items.split_first() else {
        return Err(Diagnostic::new(
            list_span,
            "cannot infer type of empty list; add a `List<T>` annotation",
        ));
    };
    let mut elem_ty = expr_type(first, env, fns)?;
    for item in rest {
        let ty = expr_type(item, env, fns)?;
        if ty == elem_ty {
            continue;
        }
        // Heterogeneous resource elements lift the element type to
        // `Resource`; that is the only non-equality unification today.
        if is_resource_singleton(&ty) && is_resource_singleton(&elem_ty) {
            elem_ty = Type::Resource;
            continue;
        }
        return Err(Diagnostic::new(
            item.span.clone(),
            format!("list element type mismatch: expected `{elem_ty}`, found `{ty}`"),
        ));
    }
    Ok(Type::List(Box::new(elem_ty)))
}

fn binop_result(op: BinOp, lhs: &Type, rhs: &Type) -> Option<Type> {
    match op {
        BinOp::Add => add_result(lhs, rhs),
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Pow => numeric_result(lhs, rhs),
        BinOp::Concat => concat_result(lhs, rhs),
        BinOp::Eq | BinOp::Neq => equality_result(lhs, rhs),
        BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => ordering_result(lhs, rhs),
    }
}

const fn add_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    match (lhs, rhs) {
        (Type::String, Type::String) => Some(Type::String),
        _ => numeric_result(lhs, rhs),
    }
}

const fn numeric_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    match (lhs, rhs) {
        (Type::Int, Type::Int) => Some(Type::Int),
        (Type::Double, Type::Double | Type::Int) | (Type::Int, Type::Double) => Some(Type::Double),
        _ => None,
    }
}

fn concat_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    let (Type::List(a), Type::List(b)) = (lhs, rhs) else {
        return None;
    };
    if a == b {
        return Some(Type::List(a.clone()));
    }
    // Mirror `list_type`: heterogeneous resource-singleton elements
    // lift to `List<Resource>`. Without this, `[sym] ++ [file]` would
    // error in synthesis even though `[sym, file]` infers cleanly.
    if is_resource_singleton(a) && is_resource_singleton(b) {
        return Some(Type::List(Box::new(Type::Resource)));
    }
    None
}

const fn is_numeric_pair(lhs: &Type, rhs: &Type) -> bool {
    matches!(
        (lhs, rhs),
        (Type::Int | Type::Double, Type::Int | Type::Double)
    )
}

const fn equality_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    if is_numeric_pair(lhs, rhs) {
        return Some(Type::Boolean);
    }
    if is_string_or_union(lhs) && is_string_or_union(rhs) {
        return Some(Type::Boolean);
    }
    // Nullable equality: comparing a `T?` against `null` (or two
    // nulls) is the canonical "is it set?" idiom and is allowed even
    // though `T?` is otherwise opaque without `match`. Comparing a
    // `T?` against a non-null value is rejected so the user is
    // forced into a `match` arm where the inhabitant has type `T`.
    match (lhs, rhs) {
        (Type::Boolean, Type::Boolean)
        | (Type::Null, Type::Null | Type::Nullable(_))
        | (Type::Nullable(_), Type::Null)
        // Secrets compare to other secrets only — no String
        // cross-comparison so a leaked literal can't be probed via
        // the type system (e.g. `secret(...) == "guess"` is not
        // legal; the user has to `unwrap_secret` and own the leak).
        | (Type::Secret, Type::Secret) => Some(Type::Boolean),
        _ => None,
    }
}

const fn ordering_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    if is_numeric_pair(lhs, rhs) {
        return Some(Type::Boolean);
    }
    if is_string_or_union(lhs) && is_string_or_union(rhs) {
        return Some(Type::Boolean);
    }
    None
}

/// True for `String` and any `StringUnion`. Used by the comparison
/// operators so a value of a string-union type can be compared with
/// a plain string (`if c == "red"`) or with another union value.
const fn is_string_or_union(ty: &Type) -> bool {
    matches!(ty, Type::String | Type::StringUnion { .. })
}

fn binop_error(op: BinOp, lhs: &Type, rhs: &Type) -> String {
    let kind = match op {
        BinOp::Add | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
            "`Int`, `Double`, or `String`"
        }
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Pow => "`Int` or `Double`",
        BinOp::Concat => "matching `List<T>`",
        BinOp::Eq | BinOp::Neq => "`String`, `Int`, `Boolean`, or `Double`",
    };
    format!(
        "`{}` requires {kind} operands, found `{lhs}` and `{rhs}`",
        op.symbol()
    )
}

#[cfg(test)]
mod tests;
