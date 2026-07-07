//! Proptest property tests for `keron-lang`. These complement the
//! corpus by asserting *shapes* rather than specific outputs:
//!
//! - any literal of a primitive type round-trips through `parse`
//! - any non-keyword identifier is accepted as a `val` name
//! - matching-type declarations always pass `check`
//! - mismatched-type declarations always produce one diagnostic per decl
//! - arithmetic on Int/Double obeys promotion rules
//!
//! Strategies emit parser-safe source strings (no exponent notation, no
//! escape characters) since we don't yet have a pretty-printer to drive
//! true round-trip testing.

use std::fmt::Write as _;

use keron_lang::{
    Diagnostic, Expr, Item, Literal, Program, Spanned, Type, UnaryOp, check_module, parse,
    resolve_type_names,
};
use proptest::prelude::*;

#[path = "support/stdlib.rs"]
mod stdlib;

fn check(program: &Program) -> Result<(), Vec<Diagnostic>> {
    let mut prog = program.clone();
    let imp = stdlib::imports();
    resolve_type_names(&mut prog, &imp)?;
    check_module(&prog, &imp)
}

fn ident_strategy() -> impl Strategy<Value = String> {
    "[a-zA-Z_][a-zA-Z0-9_]{0,16}".prop_filter("must not be a keyword", |s| {
        !stdlib::RESERVED_OR_BUILTIN_NAMES.contains(s.as_str())
    })
}

/// Guard the stdlib mirror against the specific drifts that had already
/// crept in (missing `cask`/`starts_with`/`ends_with`/`str_len`, and a
/// `brew`/`cask` without the optional `tap_url`). The mirror is
/// hand-maintained (keron-lang can't depend on keron-modules), so this
/// pins the signatures that matter.
#[test]
fn stdlib_mirror_covers_drift_prone_builtins() {
    let imp = stdlib::imports();
    for name in ["cask", "starts_with", "ends_with", "str_len"] {
        assert!(imp.fns.contains_key(name), "mirror is missing `{name}`");
    }
    for name in ["brew", "cask"] {
        let sig = imp.fns.get(name).expect("present");
        assert_eq!(sig.params.len(), 2, "`{name}` must take (name, tap_url)");
        assert_eq!(sig.params[1].name, "tap_url");
        assert!(sig.params[1].has_default, "`{name}` tap_url must default");
    }
    // The reserved-identifier set is derived from the mirror, so every
    // builtin the harness knows is automatically a reserved name.
    for b in &imp.builtins {
        assert!(
            stdlib::RESERVED_OR_BUILTIN_NAMES.contains(b.as_str()),
            "builtin `{b}` is not a reserved identifier"
        );
    }
}

fn ty_strategy() -> impl Strategy<Value = Type> {
    prop_oneof![
        Just(Type::String),
        Just(Type::Int),
        Just(Type::Boolean),
        Just(Type::Double),
    ]
}

fn numeric_ty_strategy() -> impl Strategy<Value = Type> {
    prop_oneof![Just(Type::Int), Just(Type::Double)]
}

/// Generate a literal source + the `Type` it will yield. Note: the type
/// reflects the *literal* itself (always positive); a `-` prefix in
/// `mismatched_decl_yields_one_error` does not change the type.
fn literal_source_for(ty: &Type) -> BoxedStrategy<(String, Type)> {
    match ty {
        // i64::MIN's positive form overflows, so trim one off the bottom.
        Type::Int => ((i64::MIN + 1)..=i64::MAX)
            .prop_map(|n| (n.to_string(), Type::Int))
            .boxed(),
        Type::Boolean => any::<bool>()
            .prop_map(|b| (b.to_string(), Type::Boolean))
            .boxed(),
        Type::String => "[a-zA-Z0-9 _.!?]{0,32}"
            .prop_map(|s: String| (format!("\"{s}\""), Type::String))
            .boxed(),
        Type::Double => (0i32..1_000_000_i32, 0u32..1_000_000_000u32)
            .prop_map(|(int, frac)| (format!("{int}.{frac:09}"), Type::Double))
            .boxed(),
        // The property tests only use the four primitive variants;
        // `List`/`Map`/resource types and `Void` are exercised via
        // dedicated properties below.
        Type::List(_)
        | Type::Map(_, _)
        | Type::Symlink
        | Type::Template
        | Type::Resource
        | Type::Secret
        | Type::Package
        | Type::Shell
        | Type::SshKey
        | Type::GpgKey
        | Type::Void
        | Type::Struct { .. }
        | Type::StringUnion { .. }
        | Type::Named(_)
        | Type::Null
        | Type::Nullable(_)
        | Type::Generic(_) => {
            unreachable!("structured/resource/void types are not used in literal_source_for")
        }
    }
}

