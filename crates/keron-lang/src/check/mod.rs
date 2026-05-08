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

mod builtins;

use std::collections::{HashMap, HashSet};

use crate::{
    ast::{
        BinOp, Block, CallArg, Expr, FnDecl, ForPattern, Item, MapEntry, Param, Program,
        ReconcileDecl, Span, Spanned, Stmt, StringPart, Type, UnaryOp, ValDecl,
    },
    diagnostic::Diagnostic,
};

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
pub(super) struct ParamSig {
    pub(super) name: String,
    pub(super) ty: Type,
    pub(super) has_default: bool,
}

#[derive(Debug, Clone)]
pub(super) struct FnSig {
    pub(super) params: Vec<ParamSig>,
    pub(super) return_type: Type,
}

pub(super) type FnEnv = HashMap<String, FnSig>;

#[derive(Clone, Copy)]
enum ItemKind {
    Val,
    Fn,
}

/// Type-check an entire program.
///
/// # Errors
/// Returns one [`Diagnostic`] per type problem; a sub-expression
/// error short-circuits the rest of *that* declaration but sibling
/// items are still checked.
pub fn check(program: &Program) -> Result<(), Vec<Diagnostic>> {
    let mut diags = Vec::new();

    // Pass 1: collect every top-level name; reject duplicates across
    // val/fn namespaces (including builtin fns); validate fn signatures.
    let mut top_names: HashMap<String, ItemKind> = HashMap::new();
    let mut fn_env: FnEnv = builtins::builtin_fn_env();
    for name in fn_env.keys() {
        top_names.insert(name.clone(), ItemKind::Fn);
    }
    for item in &program.items {
        match item {
            Item::Val(v) => {
                if top_names.contains_key(&v.name.node) {
                    diags.push(Diagnostic::new(
                        v.name.span.clone(),
                        format!("`{}` is already defined", v.name.node),
                    ));
                } else {
                    top_names.insert(v.name.node.clone(), ItemKind::Val);
                }
            }
            Item::Fn(f) => {
                if top_names.contains_key(&f.name.node) {
                    diags.push(Diagnostic::new(
                        f.name.span.clone(),
                        format!("`{}` is already defined", f.name.node),
                    ));
                    continue;
                }
                if let Some(sig) = build_sig(f, &mut diags) {
                    top_names.insert(f.name.node.clone(), ItemKind::Fn);
                    fn_env.insert(f.name.node.clone(), sig);
                }
            }
            Item::Reconcile(_) | Item::ExprStmt(_) => {}
        }
    }

    // Pass 2: check items in source order.
    let mut val_env = Env::default();
    for item in &program.items {
        match item {
            Item::Val(v) => check_val_decl(v, &mut val_env, &fn_env, &mut diags),
            Item::Fn(f) => check_fn_decl(f, &val_env, &fn_env, &mut diags),
            Item::Reconcile(r) => check_reconcile_decl(r, &val_env, &fn_env, &mut diags),
            Item::ExprStmt(e) => check_top_expr_stmt(e, &val_env, &fn_env, &mut diags),
        }
    }

    if diags.is_empty() { Ok(()) } else { Err(diags) }
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
    // Build the param scope: start from outer vals (re-tagged as
    // OuterVal in case the caller passed something with mixed kinds),
    // then check each default in left-to-right order before binding
    // the param. Allow params to silently shadow outer vals.
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
/// is one-way: `Symlink|File|Directory <: Resource`. List subtyping
/// is covariant in the element type so `List<Symlink> <: List<Resource>`.
/// `Map` stays invariant — keys and values are matched exactly. There
/// is **no auto-narrowing** — a `Resource`-typed value does not
/// satisfy a `Symlink`/`File`/`Directory` slot. Going from `Resource`
/// back to a specific kind would require an explicit construct
/// (pattern match or cast); none exists today, by design.
fn is_subtype(child: &Type, parent: &Type) -> bool {
    if child == parent {
        return true;
    }
    match (child, parent) {
        (Type::Symlink | Type::File | Type::Directory, Type::Resource) => true,
        (Type::List(c), Type::List(p)) => is_subtype(c, p),
        _ => false,
    }
}

const fn is_resource_singleton(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Symlink | Type::File | Type::Directory | Type::Resource
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
                    expr_type(inner, env, fns)?;
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
    }
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

    // Validate ordering: all positional before all named.
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

    // Match each arg to a parameter slot.
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

    // Type-check supplied args; fail on missing required.
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
                "`{key_ty}` is not a valid `Map` key type; allowed keys are `String`, `Int`, `Boolean`"
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
    Ok(Type::Map(Box::new(key_ty), Box::new(value_ty)))
}

const fn is_valid_map_key(ty: &Type) -> bool {
    matches!(ty, Type::String | Type::Int | Type::Boolean)
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
        | Type::File
        | Type::Directory
        | Type::Resource
        | Type::Void => {}
        Type::List(inner) => walk_type(inner, span, diags),
        Type::Map(k, v) => {
            if !is_valid_map_key(k) {
                diags.push(Diagnostic::new(
                    span.clone(),
                    format!(
                        "`{k}` is not a valid `Map` key type; allowed keys are `String`, `Int`, `Boolean`"
                    ),
                ));
            }
            walk_type(k, span, diags);
            walk_type(v, span, diags);
        }
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
        Type::Symlink | Type::File | Type::Directory | Type::Resource => true,
        Type::List(inner) => matches!(
            **inner,
            Type::Symlink | Type::File | Type::Directory | Type::Resource
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
    match (lhs, rhs) {
        (Type::String, Type::String) | (Type::Boolean, Type::Boolean) => Some(Type::Boolean),
        _ => None,
    }
}

const fn ordering_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    if is_numeric_pair(lhs, rhs) {
        return Some(Type::Boolean);
    }
    match (lhs, rhs) {
        (Type::String, Type::String) => Some(Type::Boolean),
        _ => None,
    }
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
