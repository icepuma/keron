//! `match` type-checking: pattern checking, body type uniformity, and
//! exhaustiveness. Lives in its own module to keep the parent
//! `check/mod.rs` under the file-size cap.
//!
//! The exhaustiveness rule is intentionally narrow in v1: a
//! `StringUnion` scrutinee is exhaustive when every variant appears
//! as a literal pattern (or when any catch-all arm — wildcard `_` or
//! a bind — is present). Every other scrutinee type requires a
//! catch-all arm.

use std::collections::{HashMap, HashSet};

use super::{BindingKind, Env, FnEnv, expr_type, format_variants, is_subtype};
use crate::ast::{Expr, Literal, MatchArm, Pattern, Span, Spanned, StructPatternField, Type};
use crate::diagnostic::Diagnostic;

/// Type-check a `match` expression: synth scrutinee, every arm's
/// pattern, every arm's body (uniform type), and exhaustiveness.
///
/// For a nullable scrutinee `T?`, arms after the null case has already
/// been handled see the inner type `T` instead of `T?` — so the
/// canonical idiom
///
/// ```keron
/// match maybe_name {
///   null => "anonymous",
///   n => n,        // n: T, not T?
/// }
/// ```
///
/// works. Narrowing is order-sensitive on purpose: if the user writes
/// the bind arm first, it catches null too (as any catch-all does), so
/// the bind sees `T?` and a trailing `null` arm becomes a type error.
pub(super) fn match_type(
    scrutinee: &Spanned<Expr>,
    arms: &[MatchArm],
    env: &Env,
    fns: &FnEnv,
) -> Result<Type, Diagnostic> {
    if arms.is_empty() {
        return Err(Diagnostic::new(
            scrutinee.span.clone(),
            "`match` requires at least one arm",
        ));
    }
    let scrut_ty = expr_type(scrutinee, env, fns)?;

    let mut body_ty: Option<Type> = None;
    let mut null_handled = false;
    for arm in arms {
        let arm_scrut_ty = narrowed_scrutinee(&scrut_ty, null_handled);
        let mut bindings: HashMap<String, Type> = HashMap::new();
        check_pattern(&arm.pattern, &arm_scrut_ty, &mut bindings)?;
        let mut arm_env = env.clone();
        for (n, t) in bindings {
            arm_env.bind(n, t, BindingKind::BodyLocal);
        }
        let this = expr_type(&arm.body, &arm_env, fns)?;
        body_ty = Some(match body_ty.take() {
            None => this,
            Some(prev) => unify_arm_types(&prev, &this, arm)?,
        });
        if matches!(&scrut_ty, Type::Nullable(_)) && handles_null(&arm.pattern.node) {
            null_handled = true;
        }
    }

    check_exhaustive(&scrut_ty, arms, scrutinee.span.clone())?;

    Ok(body_ty.expect("arms is non-empty so body_ty must be set"))
}

/// If we've already covered `null` in a prior arm, peel one
/// `Nullable` wrapper off so the current arm sees `T` rather than
/// `T?`. Other types pass through unchanged.
fn narrowed_scrutinee(scrut_ty: &Type, null_handled: bool) -> Type {
    match scrut_ty {
        Type::Nullable(inner) if null_handled => inner.as_ref().clone(),
        other => other.clone(),
    }
}

/// True for patterns that, when matched, would absorb a `null` value:
/// the literal `null`, a wildcard, or a bare bind. Used to flip the
/// `null_handled` flag in [`match_type`].
const fn handles_null(p: &Pattern) -> bool {
    matches!(
        p,
        Pattern::Wildcard | Pattern::Bind(_) | Pattern::Lit(Literal::Null)
    )
}

