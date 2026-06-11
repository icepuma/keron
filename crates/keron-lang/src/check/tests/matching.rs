//! `match` exhaustiveness tests for closed domains (Boolean, struct).

use super::check_src;

#[test]
fn boolean_match_with_both_arms_is_exhaustive() {
    // Both `true` and `false` covered: exhaustive without a wildcard.
    assert!(
        check_src(
            "val flag: Boolean = true\n\
             val r: Int = match flag { true => 1, false => 0 }",
        )
        .is_ok()
    );
}

#[test]
fn boolean_match_missing_false_is_non_exhaustive() {
    let err = check_src(
        "val flag: Boolean = true\n\
         val r: Int = match flag { true => 1 }",
    )
    .expect_err("should fail");
    assert!(err[0].message.contains("non-exhaustive"));
    assert!(err[0].message.contains("`false`"));
}

#[test]
fn boolean_match_with_wildcard_is_exhaustive() {
    assert!(
        check_src(
            "val flag: Boolean = true\n\
             val r: Int = match flag { true => 1, _ => 0 }",
        )
        .is_ok()
    );
}

#[test]
fn boolean_match_guarded_arm_does_not_count_for_coverage() {
    // A guarded `false` arm cannot prove coverage; still non-exhaustive.
    let err = check_src(
        "val flag: Boolean = true\n\
         val r: Int = match flag { true => 1, false if flag => 0 }",
    )
    .expect_err("should fail");
    assert!(err[0].message.contains("non-exhaustive"));
    assert!(err[0].message.contains("`false`"));
}

#[test]
fn irrefutable_struct_pattern_is_exhaustive() {
    // One struct pattern with all fields bound covers every value.
    assert!(
        check_src(
            "struct Point { x: Int, y: Int }\n\
             fn sum(p: Point): Int { match p { Point { x, y } => x } }",
        )
        .is_ok()
    );
}

#[test]
fn refutable_struct_pattern_still_needs_wildcard() {
    // A literal field pattern is refutable, so a wildcard is required.
    let err = check_src(
        "struct Point { x: Int, y: Int }\n\
         fn axis(p: Point): Int { match p { Point { x: 0, y } => y } }",
    )
    .expect_err("should fail");
    assert!(err[0].message.contains("non-exhaustive"));
    assert!(err[0].message.contains("wildcard"));
}
