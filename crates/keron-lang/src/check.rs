//! Type checker for keron AST.

use crate::{
    ast::{Item, Program},
    diagnostic::Diagnostic,
};

/// Validate that each `val` declaration's literal matches its annotated type.
///
/// # Errors
/// Returns one [`Diagnostic`] per type mismatch, spanning the offending value.
pub fn check(program: &Program) -> Result<(), Vec<Diagnostic>> {
    let diags: Vec<Diagnostic> = program
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Val(v) => {
                let want = v.ty.node;
                let got = v.value.node.type_of();
                (want != got).then(|| {
                    Diagnostic::new(
                        v.value.span.clone(),
                        format!(
                            "type mismatch: expected `{}`, found `{}`",
                            want.name(),
                            got.name()
                        ),
                    )
                })
            }
        })
        .collect();
    if diags.is_empty() { Ok(()) } else { Err(diags) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn check_src(src: &str) -> Result<(), Vec<Diagnostic>> {
        let prog = parse(src).expect("parse should succeed");
        check(&prog)
    }

    #[test]
    fn matching_string() {
        assert!(check_src(r#"val a: String = "hi""#).is_ok());
    }

    #[test]
    fn matching_int() {
        assert!(check_src("val a: Int = 1").is_ok());
    }

    #[test]
    fn matching_boolean() {
        assert!(check_src("val a: Boolean = true").is_ok());
    }

    #[test]
    fn matching_double() {
        assert!(check_src("val a: Double = 1.5").is_ok());
    }

    #[test]
    fn int_assigned_to_string() {
        let err = check_src("val a: String = 1").expect_err("should fail");
        assert_eq!(err.len(), 1);
        assert!(err[0].message.contains("expected `String`"));
        assert!(err[0].message.contains("found `Int`"));
    }

    #[test]
    fn double_assigned_to_int() {
        let err = check_src("val a: Int = 1.5").expect_err("should fail");
        assert!(err[0].message.contains("expected `Int`"));
    }

    #[test]
    fn boolean_assigned_to_double() {
        let err = check_src("val a: Double = true").expect_err("should fail");
        assert!(err[0].message.contains("expected `Double`"));
    }

    #[test]
    fn collects_multiple_errors() {
        let src = "val a: Int = \"x\"\nval b: String = 2";
        let err = check_src(src).expect_err("should fail");
        assert_eq!(err.len(), 2);
    }

    #[test]
    fn mismatch_span_points_at_value() {
        let src = "val a: Int = \"x\"";
        let err = check_src(src).expect_err("should fail");
        assert_eq!(&src[err[0].span.clone()], "\"x\"");
    }
}
