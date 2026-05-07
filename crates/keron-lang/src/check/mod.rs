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

use std::collections::{HashMap, HashSet};

use crate::{
    ast::{
        BinOp, CallArg, Expr, FnBody, FnDecl, Item, Param, Program, Span, Spanned, StringPart,
        Type, UnaryOp, ValDecl,
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
struct ParamSig {
    name: String,
    ty: Type,
    has_default: bool,
}

#[derive(Debug, Clone)]
struct FnSig {
    params: Vec<ParamSig>,
    return_type: Type,
}

type FnEnv = HashMap<String, FnSig>;

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
    // val/fn namespaces; validate fn signatures.
    let mut top_names: HashMap<String, ItemKind> = HashMap::new();
    let mut fn_env: FnEnv = HashMap::new();
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
        }
    }

    // Pass 2: check items in source order.
    let mut val_env = Env::default();
    for item in &program.items {
        match item {
            Item::Val(v) => check_val_decl(v, &mut val_env, &fn_env, &mut diags),
            Item::Fn(f) => check_fn_decl(f, &val_env, &fn_env, &mut diags),
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

    check_fn_body(&f.body, &f.return_type.node, scope, fns, diags);
}

fn check_param_default(p: &Param, env: &Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    if let Some(default) = &p.default
        && let Err(d) = check_expr(default, &p.ty.node, env, fns)
    {
        diags.push(d);
    }
}

fn check_fn_body(
    body: &FnBody,
    return_ty: &Type,
    mut env: Env,
    fns: &FnEnv,
    diags: &mut Vec<Diagnostic>,
) {
    for binding in &body.bindings {
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
            continue;
        }
        let bind_ty: Option<Type> = match &binding.ty {
            Some(annot) => {
                if let Err(d) = check_expr(&binding.value, &annot.node, &env, fns) {
                    diags.push(d);
                }
                Some(annot.node.clone())
            }
            None => match expr_type(&binding.value, &env, fns) {
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
    if let Err(d) = check_expr(&body.result, return_ty, &env, fns) {
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
        Expr::Binary {
            op: BinOp::Concat,
            lhs,
            rhs,
        } if matches!(expected, Type::List(_)) => {
            check_expr(lhs, expected, env, fns)?;
            check_expr(rhs, expected, env, fns)?;
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
    if &got == expected {
        Ok(())
    } else {
        Err(Diagnostic::new(
            e.span.clone(),
            format!("type mismatch: expected `{expected}`, found `{got}`"),
        ))
    }
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
        Expr::Call { callee, args } => check_call(e.span.clone(), callee, args, env, fns),
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
    let elem_ty = expr_type(first, env, fns)?;
    for item in rest {
        let ty = expr_type(item, env, fns)?;
        if ty != elem_ty {
            return Err(Diagnostic::new(
                item.span.clone(),
                format!("list element type mismatch: expected `{elem_ty}`, found `{ty}`"),
            ));
        }
    }
    Ok(Type::List(Box::new(elem_ty)))
}

fn binop_result(op: BinOp, lhs: &Type, rhs: &Type) -> Option<Type> {
    match op {
        BinOp::Add => add_result(lhs, rhs),
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Pow => numeric_result(lhs, rhs),
        BinOp::Concat => concat_result(lhs, rhs),
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
    if let (Type::List(a), Type::List(b)) = (lhs, rhs)
        && a == b
    {
        Some(Type::List(a.clone()))
    } else {
        None
    }
}

fn binop_error(op: BinOp, lhs: &Type, rhs: &Type) -> String {
    let kind = match op {
        BinOp::Add => "`Int`, `Double`, or `String`",
        BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Pow => "`Int` or `Double`",
        BinOp::Concat => "matching `List<T>`",
    };
    format!(
        "`{}` requires {kind} operands, found `{lhs}` and `{rhs}`",
        op.symbol()
    )
}

#[cfg(test)]
mod tests;