/// Pick the common type of two arms via mutual subtyping. Mirrors the
/// list-element widening done elsewhere: heterogeneous resource
/// singletons lift to `Resource` so a `match` returning a mix of
/// `Symlink` and `File` types as `Resource`.
fn unify_arm_types(prev: &Type, this: &Type, arm: &MatchArm) -> Result<Type, Diagnostic> {
    if is_subtype(this, prev) {
        return Ok(prev.clone());
    }
    if is_subtype(prev, this) {
        return Ok(this.clone());
    }
    if is_resource_singleton(prev) && is_resource_singleton(this) {
        return Ok(Type::Resource);
    }
    Err(Diagnostic::new(
        arm.body.span.clone(),
        format!("`match` arm body type `{this}` does not match earlier arm type `{prev}`"),
    ))
}

const fn is_resource_singleton(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Symlink | Type::Template | Type::Directory | Type::Resource
    )
}

/// Check `pat` against `scrut_ty` and accumulate bindings the pattern
/// introduces. The same `scrut_ty` is shared across the recursion,
/// since structurally we never narrow it further than the field
/// types of a struct destructure.
fn check_pattern(
    pat: &Spanned<Pattern>,
    scrut_ty: &Type,
    bindings: &mut HashMap<String, Type>,
) -> Result<(), Diagnostic> {
    match &pat.node {
        Pattern::Wildcard => Ok(()),
        Pattern::Bind(name) => {
            if bindings.insert(name.clone(), scrut_ty.clone()).is_some() {
                return Err(Diagnostic::new(
                    pat.span.clone(),
                    format!("duplicate binding `{name}` in pattern"),
                ));
            }
            Ok(())
        }
        Pattern::Lit(lit) => check_lit_pattern(lit, scrut_ty, &pat.span),
        Pattern::Struct { name, fields } => check_struct_pattern(name, fields, scrut_ty, bindings),
    }
}

fn check_lit_pattern(lit: &Literal, scrut_ty: &Type, span: &Span) -> Result<(), Diagnostic> {
    match (lit, scrut_ty) {
        (Literal::Int(_), Type::Int)
        | (Literal::Double(_), Type::Double)
        | (Literal::Boolean(_), Type::Boolean)
        | (Literal::String(_), Type::String)
        | (Literal::Null, Type::Null | Type::Nullable(_)) => Ok(()),
        (Literal::String(s), Type::StringUnion { name, variants }) => {
            if variants.iter().any(|v| v == s) {
                Ok(())
            } else {
                Err(Diagnostic::new(
                    span.clone(),
                    format!(
                        "`\"{s}\"` is not a variant of `{name}` (expected one of {})",
                        format_variants(variants)
                    ),
                ))
            }
        }
        // A non-`null` literal still has to match the inhabitant of a
        // `T?`: e.g. `match maybe_x { 7 -> ... }` is fine if `x: Int?`.
        // Recurse with the unwrapped type so the existing literal rules
        // apply.
        (lit, Type::Nullable(inner)) => check_lit_pattern(lit, inner, span),
        (lit, _) => Err(Diagnostic::new(
            span.clone(),
            format!(
                "pattern type mismatch: scrutinee is `{scrut_ty}`, found literal of type `{}`",
                lit.type_of()
            ),
        )),
    }
}

