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
//! to `Double` — and the same promotion applies wherever several
//! expressions share one value slot (list elements, map values, `if`
//! branches, `match` arms, `++`) and to `Int` *literals* written into
//! a `Double` slot (`val x: Double = 1`). Non-literal `Int`
//! expressions still do not flow into `Double` slots: there is no
//! general `Int <: Double` subtyping, only the join and the literal
//! admission. Every promotion records its expression span into
//! [`CheckOutput::double_promotions`] so the evaluator (which has no
//! type environment) coerces the runtime value at exactly those
//! positions.
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

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::{
    ast::{
        BinOp, Block, CallArg, Expr, FnDecl, ForPattern, Item, Literal, MapEntry, Param, Program,
        ReconcileDecl, Span, Spanned, Stmt, StringPart, StructDecl, StructLiteralField, Type,
        TypeAliasDecl, UnaryOp, ValDecl,
    },
    diagnostic::Diagnostic,
};

mod match_check;
mod overload;
mod suggest;

/// Red zone / grow slab for [`grow_stack`]. Matches the rustc / syn
/// idiom (64 KiB red zone, 1 MiB slab).
const STACK_RED_ZONE: usize = 64 * 1024;
const STACK_GROW_SLAB: usize = 1024 * 1024;

/// Run `f` on a freshly grown stack segment when the current one is
/// nearly exhausted. The checker recurses on the native stack through
/// the AST (`resolve_expr_types`, `expr_type`, `reject_reconcile_in_value_expr`),
/// so a left-deep AST from a long flat operator chain (`1 + 1 + 1 + …`,
/// which the parser builds iteratively without depth) would otherwise
/// overflow the stack and abort the process. Cheap when the stack is
/// healthy — just a pointer comparison against the red zone.
fn grow_stack<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(STACK_RED_ZONE, STACK_GROW_SLAB, f)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingKind {
    OuterVal,
    Param,
    BodyLocal,
}

#[derive(Debug, Default, Clone)]
pub(super) struct Env {
    bindings: HashMap<String, (Type, BindingKind)>,
    /// Spans of expressions whose checked type promoted a synthesized
    /// `Int` into a `Double` slot (`[1, 2.5]`, `if c { 1 } else
    /// { 2.5 }`, `val x: Double = 1`, …). The evaluator has no type
    /// environment, so this is how it learns to coerce the runtime
    /// `Value::Int` at exactly those positions — without it, `/` on a
    /// "promoted" Int would silently do integer division. Shared
    /// across clones (`Rc`) so every scope feeds one per-module table.
    promotions: Rc<RefCell<Vec<Span>>>,
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

    /// Fresh binding scope that still feeds the parent's promotion
    /// table — used where a scope starts from empty bindings (fn
    /// bodies) rather than by cloning.
    fn child_scope(&self) -> Self {
        Self {
            bindings: HashMap::new(),
            promotions: Rc::clone(&self.promotions),
        }
    }

    fn record_promotion(&self, span: &Span) {
        self.promotions.borrow_mut().push(span.clone());
    }

    /// Snapshot for speculative checking (see the nullable-narrowing
    /// probe in [`check_expr`]): a probe that fails must not leave its
    /// promotion spans behind.
    fn promotions_mark(&self) -> usize {
        self.promotions.borrow().len()
    }

