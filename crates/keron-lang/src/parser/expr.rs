//! Expression parser with PEMDAS precedence.
//!
//! Grammar (lowest to highest precedence):
//!
//! ```text
//! expr           := disjunction
//! disjunction    := conjunction ('||' conjunction)*               -- left-assoc
//! conjunction    := comparison ('&&' comparison)*                 -- left-assoc
//! comparison     := coalesce (cmp_op coalesce)*                   -- left-assoc
//! cmp_op         := '==' | '!=' | '<=' | '>=' | '<' | '>'
//! coalesce       := additive ('??' coalesce)?                     -- right-assoc
//! additive       := multiplicative (('+' | '-' | '++') mul)*      -- left-assoc
//! multiplicative := unary (('*' | '/') unary)*                    -- left-assoc
//! unary          := '-' unary | power
//! power          := postfix ('**' unary)?                         -- right-assoc
//! postfix        := atom ('.' ident)*                             -- left-assoc field access
//! atom           := literal | '(' expr ')' | match_expr | …
//! ```
//!
//! `**` binds tighter than unary `-` (matching Python/math), so
//! `-2 ** 2` parses as `-(2 ** 2)`. Negative literals are not part of
//! the literal grammar; `-7` is `Unary(Neg, Int(7))`. This means
//! `-9223372036854775808` is unrepresentable (the positive form
//! overflows `i64`), an accepted edge case.
//!
//! Field access is *postfix* — tighter than unary, so `-p.x` parses
//! as `-(p.x)`. Chains fold left-associatively into nested
//! [`Expr::Field`] nodes (e.g. `a.b.c` → `Field(Field(a, b), c)`).
//!
//! `??` is right-associative and sits between additive and
//! comparison (matching Swift/Kotlin Elvis). This makes the common
//! "fallback then compare" pattern parens-free:
//!
//! ```text
//! env("X") ?? "default" == "match"   // (env("X") ?? "default") == "match"
//! ```
//!
//! Mixing `??` with `+` requires parens because `+` is tighter — so
//! `env("X") ?? home + "/etc"` parses as `env("X") ?? (home + "/etc")`
//! (the fallback includes `/etc`, the success path does not). Concat
//! around a fallback should be written `(env("X") ?? home) + "/etc"`.
//!
//! `&&` and `||` are short-circuit; both `Boolean`-only. `&&` binds
//! tighter than `||` (so `a || b && c` is `a || (b && c)`); both bind
//! looser than comparison.

use chumsky::prelude::*;

use crate::ast::{
    BinOp, Block, CallArg, Expr, ForPattern, Literal, MapEntry, Span, Spanned, UnaryOp,
};

use super::{
    block::block,
    match_expr::match_expr,
    string::string_expr,
    util::{Extra, ident, pad, span_to_range},
};

pub(super) fn expr<'src>() -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone {
    recursive(|expr| {
        // `atom` is also labelled (in addition to the outer expression
        // chain below) so that secondary errors logged from inside
        // `additive.repeated()` — e.g. after `1 +` when the next
        // multiplicand fails to start — collapse to "expected
        // expression" too. chumsky's `Parser::labelled` only relabels
        // alt errors whose position equals the labelled parser's start
        // position, so a label on the *outermost* parser doesn't reach
        // nested-position failures recovered by `.repeated()`.
        let atom = choice((
            string_expr(expr.clone()).padded_by(pad()),
            non_string_literal_expr(),
            list_atom(expr.clone()),
            map_atom(expr.clone()),
            if_atom(expr.clone()),
            for_atom(expr.clone()),
            match_expr(expr.clone()).padded_by(pad()),
            var_or_call(expr.clone()),
            expr.clone()
                .delimited_by(just('(').padded_by(pad()), just(')').padded_by(pad())),
        ))
        .labelled("expression");

        let postfix = atom
            .then(
                just('.')
                    .padded_by(pad())
                    .ignore_then(spanned_ident())
                    .repeated()
                    .collect::<Vec<_>>(),
            )
            .validate(|(receiver, fields), e, emitter| {
                // Same defense as `left_assoc`: a `.f` chain parses
                // iteratively but folds into a left-deep `Box`-nested
                // `Expr::Field` tree; a ~500k-long chain from an
                // untrusted manifest would overflow the stack when that
                // tree is *dropped* (recursive `Drop` glue), aborting
                // the process. Bail with the same diagnostic and return
                // the bare receiver instead of building the deep tree.
                if fields.len() > MAX_OPERATOR_CHAIN {
                    emitter.emit(Rich::custom(
                        e.span(),
                        format!(
                            "field-access chain too long ({} accesses, limit {MAX_OPERATOR_CHAIN}); this is almost always a generated or malformed file",
                            fields.len()
                        ),
                    ));
                    return receiver;
                }
                fields.into_iter().fold(receiver, |acc, field| {
                    let span = acc.span.start..field.span.end;
                    Spanned {
                        node: Expr::Field {
                            receiver: Box::new(acc),
                            field,
                        },
                        span,
                    }
                })
            });

        let unary = unary_chain(postfix);
        let multiplicative = left_assoc(unary, mul_op());
        let additive = left_assoc(multiplicative, add_op());
        let coalesce = coalesce_chain(additive);
        let comparison = left_assoc(coalesce, cmp_op());
        let conjunction = left_assoc(comparison, and_op());
        left_assoc(conjunction, or_op()).labelled("expression")
    })
}