fn first_value(src: &str) -> Spanned<Expr> {
    let prog = parse(src).expect("parse should succeed");
    let Some(Item::Val(v)) = prog.items.into_iter().next() else {
        panic!("expected a val item");
    };
    v.value
}

/// Walk an expression and reduce literal-and-unary-only forms to a
/// concrete `Literal`. Panics on anything more complex; the property
/// tests construct only those shapes.
fn eval_simple(e: &Expr) -> Literal {
    match e {
        Expr::Literal(l) => l.clone(),
        Expr::Unary {
            op: UnaryOp::Neg,
            operand,
        } => match eval_simple(&operand.node) {
            Literal::Int(n) => Literal::Int(-n),
            Literal::Double(f) => Literal::Double(-f),
            other => panic!("cannot negate {other:?}"),
        },
        Expr::Unary {
            op: UnaryOp::Not,
            operand,
        } => match eval_simple(&operand.node) {
            Literal::Boolean(b) => Literal::Boolean(!b),
            other => panic!("cannot logically negate {other:?}"),
        },
        Expr::Binary { .. }
        | Expr::Interpolation(_)
        | Expr::List(_)
        | Expr::Map(_)
        | Expr::Var(_)
        | Expr::Call { .. }
        | Expr::If { .. }
        | Expr::For { .. }
        | Expr::Field { .. }
        | Expr::Match { .. } => {
            panic!("eval_simple only supports literals and unary")
        }
    }
}

fn resource_value(kind: u8, i: usize) -> String {
    match kind {
        0 => format!("symlink(source = \"b{i}\", target = \"a{i}\")"),
        1 => format!(
            "template(source = \"tmpl.tpl\", target = \"p{i}\", vars = {{\"body\": \"c{i}\"}})"
        ),
        _ => format!("shell(kind = \"sh\", name = \"run-{i}\", script = \"echo {i}\")"),
    }
}