    fn truncate_promotions(&self, mark: usize) {
        self.promotions.borrow_mut().truncate(mark);
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
    /// `Some(name)` when this signature is a struct's synthesized
    /// constructor. Structs are built with the brace-literal form —
    /// `check_call` uses this to reject call-syntax construction with
    /// a targeted hint, and `check_struct_literal` to find the field
    /// signature. A plain fn *returning* a struct has `None` here,
    /// which is why the marker can't be derived from `return_type`.
    pub struct_name: Option<String>,
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
                    if let Some(default) = &mut field.default {
                        resolve_expr_types(&mut default.node, &scope, &mut diags);
                    }
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

    /// Every type name in scope, sorted for deterministic
    /// nearest-name suggestions.
    fn type_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .local
            .keys()
            .chain(self.imported.types.keys())
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names
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
            Stmt::Expr(x) => resolve_expr_types(&mut x.node, scope, diags),
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
    grow_stack(|| resolve_expr_types_inner(expr, scope, diags));
}

fn resolve_expr_types_inner(
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
        Expr::StructLiteral { fields, .. } => {
            for field in fields {
                if let Some(value) = &mut field.value {
                    resolve_expr_types(&mut value.node, scope, diags);
                }
            }
        }
        Expr::Interpolation(parts) => {
            for part in parts {
                if let StringPart::Expr { expr, .. } = part {
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
                if let Some(guard) = &mut arm.guard {
                    resolve_expr_types(&mut guard.node, scope, diags);
                }
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
        Type::Named(name) => {
            if let Some(canonical) = scope.lookup(name) {
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
                let canonical = canonical.clone();
                stack.push(name.clone());
                *ty = canonical;
                resolve_type_in_place(ty, span, scope, diags, stack);
                stack.pop();
            } else {
                let mut d = Diagnostic::new(span.clone(), format!("unknown type `{name}`"));
                if let Some(sugg) = suggest::nearest(scope.type_names(), name) {
                    d = d.with_help(format!("did you mean `{sugg}`?"));
                }
                diags.push(d);
            }
        }
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
        | Type::Shell
        | Type::SshKey
        | Type::GpgKey
        | Type::Resource
        | Type::Secret
        | Type::Package
        | Type::Void
        | Type::Null
        // `Generic` is only embedded in intrinsic signatures, which
        // bypass type resolution — but if one is somehow reachable
        // (e.g. via a struct field built from a stdlib signature),
        // it's an opaque leaf for the resolver.
        | Type::Generic(_) => {}
    }
}

/// Type-check a single module against pre-resolved imported symbols.
///
/// The loader populates `imported` from this module's `use` items;
/// the checker does not look at `Item::Use` itself beyond skipping
/// it in both passes. Tests with no imports pass
/// `&ImportedSymbols::default()`.
///
/// # Errors
/// Returns one [`Diagnostic`] per type problem; a sub-expression
/// error short-circuits the rest of *that* declaration but sibling
/// items are still checked.
pub fn check_module(program: &Program, imported: &ImportedSymbols) -> Result<(), Vec<Diagnostic>> {
    check_module_full(program, imported).map(|_| ())
}

/// Byproducts of a successful check that downstream phases consume.
#[derive(Debug, Default, Clone)]
pub struct CheckOutput {
    /// `(start, end)` byte-offset spans of expressions whose checked
    /// type promoted a synthesized `Int` into a `Double` slot. The
    /// evaluator coerces the runtime value at these spans; without
    /// the table, an Int inhabiting a static `Double` (e.g. from
    /// `[1, 2.5]`) would silently take the integer-division path.
    pub double_promotions: HashSet<(usize, usize)>,
}

/// [`check_module`] plus the [`CheckOutput`] byproducts. Split so the
/// many existing callers that only care about diagnostics keep their
/// signature.
///
/// # Errors
/// Same contract as [`check_module`].
pub fn check_module_full(
    program: &Program,
    imported: &ImportedSymbols,
) -> Result<CheckOutput, Vec<Diagnostic>> {
    let mut diags = Vec::new();

    let mut top_names: HashSet<String> = HashSet::new();
    let mut fn_env: FnEnv = imported.fns.clone();
    for name in fn_env.keys() {
        top_names.insert(name.clone());
    }
    for name in imported.vals.keys() {
        top_names.insert(name.clone());
    }
    for name in imported.types.keys() {
        top_names.insert(name.clone());
    }
    for item in &program.items {
        match item {
            Item::Val(v) => {
                if top_names.contains(&v.name.node) {
                    diags.push(redefine_diagnostic(
                        v.name.span.clone(),
                        &v.name.node,
                        imported,
                    ));
                } else {
                    top_names.insert(v.name.node.clone());
                }
            }
            Item::Fn(f) => {
                if top_names.contains(&f.name.node) {
                    diags.push(redefine_diagnostic(
                        f.name.span.clone(),
                        &f.name.node,
                        imported,
                    ));
                    continue;
                }
                if let Some(sig) = build_sig(f, &mut diags) {
                    top_names.insert(f.name.node.clone());
                    fn_env.insert(f.name.node.clone(), sig);
                }
            }
            Item::Struct(s) => {
                if top_names.contains(&s.name.node) {
                    diags.push(redefine_diagnostic(
                        s.name.span.clone(),
                        &s.name.node,
                        imported,
                    ));
                    continue;
                }
                if let Some(sig) = build_struct_sig(s, &mut diags) {
                    top_names.insert(s.name.node.clone());
                    fn_env.insert(s.name.node.clone(), sig);
                }
            }
            Item::TypeAlias(t) => {
                if top_names.contains(&t.name.node) {
                    diags.push(redefine_diagnostic(
                        t.name.span.clone(),
                        &t.name.node,
                        imported,
                    ));
                    continue;
                }
                validate_type_alias(t, &mut diags);
                top_names.insert(t.name.node.clone());
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
            Item::Use(_) | Item::TypeAlias(_) => {}
            Item::Struct(s) => check_struct_decl(s, &val_env, &fn_env, &mut diags),
            Item::Val(v) => check_val_decl(v, &mut val_env, &fn_env, &mut diags),
            Item::Fn(f) => check_fn_decl(f, &val_env, &fn_env, &mut diags),
            Item::Reconcile(r) => check_reconcile_decl(r, &val_env, &fn_env, &mut diags),
            Item::ExprStmt(e) => check_top_expr_stmt(e, &val_env, &fn_env, &mut diags),
        }
    }

    if diags.is_empty() {
        let double_promotions = val_env
            .promotions
            .borrow()
            .iter()
            .map(|span| (span.start, span.end))
            .collect();
        Ok(CheckOutput { double_promotions })
    } else {
        Err(diags)
    }
}

fn redefine_diagnostic(span: Span, name: &str, imported: &ImportedSymbols) -> Diagnostic {
    if imported.builtins.contains(name) {
        Diagnostic::new(
            span,
            format!("`{name}` is a builtin and cannot be redefined"),
        )
        .with_note("builtins are implicitly in scope in every module")
        .with_help(format!("rename the declaration — e.g. `my_{name}`"))
    } else {
        Diagnostic::new(span, format!("`{name}` is already defined"))
    }
}

/// Synthesise a [`FnSig`] for a struct's implicit constructor: each
/// field becomes a positional parameter in declared order, returning
/// `Type::Struct{...}`. Fields written as `name: Type = expr` set
/// `has_default` on the corresponding `ParamSig`; the same
/// required-before-default ordering rule that applies to fn parameters
/// is enforced here. Default *expressions* are type-checked in pass 2
/// by [`check_struct_decl`], which needs the val / fn env that this
/// pass doesn't yet have.
///
/// Returns `None` when the struct has a fatal sig-level problem
/// (duplicate field names, defaulted-then-required ordering) — those
/// are reported via `diags`.
fn build_struct_sig(s: &StructDecl, diags: &mut Vec<Diagnostic>) -> Option<FnSig> {
    let mut params = Vec::with_capacity(s.fields.len());
    let mut field_pairs: Vec<(String, Type)> = Vec::with_capacity(s.fields.len());
    let mut seen: HashSet<String> = HashSet::new();
    let mut seen_default = false;
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
        let has_default = field.default.is_some();
        if !has_default && seen_default {
            diags.push(Diagnostic::new(
                field.span.clone(),
                "required fields must come before defaulted fields",
            ));
            ok = false;
        }
        if has_default {
            seen_default = true;
        }
        params.push(ParamSig {
            name: field.name.node.clone(),
            ty: field.ty.node.clone(),
            has_default,
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
            struct_name: Some(s.name.node.clone()),
        })
    } else {
        None
    }
}

/// Pass-2 hook for struct decls: type-check each defaulted field's
/// expression against the field's annotated type. Defaults see the
/// outer val / fn env only — sibling fields are *not* in scope, so
/// every default is independent (records aren't a sequence). If a
/// caller wants field-to-field derivation, they compose through
/// top-level `val` bindings before calling the constructor.
fn check_struct_decl(s: &StructDecl, outer_env: &Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    for field in &s.fields {
        if let Some(default) = &field.default {
            // Struct field defaults are value expressions too; a
            // `reconcile` here would be silently dropped at eval.
            reject_reconcile_in_value_expr(default, diags);
            if let Err(d) = check_expr(default, &field.ty.node, outer_env, fns) {
                diags.push(d);
            }
        }
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
            struct_name: None,
        })
    } else {
        None
    }
}

fn check_val_decl(v: &ValDecl, env: &mut Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    // A `val` initializer is a value-producing expression: any
    // `reconcile` nested inside it would be evaluated into a sink
    // that gets dropped (see `eval_block_value` allocating a fresh
    // sink per branch). Reject before we even synthesise types.
    reject_reconcile_in_value_expr(&v.value, diags);
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
    // `child_scope`, not `Env::default()`: the fn body's promotions
    // must land in the same per-module table.
    let mut scope = outer_env.child_scope();
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

    reject_reconcile_in_value_block(&f.body, diags);
    check_top_block(&f.body, &f.return_type.node, scope, fns, diags);
}

fn check_param_default(p: &Param, env: &Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    if let Some(default) = &p.default {
        // Param defaults are value expressions: a `reconcile` here is
        // evaluated against a throwaway sink and silently dropped, so
        // reject it like any other value position.
        reject_reconcile_in_value_expr(default, diags);
        if let Err(d) = check_expr(default, &p.ty.node, env, fns) {
            diags.push(d);
        }
    }
}

fn reject_reconcile_in_value_block(block: &Block, diags: &mut Vec<Diagnostic>) {
    for stmt in &block.stmts {
        match stmt {
            Stmt::Val(v) => reject_reconcile_in_value_expr(&v.value, diags),
            // A value-context block runs in a throwaway sink, so an
            // effect statement's reconciles are as lost as the
            // trailing expression's would be.
            Stmt::Expr(x) => reject_reconcile_in_value_expr(x, diags),
            Stmt::Reconcile(r) => diags.push(Diagnostic::new(
                r.span.clone(),
                "`reconcile` is not allowed inside a value expression; resources emitted here would be silently dropped — move it to a top-level `reconcile` or to a top-level `for` / `if` statement",
            )),
        }
    }
    if let Some(expr) = &block.trailing {
        reject_reconcile_in_value_expr(expr, diags);
    }
}

fn reject_reconcile_in_value_expr(expr: &Spanned<Expr>, diags: &mut Vec<Diagnostic>) {
    grow_stack(|| reject_reconcile_in_value_expr_inner(expr, diags));
}

fn reject_reconcile_in_value_expr_inner(expr: &Spanned<Expr>, diags: &mut Vec<Diagnostic>) {
    match &expr.node {
        Expr::Literal(_) | Expr::Var(_) => {}
        Expr::Unary { operand, .. } => reject_reconcile_in_value_expr(operand, diags),
        Expr::Binary { lhs, rhs, .. } => {
            reject_reconcile_in_value_expr(lhs, diags);
            reject_reconcile_in_value_expr(rhs, diags);
        }
        Expr::StructLiteral { fields, .. } => {
            for field in fields {
                if let Some(value) = &field.value {
                    reject_reconcile_in_value_expr(value, diags);
                }
            }
        }
        Expr::Interpolation(parts) => {
            for part in parts {
                if let StringPart::Expr { expr: inner, .. } = part {
                    reject_reconcile_in_value_expr(inner, diags);
                }
            }
        }
        Expr::List(items) => {
            for item in items {
                reject_reconcile_in_value_expr(item, diags);
            }
        }
        Expr::Map(entries) => {
            for entry in entries {
                reject_reconcile_in_value_expr(&entry.key, diags);
                reject_reconcile_in_value_expr(&entry.value, diags);
            }
        }
        Expr::Call { args, .. } => {
            for arg in args {
                reject_reconcile_in_value_expr(&arg.value, diags);
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            reject_reconcile_in_value_expr(cond, diags);
            reject_reconcile_in_value_block(then_branch, diags);
            reject_reconcile_in_value_block(else_branch, diags);
        }
        Expr::For {
            iter_expr, body, ..
        } => {
            reject_reconcile_in_value_expr(iter_expr, diags);
            reject_reconcile_in_value_block(body, diags);
        }
        Expr::Field { receiver, .. } => reject_reconcile_in_value_expr(receiver, diags),
        Expr::Match { scrutinee, arms } => {
            reject_reconcile_in_value_expr(scrutinee, diags);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    reject_reconcile_in_value_expr(guard, diags);
                }
                reject_reconcile_in_value_expr(&arm.body, diags);
            }
        }
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
            // Non-final expression statements must be Void effects.
            Stmt::Expr(x) => {
                if let Err(d) = check_expr(x, &Type::Void, env, fns) {
                    diags.push(d);
                }
            }
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
    reject_reconcile_in_value_expr(&binding.value, diags);
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
            // Non-final expression statements must be Void effects.
            Stmt::Expr(x) => check_expr(x, &Type::Void, env, fns)?,
            Stmt::Reconcile(r) => {
                for step in r.chains.iter().flatten() {
                    // Chain steps are evaluated in value position;
                    // see comment in `check_reconcile_decl`.
                    let mut sub = Vec::new();
                    reject_reconcile_in_value_expr(step, &mut sub);
                    if let Some(first) = sub.into_iter().next() {
                        return Err(first);
                    }
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
    // Same reasoning as `check_val_decl`: reconciles nested inside a
    // val initializer are evaluated into a dropped sink. Surface the
    // first such reconcile.
    let mut sub = Vec::new();
    reject_reconcile_in_value_expr(&binding.value, &mut sub);
    if let Some(first) = sub.into_iter().next() {
        return Err(first);
    }
    // Validate the annotation itself (invalid map key types, leaked
    // Named/Generic) — the collecting variant does this, and skipping
    // it here let an invalid annotation like `Map<Boolean, _>` slip
    // through in expression-nested blocks (if-branches, for bodies).
    if let Some(annot) = &binding.ty {
        let mut annot_diags = Vec::new();
        validate_type_annotation(annot, &mut annot_diags);
        if let Some(first) = annot_diags.into_iter().next() {
            return Err(first);
        }
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
    // A top-level expression statement runs in *exec-void* context
    // (`eval::exec_void_expr`): reconciles at statement level inside its
    // `if`/`for`/`match` bodies are real (routed to the plan), but a
    // reconcile in a *value* position — a condition, iterable,
    // scrutinee, guard, or `val` initializer — is evaluated into a
    // dropped sink and silently lost. Reject those. (Value contexts
    // like a `val` initializer already run this walk over their whole
    // expression; only the exec-void top level was unguarded.)
    reject_reconcile_in_exec_void_expr(e, diags);
    if let Err(d) = check_expr(e, &Type::Void, env, fns) {
        diags.push(d);
    }
}

/// Reconcile-placement walk for an expression executed in exec-void
/// context (a top-level statement, or an `if`/`for`/`match` body reached
/// from one). Mirrors [`crate::eval`]'s `exec_void_expr`: bodies recurse
/// as exec-void (reconciles allowed), while conditions / iterables /
/// scrutinees / guards are value positions (reconciles rejected).
fn reject_reconcile_in_exec_void_expr(expr: &Spanned<Expr>, diags: &mut Vec<Diagnostic>) {
    match &expr.node {
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            reject_reconcile_in_value_expr(cond, diags);
            reject_reconcile_in_exec_void_block(then_branch, diags);
            reject_reconcile_in_exec_void_block(else_branch, diags);
        }
        Expr::For {
            iter_expr, body, ..
        } => {
            reject_reconcile_in_value_expr(iter_expr, diags);
            reject_reconcile_in_exec_void_block(body, diags);
        }
        Expr::Match { scrutinee, arms } => {
            reject_reconcile_in_value_expr(scrutinee, diags);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    reject_reconcile_in_value_expr(guard, diags);
                }
                reject_reconcile_in_exec_void_expr(&arm.body, diags);
            }
        }
        // Any other trailing expression is a value evaluated for effect
        // (a `Void`-returning call, etc.) — a reconcile anywhere in it
        // is in value position.
        _ => reject_reconcile_in_value_expr(expr, diags),
    }
}

fn reject_reconcile_in_exec_void_block(block: &Block, diags: &mut Vec<Diagnostic>) {
    for stmt in &block.stmts {
        match stmt {
            // A statement-level reconcile in an exec-void block is real.
            Stmt::Reconcile(_) => {}
            Stmt::Val(v) => reject_reconcile_in_value_expr(&v.value, diags),
            // Effect statements route to the real sink at eval time
            // (`exec_void_block` gives `Stmt::Expr` the same
            // exec-void treatment as the trailing expression) — keep
            // this arm in lockstep with that one.
            Stmt::Expr(x) => reject_reconcile_in_exec_void_expr(x, diags),
        }
    }
    if let Some(trailing) = &block.trailing {
        reject_reconcile_in_exec_void_expr(trailing, diags);
    }
}

/// Checking-mode judgment: verify `e` has type `expected`.
pub(super) fn check_expr(
    e: &Spanned<Expr>,
    expected: &Type,
    env: &Env,
    fns: &FnEnv,
) -> Result<(), Diagnostic> {
    // Narrow through a single `Nullable` layer: a non-`null` expression
    // that checks against the inner type `T` also satisfies `T?`. This
    // restores literal-into-union narrowing (`val m: Mode? = "on"`) and
    // empty-container admission (`val xs: List<Int>? = []`) at nullable
    // slots — the exact-type checking arms below match only `T`, so
    // without this they widen to `String` / reject the empty container.
    // `null` itself is left to the normal path (it flows into `T?` by
    // subtyping).
    if let Type::Nullable(inner) = expected
        && !matches!(e.node, Expr::Literal(Literal::Null))
    {
        // The probe is speculative: on failure we fall through to the
        // normal path, so any promotion spans it recorded must be
        // rolled back or the evaluator would coerce a value the
        // accepted typing never promoted.
        let mark = env.promotions_mark();
        if check_expr(e, inner, env, fns).is_ok() {
            return Ok(());
        }
        env.truncate_promotions(mark);
    }
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
        return literal_variant_check(s, union_name, variants, &e.span);
    }
    // Int literal targeted at a `Double` slot: admit and record the
    // literal's span so the evaluator produces a Double there. This is
    // the literal-only companion of the numeric join — non-literal Int
    // expressions still do not flow into `Double` (no `Int <: Double`
    // subtyping), so `val xs: List<Double> = [1, 2.5]` works while
    // `val x: Double = some_int` stays an error.
    if let (Expr::Literal(Literal::Int(_)), Type::Double) = (&e.node, expected) {
        env.record_promotion(&e.span);
        return Ok(());
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
        // `match` mirrors `if` here: flow `expected` into each arm
        // body so a literal RHS narrows into a closed `StringUnion`
        // slot via the existing literal-into-union admission rule
        // (see [`check_expr`]'s top-of-function `Expr::Literal +
        // StringUnion` arm). Synthesizing first and then comparing
        // would widen each String literal to `String`, lose the
        // narrowing, and reject the assignment.
        Expr::Match { scrutinee, arms } => {
            match_check::check_match(scrutinee, arms, expected, env, fns)
        }
        // `for` produces no value; it only flows at statement position
        // where `Void` is expected. Anywhere else (val initializer,
        // match scrutinee, fn argument with a non-`Void` parameter)
        // produces a type error at check time rather than a runtime
        // `for is not a value expression` from the evaluator.
        Expr::For {
            pattern,
            iter_expr,
            body,
        } if matches!(expected, Type::Void) => {
            for_type(pattern, iter_expr, body, env, fns)?;
            Ok(())
        }
        Expr::For { .. } => Err(Diagnostic::new(
            e.span.clone(),
            format!(
                "`for` has type `Void` and does not produce a value; expected `{expected}`. Use `for` at statement position (top level, or inside a `Void`-bodied block) rather than as a value."
            ),
        )),
        // `lhs ?? rhs` in checking mode. Delegated to `check_coalesce`
        // so a literal fallback narrows into the expected slot (a
        // closed `StringUnion`) instead of being synthesised+widened.
        Expr::Binary {
            op: BinOp::Coalesce,
            lhs,
            rhs,
        } if !matches!(expected, Type::Void) => check_coalesce(lhs, rhs, expected, env, fns),
        _ => switch_to_synth(e, expected, env, fns),
    }
}

