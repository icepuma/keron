//! Function declaration, call, and scoping tests.

use super::check_src;

// ---------- declaration + call basics ----------

#[test]
fn fn_no_params_typechecks() {
    assert!(check_src("fn one(): Int { 1 }\nval x: Int = one()").is_ok());
}

#[test]
fn fn_one_param_typechecks() {
    assert!(check_src("fn double(n: Int): Int { n * 2 }\nval x: Int = double(21)").is_ok());
}

#[test]
fn fn_with_locals_typechecks() {
    let src = "
        fn area(w: Int, h: Int): Int {
            val base = w * h
            base + 2 * (w + h)
        }
        val a: Int = area(3, 4)
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn return_type_mismatch_errors() {
    let err = check_src(r#"fn f(): Int { "x" }"#).expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
    assert!(err[0].message.contains("found `String`"));
}

// ---------- recursion ----------

#[test]
fn self_recursion_typechecks() {
    // Without `if` the body loops at runtime, but it still typechecks.
    assert!(check_src("fn loop_(n: Int): Int { loop_(n - 1) }").is_ok());
}

#[test]
fn mutual_recursion_typechecks() {
    let src = "
        fn even(n: Int): Boolean { odd(n - 1) }
        fn odd(n: Int): Boolean { even(n - 1) }
    ";
    assert!(check_src(src).is_ok());
}

// ---------- arguments ----------

#[test]
fn missing_required_arg_errors() {
    let err = check_src("fn f(a: Int): Int { a }\nval x: Int = f()").expect_err("should fail");
    assert!(err[0].message.contains("missing required"));
}

#[test]
fn too_many_args_errors() {
    let err = check_src("fn f(a: Int): Int { a }\nval x: Int = f(1, 2)").expect_err("should fail");
    assert!(err[0].message.contains("too many"));
}

#[test]
fn arg_type_mismatch_errors() {
    let err = check_src("fn f(a: Int): Int { a }\nval x: Int = f(\"x\")").expect_err("should fail");
    assert!(err[0].message.contains("expected `Int`"));
}

#[test]
fn unknown_function_errors() {
    let err = check_src("val x = nope(1)").expect_err("should fail");
    assert!(err[0].message.contains("unknown function"));
    assert!(err[0].message.contains("nope"));
}

#[test]
fn unknown_named_arg_errors() {
    let err = check_src("fn f(a: Int): Int { a }\nval x = f(b = 1)").expect_err("should fail");
    assert!(err[0].message.contains("no parameter"));
    assert!(err[0].message.contains('b'));
}

#[test]
fn duplicate_arg_via_name_errors() {
    let err = check_src("fn f(a: Int): Int { a }\nval x = f(1, a = 2)").expect_err("should fail");
    assert!(err[0].message.contains("already supplied"));
}

#[test]
fn named_before_positional_errors() {
    let err = check_src("fn f(a: Int, b: Int): Int { a + b }\nval x = f(a = 1, 2)")
        .expect_err("should fail");
    assert!(err[0].message.contains("positional"));
    assert!(err[0].message.contains("named"));
}

// ---------- defaults ----------

#[test]
fn default_omitted_typechecks() {
    let src = "
        fn pad(n: Int, width: Int = 10): Int { n + width }
        val x: Int = pad(5)
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn default_overridden_by_positional() {
    let src = "
        fn pad(n: Int, width: Int = 10): Int { n + width }
        val x: Int = pad(5, 20)
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn default_overridden_by_named() {
    let src = "
        fn pad(n: Int, width: Int = 10): Int { n + width }
        val x: Int = pad(5, width = 20)
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn default_referencing_earlier_param() {
    let src = "
        fn f(a: Int, b: Int = a + 1): Int { a + b }
        val x: Int = f(10)
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn default_referencing_later_param_errors() {
    // `b` referenced in `a`'s default isn't visible yet.
    let err = check_src("fn f(a: Int = b + 1, b: Int): Int { a + b }").expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("unknown variable")));
}

#[test]
fn required_after_default_errors() {
    let err = check_src("fn f(a: Int = 0, b: Int): Int { a + b }").expect_err("should fail");
    assert!(err[0].message.contains("required"));
    assert!(err[0].message.contains("default"));
}

#[test]
fn duplicate_param_errors() {
    let err = check_src("fn f(a: Int, a: Int): Int { a }").expect_err("should fail");
    assert!(err[0].message.contains("duplicate parameter"));
}

// ---------- scoping ----------

#[test]
fn body_sees_outer_val_above() {
    assert!(check_src("val n: Int = 1\nfn f(): Int { n + 1 }").is_ok());
}

#[test]
fn body_does_not_see_outer_val_below() {
    let err = check_src("fn f(): Int { n + 1 }\nval n: Int = 1").expect_err("should fail");
    assert!(err[0].message.contains("unknown variable"));
}

#[test]
fn param_can_shadow_outer_val() {
    // `n` outside is `Int`; param `n: String` wins inside.
    let src = "
        val n: Int = 1
        fn show(n: String): String { n }
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn body_local_cannot_shadow_param() {
    let src = "
        fn f(x: Int): Int {
            val x = 2
            x
        }
    ";
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("already defined"));
    assert!(err[0].message.contains("parameter"));
}

#[test]
fn body_local_cannot_shadow_outer_val() {
    let src = "
        val n: Int = 1
        fn f(): Int {
            val n = 2
            n
        }
    ";
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("already defined"));
    assert!(err[0].message.contains("outer"));
}

#[test]
fn duplicate_body_locals_error() {
    let src = "
        fn f(): Int {
            val a = 1
            val a = 2
            a
        }
    ";
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("already defined"));
    assert!(err[0].message.contains("body"));
}

#[test]
fn body_local_forward_ref_errors() {
    let src = "
        fn f(): Int {
            val a = b + 1
            val b = 2
            a
        }
    ";
    let err = check_src(src).expect_err("should fail");
    assert!(err[0].message.contains("unknown variable"));
}

#[test]
fn val_fn_top_level_collision_errors() {
    let err = check_src("val foo = 1\nfn foo(): Int { 1 }").expect_err("should fail");
    assert!(err[0].message.contains("already defined"));
    assert!(err[0].message.contains("foo"));
}

#[test]
fn fn_val_top_level_collision_errors() {
    let err = check_src("fn foo(): Int { 1 }\nval foo = 1").expect_err("should fail");
    assert!(err[0].message.contains("already defined"));
}

// ---------- composition ----------

#[test]
fn call_used_in_arithmetic() {
    let src = "
        fn double(n: Int): Int { n * 2 }
        val x: Int = double(3) + double(4)
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn call_used_in_list() {
    let src = "
        fn id(n: Int): Int { n }
        val xs: List<Int> = [id(1), id(2), id(3)]
    ";
    assert!(check_src(src).is_ok());
}

#[test]
fn call_used_in_interpolation() {
    let src = r#"
        fn ident_n(n: Int): Int { n }
        val s: String = "value = ${ident_n(42)}"
    "#;
    assert!(check_src(src).is_ok());
}
