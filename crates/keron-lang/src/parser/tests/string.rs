//! String literal + interpolation tests.

use super::{expr_of, first_val, lit, ok};
use crate::{
    ast::{BinOp, Expr, Literal, StringPart},
    parser::parse,
};

#[test]
fn plain_string_collapses_to_literal() {
    let prog = ok(r#"val a = "hello""#);
    assert_eq!(*lit(&prog), Literal::String("hello".into()));
}

#[test]
fn bare_dollar_is_literal() {
    let prog = ok(r#"val a = "$5""#);
    assert_eq!(*lit(&prog), Literal::String("$5".into()));
}

#[test]
fn escaped_dollar_is_literal_dollar() {
    let prog = ok(r#"val a = "\${literal}""#);
    assert_eq!(*lit(&prog), Literal::String("${literal}".into()));
}

#[test]
fn interpolation_yields_interpolation_expr() {
    let e = expr_of(r#"val a = "${1 + 2}""#);
    let Expr::Interpolation(parts) = e.node else {
        panic!("expected interpolation");
    };
    assert_eq!(parts.len(), 1);
    let StringPart::Expr { expr: inner, .. } = &parts[0] else {
        panic!("expected expr part");
    };
    let Expr::Binary { op, .. } = inner.node else {
        panic!("expected binary inside");
    };
    assert_eq!(op, BinOp::Add);
}

#[test]
fn interpolation_alternates_text_and_expr() {
    let e = expr_of(r#"val a = "x=${1}, y=${2}""#);
    let Expr::Interpolation(parts) = e.node else {
        panic!("expected interpolation");
    };
    // Text("x="), Expr(1), Text(", y="), Expr(2)
    assert_eq!(parts.len(), 4);
    assert!(matches!(parts[0], StringPart::Text(_)));
    assert!(matches!(parts[1], StringPart::Expr { .. }));
    assert!(matches!(parts[2], StringPart::Text(_)));
    assert!(matches!(parts[3], StringPart::Expr { .. }));
}

#[test]
fn interpolation_can_contain_nested_string() {
    let prog = ok(r#"val a = "outer ${"inner"} end""#);
    let Expr::Interpolation(parts) = &first_val(&prog).value.node else {
        panic!("expected interpolation");
    };
    let StringPart::Expr { expr: inner, .. } = &parts[1] else {
        panic!("expected expr part");
    };
    assert_eq!(inner.node, Expr::Literal(Literal::String("inner".into())));
}

#[test]
fn cooked_multiline_strips_closing_indent() {
    let prog = ok(r#"val a = """
  hello
    world
  """"#);
    assert_eq!(*lit(&prog), Literal::String("hello\n  world".into()));
}

#[test]
fn cooked_multiline_collapses_blank_lines() {
    let prog = ok("val a = \"\"\"\n  hello\n    \n  world\n  \"\"\"");
    assert_eq!(*lit(&prog), Literal::String("hello\n\nworld".into()));
}

#[test]
fn cooked_multiline_honors_escapes() {
    let prog = ok("val a = \"\"\"\n  line\\n\\\"\\$x\n  \"\"\"");
    assert_eq!(*lit(&prog), Literal::String("line\n\"$x".into()));
}

#[test]
fn cooked_multiline_accepts_crlf_opener() {
    let prog = ok("val a = \"\"\"\r\n  hello\r\n  \"\"\"");
    assert_eq!(*lit(&prog), Literal::String("hello".into()));
}

#[test]
fn raw_multiline_keeps_dollars_and_backslashes_literal() {
    let prog = ok(r##"val a = r#"""
  ${HOME}
  line\n
  """#"##);
    assert_eq!(*lit(&prog), Literal::String("${HOME}\nline\\n".into()));
}

#[test]
fn cooked_multiline_interpolation_captures_line_indent() {
    let e = expr_of(
        r#"val a = """
  key:
    ${body}
  """"#,
    );
    let Expr::Interpolation(parts) = e.node else {
        panic!("expected interpolation");
    };
    let Some(StringPart::Expr { indent, .. }) =
        parts.iter().find(|p| matches!(p, StringPart::Expr { .. }))
    else {
        panic!("expected expr part");
    };
    assert_eq!(indent.as_deref(), Some("  "));
}

#[test]
fn rejects_unclosed_interpolation() {
    assert!(parse(r#"val a = "${1 + 2""#).is_err());
}

#[test]
fn rejects_empty_interpolation() {
    assert!(parse(r#"val a = "${}""#).is_err());
}

#[test]
fn rejects_raw_newline_in_single_line_string() {
    assert!(parse("val a = \"hello\nworld\"").is_err());
}

#[test]
fn rejects_multiline_opener_without_newline() {
    assert!(parse(r#"val a = """hello""""#).is_err());
    assert!(parse(r#"val a = r"""hello""""#).is_err());
}

#[test]
fn rejects_multiline_closer_with_trailing_text() {
    assert!(parse("val a = \"\"\"\n  hello\n  \"\"\"suffix").is_err());
    assert!(parse("val a = r#\"\"\"\n  hello\n  \"\"\"#suffix").is_err());
}

#[test]
fn rejects_interpolation_before_required_multiline_indent() {
    assert!(parse("val body = \"x\"\nval a = \"\"\"\n ${body} \n  \"\"\"").is_err());
}
