//! chumsky-based parser for keron source.

use chumsky::prelude::*;

use crate::{
    ast::{Item, Literal, Program, Span, Spanned, Type, ValDecl},
    diagnostic::Diagnostic,
};

type Extra<'src> = extra::Err<Rich<'src, char>>;

const KEYWORDS: &[&str] = &["val", "true", "false", "String", "Int", "Boolean", "Double"];

/// Parse keron source into a [`Program`].
///
/// # Errors
/// Returns one or more [`Diagnostic`]s when the source has syntax errors.
pub fn parse(src: &str) -> Result<Program, Vec<Diagnostic>> {
    let result = program().parse(src);
    if result.has_errors() {
        Err(result.errors().map(rich_to_diagnostic).collect())
    } else {
        Ok(result
            .into_output()
            .unwrap_or(Program { items: Vec::new() }))
    }
}

fn rich_to_diagnostic(r: &Rich<'_, char>) -> Diagnostic {
    let span = *r.span();
    Diagnostic::new(span.start()..span.end(), r.to_string())
}

fn program<'src>() -> impl Parser<'src, &'src str, Program, Extra<'src>> {
    item()
        .repeated()
        .collect::<Vec<_>>()
        .map(|items| Program { items })
        .padded_by(pad())
        .then_ignore(end())
}

fn item<'src>() -> impl Parser<'src, &'src str, Item, Extra<'src>> {
    val_decl().map(Item::Val).padded_by(pad())
}

fn val_decl<'src>() -> impl Parser<'src, &'src str, ValDecl, Extra<'src>> {
    let kw_val = text::keyword("val").padded_by(pad());
    let colon = just(':').padded_by(pad());
    let eq = just('=').padded_by(pad());

    kw_val
        .ignore_then(spanned(ident()))
        .then_ignore(colon)
        .then(spanned(type_annotation()))
        .then_ignore(eq)
        .then(spanned(literal()))
        .map_with(|((name, ty), value), e| ValDecl {
            name,
            ty,
            value,
            span: span_to_range(e.span()),
        })
}

fn ident<'src>() -> impl Parser<'src, &'src str, String, Extra<'src>> + Clone {
    text::ident().try_map(|s: &str, span| {
        if KEYWORDS.contains(&s) {
            Err(Rich::custom(span, format!("`{s}` is a reserved keyword")))
        } else {
            Ok(s.to_string())
        }
    })
}

fn type_annotation<'src>() -> impl Parser<'src, &'src str, Type, Extra<'src>> + Clone {
    choice((
        text::keyword("String").to(Type::String),
        text::keyword("Int").to(Type::Int),
        text::keyword("Boolean").to(Type::Boolean),
        text::keyword("Double").to(Type::Double),
    ))
}

fn literal<'src>() -> impl Parser<'src, &'src str, Literal, Extra<'src>> + Clone {
    let bool_lit = choice((
        text::keyword("true").to(Literal::Boolean(true)),
        text::keyword("false").to(Literal::Boolean(false)),
    ));
    let str_lit = string_literal().map(Literal::String);
    let num_lit = number_literal();
    choice((bool_lit, str_lit, num_lit))
}

fn string_literal<'src>() -> impl Parser<'src, &'src str, String, Extra<'src>> + Clone {
    let escape = just('\\').ignore_then(choice((
        just('"').to('"'),
        just('\\').to('\\'),
        just('n').to('\n'),
        just('r').to('\r'),
        just('t').to('\t'),
    )));
    let normal = any().filter(|c: &char| *c != '"' && *c != '\\');
    choice((escape, normal))
        .repeated()
        .collect::<String>()
        .delimited_by(just('"'), just('"'))
}

fn number_literal<'src>() -> impl Parser<'src, &'src str, Literal, Extra<'src>> + Clone {
    let sign = just('-').or_not();
    let int_part = text::int(10);
    let frac = just('.').then(text::digits(10));
    sign.then(int_part)
        .then(frac.or_not())
        .to_slice()
        .try_map(|s: &str, span| {
            if s.contains('.') {
                s.parse::<f64>()
                    .map(Literal::Double)
                    .map_err(|e| Rich::custom(span, e.to_string()))
            } else {
                s.parse::<i64>()
                    .map(Literal::Int)
                    .map_err(|e| Rich::custom(span, e.to_string()))
            }
        })
}