/// Checking-mode rule for `lhs ?? rhs`. Synthesize the LHS to keep the
/// "left side must be nullable" rule, then *check* the RHS against
/// `expected` so a literal fallback narrows into a closed
/// `StringUnion` slot (`maybe ?? "off"` against a union type) instead
/// of widening to `String` and being rejected — mirroring how `if` /
/// `match` arm bodies flow the expected type.
fn check_coalesce(
    lhs: &Spanned<Expr>,
    rhs: &Spanned<Expr>,
    expected: &Type,
    env: &Env,
    fns: &FnEnv,
) -> Result<(), Diagnostic> {
    let lhs_ty = expr_type(lhs, env, fns)?;
    match &lhs_ty {
        // `null ?? rhs` collapses to the RHS.
        Type::Null => check_expr(rhs, expected, env, fns),
        Type::Nullable(inner) => {
            let inhabitant = inner.as_ref();
            if !is_subtype(inhabitant, expected) {
                return Err(Diagnostic::new(
                    lhs.span.clone(),
                    format!(
                        "type mismatch: expected `{expected}`, found `{inhabitant}` (the non-null inhabitant of `{lhs_ty}`)"
                    ),
                ));
            }
            check_expr(rhs, expected, env, fns)
        }
        _ => {
            let mut d = Diagnostic::new(
                lhs.span.clone(),
                format!(
                    "`??` requires the left side to be a nullable type (`T?`), found `{lhs_ty}`"
                ),
            );
            if matches!(lhs_ty, Type::Secret) {
                d = d.with_note(
                    "secret resolution failure is a hard error by design; `secret(...)` never returns null",
                );
            }
            Err(d)
        }
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
        let mut d = Diagnostic::new(
            e.span.clone(),
            format!("type mismatch: expected `{expected}`, found `{got}`"),
        );
        // A dynamic `String` meeting a closed-union slot is the one
        // mismatch with an enumerable fix.
        if let (Type::StringUnion { variants, .. }, Type::String) = (expected, &got) {
            d = d.with_help(format!("expected one of {}", format_variants(variants)));
        }
        Err(d)
    }
}

