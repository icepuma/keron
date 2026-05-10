//! `val` declaration + literal tests (the non-arithmetic baseline).

use super::{expr_of, first_val, lit, ok};
use crate::{
    ast::{Expr, Literal, Type, UnaryOp},
    parser::parse,
};

#[test]
fn val_string() {
    let prog = ok(r#"val a: String = "hello""#);
    let v = first_val(&prog);
    assert_eq!(v.name.node, "a");
    assert_eq!(v.ty.as_ref().expect("annotation").node, Type::String);
    assert_eq!(*lit(&prog), Literal::String("hello".into()));
}

#[test]
fn val_without_annotation() {
    let prog = ok(r#"val greeting = "hi""#);
    let v = first_val(&prog);
    assert_eq!(v.name.node, "greeting");
    assert!(v.ty.is_none());
    assert_eq!(*lit(&prog), Literal::String("hi".into()));
}

#[test]
fn val_string_empty() {
    let prog = ok(r#"val a: String = """#);
    assert_eq!(*lit(&prog), Literal::String(String::new()));
}

#[test]
fn val_int_positive() {
    let prog = ok("val n: Int = 42");
    assert_eq!(*lit(&prog), Literal::Int(42));
}

#[test]
fn val_int_negative_is_unary() {
    let e = expr_of("val n: Int = -7");
    let Expr::Unary { op, operand } = e.node else {
        panic!("expected unary expr");
    };
    assert_eq!(op, UnaryOp::Neg);
    assert_eq!(operand.node, Expr::Literal(Literal::Int(7)));
}

#[test]
fn val_double_negative_is_unary() {
    let e = expr_of("val d: Double = -0.5");
    let Expr::Unary { operand, .. } = e.node else {
        panic!("expected unary");
    };
    assert_eq!(operand.node, Expr::Literal(Literal::Double(0.5)));
}

#[test]
fn val_boolean_true() {
    assert_eq!(*lit(&ok("val b: Boolean = true")), Literal::Boolean(true));
}

#[test]
fn val_boolean_false() {
    assert_eq!(*lit(&ok("val b: Boolean = false")), Literal::Boolean(false));
}

#[test]
fn val_double() {
    let prog = ok("val d: Double = 2.5");
    let Literal::Double(f) = lit(&prog) else {
        panic!("expected double");
    };
    assert!((f - 2.5).abs() < f64::EPSILON);
}

#[test]
fn string_escapes() {
    let prog = ok(r#"val s: String = "a\tb\n\"c\\d""#);
    assert_eq!(*lit(&prog), Literal::String("a\tb\n\"c\\d".into()));
}

#[test]
fn multiple_decls_separated_by_newlines() {
    let src = "val a: Int = 1\nval b: String = \"x\"\nval c: Boolean = true";
    assert_eq!(ok(src).items.len(), 3);
}

#[test]
fn comments_and_whitespace() {
    let src = "# leading\nval a: Int = 1 # trailing\n# tail\n";
    assert_eq!(ok(src).items.len(), 1);
}

#[test]
fn rejects_keyword_as_name() {
    let err = parse("val val: Int = 1").expect_err("should fail");
    assert!(err.iter().any(|d| d.message.contains("reserved keyword")));
}

#[test]
fn unknown_type_parses_as_named() {
    // Capitalized identifiers in type position now parse as
    // `Type::Named` placeholders — the module loader is responsible
    // for rejecting them as unresolved.
    let prog = ok("val a: Float = 1.0");
    let v = first_val(&prog);
    assert_eq!(
        v.ty.as_ref().expect("annotation").node,
        Type::Named("Float".into()),
    );
}

#[test]
fn rejects_missing_value() {
    assert!(parse("val a: Int =").is_err());
}

#[test]
fn span_covers_full_decl() {
    let src = "val a: Int = 42";
    let prog = ok(src);
    let v = first_val(&prog);
    assert_eq!(&src[v.span.clone()], src);
    assert_eq!(&src[v.name.span.clone()], "a");
    assert_eq!(&src[v.ty.as_ref().expect("annotation").span.clone()], "Int");
    assert_eq!(&src[v.value.span.clone()], "42");
}

#[test]
fn val_symlink_annotation_parses() {
    let prog = ok(r#"val s: Symlink = symlink(from = "a", to = "b")"#);
    let v = first_val(&prog);
    assert_eq!(
        v.ty.as_ref().expect("annotation").node,
        Type::Named("Symlink".into()),
    );
}

#[test]
fn val_template_annotation_parses() {
    let prog =
        ok(r#"val f: Template = template(path = "p", source = "tmpl.tpl", vars = {"body": "c"})"#);
    let v = first_val(&prog);
    assert_eq!(
        v.ty.as_ref().expect("annotation").node,
        Type::Named("Template".into()),
    );
}

#[test]
fn val_directory_annotation_parses() {
    let prog = ok(r#"val d: Directory = directory(path = "p")"#);
    let v = first_val(&prog);
    assert_eq!(
        v.ty.as_ref().expect("annotation").node,
        Type::Named("Directory".into()),
    );
}

#[test]
fn val_list_of_resources_parses() {
    let prog = ok(r"val xs: List<Template> = []");
    let v = first_val(&prog);
    assert_eq!(
        v.ty.as_ref().expect("annotation").node,
        Type::List(Box::new(Type::Named("Template".into()))),
    );
}

#[test]
fn span_covers_full_decl_without_annotation() {
    let src = "val a = 42";
    let prog = ok(src);
    let v = first_val(&prog);
    assert_eq!(&src[v.span.clone()], src);
    assert_eq!(&src[v.name.span.clone()], "a");
    assert!(v.ty.is_none());
    assert_eq!(&src[v.value.span.clone()], "42");
}
