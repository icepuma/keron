//! Type-directed overload resolution for the kind-uniform collection
//! builtins `len` and `contains`.
//!
//! These two names mean the same thing across `String`, `List`, and
//! `Map` ("how big?", "is this in that?"), so instead of three
//! prefixed variants each (`str_len` / `list_contains` / …) the
//! checker synthesizes the *first* argument's type and selects the
//! concrete rule from its kind. This is not general overloading: the
//! set of names is closed, builtins are unshadowable (locally and via
//! imports), and dispatch is on the first argument only — the same
//! shape as the evaluator's `Value`-directed dispatch.

use crate::ast::{CallArg, Span, Spanned, Type};
use crate::diagnostic::Diagnostic;

use super::{Env, FnEnv, FnSig, check_expr, expr_type, is_list_equality_comparable};

/// Whether `check_call` should route this call here instead of the
/// generic-signature path. Keyed on the name *and* the registry's
/// `Generic("C")` first-param marker: user source cannot write
/// generics, so the marker uniquely identifies the stdlib signature —
/// a unit-test `FnEnv` that registers its own plain `contains` keeps
/// ordinary call semantics.
pub(super) fn is_collection_overload(name: &str, sig: &FnSig) -> bool {
    matches!(name, "len" | "contains")
        && matches!(
            sig.params.first().map(|p| &p.ty),
            Some(Type::Generic(marker)) if marker == "C"
        )
}

/// Check a `len`/`contains` call whose args have already been matched
/// against the registry signature (`matched[i]` is the arg supplied
/// for param `i`, `None` when omitted). Returns the call's type.
pub(super) fn check_collection_overload(
    callee: &Spanned<String>,
    matched: &[Option<&CallArg>],
    env: &Env,
    fns: &FnEnv,
    call_span: &Span,
) -> Result<Type, Diagnostic> {
    let Some(first) = matched.first().copied().flatten() else {
        return Err(Diagnostic::new(
            call_span.clone(),
            format!("missing required argument `x` for `{}`", callee.node),
        ));
    };
    let first_ty = expr_type(&first.value, env, fns)?;

    if callee.node == "len" {
        return match &first_ty {
            Type::String | Type::StringUnion { .. } | Type::List(_) | Type::Map(..) => {
                Ok(Type::Int)
            }
            other => Err(Diagnostic::new(
                first.value.span.clone(),
                format!("`len` requires a `String`, `List`, or `Map`, found `{other}`"),
            )),
        };
    }

    // `contains`
    let Some(item) = matched.get(1).copied().flatten() else {
        return Err(Diagnostic::new(
            call_span.clone(),
            format!("missing required argument `item` for `{}`", callee.node),
        ));
    };
    match &first_ty {
        // Substring test. A string-union haystack is a `String` at
        // runtime, so it participates like any other string.
        Type::String | Type::StringUnion { .. } => {
            check_expr(&item.value, &Type::String, env, fns)?;
        }
        // Element membership: same equality gate as `unique` /
        // `index_of` — element types without a defined `==` can't be
        // probed for membership either.
        Type::List(elem) => {
            if !is_list_equality_comparable(elem) {
                return Err(Diagnostic::new(
                    call_span.clone(),
                    format!(
                        "`contains` requires a list element type with supported equality, found `{elem}`"
                    ),
                ));
            }
            check_expr(&item.value, elem, env, fns)?;
        }
        // Key membership.
        Type::Map(key, _) => {
            check_expr(&item.value, key, env, fns)?;
        }
        other => {
            return Err(Diagnostic::new(
                first.value.span.clone(),
                format!(
                    "`contains` requires a `String`, `List`, or `Map` as its first argument, found `{other}`"
                ),
            ));
        }
    }
    Ok(Type::Boolean)
}