/// `unary := '-' unary | power`, where `power := postfix ('**' unary)?`.
/// Kept together because `**` is right-associative *into* the unary
/// position (so `-2 ** -3` parses as `-(2 ** -3)`); the two stages
/// only make sense as a unit, hence one helper rather than two.
///
/// Labelling the outer choice as "expression" collapses the "RHS of a
/// binary op didn't parse" failure mode — e.g. after `1 +`, chumsky
/// would otherwise expose both the atom-level "expression" label and
/// the leading `-` token of the unfailed unary alternative. With this
/// label both collapse to a single `expected expression`.
fn unary_chain<'src, P>(
    postfix: P,
) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    recursive(|unary| {
        let power = postfix
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

        let neg = just('-')
            .padded_by(pad())
            .ignore_then(unary.clone())
            .map_with(|operand, e| Spanned {
                node: Expr::Unary {
                    op: UnaryOp::Neg,
                    operand: Box::new(operand),
                },
                span: span_to_range(e.span()),
            });

        // `!` is logical negation. The `and_is(!= not)` lookahead keeps
        // a stray prefix `!=` from being mis-read as `!` over `= ...`,
        // so `a != b` (handled by `cmp_op`) is never reached here.
        let not = just('!')
            .and_is(just("!=").not())
            .padded_by(pad())
            .ignore_then(unary)
            .map_with(|operand, e| Spanned {
                node: Expr::Unary {
                    op: UnaryOp::Not,
                    operand: Box::new(operand),
                },
                span: span_to_range(e.span()),
            });

        choice((neg, not, power)).labelled("expression")
    })
}

/// Longest left-associative operator chain a single tier accepts
/// before bailing. A flat `1 + 1 + … + 1` chain parses iteratively
/// (no parser-stack recursion) but folds into a left-deep AST; with no
/// bound, a 200k-term chain from an untrusted manifest builds a tree so
/// deep that *dropping* it (recursive `Drop` glue on the `Box`-nested
/// nodes) overflows the stack and aborts the process. 1024 is orders of
/// magnitude above any real expression yet keeps the tree shallow
/// enough to type-check, evaluate, and drop safely.
const MAX_OPERATOR_CHAIN: usize = 1024;

/// `tier := inner (op inner)*`, left-associative. Used for
/// multiplicative, additive, and comparison stages — every
/// left-fold-on-binary-op tier shares this shape.
fn left_assoc<'src, P, O>(
    inner: P,
    op: O,
) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
    O: Parser<'src, &'src str, BinOp, Extra<'src>> + Clone + 'src,
{
    inner
        .clone()
        .then(op.then(inner).repeated().collect::<Vec<_>>())
        .validate(|(lhs, ops), e, emitter| {
            if ops.len() > MAX_OPERATOR_CHAIN {
                // Emit a diagnostic but return a shallow placeholder
                // instead of `fold_left`-ing the deep tree: `validate`
                // (unlike `try_map`) keeps the surrounding parse on its
                // happy path, so the custom message survives instead of
                // being discarded by error recovery — and the flat,
                // shallow `ops` vector drops in linear time.
                emitter.emit(Rich::custom(
                    e.span(),
                    format!(
                        "operator chain too long ({} operands, limit {MAX_OPERATOR_CHAIN}); this is almost always a generated or malformed file",
                        ops.len() + 1
                    ),
                ));
                return lhs;
            }
            ops.into_iter().fold(lhs, fold_left)
        })
}

fn mul_op<'src>() -> impl Parser<'src, &'src str, BinOp, Extra<'src>> + Clone {
    choice((just('*').to(BinOp::Mul), just('/').to(BinOp::Div))).padded_by(pad())
}

fn add_op<'src>() -> impl Parser<'src, &'src str, BinOp, Extra<'src>> + Clone {
    choice((
        just("++").to(BinOp::Concat),
        just('+').to(BinOp::Add),
        just('-').to(BinOp::Sub),
    ))
    .padded_by(pad())
}

fn and_op<'src>() -> impl Parser<'src, &'src str, BinOp, Extra<'src>> + Clone {
    just("&&").to(BinOp::And).padded_by(pad())
}

fn or_op<'src>() -> impl Parser<'src, &'src str, BinOp, Extra<'src>> + Clone {
    just("||").to(BinOp::Or).padded_by(pad())
}

