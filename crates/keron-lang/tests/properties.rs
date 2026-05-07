//! Proptest property tests for `keron-lang`. These complement the
//! corpus by asserting *shapes* rather than specific outputs:
//!
//! - any literal of a primitive type round-trips through `parse`
//! - any non-keyword identifier is accepted as a `val` name
//! - matching-type declarations always pass `check`
//! - mismatched-type declarations always produce one diagnostic per decl
//!
//! Strategies emit parser-safe source strings (no exponent notation, no
//! escape characters) since we don't yet have a pretty-printer to drive
//! true round-trip testing.

use std::fmt::Write as _;

use keron_lang::{Item, Literal, Type, check, parse};
use proptest::prelude::*;

const KEYWORDS: &[&str] = &["val", "true", "false", "String", "Int", "Boolean", "Double"];

fn ident_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z_][a-zA-Z0-9_]{0,16}"
        .prop_filter("must not be a keyword", |s| !KEYWORDS.contains(&s.as_str()))
}

fn ty_strategy() -> impl Strategy<Value = Type> {
    prop_oneof![
        Just(Type::String),
        Just(Type::Int),
        Just(Type::Boolean),
        Just(Type::Double),
    ]
}

fn literal_source_for(ty: Type) -> BoxedStrategy<(String, Literal)> {
    match ty {
        Type::Int => any::<i64>()
            .prop_map(|n| (n.to_string(), Literal::Int(n)))
            .boxed(),
        Type::Boolean => any::<bool>()
            .prop_map(|b| (b.to_string(), Literal::Boolean(b)))
            .boxed(),
        Type::String => "[a-zA-Z0-9 _.!?]{0,32}"
            .prop_map(|s: String| (format!("\"{s}\""), Literal::String(s)))
            .boxed(),
        Type::Double => (any::<i32>(), 0u32..1_000_000_000u32)
            .prop_map(|(int, frac)| {
                let s = format!("{int}.{frac:09}");
                let f: f64 = s.parse().expect("decimal literal parses as f64");
                (s, Literal::Double(f))
            })
            .boxed(),
    }
}

fn first_val_literal(src: &str) -> Literal {
    let prog = parse(src).expect("parse should succeed");
    let Some(Item::Val(v)) = prog.items.into_iter().next() else {
        panic!("expected a val item");
    };
    v.value.node
}

proptest! {
    #[test]
    fn any_int_literal_round_trips(n in any::<i64>()) {
        let src = format!("val a: Int = {n}");
        prop_assert_eq!(first_val_literal(&src), Literal::Int(n));
    }

    #[test]
    fn any_bool_literal_round_trips(b in any::<bool>()) {
        let src = format!("val a: Boolean = {b}");
        prop_assert_eq!(first_val_literal(&src), Literal::Boolean(b));
    }

    #[test]
    fn any_double_literal_round_trips(
        int in any::<i32>(),
        frac in 0u32..1_000_000_000u32,
    ) {
        let lit = format!("{int}.{frac:09}");
        let src = format!("val a: Double = {lit}");
        let Literal::Double(parsed) = first_val_literal(&src) else {
            panic!("expected double");
        };
        let expected: f64 = lit.parse().expect("decimal literal parses as f64");
        prop_assert_eq!(parsed.to_bits(), expected.to_bits());
    }

    #[test]
    fn any_simple_string_literal_round_trips(s in "[a-zA-Z0-9 _.!?]{0,64}") {
        let src = format!("val a: String = \"{s}\"");
        prop_assert_eq!(first_val_literal(&src), Literal::String(s));
    }

    #[test]
    fn any_non_keyword_ident_accepted(name in ident_strategy()) {
        let src = format!("val {name}: Int = 0");
        let prog = parse(&src).expect("parse should succeed");
        let Some(Item::Val(v)) = prog.items.into_iter().next() else {
            panic!("expected a val item");
        };
        prop_assert_eq!(v.name.node, name);
    }

    #[test]
    fn matching_decl_checks_ok(
        name in ident_strategy(),
        case in ty_strategy().prop_flat_map(|ty| {
            literal_source_for(ty).prop_map(move |(src, lit)| (ty, src, lit))
        }),
    ) {
        let (ty, lit_src, _) = case;
        let src = format!("val {name}: {} = {lit_src}", ty.name());
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn mismatched_decl_yields_one_error(
        name in ident_strategy(),
        annot in ty_strategy(),
        lit_pair in ty_strategy().prop_flat_map(literal_source_for),
    ) {
        let (lit_src, lit) = lit_pair;
        prop_assume!(lit.type_of() != annot);
        let src = format!("val {name}: {} = {lit_src}", annot.name());
        let prog = parse(&src).expect("parse should succeed");
        let errs = check(&prog).expect_err("expected mismatch");
        prop_assert_eq!(errs.len(), 1);
    }

    #[test]
    fn n_mismatches_yield_n_errors(
        cases in prop::collection::vec(
            (
                ident_strategy(),
                ty_strategy(),
                ty_strategy().prop_flat_map(literal_source_for),
            ),
            1..6,
        ),
    ) {
        let mut src = String::new();
        let mut expected_errs = 0usize;
        for (name, annot, (lit_src, lit)) in &cases {
            writeln!(src, "val {name}: {} = {lit_src}", annot.name())
                .expect("write to String");
            if lit.type_of() != *annot {
                expected_errs += 1;
            }
        }
        let prog = parse(&src).expect("parse should succeed");
        match check(&prog) {
            Ok(()) => prop_assert_eq!(expected_errs, 0),
            Err(errs) => prop_assert_eq!(errs.len(), expected_errs),
        }
    }
}
