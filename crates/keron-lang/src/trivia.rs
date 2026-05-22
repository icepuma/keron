//! Comment extraction and attachment.
//!
//! The parser discards comments along with whitespace (see
//! `parser::util::pad`), so the AST has no comment nodes. For the
//! formatter to round-trip user-authored documentation we extract
//! comments from the original source in a second pass and attach
//! each one to the AST span it relates to. The result is a
//! `CommentMap` that the emitter consults during pretty-printing.
//!
//! The extractor walks the source byte-by-byte with a small
//! string-aware state machine (delegating to [`crate::lex`] for the
//! multi-line string opener/closer rules) so a `#` inside a string
//! literal is never mistaken for a comment.
//!
//! Attachment policy lives in [`assign_comment`]. See
//! [`CommentAttachment`] for the full taxonomy.

use crate::ast::{Block, Comment, CommentAttachment, CommentMap, Expr, Item, Program, Span, Stmt};
use crate::lex::{MultilineClose, is_multiline_close, multiline_open};

/// Extract every `#...\n` comment from `src` and attach each one to
/// the AST node it relates to, returning a `CommentMap`.
///
/// The map is empty for sources that contain no comments. `program`
/// is the parsed AST of `src`. The function is total — it never
/// fails — because the source has already been validated by the
/// parser before this point.
#[must_use]
pub fn extract_comments(src: &str, program: &Program) -> CommentMap {
    let raw = scan_comments(src);
    if raw.is_empty() {
        return CommentMap::new();
    }

    let spans = collect_spans(program);
    let mut comments = Vec::with_capacity(raw.len());
    for comment in raw {
        let attachment = assign_comment(src, &comment, &spans, program);
        comments.push((comment, attachment));
    }
    CommentMap { comments }
}

// ---------------------------------------------------------------------
// Step 1: scan raw comments from source.
// ---------------------------------------------------------------------

/// Walk `src` line-by-line and return every `#...` run, with its
/// byte span. Respects single- and multi-line string literals so
/// `#` inside a string is treated as content. The returned text
/// includes the leading `#` and trailing characters up to (but not
/// including) the newline.
fn scan_comments(src: &str) -> Vec<Comment> {
    let mut out = Vec::new();
    let mut multiline: Option<MultilineClose> = None;
    let mut offset = 0usize;
    for line in src.split_inclusive('\n') {
        let line_len = line.len();
        // The line slice may end in `\n` (or be the last line without
        // one). Strip the trailing newline for scanning so column
        // offsets line up with byte offsets within `line`.
        let body = line.strip_suffix('\n').unwrap_or(line);

        if let Some(close) = multiline {
            if is_multiline_close(body, close) {
                multiline = None;
            }
            offset += line_len;
            continue;
        }

        if let Some(start) = find_comment_start(body) {
            let abs_start = offset + start;
            let abs_end = offset + body.len();
            out.push(Comment {
                text: body[start..].to_string(),
                span: abs_start..abs_end,
            });
            offset += line_len;
            continue;
        }

        multiline = multiline_open(body);
        offset += line_len;
    }
    out
}

