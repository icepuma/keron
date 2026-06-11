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

use super::{BindingKind, Env, FnEnv, check_expr, expr_type, format_variants, is_subtype};
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
    check_unreachable_arms(arms)?;

    let mut body_ty: Option<Type> = None;
    let mut null_handled = false;
    for arm in arms {
        let arm_env = build_arm_env(arm, &scrut_ty, null_handled, env, fns)?;
        let this = expr_type(&arm.body, &arm_env, fns)?;
        body_ty = Some(match body_ty.take() {
            None => this,
            Some(prev) => unify_arm_types(&prev, &this, arm)?,
        });
        if flip_null_handled(arm, &scrut_ty) {
            null_handled = true;
        }
    }

    check_exhaustive(&scrut_ty, arms, scrutinee.span.clone())?;

    Ok(body_ty.expect("arms is non-empty so body_ty must be set"))
}

/// Bidirectional companion to [`match_type`]: same scrutinee /
/// pattern / guard / exhaustiveness logic, but each arm body is
/// *checked* against `expected` instead of synthesised + joined. This
/// is what makes `val m: Mode = match … { _ => "on" }` typecheck —
/// the `Mode` annotation flows into each arm body so a String literal
/// is admitted as a union variant via the existing
/// literal-into-`StringUnion` rule, instead of widening to `String`
/// and then failing the outer subtype check.
pub(super) fn check_match(
    scrutinee: &Spanned<Expr>,
    arms: &[MatchArm],
    expected: &Type,
    env: &Env,
    fns: &FnEnv,
) -> Result<(), Diagnostic> {
    if arms.is_empty() {
        return Err(Diagnostic::new(
            scrutinee.span.clone(),
            "`match` requires at least one arm",
        ));
    }
    let scrut_ty = expr_type(scrutinee, env, fns)?;
    check_unreachable_arms(arms)?;

    let mut null_handled = false;
    for arm in arms {
        let arm_env = build_arm_env(arm, &scrut_ty, null_handled, env, fns)?;
        check_expr(&arm.body, expected, &arm_env, fns)?;
        if flip_null_handled(arm, &scrut_ty) {
            null_handled = true;
        }
    }

    check_exhaustive(&scrut_ty, arms, scrutinee.span.clone())?;

    Ok(())
}

/// Build the per-arm environment shared by [`match_type`] (synth) and
/// [`check_match`] (bidirectional): narrow the scrutinee if a prior
/// arm covered `null`, validate the pattern against that narrowed
/// type, install pattern bindings (with the standard
/// no-shadow-an-outer-name guard), and type-check any guard. Returns
/// the body-ready environment.
fn build_arm_env(
    arm: &MatchArm,
    scrut_ty: &Type,
    null_handled: bool,
    env: &Env,
    fns: &FnEnv,
) -> Result<Env, Diagnostic> {
    let arm_scrut_ty = narrowed_scrutinee(scrut_ty, null_handled);
    let mut bindings: HashMap<String, Type> = HashMap::new();
    check_pattern(&arm.pattern, &arm_scrut_ty, &mut bindings)?;
    let mut arm_env = env.clone();
    for (n, t) in bindings {
        // Pattern binds are body-locals; like `val` and `for`
        // they must not shadow a param, outer val, or earlier
        // body-local (per the "no shadowing" rule documented in
        // `check/mod.rs`). Without this guard a pattern like
        // `match … { x => … }` inside `fn pick(x: Int) { … }`
        // would silently rebind `x` to the scrutinee value.
        if let Some(kind) = arm_env.lookup_kind(&n) {
            let what = match kind {
                BindingKind::Param => "parameter",
                BindingKind::OuterVal => "outer `val`",
                BindingKind::BodyLocal => "previous body `val`",
            };
            return Err(Diagnostic::new(
                arm.pattern.span.clone(),
                format!("pattern binding `{n}` would shadow a {what} in this scope"),
            ));
        }
        arm_env.bind(n, t, BindingKind::BodyLocal);
    }
    // Guards see pattern bindings — that's the whole point of the
    // feature (`Color { name } if contains(name, "red") => …`).
    // The guard's type must be `Boolean`; reuse the regular
    // expression-type pass so any nested type error surfaces with
    // its normal diagnostic, and just enforce the outer shape.
    if let Some(guard) = &arm.guard {
        let gt = expr_type(guard, &arm_env, fns)?;
        if gt != Type::Boolean {
            return Err(Diagnostic::new(
                guard.span.clone(),
                format!("`match` arm guard must be `Boolean`, found `{gt}`"),
            ));
        }
    }
    Ok(arm_env)
}