proptest! {
    #[test]
    fn any_int_literal_round_trips(n in (i64::MIN + 1)..=i64::MAX) {
        let src = format!("val a: Int = {n}");
        prop_assert_eq!(eval_simple(&first_value(&src).node), Literal::Int(n));
    }

    #[test]
    fn any_bool_literal_round_trips(b in any::<bool>()) {
        let src = format!("val a: Boolean = {b}");
        prop_assert_eq!(eval_simple(&first_value(&src).node), Literal::Boolean(b));
    }

    #[test]
    fn any_double_literal_round_trips(
        int in 0i32..1_000_000_i32,
        frac in 0u32..1_000_000_000u32,
    ) {
        let lit = format!("{int}.{frac:09}");
        let src = format!("val a: Double = {lit}");
        let Literal::Double(parsed) = eval_simple(&first_value(&src).node) else {
            panic!("expected double");
        };
        let expected: f64 = lit.parse().expect("decimal literal parses as f64");
        prop_assert_eq!(parsed.to_bits(), expected.to_bits());
    }

    #[test]
    fn any_simple_string_literal_round_trips(s in "[a-zA-Z0-9 _.!?]{0,64}") {
        let src = format!("val a: String = \"{s}\"");
        prop_assert_eq!(eval_simple(&first_value(&src).node), Literal::String(s));
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
    fn inferred_decl_always_checks_ok(
        name in ident_strategy(),
        case in ty_strategy().prop_flat_map(|ty| literal_source_for(&ty)),
    ) {
        let (lit_src, _) = case;
        let src = format!("val {name} = {lit_src}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn matching_decl_checks_ok(
        name in ident_strategy(),
        case in ty_strategy().prop_flat_map(|ty| {
            let ty_capture = ty.clone();
            literal_source_for(&ty)
                .prop_map(move |(src, lit_ty)| (ty_capture.clone(), src, lit_ty))
        }),
    ) {
        let (ty, lit_src, _) = case;
        let src = format!("val {name}: {ty} = {lit_src}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn mismatched_decl_yields_one_error(
        name in ident_strategy(),
        annot in ty_strategy(),
        lit_pair in ty_strategy().prop_flat_map(|ty| literal_source_for(&ty)),
    ) {
        let (lit_src, lit_ty) = lit_pair;
        prop_assume!(lit_ty != annot);
        let src = format!("val {name}: {annot} = {lit_src}");
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
                ty_strategy().prop_flat_map(|ty| literal_source_for(&ty)),
            ),
            1..6,
        ),
    ) {
        let mut src = String::new();
        let mut expected_errs = 0usize;
        for (idx, (name, annot, (lit_src, lit_ty))) in cases.iter().enumerate() {
            // Suffix the index to dodge duplicate-`val` errors when the
            // strategy happens to draw the same name twice.
            writeln!(src, "val {name}_{idx}: {annot} = {lit_src}").expect("write to String");
            if lit_ty != annot {
                expected_errs += 1;
            }
        }
        let prog = parse(&src).expect("parse should succeed");
        match check(&prog) {
            Ok(()) => prop_assert_eq!(expected_errs, 0),
            Err(errs) => prop_assert_eq!(errs.len(), expected_errs),
        }
    }

    // ---------- arithmetic ----------

    #[test]
    fn int_plus_int_typechecks_int(a in 0i32..1000, b in 0i32..1000) {
        let src = format!("val r: Int = {a} + {b}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn double_plus_double_typechecks_double(a in 0i32..100, b in 0i32..100) {
        let src = format!("val r: Double = {a}.5 + {b}.25");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn int_promotes_to_double_in_mixed_arithmetic(a in 0i32..1000, b in 0i32..100) {
        // Mixed Int + Double satisfies a Double annotation, never an Int one.
        let src_double = format!("val r: Double = {a} + {b}.5");
        let src_int = format!("val r: Int = {a} + {b}.5");
        prop_assert!(check(&parse(&src_double).unwrap()).is_ok());
        prop_assert!(check(&parse(&src_int).unwrap()).is_err());
    }

    #[test]
    fn arithmetic_with_non_numeric_errors(
        ty in prop_oneof![Just(Type::String), Just(Type::Boolean)],
        op in prop_oneof![Just("+"), Just("-"), Just("*"), Just("/"), Just("**")],
    ) {
        let (lit_src, _) = match ty {
            Type::String => (String::from("\"x\""), Type::String),
            Type::Boolean => (String::from("true"), Type::Boolean),
            _ => unreachable!(),
        };
        let src = format!("val r = {lit_src} {op} 1");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_err());
    }

    #[test]
    fn any_interpolation_typechecks_as_string(
        prefix in "[a-zA-Z0-9 ]{0,16}",
        n in 0i32..1000,
        suffix in "[a-zA-Z0-9 ]{0,16}",
    ) {
        // An interpolation always typechecks as String regardless of
        // the inner expression's type (so any well-typed inner works).
        let src = format!("val a: String = \"{prefix}${{{n}}}{suffix}\"");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn interpolation_with_inner_arithmetic_typechecks(
        a in 0i32..1000,
        b in 0i32..1000,
    ) {
        let src = format!("val s = \"{a} + {b} = ${{{a} + {b}}}\"");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    // ---------- lists ----------

    #[test]
    fn homogeneous_int_list_typechecks(
        elems in prop::collection::vec((i64::MIN + 1)..=i64::MAX, 1..8),
    ) {
        let body = elems
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!("val xs: List<Int> = [{body}]");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn empty_list_without_annotation_always_errors(name in ident_strategy()) {
        let src = format!("val {name} = []");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_err());
    }

    #[test]
    fn string_plus_string_typechecks(
        a in "[a-zA-Z0-9 ]{0,32}",
        b in "[a-zA-Z0-9 ]{0,32}",
    ) {
        let src = format!("val s: String = \"{a}\" + \"{b}\"");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn matching_list_concat_typechecks(
        elems_a in prop::collection::vec((i64::MIN + 1)..=i64::MAX, 1..6),
        elems_b in prop::collection::vec((i64::MIN + 1)..=i64::MAX, 1..6),
    ) {
        let body_a = elems_a.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
        let body_b = elems_b.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
        let src = format!("val xs: List<Int> = [{body_a}] ++ [{body_b}]");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn var_ref_propagates_type(
        a in ident_strategy(),
        b in ident_strategy(),
        n in any::<i32>(),
    ) {
        // `val a = N; val b: Int = a` typechecks for any non-keyword
        // `a` and any non-keyword `b != a`.
        prop_assume!(a != b);
        let src = format!("val {a} = {n}\nval {b}: Int = {a}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn random_string_int_map_typechecks(
        keys in prop::collection::hash_set("[a-zA-Z0-9_]{1,8}", 1..6),
        values in prop::collection::vec((i64::MIN + 1)..=i64::MAX, 1..6),
    ) {
        let n = keys.len().min(values.len());
        let entries = keys
            .iter()
            .zip(values.iter())
            .take(n)
            .map(|(k, v)| format!("\"{k}\": {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!("val m: Map<String, Int> = {{{entries}}}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn empty_map_with_map_annotation_typechecks(
        k in prop_oneof![Just(Type::String), Just(Type::Int)],
        v in ty_strategy(),
        name in ident_strategy(),
    ) {
        let src = format!("val {name}: Map<{k}, {v}> = {{}}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn empty_map_without_annotation_always_errors(name in ident_strategy()) {
        let src = format!("val {name} = {{}}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_err());
    }

    #[test]
    fn random_int_args_call_typechecks(
        a in (i64::MIN + 1)..=i64::MAX,
        b in (i64::MIN + 1)..=i64::MAX,
    ) {
        let src = format!("fn add(x: Int, y: Int): Int {{ x + y }}\nval r: Int = add({a}, {b})");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn default_omitted_typechecks_for_random_ints(n in (i64::MIN + 1)..=i64::MAX) {
        let src = format!(
            "fn pad(value: Int, slack: Int = 5): Int {{ value + slack }}\nval r: Int = pad({n})"
        );
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn named_args_in_any_order_typecheck(
        a in (i64::MIN + 1)..=i64::MAX,
        b in (i64::MIN + 1)..=i64::MAX,
    ) {
        // a=A,b=B and b=B,a=A produce the same typecheck outcome.
        let s1 = format!("fn f(a: Int, b: Int): Int {{ a + b }}\nval r: Int = f(a = {a}, b = {b})");
        let s2 = format!("fn f(a: Int, b: Int): Int {{ a + b }}\nval r: Int = f(b = {b}, a = {a})");
        prop_assert!(check(&parse(&s1).unwrap()).is_ok());
        prop_assert!(check(&parse(&s2).unwrap()).is_ok());
    }

    #[test]
    fn forward_reference_errors(a in ident_strategy(), b in ident_strategy()) {
        // `val a = b` before `b` is defined errors.
        prop_assume!(a != b);
        let src = format!("val {a} = {b}\nval {b} = 1");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_err());
    }

    #[test]
    fn empty_list_with_list_annotation_typechecks(
        elem_ty in ty_strategy(),
        name in ident_strategy(),
    ) {
        let src = format!("val {name}: List<{elem_ty}> = []");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    // ---------- comparisons ----------

    #[test]
    fn random_int_equality_typechecks(
        a in (i64::MIN + 1)..=i64::MAX,
        b in (i64::MIN + 1)..=i64::MAX,
        op in prop_oneof![Just("=="), Just("!="), Just("<"), Just("<="), Just(">"), Just(">=")],
    ) {
        let src = format!("val r: Boolean = {a} {op} {b}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn random_string_equality_typechecks(
        a in "[a-zA-Z0-9 ]{0,16}",
        b in "[a-zA-Z0-9 ]{0,16}",
        op in prop_oneof![Just("=="), Just("!="), Just("<"), Just("<="), Just(">"), Just(">=")],
    ) {
        let src = format!("val r: Boolean = \"{a}\" {op} \"{b}\"");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn ordering_on_boolean_always_errors(
        a in any::<bool>(),
        b in any::<bool>(),
        op in prop_oneof![Just("<"), Just("<="), Just(">"), Just(">=")],
    ) {
        let src = format!("val r = {a} {op} {b}");
        let prog = parse(&src).expect("parse should succeed");
        let errs = check(&prog).expect_err("ordering on bool must fail");
        prop_assert!(errs[0].message.contains("requires"));
    }

    #[test]
    fn comparison_in_if_cond_random_ints_typechecks(
        a in (i64::MIN + 1)..=i64::MAX,
        b in (i64::MIN + 1)..=i64::MAX,
    ) {
        let src = format!("val r: Int = if {a} < {b} {{ 1 }} else {{ 2 }}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    // ---------- conditionals ----------

    #[test]
    fn if_with_random_int_branches_typechecks(
        a in (i64::MIN + 1)..=i64::MAX,
        b in (i64::MIN + 1)..=i64::MAX,
        cond in any::<bool>(),
    ) {
        let src = format!("val r: Int = if {cond} {{ {a} }} else {{ {b} }}");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn if_with_mismatched_branch_types_always_errors(
        n in (i64::MIN + 1)..=i64::MAX,
        s in "[a-zA-Z0-9 ]{0,16}",
    ) {
        // Int then-branch, String else-branch never share a type.
        let src = format!("val r = if true {{ {n} }} else {{ \"{s}\" }}");
        let prog = parse(&src).expect("parse should succeed");
        let errs = check(&prog).expect_err("should fail");
        prop_assert!(
            errs[0].message.contains("mismatched types"),
            "got: {}", errs[0].message
        );
    }

    #[test]
    fn if_with_non_boolean_cond_always_errors(n in (i64::MIN + 1)..=i64::MAX) {
        let src = format!("val r: Int = if {n} {{ 1 }} else {{ 2 }}");
        let prog = parse(&src).expect("parse should succeed");
        let errs = check(&prog).expect_err("should fail");
        prop_assert!(
            errs[0].message.contains("expected `Boolean`"),
            "got: {}", errs[0].message
        );
    }

    // ---------- resources & reconcile ----------

    #[test]
    fn random_symlink_pair_typechecks(
        from in "[a-zA-Z0-9 _./]{1,32}",
        to in "[a-zA-Z0-9 _./]{1,32}",
    ) {
        let src = format!(
            "val s: Symlink = symlink(source = \"{to}\", target = \"{from}\")\nreconcile s"
        );
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn reconcile_of_int_always_errors(n in (i64::MIN + 1)..=i64::MAX) {
        let src = format!("reconcile {n}");
        let prog = parse(&src).expect("parse should succeed");
        let errs = check(&prog).expect_err("should fail");
        prop_assert!(
            errs[0].message.contains("`reconcile` expects a resource or list of resources"),
            "got: {}", errs[0].message
        );
    }

    #[test]
    fn reconcile_chain_of_symlink_vars_typechecks(
        names in prop::collection::vec(ident_strategy(), 1..6),
    ) {
        // De-duplicate names so the val declarations don't collide.
        let mut seen = std::collections::HashSet::new();
        let dedup: Vec<_> = names.into_iter().filter(|n| seen.insert(n.clone())).collect();
        prop_assume!(!dedup.is_empty());
        let mut src = String::new();
        for (i, name) in dedup.iter().enumerate() {
            writeln!(
                src,
                "val {name}: Symlink = symlink(source = \"~/to-{i}\", target = \"~/from-{i}\")"
            )
            .expect("write to String");
        }
        write!(src, "reconcile {}", dedup.join(" -> ")).expect("write to String");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok(), "check failed for: {src}");
    }

    #[test]
    fn reconcile_block_of_symlinks_matches_consecutive_reconciles(
        names in prop::collection::vec(ident_strategy(), 1..5),
    ) {
        // The block form should be semantically equivalent to N
        // consecutive bare `reconcile` statements: both must check
        // with no diagnostics.
        let mut seen = std::collections::HashSet::new();
        let dedup: Vec<_> = names.into_iter().filter(|n| seen.insert(n.clone())).collect();
        prop_assume!(!dedup.is_empty());

        let mut decls = String::new();
        for (i, name) in dedup.iter().enumerate() {
            writeln!(
                decls,
                "val {name}: Symlink = symlink(source = \"~/to-{i}\", target = \"~/from-{i}\")"
            )
            .expect("write to String");
        }

        let block_src = format!(
            "{decls}reconcile {{\n{}\n}}\n",
            dedup
                .iter()
                .map(|n| format!("  {n}"))
                .collect::<Vec<_>>()
                .join(";\n"),
        );
        let mut separate_src = decls.clone();
        for n in &dedup {
            writeln!(separate_src, "reconcile {n}").expect("write to String");
        }

        prop_assert!(
            check(&parse(&block_src).expect("block parses")).is_ok(),
            "block form failed: {block_src}"
        );
        prop_assert!(
            check(&parse(&separate_src).expect("separate parses")).is_ok(),
            "separate form failed: {separate_src}"
        );
    }

    #[test]
    fn arbitrary_mix_of_resource_kinds_in_chain_typechecks(
        kinds in prop::collection::vec(0u8..3, 1..6),
    ) {
        // Bind each step as a `Resource` val and chain them with `->`.
        // The chain checker walks each step against `is_reconcilable`,
        // which accepts any specific resource and `Resource` itself.
        let mut decls = String::new();
        let mut names = Vec::new();
        for (i, k) in kinds.iter().enumerate() {
            let name = format!("r{i}");
            let value = resource_value(*k, i);
            writeln!(decls, "val {name}: Resource = {value}").expect("write to String");
            names.push(name);
        }
        let src = format!("{decls}reconcile {}", names.join(" -> "));
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok(), "check failed for: {src}");
    }

    #[test]
    fn arbitrary_mix_of_resource_kinds_infers_list_of_resource(
        kinds in prop::collection::vec(0u8..3, 2..6),
    ) {
        // Build a list literal with elements drawn from the two
        // resource kinds. As long as at least two kinds appear, the
        // inferred element type must be `Resource` and the list must
        // satisfy a `List<Resource>` annotation. Homogeneous draws are
        // skipped because they synthesize the specific kind, not
        // `Resource`, and only the `List<<specific>> <: List<Resource>`
        // path is exercised.
        let distinct: std::collections::HashSet<u8> = kinds.iter().copied().collect();
        prop_assume!(distinct.len() >= 2);
        let mut entries = String::new();
        for (i, k) in kinds.iter().enumerate() {
            let frag = resource_value(*k, i);
            entries.push_str(&frag);
            entries.push_str(", ");
        }
        let src = format!("val xs: List<Resource> = [{entries}]\nreconcile xs");
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok(), "check failed for: {src}");
    }

    #[test]
    fn reconcile_chain_with_int_step_always_errors(
        n in (i64::MIN + 1)..=i64::MAX,
        name in ident_strategy(),
    ) {
        // A chain mixing a symlink and a non-resource always fails.
        let src = format!(
            "val {name}: Symlink = symlink(source = \"b\", target = \"a\")\nreconcile {name} -> {n}"
        );
        let prog = parse(&src).expect("parse should succeed");
        let errs = check(&prog).expect_err("should fail");
        prop_assert!(
            errs.iter().any(|d| d.message.contains("found `Int`")),
            "got: {errs:?}"
        );
    }

    #[test]
    fn reconcile_of_homogeneous_symlink_list_typechecks(
        names in prop::collection::vec("[a-zA-Z0-9_]{1,8}", 0..6),
    ) {
        // Build a List<Symlink> via repeated calls and reconcile the whole list.
        // Length 0 stays well-typed because the explicit annotation drives
        // bidirectional inference into the empty literal.
        let body = if names.is_empty() {
            String::new()
        } else {
            names
                .iter()
                .map(|n| format!("symlink(source = \"~/.{n}\", target = \"~/{n}\")"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let src = format!(
            "val xs: List<Symlink> = [{body}]\nreconcile xs"
        );
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok());
    }

    #[test]
    fn parens_do_not_change_typing(
        ty in numeric_ty_strategy(),
        a in 0i32..1000,
        b in 0i32..1000,
        c in 1i32..1000,
    ) {
        let lit = match ty {
            Type::Int => String::new(),
            Type::Double => String::from(".0"),
            _ => unreachable!(),
        };
        let plain = format!("val r: {ty} = {a}{lit} + {b}{lit} * {c}{lit}");
        let parens = format!("val r: {ty} = {a}{lit} + ({b}{lit} * {c}{lit})");
        prop_assert!(check(&parse(&plain).unwrap()).is_ok());
        prop_assert!(check(&parse(&parens).unwrap()).is_ok());
    }

    // ---------- for loops ----------

    #[test]
    fn for_over_random_int_list_typechecks(
        items in prop::collection::vec((i64::MIN + 1)..=i64::MAX, 0..6),
    ) {
        let body = items
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        // The loop's body uses the bound element to construct a File,
        // exercising the binding's type. A `Void` loop with body
        // `reconcile <File>` is well-typed.
        let src = format!(
            "val xs: List<Int> = [{body}]\nfor x in xs {{ reconcile template(source = \"tmpl.tpl\", target = \"/tmp/${{x}}\", vars = {{\"body\": \"\"}}) }}"
        );
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok(), "check failed for: {src}");
    }

    #[test]
    fn for_over_random_string_int_map_typechecks(
        entries in prop::collection::vec(("[a-zA-Z]{1,8}", 0i64..1000), 0..6),
    ) {
        // De-duplicate by key so the map literal has no repeats.
        let mut seen = std::collections::HashSet::new();
        let dedup: Vec<_> = entries
            .into_iter()
            .filter(|(k, _)| seen.insert(k.clone()))
            .collect();
        let body = dedup
            .iter()
            .map(|(k, v)| format!("\"{k}\": {v}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            "val m: Map<String, Int> = {{{body}}}\nfor (k, v) in m {{ reconcile template(source = \"tmpl.tpl\", target = \"/tmp/${{k}}\", vars = {{\"body\": \"${{v}}\"}}) }}"
        );
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok(), "check failed for: {src}");
    }

    #[test]
    fn for_with_non_iterable_always_errors(n in (i64::MIN + 1)..=i64::MAX) {
        // An `Int` is neither a `List<T>` nor a `Map<K, V>`.
        let src = format!("val n: Int = {n}\nfor x in n {{ reconcile x }}");
        let prog = parse(&src).expect("parse should succeed");
        let errs = check(&prog).expect_err("should fail");
        prop_assert!(
            errs[0].message.contains("`for`") && errs[0].message.contains("List"),
            "got: {}", errs[0].message
        );
    }

    #[test]
    fn for_value_producing_body_always_errors(n in (i64::MIN + 1)..=i64::MAX) {
        // Body trailing of type `Int` is not `Void`, so the loop's
        // body fails the Void check.
        let src = format!("val xs: List<Int> = [{n}]\nfor x in xs {{ x }}");
        let prog = parse(&src).expect("parse should succeed");
        let errs = check(&prog).expect_err("should fail");
        prop_assert!(
            errs[0].message.contains("Void"),
            "got: {}", errs[0].message
        );
    }

    // ---------- structs / unions / match ----------

    #[test]
    fn struct_construction_round_trip(
        name in ident_strategy().prop_filter("must start uppercase",
            |s| s.chars().next().is_some_and(|c| c.is_ascii_uppercase())),
        n_fields in 1usize..=6,
    ) {
        // Generate `struct Name { f0: Int, f1: Int, ... }` plus a
        // construction val. The constructor should typecheck cleanly
        // when the right number of `Int`-typed positional args is
        // supplied.
        let fields = (0..n_fields)
            .map(|i| format!("f{i}: Int"))
            .collect::<Vec<_>>()
            .join(", ");
        let args = (0..n_fields)
            .map(|i| format!("{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            "struct {name} {{ {fields} }}\nval v: {name} = {name}({args})"
        );
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok(), "check failed for: {src}");
    }

    #[test]
    fn match_full_union_coverage_typechecks(n_variants in 1usize..=5) {
        // Build `type Color = "v0" | "v1" | ...` and a fn whose
        // `match` lists every variant. Should typecheck without a
        // wildcard (the union is closed and exhaustively covered).
        let variants_decl = (0..n_variants)
            .map(|i| format!("\"v{i}\""))
            .collect::<Vec<_>>()
            .join(" | ");
        let arms = (0..n_variants)
            .map(|i| format!("\"v{i}\" => {i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            "type Color = {variants_decl}\n\
             fn label(c: Color): Int {{\n\
               match c {{ {arms} }}\n\
             }}\n",
        );
        let prog = parse(&src).expect("parse should succeed");
        prop_assert!(check(&prog).is_ok(), "check failed for: {src}");
    }

    #[test]
    fn match_one_missing_variant_always_errors(n_variants in 2usize..=5) {
        // Same as above, but drop the last arm. Without a wildcard
        // the checker must flag the missing variant.
        let variants_decl = (0..n_variants)
            .map(|i| format!("\"v{i}\""))
            .collect::<Vec<_>>()
            .join(" | ");
        let arms = (0..n_variants - 1)
            .map(|i| format!("\"v{i}\" => {i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let src = format!(
            "type Color = {variants_decl}\n\
             fn label(c: Color): Int {{\n\
               match c {{ {arms} }}\n\
             }}\n",
        );
        let prog = parse(&src).expect("parse should succeed");
        let errs = check(&prog).expect_err("missing variant should fail");
        prop_assert!(
            errs.iter().any(|d| d.message.contains("non-exhaustive")),
            "got: {:?}", errs
        );
    }
}