/// Return the byte offset of the first real `#` comment opener in
/// `line`, or `None` if every `#` lives inside a string literal or a
/// raw-string opener. The scanner mirrors `multiline_open` in
/// [`crate::lex`] but reports a position instead of just signaling
/// "multi-line opener present".
fn find_comment_start(line: &str) -> Option<usize> {
    let mut in_string = false;
    let mut escaped = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '#' => return Some(i),
            '"' => {
                // Triple-quote: same-line `"""..."""` closes inline,
                // otherwise this line opens a multi-line string that
                // continues onto the next line. Either way no `#`
                // after the opener on this line is a comment.
                if i + 3 <= bytes.len() && &bytes[i..i + 3] == b"\"\"\"" {
                    if let Some(close_rel) = line[i + 3..].find("\"\"\"") {
                        i += 3 + close_rel + 3;
                        continue;
                    }
                    return None;
                }
                in_string = true;
            }
            'r' if crate::lex::raw_multiline_open_at(line, i).is_some() => {
                // `r#*"""` opens a raw multi-line string. The keron
                // grammar requires the opener to be followed by a
                // newline (see `parser::string`), so everything after
                // `r#*"""` on this line is string body, not comment.
                return None;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

// ---------------------------------------------------------------------
// Step 2: collect every span that a comment might attach to, sorted
// by start offset. Used by `assign_comment` for binary search.
// ---------------------------------------------------------------------

fn collect_spans(program: &Program) -> Vec<Span> {
    // Spans are stored *raw* (with whatever trailing pad the parser
    // captured) so the emitter can look up an attachment by
    // `Item::span()` or `stmt_span` directly. Trailing-detection
    // trims on the fly via `trim_pad_end` when it needs a semantic
    // end position.
    let mut spans = Vec::new();
    for item in &program.items {
        collect_item_spans(item, &mut spans);
    }
    spans.sort_by_key(|s| s.start);
    spans
}

fn collect_item_spans(item: &Item, spans: &mut Vec<Span>) {
    spans.push(item.span());
    match item {
        Item::Val(v) => collect_expr_spans(&v.value.node, spans),
        Item::Fn(f) => collect_block_spans(&f.body, spans),
        Item::Struct(s) => {
            for field in &s.fields {
                if let Some(default) = &field.default {
                    collect_expr_spans(&default.node, spans);
                }
            }
        }
        Item::Reconcile(r) => {
            for chain in &r.chains {
                for expr in chain {
                    collect_expr_spans(&expr.node, spans);
                }
            }
        }
        Item::ExprStmt(e) => collect_expr_spans(&e.node, spans),
        Item::Use(_) | Item::TypeAlias(_) => {}
    }
}

fn collect_block_spans(block: &Block, spans: &mut Vec<Span>) {
    for stmt in &block.stmts {
        spans.push(stmt_span(stmt));
        match stmt {
            Stmt::Val(v) => collect_expr_spans(&v.value.node, spans),
            Stmt::Reconcile(r) => {
                for chain in &r.chains {
                    for expr in chain {
                        collect_expr_spans(&expr.node, spans);
                    }
                }
            }
        }
    }
    if let Some(trailing) = &block.trailing {
        spans.push(trailing.span.clone());
        collect_expr_spans(&trailing.node, spans);
    }
}

fn collect_expr_spans(expr: &Expr, spans: &mut Vec<Span>) {
    match expr {
        Expr::Unary { operand, .. }
        | Expr::Field {
            receiver: operand, ..
        } => {
            collect_expr_spans(&operand.node, spans);
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr_spans(&lhs.node, spans);
            collect_expr_spans(&rhs.node, spans);
        }
        Expr::Interpolation(parts) => {
            for part in parts {
                if let crate::ast::StringPart::Expr { expr, .. } = part {
                    collect_expr_spans(&expr.node, spans);
                }
            }
        }
        Expr::List(items) => {
            for item in items {
                collect_expr_spans(&item.node, spans);
            }
        }
        Expr::Map(entries) => {
            for entry in entries {
                collect_expr_spans(&entry.key.node, spans);
                collect_expr_spans(&entry.value.node, spans);
            }
        }
        Expr::Call { args, .. } => {
            for arg in args {
                collect_expr_spans(&arg.value.node, spans);
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_expr_spans(&cond.node, spans);
            collect_block_spans(then_branch, spans);
            collect_block_spans(else_branch, spans);
        }
        Expr::For {
            iter_expr, body, ..
        } => {
            collect_expr_spans(&iter_expr.node, spans);
            collect_block_spans(body, spans);
        }
        Expr::Match { scrutinee, arms } => {
            collect_expr_spans(&scrutinee.node, spans);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_expr_spans(&guard.node, spans);
                }
                spans.push(arm.body.span.clone());
                collect_expr_spans(&arm.body.node, spans);
            }
        }
        Expr::Literal(_) | Expr::Var(_) => {}
    }
}

fn stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::Val(v) => v.span.clone(),
        Stmt::Reconcile(r) => r.span.clone(),
    }
}