fn spanned<'src, T, P>(p: P) -> impl Parser<'src, &'src str, Spanned<T>, Extra<'src>>
where
    P: Parser<'src, &'src str, T, Extra<'src>>,
{
    p.map_with(|node, e| Spanned {
        node,
        span: span_to_range(e.span()),
    })
}

fn pad<'src>() -> impl Parser<'src, &'src str, (), Extra<'src>> + Clone {
    let comment = just('#')
        .then(any().filter(|c: &char| *c != '\n').repeated())
        .ignored();
    let ws = any().filter(|c: &char| c.is_whitespace()).ignored();
    choice((ws, comment)).repeated().ignored()
}

const fn span_to_range(s: SimpleSpan<usize>) -> Span {
    s.start..s.end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Program {
        parse(src).expect("parse should succeed")
    }

    fn first_val(prog: &Program) -> &ValDecl {
        match prog.items.first().expect("at least one item") {
            Item::Val(v) => v,
        }
    }

    #[test]
    fn val_string() {
        let prog = ok(r#"val a: String = "hello""#);
        let v = first_val(&prog);
        assert_eq!(v.name.node, "a");
        assert_eq!(v.ty.node, Type::String);
        assert_eq!(v.value.node, Literal::String("hello".into()));
    }

    #[test]
    fn val_string_empty() {
        let prog = ok(r#"val a: String = """#);
        assert_eq!(first_val(&prog).value.node, Literal::String(String::new()));
    }

    #[test]
    fn val_int_positive() {
        let prog = ok("val n: Int = 42");
        assert_eq!(first_val(&prog).value.node, Literal::Int(42));
    }

    #[test]
    fn val_int_negative() {
        let prog = ok("val n: Int = -7");
        assert_eq!(first_val(&prog).value.node, Literal::Int(-7));
    }

    #[test]
    fn val_boolean_true() {
        let prog = ok("val b: Boolean = true");
        assert_eq!(first_val(&prog).value.node, Literal::Boolean(true));
    }

    #[test]
    fn val_boolean_false() {
        let prog = ok("val b: Boolean = false");
        assert_eq!(first_val(&prog).value.node, Literal::Boolean(false));
    }

    #[test]
    fn val_double() {
        let prog = ok("val d: Double = 2.5");
        let Literal::Double(f) = first_val(&prog).value.node else {
            panic!("expected double literal");
        };
        assert!((f - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn val_double_negative() {
        let prog = ok("val d: Double = -0.5");
        let Literal::Double(f) = first_val(&prog).value.node else {
            panic!("expected double literal");
        };
        assert!((f + 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn string_escapes() {
        let prog = ok(r#"val s: String = "a\tb\n\"c\\d""#);
        assert_eq!(
            first_val(&prog).value.node,
            Literal::String("a\tb\n\"c\\d".into())
        );
    }

    #[test]
    fn multiple_decls_separated_by_newlines() {
        let src = "val a: Int = 1\nval b: String = \"x\"\nval c: Boolean = true";
        let prog = ok(src);
        assert_eq!(prog.items.len(), 3);
    }

    #[test]
    fn comments_and_whitespace() {
        let src = "# leading\nval a: Int = 1 # trailing\n# tail\n";
        let prog = ok(src);
        assert_eq!(prog.items.len(), 1);
    }

    #[test]
    fn rejects_keyword_as_name() {
        let err = parse("val val: Int = 1").expect_err("should fail");
        assert!(err.iter().any(|d| d.message.contains("reserved keyword")));
    }

    #[test]
    fn rejects_unknown_type() {
        assert!(parse("val a: Float = 1.0").is_err());
    }

    #[test]
    fn rejects_missing_value() {
        assert!(parse("val a: Int =").is_err());
    }

    #[test]
    fn rejects_missing_type() {
        assert!(parse("val a = 1").is_err());
    }

    #[test]
    fn span_covers_full_decl() {
        let src = "val a: Int = 42";
        let prog = ok(src);
        let v = first_val(&prog);
        assert_eq!(&src[v.span.clone()], src);
        assert_eq!(&src[v.name.span.clone()], "a");
        assert_eq!(&src[v.ty.span.clone()], "Int");
        assert_eq!(&src[v.value.span.clone()], "42");
    }
}
