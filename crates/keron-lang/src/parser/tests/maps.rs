//! Map literal parser tests.

use super::expr_of;
use crate::{
    ast::{Expr, Literal},
    parser::parse,
};

#[test]
fn empty_map_parses() {
    let e = expr_of("val m: Map<String, Int> = {}");
    let Expr::Map(entries) = e.node else {
        panic!("expected map");
    };
    assert_eq!(entries.len(), 0);
}

#[test]
fn single_entry_map() {
    let e = expr_of(r#"val m = {"a": 1}"#);
    let Expr::Map(entries) = e.node else {
        panic!("expected map");
    };
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].key.node,
        Expr::Literal(Literal::String("a".into()))
    );
    assert_eq!(entries[0].value.node, Expr::Literal(Literal::Int(1)));
}

#[test]
fn multiple_entries() {
    let e = expr_of(r#"val m = {"a": 1, "b": 2, "c": 3}"#);
    let Expr::Map(entries) = e.node else {
        panic!("expected map");
    };
    assert_eq!(entries.len(), 3);
}

#[test]
fn trailing_comma() {
    assert!(parse(r#"val m: Map<String, Int> = {"a": 1, "b": 2,}"#).is_ok());
}

#[test]
fn int_keys() {
    let e = expr_of(r#"val m: Map<Int, String> = {1: "a", 2: "b"}"#);
    let Expr::Map(entries) = e.node else { panic!() };
    assert_eq!(entries[0].key.node, Expr::Literal(Literal::Int(1)));
}

#[test]
fn boolean_keys() {
    let e = expr_of(r"val m: Map<Boolean, Int> = {true: 1, false: 0}");
    let Expr::Map(entries) = e.node else { panic!() };
    assert_eq!(entries[0].key.node, Expr::Literal(Literal::Boolean(true)));
}

#[test]
fn nested_map_value() {
    let e = expr_of(r#"val m = {"k": {"inner": 1}}"#);
    let Expr::Map(entries) = e.node else { panic!() };
    let Expr::Map(_) = &entries[0].value.node else {
        panic!("expected nested map");
    };
}

#[test]
fn map_in_list() {
    let e = expr_of(r#"val xs: List<Map<String, Int>> = [{"a": 1}, {"b": 2}]"#);
    let Expr::List(items) = e.node else { panic!() };
    assert_eq!(items.len(), 2);
    let Expr::Map(_) = &items[0].node else {
        panic!()
    };
}

#[test]
fn map_with_expr_keys_and_values() {
    let e = expr_of(r#"val m: Map<String, Int> = {"a" + "b": 1 + 2}"#);
    let Expr::Map(entries) = e.node else { panic!() };
    assert!(matches!(entries[0].key.node, Expr::Binary { .. }));
    assert!(matches!(entries[0].value.node, Expr::Binary { .. }));
}

#[test]
fn rejects_missing_colon() {
    assert!(parse(r#"val m = {"a"}"#).is_err());
}

#[test]
fn rejects_missing_value() {
    assert!(parse(r#"val m = {"a":}"#).is_err());
}

#[test]
fn rejects_missing_key() {
    assert!(parse(r"val m = {: 1}").is_err());
}

#[test]
fn rejects_unclosed_map() {
    assert!(parse(r#"val m = {"a": 1"#).is_err());
}

#[test]
fn rejects_double_comma() {
    assert!(parse(r#"val m = {"a": 1,, "b": 2}"#).is_err());
}
