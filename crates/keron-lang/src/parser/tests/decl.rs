//! `val` declaration + literal tests (the non-arithmetic baseline).

use super::{expr_of, first_val, lit, ok};
use crate::{
    ast::{Expr, Item, Literal, Type},
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
fn val_int_negative_is_a_folded_literal() {
    // `-` on a bare numeric literal folds — same AST shape the
    // pattern grammar produces for `-7`.
    let e = expr_of("val n: Int = -7");
    assert_eq!(e.node, Expr::Literal(Literal::Int(-7)));
}

#[test]
fn val_double_negative_is_a_folded_literal() {
    let e = expr_of("val d: Double = -0.5");
    assert_eq!(e.node, Expr::Literal(Literal::Double(-0.5)));
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
    let prog = ok(r#"val s: Symlink = symlink(source = "b", target = "a")"#);
    let v = first_val(&prog);
    assert_eq!(
        v.ty.as_ref().expect("annotation").node,
        Type::Named("Symlink".into()),
    );
}

#[test]
fn val_template_annotation_parses() {
    let prog = ok(
        r#"val f: Template = template(source = "tmpl.tpl", target = "p", vars = {"body": "c"})"#,
    );
    let v = first_val(&prog);
    assert_eq!(
        v.ty.as_ref().expect("annotation").node,
        Type::Named("Template".into()),
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

#[test]
fn type_alias_tolerates_leading_pipe() {
    // Matches the hand-written multi-line union style; the formatter
    // canonicalizes the leading `|` away.
    let prog = ok("type Mode =\n  | \"on\"\n  | \"off\"\n");
    let Item::TypeAlias(t) = prog.items.first().expect("one item") else {
        panic!("expected type alias");
    };
    assert_eq!(t.variants.len(), 2);
    assert_eq!(t.variants[0].node, "on");
}

#[test]
fn type_alias_rejects_trailing_pipe() {
    // Unlike comma lists, the last variant is the natural terminator;
    // a dangling `|` reads as a missing variant.
    assert!(parse("type Mode = \"on\" | \"off\" |\n").is_err());
}

#[test]
fn nullable_run_must_be_adjacent() {
    // Padding before the first `?` is fine; padding *inside* the run
    // is a stuttered annotation and rejected.
    assert!(parse("val x: String ? = null").is_ok());
    assert!(parse("val x: String?? = null").is_ok());
    assert!(parse("val x: String ? ? = null").is_err());
}
