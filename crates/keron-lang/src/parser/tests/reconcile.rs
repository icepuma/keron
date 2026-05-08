//! `reconcile` declaration parser tests.

use super::ok;
use crate::{
    ast::{Expr, Item, ReconcileDecl, Spanned},
    parser::parse,
};

fn first_reconcile(src: &str) -> ReconcileDecl {
    let prog = ok(src);
    prog.items
        .into_iter()
        .find_map(|i| match i {
            Item::Reconcile(r) => Some(r),
            _ => None,
        })
        .expect("expected at least one reconcile item")
}

fn only_chain(r: &ReconcileDecl) -> &[Spanned<Expr>] {
    assert_eq!(r.chains.len(), 1, "expected a single chain, got {r:?}");
    &r.chains[0]
}

fn only_step(r: &ReconcileDecl) -> &Spanned<Expr> {
    let chain = only_chain(r);
    assert_eq!(chain.len(), 1, "expected a single step, got {chain:?}");
    &chain[0]
}

#[test]
fn reconcile_call_expr_parses() {
    let r = first_reconcile(r#"reconcile symlink(from = "a", to = "b")"#);
    assert!(matches!(only_step(&r).node, Expr::Call { .. }));
}

#[test]
fn reconcile_list_parses() {
    let r = first_reconcile(
        r#"reconcile [symlink(from = "a", to = "b"), symlink(from = "c", to = "d")]"#,
    );
    let Expr::List(items) = &only_step(&r).node else {
        panic!("expected list");
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn reconcile_var_parses() {
    let r = first_reconcile("val x: Symlink = symlink(from = \"a\", to = \"b\")\nreconcile x");
    assert!(matches!(only_step(&r).node, Expr::Var(_)));
}

#[test]
fn reconcile_user_fn_call_parses() {
    let src = "
        fn make(): Symlink {
            symlink(from = \"a\", to = \"b\")
        }
        reconcile make()
    ";
    let r = first_reconcile(src);
    assert!(matches!(only_step(&r).node, Expr::Call { .. }));
}

#[test]
fn rejects_reconcile_without_expr() {
    assert!(parse("reconcile").is_err());
}

#[test]
fn rejects_val_named_reconcile() {
    assert!(parse("val reconcile = 1").is_err());
}

#[test]
fn rejects_fn_named_reconcile() {
    assert!(parse("fn reconcile(): Int { 1 }").is_err());
}

#[test]
fn rejects_val_named_capitalized_type() {
    assert!(parse("val Symlink = 1").is_err());
    assert!(parse("val File = 1").is_err());
    assert!(parse("val Directory = 1").is_err());
}

#[test]
fn rejects_fn_named_capitalized_type() {
    assert!(parse("fn Symlink(): Int { 1 }").is_err());
}

#[test]
fn multiple_reconcile_decls_parse() {
    let prog = ok("
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: File = file(path = \"p\", content = \"c\")
        reconcile a
        reconcile b
    ");
    assert_eq!(prog.items.len(), 4);
    let reconcile_count = prog
        .items
        .iter()
        .filter(|i| matches!(i, Item::Reconcile(_)))
        .count();
    assert_eq!(reconcile_count, 2);
}

// ---------- inline `~>` chain ----------

#[test]
fn reconcile_chain_two_parses() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: Symlink = symlink(from = \"p\", to = \"q\")
        reconcile a ~> b
        ",
    );
    let chain = only_chain(&r);
    assert_eq!(chain.len(), 2);
    assert!(matches!(chain[0].node, Expr::Var(_)));
    assert!(matches!(chain[1].node, Expr::Var(_)));
}

#[test]
fn reconcile_chain_three_parses() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: Symlink = symlink(from = \"p\", to = \"q\")
        val c: Symlink = symlink(from = \"u\", to = \"v\")
        reconcile a ~> b ~> c
        ",
    );
    assert_eq!(only_chain(&r).len(), 3);
}

#[test]
fn reconcile_chain_with_call_parses() {
    let src = r#"reconcile symlink(from = "a", to = "b") ~> symlink(from = "c", to = "d")"#;
    let r = first_reconcile(src);
    let chain = only_chain(&r);
    assert_eq!(chain.len(), 2);
    for step in chain {
        assert!(matches!(step.node, Expr::Call { .. }));
    }
}

#[test]
fn reconcile_chain_with_list_step_parses() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: Symlink = symlink(from = \"p\", to = \"q\")
        val c: Symlink = symlink(from = \"u\", to = \"v\")
        reconcile [a, b] ~> c
        ",
    );
    let chain = only_chain(&r);
    assert_eq!(chain.len(), 2);
    assert!(matches!(chain[0].node, Expr::List(_)));
    assert!(matches!(chain[1].node, Expr::Var(_)));
}

#[test]
fn rejects_chain_with_trailing_arrow() {
    assert!(parse("reconcile a ~>").is_err());
}

#[test]
fn rejects_chain_with_missing_head() {
    assert!(parse("reconcile ~> a").is_err());
}

#[test]
fn rejects_chain_outside_reconcile() {
    assert!(parse("val x = a ~> b").is_err());
}

// ---------- block form ----------

#[test]
fn reconcile_block_single_step_parses() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        reconcile { a }
        ",
    );
    assert_eq!(r.chains.len(), 1);
    assert_eq!(r.chains[0].len(), 1);
}

#[test]
fn reconcile_block_three_chains_parses() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: Symlink = symlink(from = \"p\", to = \"q\")
        val c: Symlink = symlink(from = \"u\", to = \"v\")
        reconcile {
          a;
          b;
          c
        }
        ",
    );
    assert_eq!(r.chains.len(), 3);
    for chain in &r.chains {
        assert_eq!(chain.len(), 1);
    }
}

#[test]
fn reconcile_block_chain_can_use_arrow_within_step() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: Symlink = symlink(from = \"p\", to = \"q\")
        val c: Symlink = symlink(from = \"u\", to = \"v\")
        reconcile {
          a;
          b ~> c
        }
        ",
    );
    assert_eq!(r.chains.len(), 2);
    assert_eq!(r.chains[0].len(), 1);
    assert_eq!(r.chains[1].len(), 2);
}

#[test]
fn reconcile_block_allows_trailing_semicolon() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: Symlink = symlink(from = \"p\", to = \"q\")
        reconcile {
          a;
          b;
        }
        ",
    );
    assert_eq!(r.chains.len(), 2);
}

#[test]
fn reconcile_block_tolerates_blank_lines_and_comments() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: Symlink = symlink(from = \"p\", to = \"q\")
        reconcile {
          # leading comment
          a;   # trailing comment

          b;
        }
        ",
    );
    assert_eq!(r.chains.len(), 2);
}

#[test]
fn rejects_empty_reconcile_block() {
    assert!(parse("reconcile { }").is_err());
}

#[test]
fn rejects_reconcile_block_with_missing_separator() {
    assert!(
        parse(
            "
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: Symlink = symlink(from = \"p\", to = \"q\")
        reconcile { a b }
        "
        )
        .is_err()
    );
}

#[test]
fn block_inside_if_branch_parses() {
    let prog = ok("
        val a: Symlink = symlink(from = \"x\", to = \"y\")
        val b: Symlink = symlink(from = \"p\", to = \"q\")
        if true {
          reconcile {
            a;
            b
          }
        }
        ");
    let count = prog
        .items
        .iter()
        .filter(|i| matches!(i, Item::ExprStmt(_)))
        .count();
    assert_eq!(count, 1);
}

#[test]
fn reconcile_chain_four_parses() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"a\", to = \"b\")
        val b: Symlink = symlink(from = \"c\", to = \"d\")
        val c: Symlink = symlink(from = \"e\", to = \"f\")
        val d: Symlink = symlink(from = \"g\", to = \"h\")
        reconcile a ~> b ~> c ~> d
        ",
    );
    assert_eq!(only_chain(&r).len(), 4);
}

#[test]
fn reconcile_block_one_line_parses() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"a\", to = \"b\")
        val b: Symlink = symlink(from = \"c\", to = \"d\")
        reconcile { a; b }
        ",
    );
    assert_eq!(r.chains.len(), 2);
    for chain in &r.chains {
        assert_eq!(chain.len(), 1);
    }
}

#[test]
fn reconcile_block_inside_fn_body_parses() {
    let prog = ok("
        fn install(): Void {
            val a: Symlink = symlink(from = \"a\", to = \"b\")
            val b: Symlink = symlink(from = \"c\", to = \"d\")
            reconcile {
              a;
              b
            }
        }
        ");
    let fn_count = prog
        .items
        .iter()
        .filter(|i| matches!(i, Item::Fn(_)))
        .count();
    assert_eq!(fn_count, 1);
}

#[test]
fn block_with_chain_only_step_parses_one_chain() {
    let r = first_reconcile(
        "
        val a: Symlink = symlink(from = \"a\", to = \"b\")
        val b: Symlink = symlink(from = \"c\", to = \"d\")
        val c: Symlink = symlink(from = \"e\", to = \"f\")
        reconcile { a ~> b ~> c }
        ",
    );
    assert_eq!(r.chains.len(), 1);
    assert_eq!(r.chains[0].len(), 3);
}
