//! Parser unit tests, grouped by topic.

mod arithmetic;
mod calls;
mod comparisons;
mod conditional;
mod decl;
mod fn_decl;
mod maps;
mod reconcile;
mod string;

use crate::{
    ast::{Expr, Item, Literal, Program, Spanned, ValDecl},
    parser::parse,
};

fn ok(src: &str) -> Program {
    parse(src).expect("parse should succeed")
}

fn first_val(prog: &Program) -> &ValDecl {
    match prog.items.first().expect("at least one item") {
        Item::Val(v) => v,
        Item::Use(_) | Item::Fn(_) | Item::Reconcile(_) | Item::ExprStmt(_) => {
            panic!("expected a val item")
        }
    }
}

fn lit(prog: &Program) -> &Literal {
    let Expr::Literal(l) = &first_val(prog).value.node else {
        panic!("expected a literal expression");
    };
    l
}

fn expr_of(src: &str) -> Spanned<Expr> {
    let prog = ok(src);
    first_val(&prog).value.clone()
}