/// Subtyping judgment used wherever a synthesised type meets an
/// expected type. Reflexive on every kind. The only non-trivial rule
/// is one-way: `Symlink|Template|Package|Shell <: Resource`. List
/// subtyping is covariant in the element type so `List<Symlink> <:
/// List<Resource>`. `Map` stays invariant — keys and values are
/// matched exactly. There is **no auto-narrowing** — a
/// `Resource`-typed value does not satisfy a
/// `Symlink`/`Template`/`Package`/`Shell` slot. Going from `Resource`
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
        (
            Type::Symlink
            | Type::Template
            | Type::Package
            | Type::Shell
            | Type::SshKey
            | Type::GpgKey,
            Type::Resource,
        )
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
        Type::Symlink
            | Type::Template
            | Type::Package
            | Type::Shell
            | Type::SshKey
            | Type::GpgKey
            | Type::Resource
    )
}

fn expr_type(e: &Spanned<Expr>, env: &Env, fns: &FnEnv) -> Result<Type, Diagnostic> {
    grow_stack(|| expr_type_inner(e, env, fns))
}

fn expr_type_inner(e: &Spanned<Expr>, env: &Env, fns: &FnEnv) -> Result<Type, Diagnostic> {
    match &e.node {
        Expr::Literal(lit) => Ok(lit.type_of()),
        Expr::Var(name) => env.lookup(name).cloned().ok_or_else(|| {
            let mut d = Diagnostic::new(e.span.clone(), format!("unknown variable `{name}`"));
            let mut in_scope: Vec<&str> = env.bindings.keys().map(String::as_str).collect();
            in_scope.sort_unstable();
            if let Some(sugg) = suggest::nearest(in_scope, name) {
                d = d.with_help(format!("did you mean `{sugg}`?"));
            }
            d
        }),
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
                (UnaryOp::Not, Type::Boolean) => Ok(Type::Boolean),
                (UnaryOp::Not, t) => Err(Diagnostic::new(
                    e.span.clone(),
                    format!("unary `!` requires `Boolean`, found `{t}`"),
                )),
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let lt = expr_type(lhs, env, fns)?;
            let rt = expr_type(rhs, env, fns)?;
            if *op == BinOp::Coalesce {
                return coalesce_result(&lt, &rt)
                    .map_err(|msg| Diagnostic::new(e.span.clone(), msg));
            }
            if matches!(op, BinOp::Eq | BinOp::Neq) {
                check_union_comparison(&e.span, &lt, lhs, &rt, rhs)?;
            }
            let result = binop_result(*op, &lt, &rt)
                .ok_or_else(|| Diagnostic::new(e.span.clone(), binop_error(*op, &lt, &rt)))?;
            // `[1] ++ [2.5]` joins element types to Double like the
            // literal `[1, 2.5]` does — record so the evaluator
            // coerces the concatenated list's Int elements.
            if *op == BinOp::Concat
                && matches!(&result, Type::List(elem) if **elem == Type::Double)
                && (matches!(&lt, Type::List(elem) if **elem == Type::Int)
                    || matches!(&rt, Type::List(elem) if **elem == Type::Int))
            {
                env.record_promotion(&e.span);
            }
            Ok(result)
        }
        Expr::Interpolation(parts) => {
            for part in parts {
                if let StringPart::Expr { expr: inner, .. } = part {
                    let ty = expr_type(inner, env, fns)?;
                    if !is_interpolable(&ty) {
                        return Err(Diagnostic::new(
                            inner.span.clone(),
                            interpolation_error(&ty),
                        ));
                    }
                }
            }
            Ok(Type::String)
        }
        Expr::List(items) => list_type(e.span.clone(), items, env, fns),
        Expr::Map(entries) => map_type(e.span.clone(), entries, env, fns),
        Expr::Call { callee, args } => check_call(e.span.clone(), callee, args, env, fns),
        Expr::StructLiteral { name, fields } => {
            check_struct_literal(&e.span, name, fields, env, fns)
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => if_type(&e.span, cond, then_branch, else_branch, env, fns),
        Expr::For { .. } => {
            // Synthesis mode means a value is expected; `for` has type
            // `Void` and produces none. The dedicated arm in
            // `check_expr` admits `for` only when the surrounding
            // context is `Void` (top-level statement, Void-bodied
            // block trailing). Anything else lands here.
            Err(Diagnostic::new(
                e.span.clone(),
                "`for` has type `Void` and does not produce a value",
            )
            .with_help(
                "use `for` at statement position rather than in a value position (match scrutinee, val initializer, argument)",
            ))
        }
        Expr::Field { receiver, field } => field_type(receiver, field, env, fns),
        Expr::Match { scrutinee, arms } => {
            match_check::match_type(&e.span, scrutinee, arms, env, fns)
        }
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

/// Shared admission rule for a string literal targeted at a closed
/// `StringUnion`: `Ok` when the literal names a variant, otherwise the
/// canonical "not a variant of" diagnostic. One implementation serves
/// the assignment path (`check_expr`), literal patterns
/// (`match_check::check_lit_pattern`), and `==`/`!=` comparisons so
/// the three sites can never drift apart.
pub(super) fn literal_variant_check(
    s: &str,
    union_name: &str,
    variants: &[String],
    span: &Span,
) -> Result<(), Diagnostic> {
    if variants.iter().any(|v| v == s) {
        Ok(())
    } else {
        let mut d = Diagnostic::new(
            span.clone(),
            format!(
                "`\"{s}\"` is not a variant of `{union_name}` (expected one of {})",
                format_variants(variants)
            ),
        );
        if let Some(sugg) = suggest::nearest(variants.iter().map(String::as_str), s) {
            d = d.with_help(format!("did you mean `\"{sugg}\"`?"));
        }
        Err(d)
    }
}

/// Wrap `inner` in a single `Nullable` layer, idempotently: an
/// already-nullable type passes through unchanged so no context ever
/// constructs the unwritable `T??`.
pub(super) fn wrap_nullable(inner: Type) -> Type {
    match inner {
        already @ Type::Nullable(_) => already,
        other => Type::Nullable(Box::new(other)),
    }
}

/// Join two types that must inhabit one value slot — list elements,
/// map values, `if` branches, `match` arm bodies, and the element
/// types of `++`. The rules are exact equality plus the single
/// non-equality unification: mixed resource singletons lift to
/// `Resource`. Returns `None` when the types can't share the slot.
///
/// Having one join keeps every "several expressions, one slot"
/// context in agreement — before it existed, `[symlink, template]`
/// inferred `List<Resource>` while the equivalent `if`/`else` pair
/// was rejected.
pub(super) fn join_types(a: &Type, b: &Type) -> Option<Type> {
    if a == b {
        return Some(a.clone());
    }
    if is_resource_singleton(a) && is_resource_singleton(b) {
        return Some(Type::Resource);
    }
    // Mixed numerics promote to `Double`, mirroring arithmetic. Top
    // level only — no recursion into containers, so `[[1], [2.5]]`
    // stays an error. Every caller that can produce this join records
    // the expression's span for the evaluator's runtime coercion.
    if join_promotes(a, b) {
        return Some(Type::Double);
    }
    None
}

/// True when joining `a` and `b` promotes an `Int` side to `Double` —
/// the callers of [`join_types`] use this to know when to record a
/// promotion span for the evaluator.
const fn join_promotes(a: &Type, b: &Type) -> bool {
    matches!(
        (a, b),
        (Type::Int, Type::Double) | (Type::Double, Type::Int)
    )
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
    if_span: &Span,
    cond: &Spanned<Expr>,
    then_branch: &Block,
    else_branch: &Block,
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    check_expr(cond, &Type::Boolean, env, fns)?;
    let then_ty = block_type(then_branch, env, fns)?;
    let else_ty = block_type(else_branch, env, fns)?;
    join_types(&then_ty, &else_ty).map_or_else(
        || {
            // The branch we point at depends on which side is the
            // "implicit empty Void block" (an omitted `else`);
            // pointing at the non-trailing else with span at the
            // closing `}` is more legible than at the missing token.
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
        },
        |joined| {
            if join_promotes(&then_ty, &else_ty) {
                // Only one branch runs; the evaluator coerces
                // whichever value arrives at this span.
                env.record_promotion(if_span);
            }
            Ok(joined)
        },
    )
}

fn check_call(
    call_span: Span,
    callee: &Spanned<String>,
    args: &[CallArg],
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    let sig = fns.get(&callee.node).ok_or_else(|| {
        let mut d = Diagnostic::new(
            callee.span.clone(),
            format!("unknown function `{}`", callee.node),
        );
        let mut known: Vec<&str> = fns.keys().map(String::as_str).collect();
        known.sort_unstable();
        if let Some(sugg) = suggest::nearest(known, &callee.node) {
            d = d.with_help(format!("did you mean `{sugg}`?"));
        }
        d
    })?;

    // Structs are constructed with the brace-literal form, not call
    // syntax — reject with the migration hint.
    if let Some(struct_name) = &sig.struct_name {
        return Err(Diagnostic::new(
            call_span,
            format!(
                "`{struct_name}` is a struct; construct it with `{struct_name} {{ field: value, … }}`"
            ),
        ));
    }

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

    // Type-directed overloads: `len` / `contains` are resolved from
    // the first argument's kind (String / List / Map), not from the
    // registry's representative generic signature. Must run before
    // the generic path so `Generic("C")` is never bound literally.
    if overload::is_collection_overload(&callee.node, sig) {
        return overload::check_collection_overload(callee, &matched, env, fns, &call_span);
    }

    // Generic-aware path: when the signature mentions `Type::Generic`
    // anywhere (set up only by stdlib intrinsics like `sort`,
    // `unique`, `get`), switch from the cheap bidirectional
    // `check_expr` mode to an inference-then-bind mode that lets us
    // resolve `T`/`K`/`V` from concrete argument types. Non-generic
    // signatures keep the existing fast path.
    let has_generics = sig.params.iter().any(|p| type_contains_generic(&p.ty))
        || type_contains_generic(&sig.return_type);
    if has_generics {
        return check_generic_call(callee, sig, &matched, env, fns, call_span);
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

/// Type a `Name { field: value, shorthand }` struct literal against
/// the struct's synthesized constructor signature. Fields may appear
/// in any order; shorthand `field` binds a same-named variable;
/// omitted fields must have declared defaults.
fn check_struct_literal(
    span: &Span,
    name: &Spanned<String>,
    fields: &[StructLiteralField],
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    let Some(sig) = fns.get(&name.node) else {
        return Err(Diagnostic::new(
            name.span.clone(),
            format!("unknown struct `{}`", name.node),
        ));
    };
    if sig.struct_name.is_none() {
        return Err(Diagnostic::new(
            name.span.clone(),
            format!(
                "`{0}` is a function, not a struct; call it with `{0}(…)`",
                name.node
            ),
        ));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for field in fields {
        if !seen.insert(field.name.node.as_str()) {
            return Err(Diagnostic::new(
                field.name.span.clone(),
                format!("field `{}` is already supplied", field.name.node),
            ));
        }
        if !sig.params.iter().any(|p| p.name == field.name.node) {
            return Err(Diagnostic::new(
                field.name.span.clone(),
                format!("struct `{}` has no field `{}`", name.node, field.name.node),
            ));
        }
    }
    for param in &sig.params {
        match fields.iter().find(|f| f.name.node == param.name) {
            Some(field) => {
                if let Some(value) = &field.value {
                    check_expr(value, &param.ty, env, fns)?;
                } else {
                    // Shorthand: check a synthesized `Var` so the
                    // lookup and type diagnostics match an explicit
                    // `f: f`.
                    let var = Spanned {
                        node: Expr::Var(field.name.node.clone()),
                        span: field.name.span.clone(),
                    };
                    check_expr(&var, &param.ty, env, fns)?;
                }
            }
            None if param.has_default => {}
            None => {
                return Err(Diagnostic::new(
                    span.clone(),
                    format!("missing field `{}` for struct `{}`", param.name, name.node),
                ));
            }
        }
    }
    Ok(sig.return_type.clone())
}

/// Generic-aware variant of the per-param check used by the
/// inference-then-bind branch above. Each present argument's type is
/// inferred independently and unified against the (possibly-generic)
/// parameter type; the resulting binding map then substitutes
/// generics in the declared return type.
fn check_generic_call(
    callee: &Spanned<String>,
    sig: &FnSig,
    matched: &[Option<&CallArg>],
    env: &Env,
    fns: &FnEnv,
    call_span: Span,
) -> Result<Type, Diagnostic> {
    let mut bindings: HashMap<String, Type> = HashMap::new();
    for (i, param) in sig.params.iter().enumerate() {
        let Some(arg) = matched[i] else {
            if !param.has_default {
                return Err(Diagnostic::new(
                    call_span,
                    format!(
                        "missing required argument `{}` for `{}`",
                        param.name, callee.node
                    ),
                ));
            }
            continue;
        };
        // Inference-mode: ask for the argument's concrete type. Empty
        // containers and other expressions that need an expected type
        // to be inferrable will fail here with their own diagnostic.
        let arg_ty = expr_type(&arg.value, env, fns)?;
        bind_generics(
            &param.ty,
            &arg_ty,
            &mut bindings,
            &arg.value.span,
            &callee.node,
        )?;
    }
    if matches!(callee.node.as_str(), "unique" | "index_of")
        && let Some(elem) = bindings.get("T")
        && !is_list_equality_comparable(elem)
    {
        return Err(Diagnostic::new(
            call_span,
            format!(
                "`{}` requires a list element type with supported equality, found `{elem}`",
                callee.node
            ),
        ));
    }
    // `sort` additionally needs a total order, which is narrower than
    // equality (no `Boolean`/`Null`/`Secret` ordering).
    if callee.node == "sort"
        && let Some(elem) = bindings.get("T")
        && !is_orderable(elem)
    {
        return Err(Diagnostic::new(
            call_span,
            format!(
                "`sort` requires an orderable list element type (`String`, `Int`, `Double`, or a string union), found `{elem}`"
            ),
        ));
    }
    Ok(substitute_generics(&sig.return_type, &bindings))
}

/// Element types `sort` can order: the same set the `<`-family
/// operators accept, plus string unions (which order as their
/// underlying strings at runtime).
const fn is_orderable(ty: &Type) -> bool {
    matches!(
        ty,
        Type::String | Type::Int | Type::Double | Type::StringUnion { .. }
    )
}

const fn is_list_equality_comparable(ty: &Type) -> bool {
    matches!(
        ty,
        Type::String
            | Type::Int
            | Type::Boolean
            | Type::Double
            | Type::Null
            | Type::Secret
            | Type::StringUnion { .. }
    )
}

fn type_contains_generic(t: &Type) -> bool {
    match t {
        Type::Generic(_) => true,
        Type::List(inner) | Type::Nullable(inner) => type_contains_generic(inner),
        Type::Map(k, v) => type_contains_generic(k) || type_contains_generic(v),
        Type::Struct { fields, .. } => fields.iter().any(|(_, t)| type_contains_generic(t)),
        Type::String
        | Type::Int
        | Type::Boolean
        | Type::Double
        | Type::Void
        | Type::Null
        | Type::Symlink
        | Type::Template
        | Type::Package
        | Type::Shell
        | Type::SshKey
        | Type::GpgKey
        | Type::Resource
        | Type::Secret
        | Type::StringUnion { .. }
        | Type::Named(_) => false,
    }
}

/// Walk `param` and `arg` in parallel, recording the binding for each
/// `Type::Generic` encountered. Repeat occurrences of the same name
/// must unify exactly — no LUB / subtype-widening — so the user gets a
/// clear diagnostic instead of a silently-broadened result type.
fn bind_generics(
    param: &Type,
    arg: &Type,
    bindings: &mut HashMap<String, Type>,
    span: &Span,
    callee: &str,
) -> Result<(), Diagnostic> {
    if let Type::Generic(name) = param {
        match bindings.get(name) {
            None => {
                bindings.insert(name.clone(), arg.clone());
                return Ok(());
            }
            // Accept the new argument when it fits the prior binding
            // via the existing subtype rule (e.g. `OsType <: String`,
            // a specific resource fits a `Resource` slot). The binding
            // itself stays at the broader prior type — no widening
            // beyond what was first inferred, matching how the rest
            // of the type checker handles invariant containers.
            Some(prior) if prior == arg || is_subtype(arg, prior) => return Ok(()),
            Some(prior) => {
                return Err(Diagnostic::new(
                    span.clone(),
                    format!(
                        "type mismatch in `{callee}`: type parameter `{name}` was inferred as `{prior}` from an earlier argument, but this argument is `{arg}`",
                    ),
                ));
            }
        }
    }
    match (param, arg) {
        (Type::List(p), Type::List(a)) | (Type::Nullable(p), Type::Nullable(a)) => {
            bind_generics(p, a, bindings, span, callee)
        }
        (Type::Map(pk, pv), Type::Map(ak, av)) => {
            bind_generics(pk, ak, bindings, span, callee)?;
            bind_generics(pv, av, bindings, span, callee)
        }
        (p, a) if is_subtype(a, p) => Ok(()),
        (p, a) => Err(Diagnostic::new(
            span.clone(),
            format!("type mismatch in `{callee}`: expected `{p}`, found `{a}`"),
        )),
    }
}

fn substitute_generics(t: &Type, bindings: &HashMap<String, Type>) -> Type {
    match t {
        // An unbound generic in the return type means the signature
        // was malformed (a return-only `T` with no parameter using
        // it). Leave it as `Generic(...)` so the calling site renders
        // it verbatim — the consequent diagnostic will be louder than
        // a silent collapse.
        Type::Generic(name) => bindings
            .get(name)
            .cloned()
            .unwrap_or_else(|| Type::Generic(name.clone())),
        Type::List(inner) => Type::List(Box::new(substitute_generics(inner, bindings))),
        // Collapse through `wrap_nullable`: binding `T` to `String?`
        // in a `T?` return (e.g. `first(xs)` on `List<String?>`) must
        // yield `String?`, not the unwritable `String??`. The two
        // absences merge — `null` then means "empty list OR the first
        // element is null" — which matches the runtime, where both
        // arrive through the same `Value::Null` channel.
        Type::Nullable(inner) => wrap_nullable(substitute_generics(inner, bindings)),
        Type::Map(k, v) => Type::Map(
            Box::new(substitute_generics(k, bindings)),
            Box::new(substitute_generics(v, bindings)),
        ),
        Type::Struct { name, fields } => Type::Struct {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|(n, t)| (n.clone(), substitute_generics(t, bindings)))
                .collect(),
        },
        _ => t.clone(),
    }
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
    let mut value_ty = expr_type(&first.value, env, fns)?;
    let mut promoted = false;
    for entry in rest {
        // Keys stay exact-equality: the valid key types (`String`,
        // `Int`) have no join, so delegating would only blur the error.
        let k = expr_type(&entry.key, env, fns)?;
        if k != key_ty {
            return Err(Diagnostic::new(
                entry.key.span.clone(),
                format!("map key type mismatch: expected `{key_ty}`, found `{k}`"),
            ));
        }
        let v = expr_type(&entry.value, env, fns)?;
        match join_types(&value_ty, &v) {
            Some(joined) => {
                promoted |= join_promotes(&value_ty, &v);
                value_ty = joined;
            }
            None => {
                return Err(Diagnostic::new(
                    entry.value.span.clone(),
                    format!("map value type mismatch: expected `{value_ty}`, found `{v}`"),
                ));
            }
        }
    }
    if promoted {
        // Evaluator coerces every Int *value* of this map to Double;
        // keys are untouched (Double is not a valid key type).
        env.record_promotion(&map_span);
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
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => match &operand.node {
            Expr::Literal(Literal::Int(n)) => n.checked_neg().map(StaticMapKey::Int),
            _ => None,
        },
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
        | Type::Shell
        | Type::SshKey
        | Type::GpgKey
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
        // `Type::Generic` lives only in intrinsic signatures and is
        // substituted away by `check_call`. A user-visible annotation
        // can't construct it (the parser produces `Type::Named` for
        // any capitalized identifier in type position), so reaching
        // here is a stdlib bug, not a user error — flag it loudly.
        Type::Generic(name) => diags.push(Diagnostic::new(
            span.clone(),
            format!("internal error: unresolved type variable `{name}`"),
        )),
    }
}

fn check_reconcile_decl(r: &ReconcileDecl, env: &Env, fns: &FnEnv, diags: &mut Vec<Diagnostic>) {
    for step in r.chains.iter().flatten() {
        // A chain step is evaluated in *value position* (its
        // `Resource`/`List<Resource>` value is what gets reconciled).
        // Any nested `Stmt::Reconcile` inside an `if`/`match`/etc.
        // branch is therefore evaluated via `eval_block_value`, which
        // allocates a local sink that gets dropped. Reject upfront.
        reject_reconcile_in_value_expr(step, diags);
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
        Type::Symlink
        | Type::Template
        | Type::Package
        | Type::Shell
        | Type::SshKey
        | Type::GpgKey
        | Type::Resource => true,
        Type::List(inner) => matches!(
            **inner,
            Type::Symlink
                | Type::Template
                | Type::Package
                | Type::Shell
                | Type::SshKey
                | Type::GpgKey
                | Type::Resource
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
    let mut promoted = false;
    for item in rest {
        let ty = expr_type(item, env, fns)?;
        match join_types(&elem_ty, &ty) {
            Some(joined) => {
                promoted |= join_promotes(&elem_ty, &ty);
                elem_ty = joined;
            }
            None => {
                return Err(Diagnostic::new(
                    item.span.clone(),
                    format!("list element type mismatch: expected `{elem_ty}`, found `{ty}`"),
                ));
            }
        }
    }
    if promoted {
        // Evaluator coerces every Int element of this list to Double.
        env.record_promotion(&list_span);
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
        BinOp::And | BinOp::Or => boolean_result(lhs, rhs),
        // `??` has its own check path (`coalesce_result`) called
        // directly from `expr_type`; it never reaches here.
        BinOp::Coalesce => None,
    }
}

/// Types whose values the evaluator's `stringify` can render into an
/// interpolation. A closed `StringUnion` is a `String` at runtime, so
/// it interpolates too. Everything else (`List`, `Map`, `Struct`,
/// resource singletons, `Void`, `Nullable`, `Secret`) is rejected at
/// check time instead of failing span-less in the evaluator.
const fn is_interpolable(ty: &Type) -> bool {
    matches!(
        ty,
        Type::String | Type::Int | Type::Boolean | Type::Double | Type::StringUnion { .. }
    )
}

/// Diagnostic for a non-stringifiable interpolation part. `Nullable`
/// and `Secret` keep their tailored guidance; everything else gets the
/// general "convert to a string first" message.
fn interpolation_error(ty: &Type) -> String {
    match ty {
        Type::Nullable(_) => format!(
            "cannot interpolate a `{ty}` directly; `match` it to extract the inhabitant first",
        ),
        Type::Secret => "cannot interpolate a `Secret` directly; call `unwrap_secret(...)` to opt into a String first"
            .to_string(),
        _ => format!(
            "cannot interpolate a `{ty}`; only `String`, `Int`, `Boolean`, and `Double` values can be interpolated — convert it to a string first (e.g. `join(xs, sep)` for a list)",
        ),
    }
}

const fn boolean_result(lhs: &Type, rhs: &Type) -> Option<Type> {
    if matches!((lhs, rhs), (Type::Boolean, Type::Boolean)) {
        Some(Type::Boolean)
    } else {
        None
    }
}

/// `??` type rule. Diverges from the regular `binop_result` shape
/// (`(T, T) -> T`) because the operator unwraps a `Nullable<T>` rather
/// than combining two same-typed operands.
///
/// Accepted forms:
///   - `null ?? x`         → type of `x` (LHS is statically null)
///   - `T? ?? T`           → `T`        (LHS unwrapped, RHS pins it)
///   - `T? ?? T?`          → `T?`       (RHS may itself be null)
///   - `T? ?? null`        → `T?`       (RHS is statically null)
///
/// Rejected when the LHS isn't nullable — that's a coding mistake, and
/// surfacing it as a type error preserves keron's "no implicit
/// promotions" stance. The error string is plain prose; callers wrap
/// it into a `Diagnostic` with the binop's span.
fn coalesce_result(lhs: &Type, rhs: &Type) -> Result<Type, String> {
    // `null ?? x` collapses to `x` — the LHS contributes no value.
    if matches!(lhs, Type::Null) {
        return Ok(rhs.clone());
    }
    let Type::Nullable(inner) = lhs else {
        return Err(format!(
            "`??` requires the left side to be a nullable type (`T?`), found `{lhs}`"
        ));
    };
    let inner_ty = inner.as_ref();
    // RHS pins the inhabitant: accept any subtype of the inner type
    // (a `StringUnion` fits its `String` inner, a specific resource
    // fits a `Resource` inner), collapsing to the wider inner type.
    if is_subtype(rhs, inner_ty) {
        return Ok(inner_ty.clone());
    }
    // RHS may itself stay nullable (`T? ?? T?`) or be statically null
    // (`T? ?? null`); the whole expression then stays nullable.
    if is_subtype(rhs, lhs) || matches!(rhs, Type::Null) {
        return Ok(lhs.clone());
    }
    Err(format!(
        "`??` requires the right side to be `{inner_ty}` or `{lhs}`, found `{rhs}`"
    ))
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
    // Mirror `list_type` exactly via the shared join: `[sym] ++ [file]`
    // lifts to `List<Resource>` just like `[sym, file]` does.
    join_types(a, b).map(|elem| Type::List(Box::new(elem)))
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
    // Plain strings order lexicographically. Closed string unions are
    // deliberately excluded: their variants are categories, not points
    // on a scale, so `os_type() < "Macos"` is a coding mistake. A
    // union value widened through an explicit `String` annotation
    // remains orderable — that annotation is the opt-in.
    if matches!((lhs, rhs), (Type::String, Type::String)) {
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

/// Extra strictness for `==`/`!=` involving a closed `StringUnion`,
/// layered on top of `equality_result`'s string admission:
///
/// - a string *literal* compared against a union must name one of its
///   variants — `os_type() == "Banana"` is statically always-false
///   and gets the same "not a variant of" error as assignment and
///   `match` arms;
/// - two distinct unions have disjoint variant spaces, so comparing
///   them (`os_type() == os_arch()`) is rejected outright.
///
/// A union compared against a *dynamic* `String` expression stays
/// legal — runtime strings (env vars, file contents) are the reason
/// the union/String comparison exists at all.
fn check_union_comparison(
    span: &Span,
    lt: &Type,
    lhs: &Spanned<Expr>,
    rt: &Type,
    rhs: &Spanned<Expr>,
) -> Result<(), Diagnostic> {
    match (lt, rt) {
        (Type::StringUnion { name: ln, .. }, Type::StringUnion { name: rn, .. }) => {
            if lt == rt {
                Ok(())
            } else {
                Err(Diagnostic::new(
                    span.clone(),
                    format!("cannot compare distinct string unions `{ln}` and `{rn}`"),
                ))
            }
        }
        (Type::StringUnion { name, variants }, _) => {
            if let Expr::Literal(Literal::String(s)) = &rhs.node {
                literal_variant_check(s, name, variants, &rhs.span)
            } else {
                Ok(())
            }
        }
        (_, Type::StringUnion { name, variants }) => {
            if let Expr::Literal(Literal::String(s)) = &lhs.node {
                literal_variant_check(s, name, variants, &lhs.span)
            } else {
                Ok(())
            }
        }
        _ => Ok(()),
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
        BinOp::And | BinOp::Or => "`Boolean`",
        // `??` builds its own message in `coalesce_result`; never reached.
        BinOp::Coalesce => "nullable left side with matching right side",
    };
    format!(
        "`{}` requires {kind} operands, found `{lhs}` and `{rhs}`",
        op.symbol()
    )
}

#[cfg(test)]
mod tests;
