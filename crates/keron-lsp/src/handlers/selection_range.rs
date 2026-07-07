//! `textDocument/selectionRange` — the expand-selection chain: every
//! enclosing expression/block/item span around the cursor, innermost
//! first.

use keron_lang::{Block, Expr, Span, Spanned, Stmt};
use lsp_types::{SelectionRange, SelectionRangeParams};

use crate::handlers::snapshot_at;
use crate::state::ServerState;

pub fn handle(state: &ServerState, params: &SelectionRangeParams) -> Option<Vec<SelectionRange>> {
    let snap = snapshot_at(state, &params.text_document.uri)?;
    let chains = params
        .positions
        .iter()
        .map(|&position| {
            let offset = snap
                .index
                .offset(snap.text, position, snap.enc)
                .unwrap_or(0);
            // Outermost → innermost enclosing spans at this offset.
            let mut spans: Vec<Span> = Vec::new();
            for item in &snap.program.items {
                let span = item.span();
                if span.start <= offset && offset < span.end {
                    spans.push(span);
                    item_spans(item, offset, &mut spans);
                }
            }
            // Fold into a linked SelectionRange, outermost as the tail.
            let mut chain: Option<SelectionRange> = None;
            for span in spans {
                chain = Some(SelectionRange {
                    range: snap.index.range(snap.text, &span, snap.enc),
                    parent: chain.map(Box::new),
                });
            }
            chain.unwrap_or_else(|| SelectionRange {
                range: lsp_types::Range::new(position, position),
                parent: None,
            })
        })
        .collect();
    Some(chains)
}

fn item_spans(item: &keron_lang::Item, offset: usize, out: &mut Vec<Span>) {
    use keron_lang::Item;
    match item {
        Item::Val(v) => expr_spans(&v.value, offset, out),
        Item::Fn(f) => {
            for p in &f.params {
                if let Some(d) = &p.default {
                    expr_spans(d, offset, out);
                }
            }
            block_spans(&f.body, offset, out);
        }
        Item::Struct(s) => {
            for f in &s.fields {
                if let Some(d) = &f.default {
                    expr_spans(d, offset, out);
                }
            }
        }
        Item::Reconcile(r) => {
            for e in r.chains.iter().flatten() {
                expr_spans(e, offset, out);
            }
        }
        Item::ExprStmt(e) => expr_spans(e, offset, out),
        Item::Use(_) | Item::TypeAlias(_) => {}
    }
}

fn block_spans(b: &Block, offset: usize, out: &mut Vec<Span>) {
    if !(b.span.start <= offset && offset < b.span.end) {
        return;
    }
    out.push(b.span.clone());
    for stmt in &b.stmts {
        match stmt {
            Stmt::Val(v) => expr_spans(&v.value, offset, out),
            Stmt::Reconcile(r) => {
                for e in r.chains.iter().flatten() {
                    expr_spans(e, offset, out);
                }
            }
            Stmt::Expr(e) => expr_spans(e, offset, out),
        }
    }
    if let Some(t) = &b.trailing {
        expr_spans(t, offset, out);
    }
}

fn expr_spans(e: &Spanned<Expr>, offset: usize, out: &mut Vec<Span>) {
    if !(e.span.start <= offset && offset < e.span.end) {
        return;
    }
    out.push(e.span.clone());
    match &e.node {
        Expr::Unary { operand, .. } => expr_spans(operand, offset, out),
        Expr::Binary { lhs, rhs, .. } => {
            expr_spans(lhs, offset, out);
            expr_spans(rhs, offset, out);
        }
        Expr::Interpolation(parts) => {
            for p in parts {
                if let keron_lang::StringPart::Expr { expr, .. } = p {
                    expr_spans(expr, offset, out);
                }
            }
        }
        Expr::List(items) => {
            for x in items {
                expr_spans(x, offset, out);
            }
        }
        Expr::Map(entries) => {
            for en in entries {
                expr_spans(&en.key, offset, out);
                expr_spans(&en.value, offset, out);
            }
        }
        Expr::Call { args, .. } => {
            for a in args {
                expr_spans(&a.value, offset, out);
            }
        }
        Expr::StructLiteral { fields, .. } => {
            for f in fields {
                if let Some(v) = &f.value {
                    expr_spans(v, offset, out);
                }
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            expr_spans(cond, offset, out);
            block_spans(then_branch, offset, out);
            block_spans(else_branch, offset, out);
        }
        Expr::For {
            iter_expr, body, ..
        } => {
            expr_spans(iter_expr, offset, out);
            block_spans(body, offset, out);
        }
        Expr::Field { receiver, .. } => expr_spans(receiver, offset, out),
        Expr::Match { scrutinee, arms } => {
            expr_spans(scrutinee, offset, out);
            for arm in arms {
                if let Some(g) = &arm.guard {
                    expr_spans(g, offset, out);
                }
                expr_spans(&arm.body, offset, out);
            }
        }
        Expr::Literal(_) | Expr::Var(_) => {}
    }
}
