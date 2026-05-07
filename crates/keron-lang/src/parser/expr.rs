//! Expression parser with PEMDAS precedence.
//!
//! Grammar (lowest to highest precedence):
//!
//! ```text
//! expr           := additive
//! additive       := multiplicative (('+' | '-') multiplicative)*  -- left-assoc
//! multiplicative := unary (('*' | '/') unary)*                    -- left-assoc
//! unary          := '-' unary | power
//! power          := atom ('**' unary)?                            -- right-assoc
//! atom           := literal | '(' expr ')'
//! ```
//!
//! `**` binds tighter than unary `-` (matching Python/math), so
//! `-2 ** 2` parses as `-(2 ** 2)`. Negative literals are not part of
//! the literal grammar; `-7` is `Unary(Neg, Int(7))`. This means
//! `-9223372036854775808` is unrepresentable (the positive form
//! overflows `i64`), an accepted edge case.

use chumsky::prelude::*;

use crate::ast::{BinOp, CallArg, Expr, Literal, MapEntry, Spanned, UnaryOp};

use super::{
    string::string_expr,
    util::{Extra, ident, pad, span_to_range},
};

pub(super) fn expr<'src>() -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone {
    recursive(|expr| {
        let atom = choice((
            string_expr(expr.clone()).padded_by(pad()),
            non_string_literal_expr(),
            list_atom(expr.clone()),
            map_atom(expr.clone()),
            if_atom(expr.clone()),
            var_or_call(expr.clone()),
            expr.clone()
                .delimited_by(just('(').padded_by(pad()), just(')').padded_by(pad())),
        ));

        // unary and power are mutually recursive: unary's RHS recurses on
        // unary; power's RHS is unary; unary's fall-through is power.
        let unary = recursive(|unary| {
            let power = atom
                .clone()
                .then(
                    just("**")
                        .padded_by(pad())
                        .ignore_then(unary.clone())
                        .or_not(),
                )
                .map(|(lhs, rhs)| match rhs {
                    None => lhs,
                    Some(rhs) => merge_binary(BinOp::Pow, lhs, rhs),
                });

            choice((
                just('-')
                    .padded_by(pad())
                    .ignore_then(unary)
                    .map_with(|operand, e| Spanned {
                        node: Expr::Unary {
                            op: UnaryOp::Neg,
                            operand: Box::new(operand),
                        },
                        span: span_to_range(e.span()),
                    }),
                power,
            ))
        });

        let mul_op = choice((just('*').to(BinOp::Mul), just('/').to(BinOp::Div))).padded_by(pad());
        let multiplicative = unary
            .clone()
            .then(mul_op.then(unary).repeated().collect::<Vec<_>>())
            .map(|(lhs, ops)| ops.into_iter().fold(lhs, fold_left));

        let add_op = choice((
            just("++").to(BinOp::Concat),
            just('+').to(BinOp::Add),
            just('-').to(BinOp::Sub),
        ))
        .padded_by(pad());
        multiplicative
            .clone()
            .then(add_op.then(multiplicative).repeated().collect::<Vec<_>>())
            .map(|(lhs, ops)| ops.into_iter().fold(lhs, fold_left))
    })
}

fn spanned_ident<'src>() -> impl Parser<'src, &'src str, Spanned<String>, Extra<'src>> + Clone {
    ident().map_with(|name, e| Spanned {
        node: name,
        span: span_to_range(e.span()),
    })
}

fn list_atom<'src, P>(expr: P) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    expr.separated_by(just(',').padded_by(pad()))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just('[').padded_by(pad()), just(']').padded_by(pad()))
        .map_with(|items, e| Spanned {
            node: Expr::List(items),
            span: span_to_range(e.span()),
        })
}

fn map_atom<'src, P>(expr: P) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let entry = expr
        .clone()
        .then_ignore(just(':').padded_by(pad()))
        .then(expr)
        .map_with(|(key, value), e| MapEntry {
            key,
            value,
            span: span_to_range(e.span()),
        });

    entry
        .separated_by(just(',').padded_by(pad()))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just('{').padded_by(pad()), just('}').padded_by(pad()))
        .map_with(|entries, e| Spanned {
            node: Expr::Map(entries),
            span: span_to_range(e.span()),
        })
}

