//! `textDocument/foldingRange` — fold multi-line items, blocks inside
//! expressions, multiline strings, and runs of consecutive comments.

use keron_lang::{Expr, LexTokenKind, lex_tokens};
use lsp_types::{FoldingRange, FoldingRangeKind, FoldingRangeParams};

use crate::analysis::node_at::walk_exprs;
use crate::handlers::snapshot_at;
use crate::line_index::LineIndex;
use crate::state::ServerState;

pub fn handle(state: &ServerState, params: &FoldingRangeParams) -> Option<Vec<FoldingRange>> {
    let snap = snapshot_at(state, &params.text_document.uri)?;
    let mut ranges = Vec::new();
    let push = |ranges: &mut Vec<FoldingRange>,
                index: &LineIndex,
                span: &keron_lang::Span,
                kind: Option<FoldingRangeKind>| {
        let start = index.position(snap.text, span.start, snap.enc);
        let end = index.position(snap.text, span.end, snap.enc);
        // Item spans include the trailing newline; a fold ending at
        // column 0 really ends on the previous line.
        let end_line = if end.character == 0 && end.line > start.line {
            end.line - 1
        } else {
            end.line
        };
        if end_line > start.line {
            ranges.push(FoldingRange {
                start_line: start.line,
                start_character: None,
                end_line,
                end_character: None,
                kind,
                collapsed_text: None,
            });
        }
    };

    for item in &snap.program.items {
        push(&mut ranges, snap.index, &item.span(), None);
    }
    walk_exprs(snap.program, &mut |e| {
        if matches!(
            e.node,
            Expr::If { .. } | Expr::For { .. } | Expr::Match { .. } | Expr::Map(_) | Expr::List(_)
        ) {
            push(&mut ranges, snap.index, &e.span, None);
        }
    });

    // Multiline strings and comment runs come from the lexical layer.
    let mut comment_run: Option<(u32, u32)> = None;
    for token in lex_tokens(snap.text) {
        match token.kind {
            LexTokenKind::Str => {
                push(&mut ranges, snap.index, &token.span, None);
            }
            LexTokenKind::Comment => {
                let line = snap
                    .index
                    .position(snap.text, token.span.start, snap.enc)
                    .line;
                comment_run = match comment_run {
                    Some((start, end)) if line == end + 1 => Some((start, line)),
                    Some((start, end)) => {
                        if end > start {
                            ranges.push(comment_fold(start, end));
                        }
                        Some((line, line))
                    }
                    None => Some((line, line)),
                };
            }
            _ => {}
        }
    }
    if let Some((start, end)) = comment_run
        && end > start
    {
        ranges.push(comment_fold(start, end));
    }

    ranges.sort_by_key(|r| (r.start_line, r.end_line));
    ranges.dedup_by_key(|r| (r.start_line, r.end_line));
    Some(ranges)
}

const fn comment_fold(start_line: u32, end_line: u32) -> FoldingRange {
    FoldingRange {
        start_line,
        start_character: None,
        end_line,
        end_character: None,
        kind: Some(FoldingRangeKind::Comment),
        collapsed_text: None,
    }
}