/// Return the byte offset of the position just after the last
/// non-trivia byte inside `span` (i.e., not whitespace and not part
/// of a `# …` comment run). For a span whose contents are entirely
/// trivia, returns `span.start`.
fn trim_pad_end(src: &str, span: &Span) -> usize {
    let mut end = span.end.min(src.len());
    let bytes = src.as_bytes();
    loop {
        while end > span.start {
            let b = bytes[end - 1];
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                end -= 1;
            } else {
                break;
            }
        }
        if end <= span.start {
            return span.start;
        }
        // Check whether the current line within `span` ends in a `#`
        // comment; if so, snip the comment off and keep trimming.
        let line_start = src[..end].rfind('\n').map_or(0, |i| i + 1).max(span.start);
        if let Some(hash) = find_comment_start(&src[line_start..end]) {
            end = line_start + hash;
            continue;
        }
        return end;
    }
}

// ---------------------------------------------------------------------
// Step 3: classify each comment per attachment policy.
// ---------------------------------------------------------------------

fn assign_comment(
    src: &str,
    comment: &Comment,
    spans: &[Span],
    _program: &Program,
) -> CommentAttachment {
    // Trailing first: same source line as the end of some node, with
    // only whitespace between.
    if let Some(span) = find_trailing_target(src, comment, spans) {
        return CommentAttachment::Trailing(span);
    }
    // Leading: comment ends before the next node, with only
    // whitespace between (blank lines OK).
    if let Some(span) = find_leading_target(src, comment, spans) {
        return CommentAttachment::Leading(span);
    }
    // After last item: ModuleTrailing.
    if comment_after_last_item(src, comment, spans) {
        return CommentAttachment::ModuleTrailing;
    }
    // Default: treat as block-interior of the file as a whole. This
    // is a coarse approximation for v1 — nested-block placement will
    // refine in a follow-up.
    CommentAttachment::BlockInterior {
        block_span: spans
            .first()
            .map_or(0..src.len(), |first| first.start..src.len()),
        after: None,
    }
}

fn find_trailing_target(src: &str, comment: &Comment, spans: &[Span]) -> Option<Span> {
    // Spans come in raw from the parser (with trailing pad). For the
    // same-line check we need the semantic end, computed lazily via
    // `trim_pad_end`. The returned span stays raw so the emitter can
    // match it against `Item::span()` byte-for-byte.
    for span in spans.iter().rev() {
        let semantic_end = trim_pad_end(src, span);
        if semantic_end > comment.span.start {
            continue;
        }
        let between = &src[semantic_end..comment.span.start];
        if between.chars().all(|c| c == ' ' || c == '\t') {
            return Some(span.clone());
        }
        return None;
    }
    None
}

fn find_leading_target(src: &str, comment: &Comment, spans: &[Span]) -> Option<Span> {
    for span in spans {
        if span.start < comment.span.end {
            continue;
        }
        let between = &src[comment.span.end..span.start];
        if gap_is_only_whitespace_or_comments(between) {
            return Some(span.clone());
        }
        return None;
    }
    None
}

/// True when `gap` contains only whitespace and `# ...` comment runs.
/// Used to let a multi-line block of consecutive comments still
/// register as "leading" the next AST node — the gap between them is
/// other comment lines, which must not disqualify the attachment.
fn gap_is_only_whitespace_or_comments(gap: &str) -> bool {
    for line in gap.split('\n') {
        let trimmed = line.trim_start_matches([' ', '\t']);
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        return false;
    }
    true
}