fn check_struct_pattern(
    name: &Spanned<String>,
    fields: &[StructPatternField],
    scrut_ty: &Type,
    bindings: &mut HashMap<String, Type>,
) -> Result<(), Diagnostic> {
    let Type::Struct {
        name: ty_name,
        fields: ty_fields,
    } = scrut_ty
    else {
        return Err(Diagnostic::new(
            name.span.clone(),
            format!(
                "struct pattern `{}` does not match scrutinee `{scrut_ty}`",
                name.node
            ),
        ));
    };
    if ty_name != &name.node {
        return Err(Diagnostic::new(
            name.span.clone(),
            format!(
                "struct pattern `{}` does not match scrutinee `{ty_name}`",
                name.node
            ),
        ));
    }
    let mut seen: HashSet<String> = HashSet::new();
    for f in fields {
        if !seen.insert(f.name.node.clone()) {
            return Err(Diagnostic::new(
                f.name.span.clone(),
                format!("duplicate field `{}` in struct pattern", f.name.node),
            ));
        }
        let Some((_, fty)) = ty_fields.iter().find(|(n, _)| n == &f.name.node) else {
            return Err(Diagnostic::new(
                f.name.span.clone(),
                format!("unknown field `{}` on struct `{ty_name}`", f.name.node),
            ));
        };
        match &f.pattern {
            Some(sub) => check_pattern(sub, fty, bindings)?,
            None => {
                // Shorthand: `Point { x }` is `Point { x: x }`. Bind
                // the field's value to a binding named after the
                // field.
                if bindings.insert(f.name.node.clone(), fty.clone()).is_some() {
                    return Err(Diagnostic::new(
                        f.name.span.clone(),
                        format!("duplicate binding `{}` in pattern", f.name.node),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Apply v1's exhaustiveness rule:
/// - `StringUnion`: every variant must appear as a literal pattern,
///   OR there must be a catch-all (wildcard / bind).
/// - `Nullable(T)`: a `null` literal arm is required, AND the
///   non-null side must be exhaustive against `T` (recursive rule;
///   for the common case `T = String` / `T = primitive`, that means
///   a catch-all arm covers it).
/// - Otherwise: a catch-all arm is required.
fn check_exhaustive(
    scrut_ty: &Type,
    arms: &[MatchArm],
    scrutinee_span: Span,
) -> Result<(), Diagnostic> {
    let has_catch_all = arms.iter().any(|a| is_catch_all(&a.pattern.node));
    match scrut_ty {
        Type::StringUnion { name, variants } => {
            if has_catch_all {
                return Ok(());
            }
            let covered: Vec<&str> = arms
                .iter()
                .filter_map(|a| match &a.pattern.node {
                    Pattern::Lit(Literal::String(s)) => Some(s.as_str()),
                    _ => None,
                })
                .collect();
            let missing: Vec<&String> = variants
                .iter()
                .filter(|v| !covered.iter().any(|c| c == &v.as_str()))
                .collect();
            if missing.is_empty() {
                return Ok(());
            }
            let names: Vec<String> = missing.iter().map(|v| format!("`\"{v}\"`")).collect();
            Err(Diagnostic::new(
                scrutinee_span,
                format!(
                    "non-exhaustive `match` on `{name}`: missing {}",
                    names.join(", ")
                ),
            ))
        }
        Type::Nullable(inner) => {
            // A catch-all covers both halves at once, so we're done.
            if has_catch_all {
                return Ok(());
            }
            let has_null_arm = arms
                .iter()
                .any(|a| matches!(&a.pattern.node, Pattern::Lit(Literal::Null)));
            if !has_null_arm {
                return Err(Diagnostic::new(
                    scrutinee_span,
                    format!(
                        "non-exhaustive `match` on `{scrut_ty}`: missing a `null` arm (or a wildcard `_`)"
                    ),
                ));
            }
            // The non-null arms each typecheck against `inner` (via
            // `check_lit_pattern`'s `Nullable` recursion), so we just
            // need them to be exhaustive *for `inner`*. Reuse the
            // same checker by filtering out the `null` arm — its
            // presence has already been recorded.
            let inner_arms: Vec<MatchArm> = arms
                .iter()
                .filter(|a| !matches!(&a.pattern.node, Pattern::Lit(Literal::Null)))
                .cloned()
                .collect();
            check_exhaustive(inner, &inner_arms, scrutinee_span)
        }
        _ => {
            if has_catch_all {
                Ok(())
            } else {
                Err(Diagnostic::new(
                    scrutinee_span,
                    format!(
                        "non-exhaustive `match` on `{scrut_ty}`: a wildcard arm `_` is required"
                    ),
                ))
            }
        }
    }
}

const fn is_catch_all(p: &Pattern) -> bool {
    matches!(p, Pattern::Wildcard | Pattern::Bind(_))
}