/// `if cond { a } else { b }` and `if cond { a } else if cond2 { b }
/// else { c }` chains. Each branch block contains exactly one
/// expression; `else` is mandatory because keron is expression-based.
/// `else if` is parsed right-associatively by recursing into the same
/// parser for the else branch.
fn if_atom<'src, P>(expr: P) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let block = expr
        .clone()
        .delimited_by(just('{').padded_by(pad()), just('}').padded_by(pad()));

    recursive(|if_chain| {
        let else_branch = text::keyword("else")
            .padded_by(pad())
            .ignore_then(choice((if_chain, block.clone())));

        text::keyword("if")
            .padded_by(pad())
            .ignore_then(expr)
            .then(block)
            .then(else_branch)
            .map_with(|((cond, then_branch), else_branch), e| Spanned {
                node: Expr::If {
                    cond: Box::new(cond),
                    then_branch: Box::new(then_branch),
                    else_branch: Box::new(else_branch),
                },
                span: span_to_range(e.span()),
            })
    })
}

/// Combined Var/Call parser: a bare ident is a Var; an ident followed
/// by `(args)` is a Call. Sharing one parser avoids the consume-then-
/// fail problem when `(` doesn't follow.
fn var_or_call<'src, P>(expr: P) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    spanned_ident()
        .then(arg_list(expr).or_not())
        .map_with(|(callee, maybe_args), e| {
            let span = span_to_range(e.span());
            let node = match maybe_args {
                Some(args) => Expr::Call { callee, args },
                None => Expr::Var(callee.node),
            };
            Spanned { node, span }
        })
        .padded_by(pad())
}

fn arg_list<'src, P>(expr: P) -> impl Parser<'src, &'src str, Vec<CallArg>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    call_arg(expr)
        .separated_by(just(',').padded_by(pad()))
        .allow_trailing()
        .collect::<Vec<_>>()
        .delimited_by(just('(').padded_by(pad()), just(')').padded_by(pad()))
}

fn call_arg<'src, P>(expr: P) -> impl Parser<'src, &'src str, CallArg, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    expr.clone()
        .then(just('=').padded_by(pad()).ignore_then(expr).or_not())
        .try_map(|(first, second), span| {
            if let Some(value) = second {
                let Expr::Var(name) = first.node else {
                    return Err(Rich::custom(
                        span,
                        "named-argument LHS must be an identifier",
                    ));
                };
                Ok(CallArg {
                    name: Some(Spanned {
                        node: name,
                        span: first.span,
                    }),
                    value,
                    span: span.start()..span.end(),
                })
            } else {
                let arg_span = first.span.clone();
                Ok(CallArg {
                    name: None,
                    value: first,
                    span: arg_span,
                })
            }
        })
}

fn non_string_literal_expr<'src>()
-> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone {
    let bool_lit = choice((
        text::keyword("true").to(Literal::Boolean(true)),
        text::keyword("false").to(Literal::Boolean(false)),
    ));
    let num_lit = number_literal();
    choice((bool_lit, num_lit))
        .map_with(|lit, e| Spanned {
            node: Expr::Literal(lit),
            span: span_to_range(e.span()),
        })
        .padded_by(pad())
}

fn number_literal<'src>() -> impl Parser<'src, &'src str, Literal, Extra<'src>> + Clone {
    let int_part = text::int(10);
    let frac = just('.').then(text::digits(10));
    int_part
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

fn fold_left(lhs: Spanned<Expr>, (op, rhs): (BinOp, Spanned<Expr>)) -> Spanned<Expr> {
    merge_binary(op, lhs, rhs)
}

fn merge_binary(op: BinOp, lhs: Spanned<Expr>, rhs: Spanned<Expr>) -> Spanned<Expr> {
    let span = lhs.span.start..rhs.span.end;
    Spanned {
        node: Expr::Binary {
            op,
            lhs: Box::new(lhs),
            rhs: Box::new(rhs),
        },
        span,
    }
}
