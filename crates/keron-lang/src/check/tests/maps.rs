//! `Map<K, V>` type and literal tests.

use super::check_src;

// ---------- typed annotation + literal matching ----------

#[test]
fn map_string_int_typechecks() {
    assert!(check_src(r#"val m: Map<String, Int> = {"a": 1, "b": 2}"#).is_ok());
}

#[test]
fn map_int_string_typechecks() {
    assert!(check_src(r#"val m: Map<Int, String> = {1: "a", 2: "b"}"#).is_ok());
}

#[test]
fn map_boolean_key_annotation_errors() {
    // Boolean is no longer a permitted Map key type; the diagnostic
    // names the rejected key and the closed allow-list (`String`,
    // `Int`).
    let err =
        check_src(r"val m: Map<Boolean, Int> = {true: 1, false: 0}").expect_err("should fail");
    assert!(err[0].message.contains("not a valid `Map` key type"));
    assert!(err[0].message.contains("Boolean"));
}

#[test]
fn empty_map_with_annotation_typechecks() {
    assert!(check_src("val m: Map<String, Int> = {}").is_ok());
    assert!(check_src("val m: Map<Int, List<String>> = {}").is_ok());
}

#[test]
fn empty_map_without_annotation_errors() {
    let err = check_src("val m = {}").expect_err("should fail");
    assert!(err[0].message.contains("empty map"));
}

#[test]
fn empty_map_with_non_map_annotation_errors() {
    let err = check_src("val n: Int = {}").expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("empty map"));
}

#[test]
fn nonempty_map_with_non_map_annotation_errors() {
    let err = check_src(r#"val n: Int = {"a": 1}"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `Map<String, Int>`"));
    assert!(!err[0].message.contains("empty map"));
}

#[test]
fn map_inferred_string_int() {
    assert!(check_src(r#"val m = {"a": 1, "b": 2, "c": 3}"#).is_ok());
}

// ---------- key type validation ----------

#[test]
fn map_double_key_annotation_errors() {
    let err = check_src("val m: Map<Double, Int> = {}").expect_err("should fail");
    assert!(err[0].message.contains("not a valid `Map` key type"));
    assert!(err[0].message.contains("Double"));
}

#[test]
fn map_list_key_annotation_errors() {
    let err = check_src("val m: Map<List<Int>, Int> = {}").expect_err("should fail");
    assert!(err[0].message.contains("not a valid `Map` key type"));
}

#[test]
fn map_nested_map_key_annotation_errors() {
    let err = check_src("val m: Map<Map<String, Int>, Int> = {}").expect_err("should fail");
    assert!(err[0].message.contains("not a valid `Map` key type"));
}

#[test]
fn map_inferred_double_key_errors() {
    let err = check_src("val m = {1.5: 1}").expect_err("should fail");
    assert!(err[0].message.contains("not a valid `Map` key type"));
}

// ---------- heterogeneous + mismatch errors ----------

#[test]
fn heterogeneous_keys_error() {
    let err = check_src(r#"val m = {"a": 1, 2: 2}"#).expect_err("should fail");
    assert!(err[0].message.contains("map key type mismatch"));
    assert!(err[0].message.contains("expected `String`"));
    assert!(err[0].message.contains("found `Int`"));
}

#[test]
fn heterogeneous_values_error() {
    let err = check_src(r#"val m = {"a": 1, "b": "x"}"#).expect_err("should fail");
    assert!(err[0].message.contains("map value type mismatch"));
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn map_annotation_mismatch_points_at_value() {
    // Bidirectional: each entry's value is checked against expected V.
    let err = check_src(r#"val m: Map<String, Int> = {"a": "x"}"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `String`"));
}

#[test]
fn map_annotation_mismatch_points_at_key() {
    let err = check_src(r"val m: Map<String, Int> = {1: 2}").expect_err("should fail");
    assert!(err[0].message.contains("expected `String`"));
    assert!(err[0].message.contains("found `Int`"));
}

// ---------- composition ----------

#[test]
fn map_with_list_values() {
    assert!(check_src(r#"val m: Map<String, List<Int>> = {"a": [1, 2], "b": []}"#).is_ok());
}

#[test]
fn map_of_maps() {
    assert!(check_src(r#"val m: Map<String, Map<String, Int>> = {"k": {"inner": 1}}"#).is_ok());
}

#[test]
fn map_value_with_var_ref() {
    let src = "
        val n: Int = 42
        val m: Map<String, Int> = {\"answer\": n, \"double\": n * 2}
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn map_in_fn_return() {
    let src = r#"
        fn lookup(): Map<String, Int> {
            {"a": 1, "b": 2}
        }
        val m: Map<String, Int> = lookup()
    "#;
    assert!(check_src(src).is_ok());
}

#[test]
fn empty_map_in_fn_return() {
    let src = r"
        fn empty(): Map<String, Int> {
            {}
        }
        val m: Map<String, Int> = empty()
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn map_does_not_promote_int_to_double_in_values() {
    let err = check_src(r#"val m = {"a": 1, "b": 2.5}"#).expect_err("should fail");
    assert!(err[0].message.contains("Int"));
    assert!(err[0].message.contains("Double"));
}