/// Two-character comparisons (`==`, `!=`, `<=`, `>=`) are tried before
/// their single-character prefixes (`<`, `>`) so we don't commit to
/// the prefix and then fail on the trailing `=`.
fn cmp_op<'src>() -> impl Parser<'src, &'src str, BinOp, Extra<'src>> + Clone {
    choice((
        just("==").to(BinOp::Eq),
        just("!=").to(BinOp::Neq),
        just("<=").to(BinOp::Le),
        just(">=").to(BinOp::Ge),
        just('<').to(BinOp::Lt),
        just('>').to(BinOp::Gt),
    ))
    .padded_by(pad())
}

/// `coalesce := additive ('??' coalesce)?`. Right-associative — sits
/// between additive and comparison so the common "fallback then
/// compare" pattern parses without parens. See the module doc.
fn coalesce_chain<'src, P>(
    additive: P,
) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    recursive(|coalesce| {
        additive
            .clone()
            .then(just("??").padded_by(pad()).ignore_then(coalesce).or_not())
            .map(|(lhs, rhs)| match rhs {
                None => lhs,
                Some(rhs) => merge_binary(BinOp::Coalesce, lhs, rhs),
            })
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

/// `if cond { … } [else { … } | else if … ]` chains.
///
/// Each branch is a [`Block`] (statements + optional trailing expr).
/// `else` is **optional**; an omitted else parses as an empty block,
/// which has type `Void`. The "branches must match" rule still holds,
/// so omitting `else` is well-typed only when the then-branch is also
/// `Void` — i.e. the `if` is being used as control flow.
///
/// `else if` is parsed right-associatively: the else branch may be
/// either another `if`-expression (wrapped in a synthetic block whose
/// trailing is that if-expr) or a literal `{ … }` block.
fn if_atom<'src, P>(expr: P) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let block_parser = block(expr.clone());

    recursive(|if_chain| {
        let else_block = block_parser.clone();
        let else_if = if_chain.map(else_if_to_block);

        let else_branch = text::keyword("else")
            .padded_by(pad())
            .ignore_then(choice((else_if, else_block)));

        text::keyword("if")
            .padded_by(pad())
            .ignore_then(expr.clone())
            .then(block_parser.clone())
            .then(else_branch.or_not())
            .map_with(|((cond, then_branch), else_branch), e| {
                let span = span_to_range(e.span());
                let else_branch =
                    else_branch.unwrap_or_else(|| empty_block_at(then_branch.span.end..span.end));
                Spanned {
                    node: Expr::If {
                        cond: Box::new(cond),
                        then_branch: Box::new(then_branch),
                        else_branch: Box::new(else_branch),
                    },
                    span,
                }
            })
    })
}

fn else_if_to_block(if_expr: Spanned<Expr>) -> Block {
    let span = if_expr.span.clone();
    Block {
        stmts: Vec::new(),
        trailing: Some(if_expr),
        span,
    }
}

const fn empty_block_at(span: Span) -> Block {
    Block {
        stmts: Vec::new(),
        trailing: None,
        span,
    }
}

/// `for x in xs { … }` (list iteration) and
/// `for (k, v) in m { … }` (map iteration).
///
/// The pair form is tried first because `(` after `for` is unambiguous.
/// A successful pair commits to two distinct identifiers separated by
/// `,`. Both forms then expect the `in` keyword, an iterable
/// expression, and a brace-delimited block. The body's trailing
/// expression must be `Void` (enforced at type-check time, not here).
fn for_atom<'src, P>(expr: P) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let block_parser = block(expr.clone());

    let pair = spanned_ident()
        .then_ignore(just(',').padded_by(pad()))
        .then(spanned_ident())
        .delimited_by(just('(').padded_by(pad()), just(')').padded_by(pad()))
        .map(|(key, value)| ForPattern::Entry { key, value });

    let single = spanned_ident().map(ForPattern::Elem);

    text::keyword("for")
        .padded_by(pad())
        .ignore_then(choice((pair, single)))
        .then_ignore(text::keyword("in").padded_by(pad()))
        .then(expr)
        .then(block_parser)
        .map_with(|((pattern, iter_expr), body), e| Spanned {
            node: Expr::For {
                pattern,
                iter_expr: Box::new(iter_expr),
                body: Box::new(body),
            },
            span: span_to_range(e.span()),
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
    let null_lit = text::keyword("null").to(Literal::Null);
    let num_lit = number_literal();
    choice((bool_lit, null_lit, num_lit))
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
                let value = s
                    .parse::<f64>()
                    .map_err(|e| Rich::custom(span, e.to_string()))?;
                if value.is_finite() {
                    Ok(Literal::Double(value))
                } else {
                    Err(Rich::custom(span, "double literal is out of range"))
                }
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
