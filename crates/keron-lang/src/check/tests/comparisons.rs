//! Comparison operator type-checker tests.

use super::check_src;

// ---------- equality (==, !=) ----------

#[test]
fn int_equality_typechecks() {
    assert!(check_src("val r: Boolean = 1 == 2").is_ok());
    assert!(check_src("val r: Boolean = 1 != 2").is_ok());
}

#[test]
fn double_equality_typechecks() {
    assert!(check_src("val r: Boolean = 1.5 == 2.5").is_ok());
}

#[test]
fn string_equality_typechecks() {
    assert!(check_src(r#"val r: Boolean = "a" == "b""#).is_ok());
}

#[test]
fn boolean_equality_typechecks() {
    assert!(check_src("val r: Boolean = true == false").is_ok());
    assert!(check_src("val r: Boolean = true != false").is_ok());
}

#[test]
fn int_double_equality_promotes() {
    assert!(check_src("val r: Boolean = 1 == 1.0").is_ok());
    assert!(check_src("val r: Boolean = 2.5 != 3").is_ok());
}

#[test]
fn equality_inferred_to_boolean() {
    assert!(check_src("val r = 1 == 2").is_ok());
}

#[test]
fn equality_with_resource_errors() {
    let err = check_src(r#"val r = symlink(from="a", to="b") == symlink(from="c", to="d")"#)
        .expect_err("should fail");
    assert!(err[0].message.contains("`==` requires"));
    assert!(err[0].message.contains("`Symlink`"));
}

#[test]
fn equality_with_list_errors() {
    let err = check_src("val r = [1, 2] == [3, 4]").expect_err("should fail");
    assert!(err[0].message.contains("`==` requires"));
}

#[test]
fn equality_with_map_errors() {
    let err = check_src(r#"val r = {"a": 1} != {"b": 2}"#).expect_err("should fail");
    assert!(err[0].message.contains("`!=` requires"));
}

#[test]
fn equality_int_with_string_errors() {
    let err = check_src(r#"val r = 1 == "a""#).expect_err("should fail");
    assert!(err[0].message.contains("found `Int` and `String`"));
}

#[test]
fn equality_boolean_with_int_errors() {
    let err = check_src("val r = true == 1").expect_err("should fail");
    assert!(err[0].message.contains("found `Boolean` and `Int`"));
}

// ---------- ordering (<, <=, >, >=) ----------

#[test]
fn int_ordering_typechecks() {
    for op in ["<", "<=", ">", ">="] {
        let src = format!("val r: Boolean = 1 {op} 2");
        assert!(check_src(&src).is_ok(), "should typecheck: {src}");
    }
}

#[test]
fn double_ordering_typechecks() {
    assert!(check_src("val r: Boolean = 1.5 < 2.5").is_ok());
    assert!(check_src("val r: Boolean = 3.14 >= 2.71").is_ok());
}

#[test]
fn int_double_ordering_promotes() {
    assert!(check_src("val r: Boolean = 1 < 2.5").is_ok());
    assert!(check_src("val r: Boolean = 3.14 >= 3").is_ok());
}

#[test]
fn string_ordering_typechecks() {
    assert!(check_src(r#"val r: Boolean = "a" < "b""#).is_ok());
    assert!(check_src(r#"val r: Boolean = "z" >= "a""#).is_ok());
}

#[test]
fn boolean_ordering_errors() {
    // Boolean has no defined order in keron.
    let err = check_src("val r = true < false").expect_err("should fail");
    assert!(err[0].message.contains("`<` requires"));
    assert!(err[0].message.contains("`Boolean`"));
}

#[test]
fn ordering_int_with_string_errors() {
    let err = check_src(r#"val r = 1 < "a""#).expect_err("should fail");
    assert!(err[0].message.contains("`<` requires"));
}

#[test]
fn ordering_with_list_errors() {
    let err = check_src("val r = [1] < [2]").expect_err("should fail");
    assert!(err[0].message.contains("`<` requires"));
}

// ---------- in if cond ----------

#[test]
fn comparison_in_if_cond_typechecks() {
    let src = r"
        val n: Int = 5
        val r: String = if n < 10 { 'small' } else { 'big' }
    ";
    // single quotes aren't valid; use double quotes
    let src = src.replace('\'', "\"");
    assert!(check_src(&src).is_ok());
}

#[test]
fn comparison_chain_via_fns_typechecks() {
    let src = r"
        fn classify(n: Int): String {
            if n == 0 {
                'zero'
            } else if n < 0 {
                'negative'
            } else {
                'positive'
            }
        }
    ";
    let src = src.replace('\'', "\"");
    assert!(check_src(&src).is_ok());
}

// ---------- precedence ----------

#[test]
fn comparison_binds_looser_than_arithmetic() {
    // `1 + 2 < 3 + 4` should typecheck as `(1+2) < (3+4)` -> Boolean.
    assert!(check_src("val r: Boolean = 1 + 2 < 3 + 4").is_ok());
}

#[test]
fn comparison_chained_errors() {
    // `a < b < c` parses left-assoc as `(a < b) < c`. The first
    // produces Boolean, then `Boolean < c` errors.
    let err = check_src("val r = 1 < 2 < 3").expect_err("should fail");
    assert!(err[0].message.contains("`<` requires"));
    assert!(err[0].message.contains("`Boolean`"));
}

// ---------- annotation flow ----------

#[test]
fn comparison_assigned_to_int_errors() {
    let err = check_src("val r: Int = 1 == 2").expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `Boolean`"));
}