fn comment_after_last_item(src: &str, comment: &Comment, spans: &[Span]) -> bool {
    spans
        .last()
        .is_some_and(|last| comment.span.start >= trim_pad_end(src, last))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_program(src: &str) -> Program {
        crate::parser::parse(src).expect("test source must parse")
    }

    fn extract(src: &str) -> CommentMap {
        let program = parse_program(src);
        extract_comments(src, &program)
    }

    #[test]
    fn empty_source_yields_empty_map() {
        let m = extract("");
        assert!(m.is_empty());
    }

    #[test]
    fn source_without_comments_yields_empty_map() {
        let m = extract("val x: Int = 1\n");
        assert!(m.is_empty());
    }

    /// Span ranges may include trailing whitespace/newline depending
    /// on how the parser anchors `Spanned<T>` — for tests we compare
    /// the trimmed slice against the expected node text.
    fn span_text<'a>(src: &'a str, span: &Span) -> &'a str {
        src[span.clone()].trim_end()
    }

    #[test]
    fn leading_comment_attaches_to_following_val() {
        let src = "# header\nval x: Int = 1\n";
        let m = extract(src);
        assert_eq!(m.comments.len(), 1);
        let (comment, attach) = &m.comments[0];
        assert_eq!(comment.text, "# header");
        match attach {
            CommentAttachment::Leading(span) => {
                assert_eq!(span_text(src, span), "val x: Int = 1");
            }
            other => panic!("expected Leading, got {other:?}"),
        }
    }

    #[test]
    fn trailing_comment_attaches_to_preceding_val() {
        let src = "val x: Int = 1 # eol\n";
        let m = extract(src);
        assert_eq!(m.comments.len(), 1);
        let (comment, attach) = &m.comments[0];
        assert_eq!(comment.text, "# eol");
        match attach {
            CommentAttachment::Trailing(span) => {
                // Raw span may include trailing pad (whitespace, even
                // the trailing comment) — trim before comparing.
                assert!(
                    src[span.clone()].trim_end().starts_with("val x: Int = 1"),
                    "trailing-attached span text was {:?}",
                    &src[span.clone()]
                );
            }
            other => panic!("expected Trailing, got {other:?}"),
        }
    }

    #[test]
    fn comment_between_two_items_is_leading_on_the_next() {
        let src = "val x: Int = 1\n\n# between\nval y: Int = 2\n";
        let m = extract(src);
        assert_eq!(m.comments.len(), 1);
        let (_, attach) = &m.comments[0];
        match attach {
            CommentAttachment::Leading(span) => {
                assert_eq!(span_text(src, span), "val y: Int = 2");
            }
            other => panic!("expected Leading on `val y`, got {other:?}"),
        }
    }

    #[test]
    fn module_trailing_comment_marked_after_last_item() {
        let src = "val x: Int = 1\n# trailing\n";
        let m = extract(src);
        assert_eq!(m.comments.len(), 1);
        assert!(matches!(m.comments[0].1, CommentAttachment::ModuleTrailing));
    }

    #[test]
    fn multiple_comments_all_attach_in_source_order() {
        let src = "# a\n# b\nval x: Int = 1\n";
        let m = extract(src);
        assert_eq!(m.comments.len(), 2);
        assert_eq!(m.comments[0].0.text, "# a");
        assert_eq!(m.comments[1].0.text, "# b");
    }

    #[test]
    fn hash_inside_string_literal_is_not_a_comment() {
        let src = "val s: String = \"value # not-a-comment\"\n";
        let m = extract(src);
        assert!(m.is_empty(), "expected no comments, got {:?}", m.comments);
    }

    #[test]
    fn comment_inside_multiline_string_is_not_extracted() {
        let src = "val s: String = \"\"\"\n# inside\n\"\"\"\n";
        let m = extract(src);
        assert!(
            m.is_empty(),
            "comments inside multiline strings stay as string content, got {:?}",
            m.comments
        );
    }

    #[test]
    fn raw_multiline_with_hashes_in_content_is_not_extracted() {
        let src = "val s: String = r#\"\"\"\n# also inside\n\"\"\"#\n";
        let m = extract(src);
        assert!(m.is_empty(), "got {:?}", m.comments);
    }

    #[test]
    fn comment_span_excludes_trailing_newline() {
        let src = "# hi\nval x: Int = 1\n";
        let m = extract(src);
        let (comment, _) = &m.comments[0];
        assert_eq!(&src[comment.span.clone()], "# hi");
    }
}