/// Whether processing this arm should mark `null` as handled for the
/// remaining arms. A guarded arm cannot prove coverage — its guard may
/// always be false, in which case the null value would fall through
/// to a later arm. Only unguarded null-handling arms flip the flag.
const fn flip_null_handled(arm: &MatchArm, scrut_ty: &Type) -> bool {
    matches!(scrut_ty, Type::Nullable(_)) && arm.guard.is_none() && handles_null(&arm.pattern.node)
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
/// `Symlink` and `File` types as `Resource`. Also lifts `Null + T`
/// to `T?` so the canonical `match n { null => null, x => x }` idiom
/// over a nullable scrutinee type-checks.
fn unify_arm_types(prev: &Type, this: &Type, arm: &MatchArm) -> Result<Type, Diagnostic> {
    join_arm_types(prev, this).ok_or_else(|| {
        Diagnostic::new(
            arm.body.span.clone(),
            format!("`match` arm body type `{this}` does not match earlier arm type `{prev}`"),
        )
    })
}

fn join_arm_types(a: &Type, b: &Type) -> Option<Type> {
    if is_subtype(b, a) {
        return Some(a.clone());
    }
    if is_subtype(a, b) {
        return Some(b.clone());
    }
    if is_resource_singleton(a) && is_resource_singleton(b) {
        return Some(Type::Resource);
    }
    // `Null` joined with `T` becomes `T?`. The reverse direction is
    // already caught by `is_subtype(Null, Nullable(_))` above.
    if matches!(a, Type::Null) {
        return Some(Type::Nullable(Box::new(b.clone())));
    }
    if matches!(b, Type::Null) {
        return Some(Type::Nullable(Box::new(a.clone())));
    }
    // `T? + U` (with neither side `Null` and no subtype relation):
    // peel the wrapper, join the inner types, re-wrap. Covers
    // `Symlink? + Template? → Resource?` and similar. Re-wrap is
    // guarded against `Nullable(Nullable(_))` because the recursive
    // peel may itself produce a `Nullable` (e.g. `Null + Nullable<T>`).
    if let Type::Nullable(inner_a) = a {
        let other = match b {
            Type::Nullable(inner_b) => inner_b.as_ref(),
            other => other,
        };
        let inner = join_arm_types(inner_a, other)?;
        return Some(wrap_nullable(inner));
    }
    if let Type::Nullable(inner_b) = b {
        let inner = join_arm_types(a, inner_b)?;
        return Some(wrap_nullable(inner));
    }
    None
}

fn wrap_nullable(inner: Type) -> Type {
    match inner {
        already @ Type::Nullable(_) => already,
        other => Type::Nullable(Box::new(other)),
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
/// - `Boolean`: both `true` and `false` literal arms, OR a catch-all.
/// - `Struct`: one irrefutable struct pattern (all fields bound /
///   wildcarded), OR a catch-all.
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
    // A guarded arm cannot prove coverage — its guard may always be
    // false at runtime, leaving the scrutinee unhandled. For every
    // coverage-style question below we look only at *unguarded* arms.
    let has_catch_all = arms
        .iter()
        .any(|a| a.guard.is_none() && is_catch_all(&a.pattern.node));
    match scrut_ty {
        Type::StringUnion { name, variants } => {
            if has_catch_all {
                return Ok(());
            }
            let covered: Vec<&str> = arms
                .iter()
                .filter(|a| a.guard.is_none())
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
            if has_catch_all {
                return Ok(());
            }
            let has_null_arm = arms.iter().any(|a| {
                a.guard.is_none() && matches!(&a.pattern.node, Pattern::Lit(Literal::Null))
            });
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
        // `Boolean` has a closed two-value domain: covering both
        // `true` and `false` literal arms is exhaustive without a
        // wildcard, exactly like a fully-covered `StringUnion`.
        Type::Boolean if !has_catch_all => check_boolean_exhaustive(arms, scrutinee_span),
        // A struct scrutinee is a single nominal type, so one
        // irrefutable struct pattern (`Point { x, y }`, all fields
        // bound or wildcarded) covers every value. A refutable field
        // pattern (`Point { x: 0, y }`) does not, and still needs a
        // wildcard — matching the `match_missing_wildcard_struct`
        // corpus case.
        Type::Struct { .. }
            if has_catch_all
                || arms
                    .iter()
                    .any(|a| a.guard.is_none() && is_irrefutable_pattern(&a.pattern.node)) =>
        {
            Ok(())
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

/// Exhaustiveness for a `Boolean` scrutinee with no catch-all: both
/// `true` and `false` literal arms (unguarded) must be present.
fn check_boolean_exhaustive(arms: &[MatchArm], scrutinee_span: Span) -> Result<(), Diagnostic> {
    let covers = |want: bool| {
        arms.iter().any(|a| {
            a.guard.is_none()
                && matches!(&a.pattern.node, Pattern::Lit(Literal::Boolean(b)) if *b == want)
        })
    };
    let mut missing = Vec::new();
    if !covers(true) {
        missing.push("`true`");
    }
    if !covers(false) {
        missing.push("`false`");
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Diagnostic::new(
            scrutinee_span,
            format!(
                "non-exhaustive `match` on `Boolean`: missing {}",
                missing.join(", ")
            ),
        ))
    }
}

const fn is_catch_all(p: &Pattern) -> bool {
    matches!(p, Pattern::Wildcard | Pattern::Bind(_))
}

/// A pattern that matches every value of its type: a wildcard, a bare
/// bind, or a struct pattern whose every field sub-pattern is itself
/// irrefutable. A literal field pattern (`Point { x: 0 }`) is
/// refutable and makes the whole pattern refutable.
fn is_irrefutable_pattern(p: &Pattern) -> bool {
    match p {
        Pattern::Wildcard | Pattern::Bind(_) => true,
        Pattern::Struct { fields, .. } => fields.iter().all(|f| {
            f.pattern
                .as_ref()
                .is_none_or(|sub| is_irrefutable_pattern(&sub.node))
        }),
        Pattern::Lit(_) => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum StaticPatternKey {
    String(String),
    Int(i64),
    Bool(bool),
    Double(u64),
    Null,
}

fn check_unreachable_arms(arms: &[MatchArm]) -> Result<(), Diagnostic> {
    let mut seen_literals: HashSet<StaticPatternKey> = HashSet::new();
    let mut catch_all_seen = false;
    for arm in arms {
        if catch_all_seen {
            return Err(Diagnostic::new(
                arm.pattern.span.clone(),
                "unreachable `match` arm: a previous wildcard or binding pattern matches every remaining value",
            ));
        }
        // A guarded arm carries a runtime predicate, so its literal
        // pattern doesn't conclusively absorb the value — a later
        // duplicate literal stays reachable when the guard is false.
        // Same logic for catch-alls: `_ if cond => …` is not a true
        // catch-all and following arms remain reachable.
        if arm.guard.is_some() {
            continue;
        }
        if let Some(key) = static_pattern_key(&arm.pattern.node)
            && !seen_literals.insert(key)
        {
            return Err(Diagnostic::new(
                arm.pattern.span.clone(),
                "unreachable `match` arm: duplicate literal pattern",
            ));
        }
        if is_catch_all(&arm.pattern.node) {
            catch_all_seen = true;
        }
    }
    Ok(())
}

fn static_pattern_key(pattern: &Pattern) -> Option<StaticPatternKey> {
    match pattern {
        Pattern::Lit(Literal::String(s)) => Some(StaticPatternKey::String(s.clone())),
        Pattern::Lit(Literal::Int(n)) => Some(StaticPatternKey::Int(*n)),
        Pattern::Lit(Literal::Boolean(b)) => Some(StaticPatternKey::Bool(*b)),
        Pattern::Lit(Literal::Double(d)) => Some(StaticPatternKey::Double(canonical_f64_bits(*d))),
        Pattern::Lit(Literal::Null) => Some(StaticPatternKey::Null),
        Pattern::Wildcard | Pattern::Bind(_) | Pattern::Struct { .. } => None,
    }
}

/// Bit pattern used to dedup `Double` literal arms. Plain `to_bits`
/// distinguishes `0.0` from `-0.0` (they have different sign bits)
/// even though they compare equal at runtime, so a `0.0` arm
/// followed by a `-0.0` arm would silently leave the second
/// unreachable. Normalize `-0.0 → 0.0` and route every NaN through
/// a single canonical encoding so `NaN` arms also collide.
fn canonical_f64_bits(d: f64) -> u64 {
    if d == 0.0 {
        return 0.0_f64.to_bits();
    }
    if d.is_nan() {
        return f64::NAN.to_bits();
    }
    d.to_bits()
}
