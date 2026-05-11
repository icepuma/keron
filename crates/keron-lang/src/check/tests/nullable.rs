//! Nullable types: `T?` subtyping, the `==`/`!=` exception, the
//! "must `match`" diagnostic, and `match` exhaustiveness with
//! flow-sensitive narrowing once `null` is handled.

use super::check_src;

#[test]
fn t_value_is_assignable_to_nullable_t() {
    // `String <: String?` — the bare value flows into a nullable slot.
    assert!(check_src("val x: String? = \"hi\"").is_ok());
    assert!(check_src("val n: Int? = 7").is_ok());
}

#[test]
fn null_literal_is_assignable_to_nullable_t() {
    assert!(check_src("val x: String? = null").is_ok());
    assert!(check_src("val xs: List<Int>? = null").is_ok());
}

#[test]
fn nullable_t_is_not_assignable_to_t() {
    // The whole reason for the type: a `T?` cannot silently flow into
    // a `T` slot. The user must `match` (or `== null`) to extract.
    let err = check_src("val maybe: String? = \"hi\"\nval name: String = maybe")
        .expect_err("should fail");
    assert!(
        err.iter()
            .any(|d| d.message.contains("expected `String`")
                && d.message.contains("found `String?`")),
        "got: {err:?}",
    );
}

#[test]
fn arithmetic_on_nullable_is_rejected() {
    // Strict default: nullable operands are not auto-propagated. The
    // user must `match` to extract before doing arithmetic.
    let err = check_src("val a: Int? = 1\nval b: Int? = 2\nval c = a + b")
        .expect_err("nullable arithmetic should fail");
    assert!(
        err.iter().any(|d| d.message.contains("found `Int?`")),
        "got: {err:?}",
    );
}

#[test]
fn nullable_in_interpolation_is_rejected() {
    let err = check_src("val n: String? = \"x\"\nval s: String = \"hi ${n}\"")
        .expect_err("nullable interpolation should fail");
    assert!(
        err.iter()
            .any(|d| d.message.contains("interpolate") && d.message.contains("`match`")),
        "got: {err:?}",
    );
}

#[test]
fn equality_with_null_is_the_one_ergonomic_exception() {
    // `T? == null` and `null == T?` are allowed even though every
    // other operator rejects nullable operands. Mirrors the canonical
    // "is it set?" idiom in every nullable-typed language.
    assert!(check_src("val n: String? = null\nval is_set: Boolean = n == null").is_ok());
    assert!(check_src("val n: String? = null\nval is_set: Boolean = null != n").is_ok());
}

#[test]
fn match_on_nullable_requires_null_arm() {
    let err = check_src(
        "val n: String? = null\n\
         val x: String = match n { \"alice\" => \"a\" }",
    )
    .expect_err("missing null arm");
    assert!(
        err.iter()
            .any(|d| d.message.contains("non-exhaustive") && d.message.contains("null")),
        "got: {err:?}",
    );
}

#[test]
fn match_on_nullable_narrows_bind_after_null_arm() {
    // The canonical idiom: `null => default, n => n` — the bind in
    // the second arm sees `T`, not `T?`, because the prior `null`
    // arm has already absorbed the null case.
    assert!(
        check_src(
            "val maybe: String? = \"hi\"\n\
             val name: String = match maybe { null => \"anon\", n => n }",
        )
        .is_ok()
    );
}

#[test]
fn match_on_nullable_without_null_arm_first_does_not_narrow() {
    let err = check_src(
        "val maybe: String? = \"hi\"\n\
         val name: String = match maybe { n => n, null => \"anon\" }",
    )
    .expect_err("bind before null arm should not narrow");
    assert!(
        err[0].message.contains("unreachable `match` arm"),
        "got: {err:?}",
    );
}

#[test]
fn map_key_cannot_be_nullable() {
    let err = check_src("val m: Map<String?, Int> = {}").expect_err("nullable key should fail");
    assert!(
        err.iter()
            .any(|d| d.message.contains("not a valid `Map` key type")),
        "got: {err:?}",
    );
}

#[test]
fn map_value_can_be_nullable() {
    // Boundary check: only keys are restricted; nullable values are
    // useful (e.g. an env-var lookup map where some keys legitimately
    // resolve to "no value").
    assert!(check_src("val m: Map<String, String?> = {\"a\": \"x\", \"b\": null}").is_ok());
}

#[test]
fn double_question_collapses_in_annotations() {
    // `T??` is just `T?` after parser normalization. Both forms must
    // accept the same null literal without complaint.
    assert!(check_src("val x: String?? = null").is_ok());
    assert!(check_src("val x: String??? = null").is_ok());
}

#[test]
fn non_null_literal_arm_does_not_flip_narrowing() {
    // The narrowing flag (`null_handled`) must only flip on patterns
    // that actually absorb null: the literal `null`, a wildcard, or a
    // bind. A non-null literal like `"alice"` must NOT count, so a
    // later `null` arm still typechecks against `T?` rather than the
    // narrowed `T`.
    //
    // Two mutants live here:
    //   * `matches!(scrut, Nullable) && handles_null(pat)` → `||`
    //     would flip on every arm when the scrutinee is nullable.
    //   * `handles_null` body → `true` would make every pattern
    //     count as a null-absorber.
    // Both break this fixture: the second arm's `null` literal
    // would be typechecked against `String` (the narrowed inner)
    // and fail. The `n` bind in the third arm must see `String`
    // (post-narrowing) so the result fits a `String` slot.
    assert!(
        check_src(
            "val maybe: String? = \"alice\"\n\
             val name: String = match maybe {\n\
               \"alice\" => \"a\",\n\
               null => \"n\",\n\
               n => n,\n\
             }"
        )
        .is_ok()
    );
}

#[test]
fn match_arm_unification_rejects_cross_type_results() {
    // `unify_arm_types` lifts heterogeneous resource singletons to
    // `Resource`; the `&&` guard ensures the lift only fires when
    // BOTH arms are resource singletons. If the `&&` were a `||`,
    // a mismatched Int-vs-Symlink pair would silently lift to
    // `Resource` instead of erroring.
    let err = check_src(
        "fn pick(b: Boolean): Resource { match b {\n\
           true => 7,\n\
           false => symlink(from = \"a\", to = \"b\"),\n\
         }}",
    )
    .expect_err("Int + Symlink should not unify");
    assert!(
        err.iter().any(|d| d.message.contains("does not match")),
        "got: {err:?}",
    );
}

#[test]
fn null_arm_plus_exhaustive_inner_typechecks_without_catch_all() {
    // `check_exhaustive` for `Nullable(T)` recurses on the non-null
    // arms; the filter is `!matches!(pat, Pattern::Lit(Literal::Null))`
    // so the null literal is dropped before the recursion checks the
    // remaining arms against `T`'s exhaustiveness rules. Deleting
    // the `!` would invert the filter — only the null arm would
    // survive — and the inner string-union would report all
    // variants missing.
    assert!(
        check_src(
            "type Color = \"red\" | \"green\"\n\
             fn label(c: Color?): String { match c {\n\
               null => \"none\",\n\
               \"red\" => \"r\",\n\
               \"green\" => \"g\",\n\
             }}\n",
        )
        .is_ok()
    );
}

#[test]
fn nullable_nullable_widens_through_inner_subtyping() {
    // `is_subtype` needs an explicit `(Nullable, Nullable) => recurse`
    // arm because the general `(other, Nullable(inner))` fallback
    // has a guard that excludes nullable-on-the-left. Without the
    // explicit arm, `Symlink? <: Resource?` falls to `_ => false`
    // — a Symlink? value would not fit a Resource? slot.
    //
    // Mutating the guard `!matches!(other, Nullable(_))` to `true`
    // would catch (Nullable, Nullable) too but with the wrong
    // recursion shape (`is_subtype(Nullable(c), p)` instead of
    // `is_subtype(c, p)`), giving the same false negative.
    assert!(
        check_src(
            "val s: Symlink? = symlink(from = \"a\", to = \"b\")\n\
             val r: Resource? = s",
        )
        .is_ok()
    );
}
