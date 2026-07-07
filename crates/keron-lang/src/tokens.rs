//! Error-tolerant lexical scanner for editor tooling.
//!
//! The chumsky grammar consumes `&str` directly and never materializes
//! a token stream, which is fine for parsing but useless for syntax
//! highlighting: a half-typed buffer has no AST, yet an editor still
//! wants keywords and strings colored. [`lex_tokens`] fills that gap —
//! it never fails, never panics, and degrades gracefully on broken
//! input (an unterminated string simply ends at the line break or
//! end of file).
//!
//! The scanner deliberately re-implements the *lexical* surface of the
//! grammar (comments, the three string forms, numbers, identifiers,
//! operators) instead of reusing parser combinators: it must survive
//! inputs the parser rejects. Interpolation is scanned structurally —
//! `"a${f(x)}b"` yields real tokens for `f`, `(`, `x`, `)` between the
//! string segments — so highlighting inside `${…}` works.

use crate::ast::Span;

/// Lexical class of one scanned token. Coarser than the parser's
/// grammar — just enough to drive syntax highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexTokenKind {
    /// `# …` to end of line.
    Comment,
    /// A string literal, or one contiguous segment of a string that is
    /// interrupted by `${…}` interpolations. Quotes are included in
    /// the adjacent segment's span.
    Str,
    /// Integer or double literal.
    Number,
    /// Reserved keyword (`val`, `fn`, `if`, …) or the contextual
    /// import keywords `from` / `use`, plus `true` / `false` / `null`.
    Keyword,
    /// Reserved builtin type name: `String`, `Int`, `Boolean`,
    /// `Double`, `List`, `Map`, `Void`.
    BuiltinType,
    /// Any other identifier.
    Ident,
    /// Operator such as `==`, `??`, `+`, `|`.
    Operator,
    /// Structural punctuation: `(){}[],:` and the `${` / `}` around
    /// an interpolation.
    Punct,
}

/// One token produced by [`lex_tokens`]. Spans are byte offsets into
/// the scanned source and always lie on `char` boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexToken {
    pub kind: LexTokenKind,
    pub span: Span,
}

/// Matches the parser's reserved words in `parser/util.rs`, plus the
/// contextual `from` / `use` — those are legal identifiers in the
/// grammar, but in practice they only appear as import keywords and
/// highlighting them as such is the useful default.
const KEYWORDS: &[&str] = &[
    "val",
    "fn",
    "reconcile",
    "if",
    "else",
    "for",
    "in",
    "match",
    "struct",
    "type",
    "true",
    "false",
    "null",
    "from",
    "use",
];

const BUILTIN_TYPES: &[&str] = &["String", "Int", "Boolean", "Double", "List", "Map", "Void"];

/// Deeper `"…${"…${…}…"}…"` nesting than this is scanned as plain
/// string content. Bounds the scanner's recursion on adversarial
/// input; the parser's own nesting limit is 256, so real programs
/// never get close.
const MAX_INTERPOLATION_DEPTH: usize = 32;

/// Scan `src` into a flat, ordered token list. Whitespace and
/// unrecognized characters produce no token; every returned span is
/// non-empty and the list is sorted by start offset.
#[must_use]
pub fn lex_tokens(src: &str) -> Vec<LexToken> {
    let mut scanner = Scanner {
        src,
        pos: 0,
        depth: 0,
        out: Vec::new(),
    };
    while let Some(c) = scanner.peek() {
        if c.is_whitespace() {
            scanner.bump(c);
        } else {
            scanner.scan_token(c);
        }
    }
    scanner.out
}

struct Scanner<'s> {
    src: &'s str,
    pos: usize,
    depth: usize,
    out: Vec<LexToken>,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    const fn bump(&mut self, c: char) {
        self.pos += c.len_utf8();
    }

    fn starts_with(&self, s: &str) -> bool {
        self.src[self.pos..].starts_with(s)
    }

    fn push(&mut self, kind: LexTokenKind, start: usize) {
        if self.pos > start {
            self.out.push(LexToken {
                kind,
                span: start..self.pos,
            });
        }
    }

    fn scan_token(&mut self, c: char) {
        match c {
            '#' => self.comment(),
            '"' => self.cooked_string(self.starts_with("\"\"\"")),
            'r' => {
                if let Some(hashes) = self.raw_open_hashes() {
                    self.raw_string(hashes);
                } else {
                    self.ident();
                }
            }
            _ if c.is_ascii_digit() => self.number(),
            _ if c.is_alphabetic() || c == '_' => self.ident(),
            _ => self.operator_or_punct(c),
        }
    }

    fn comment(&mut self) {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c == '\n' {
                break;
            }
            self.bump(c);
        }
        self.push(LexTokenKind::Comment, start);
    }

    /// Scan a cooked string (single-line `"…"` or multi-line
    /// `"""…"""`), splitting around `${…}` interpolations so their
    /// contents come out as real tokens.
    fn cooked_string(&mut self, multiline: bool) {
        let mut seg_start = self.pos;
        self.pos += if multiline { 3 } else { 1 };
        while let Some(c) = self.peek() {
            if multiline {
                if self.starts_with("\"\"\"") {
                    self.pos += 3;
                    break;
                }
            } else if c == '"' {
                self.bump(c);
                break;
            } else if c == '\n' {
                // Unterminated single-line string: stop at the line
                // break instead of swallowing the rest of the buffer.
                break;
            }
            if c == '\\' {
                self.bump(c);
                if let Some(escaped) = self.peek() {
                    self.bump(escaped);
                }
                continue;
            }
            if self.starts_with("${") && self.depth < MAX_INTERPOLATION_DEPTH {
                self.push(LexTokenKind::Str, seg_start);
                self.interpolation();
                seg_start = self.pos;
                continue;
            }
            self.bump(c);
        }
        self.push(LexTokenKind::Str, seg_start);
    }

    /// Scan the `${…}` following a string segment. Brace depth is
    /// tracked so map literals inside the interpolation don't end it
    /// early.
    fn interpolation(&mut self) {
        let start = self.pos;
        self.pos += 2;
        self.push(LexTokenKind::Punct, start);
        self.depth += 1;
        let mut braces = 1usize;
        while let Some(c) = self.peek() {
            if c == '{' || c == '}' {
                if c == '{' {
                    braces += 1;
                } else {
                    braces -= 1;
                }
                let s = self.pos;
                self.bump(c);
                self.push(LexTokenKind::Punct, s);
                if braces == 0 {
                    break;
                }
            } else if c.is_whitespace() {
                self.bump(c);
            } else {
                self.scan_token(c);
            }
        }
        self.depth -= 1;
    }

    /// `Some(n)` when the scanner sits on a raw-string opener
    /// `r#…#"""` with `n` hashes. Unlike `lex::raw_multiline_open_at`
    /// this accepts same-line raw strings too — the scanner does not
    /// care whether the close is on the same line.
    fn raw_open_hashes(&self) -> Option<usize> {
        let rest = self.src[self.pos..].strip_prefix('r')?;
        let hashes = rest.bytes().take_while(|b| *b == b'#').count();
        rest[hashes..].starts_with("\"\"\"").then_some(hashes)
    }

    fn raw_string(&mut self, hashes: usize) {
        let start = self.pos;
        self.pos += 1 + hashes + 3;
        let close = format!("\"\"\"{}", "#".repeat(hashes));
        match self.src[self.pos..].find(&close) {
            Some(i) => self.pos += i + close.len(),
            None => self.pos = self.src.len(),
        }
        self.push(LexTokenKind::Str, start);
    }

    fn number(&mut self) {
        let start = self.pos;
        self.eat_digits();
        // `1.x` is a field access on `1`, not a malformed double, so
        // only take the dot when a digit follows — same rule as the
        // grammar's `int . digits` fraction.
        let mut lookahead = self.src[self.pos..].chars();
        if lookahead.next() == Some('.') && lookahead.next().is_some_and(|c| c.is_ascii_digit()) {
            self.bump('.');
            self.eat_digits();
        }
        self.push(LexTokenKind::Number, start);
    }

    fn eat_digits(&mut self) {
        while let Some(c) = self.peek() {
            if !c.is_ascii_digit() {
                break;
            }
            self.bump(c);
        }
    }

    fn ident(&mut self) {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if !(c.is_alphanumeric() || c == '_') {
                break;
            }
            self.bump(c);
        }
        let text = &self.src[start..self.pos];
        let kind = if KEYWORDS.contains(&text) {
            LexTokenKind::Keyword
        } else if BUILTIN_TYPES.contains(&text) {
            LexTokenKind::BuiltinType
        } else {
            LexTokenKind::Ident
        };
        self.push(kind, start);
    }

    fn operator_or_punct(&mut self, c: char) {
        const TWO_CHAR_OPS: &[&str] = &[
            "->", "!=", "??", "**", "&&", "++", "<=", "==", "=>", ">=", "||",
        ];
        let start = self.pos;
        for op in TWO_CHAR_OPS {
            if self.starts_with(op) {
                self.pos += op.len();
                self.push(LexTokenKind::Operator, start);
                return;
            }
        }
        self.bump(c);
        match c {
            '+' | '-' | '*' | '/' | '%' | '<' | '>' | '=' | '!' | '?' | '.' | '|' | '&' => {
                self.push(LexTokenKind::Operator, start);
            }
            '(' | ')' | '{' | '}' | '[' | ']' | ',' | ':' | ';' => {
                self.push(LexTokenKind::Punct, start);
            }
            // Anything else (stray bytes in a broken buffer) produces
            // no token; the bump above guarantees forward progress.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(LexTokenKind, &str)> {
        lex_tokens(src)
            .into_iter()
            .map(|t| (t.kind, &src[t.span]))
            .collect()
    }

    #[test]
    fn keywords_types_and_idents_classify() {
        use LexTokenKind::{BuiltinType, Ident, Keyword, Operator, Punct};
        assert_eq!(
            kinds("val greeting: String = name"),
            vec![
                (Keyword, "val"),
                (Ident, "greeting"),
                (Punct, ":"),
                (BuiltinType, "String"),
                (Operator, "="),
                (Ident, "name"),
            ]
        );
    }

    #[test]
    fn contextual_import_keywords_classify_as_keywords() {
        let toks = kinds("from \"./lib.keron\" use helper");
        assert_eq!(toks[0], (LexTokenKind::Keyword, "from"));
        assert_eq!(toks[1], (LexTokenKind::Str, "\"./lib.keron\""));
        assert_eq!(toks[2], (LexTokenKind::Keyword, "use"));
        assert_eq!(toks[3], (LexTokenKind::Ident, "helper"));
    }

    #[test]
    fn comment_runs_to_end_of_line_only() {
        assert_eq!(
            kinds("# hello\nval"),
            vec![
                (LexTokenKind::Comment, "# hello"),
                (LexTokenKind::Keyword, "val"),
            ]
        );
    }

    #[test]
    fn hash_inside_string_is_not_a_comment() {
        assert_eq!(kinds("\"a # b\""), vec![(LexTokenKind::Str, "\"a # b\"")]);
    }

    #[test]
    fn numbers_split_int_and_double() {
        assert_eq!(
            kinds("1 2.5"),
            vec![(LexTokenKind::Number, "1"), (LexTokenKind::Number, "2.5")]
        );
    }

    #[test]
    fn dot_without_digit_is_field_access_not_double() {
        assert_eq!(
            kinds("1.x"),
            vec![
                (LexTokenKind::Number, "1"),
                (LexTokenKind::Operator, "."),
                (LexTokenKind::Ident, "x"),
            ]
        );
    }

    #[test]
    fn escaped_quote_does_not_close_string() {
        assert_eq!(kinds(r#""a\"b""#), vec![(LexTokenKind::Str, r#""a\"b""#)]);
    }

    #[test]
    fn unterminated_string_stops_at_line_break() {
        assert_eq!(
            kinds("\"open\nval x"),
            vec![
                (LexTokenKind::Str, "\"open"),
                (LexTokenKind::Keyword, "val"),
                (LexTokenKind::Ident, "x"),
            ]
        );
    }

    #[test]
    fn interpolation_yields_inner_tokens() {
        use LexTokenKind::{Ident, Punct, Str};
        assert_eq!(
            kinds("\"a${f(x)}b\""),
            vec![
                (Str, "\"a"),
                (Punct, "${"),
                (Ident, "f"),
                (Punct, "("),
                (Ident, "x"),
                (Punct, ")"),
                (Punct, "}"),
                (Str, "b\""),
            ]
        );
    }

    #[test]
    fn nested_braces_inside_interpolation_do_not_end_it() {
        let toks = kinds("\"${ {\"k\": 1} }\"");
        assert!(
            toks.contains(&(LexTokenKind::Number, "1")),
            "map value should tokenize: {toks:?}"
        );
        assert_eq!(*toks.last().unwrap(), (LexTokenKind::Str, "\""));
    }

    #[test]
    fn escaped_dollar_is_not_interpolation() {
        assert_eq!(
            kinds(r#""a\${x}b""#),
            vec![(LexTokenKind::Str, r#""a\${x}b""#)]
        );
    }

    #[test]
    fn multiline_cooked_string_scans_to_close() {
        assert_eq!(
            kinds("val s = \"\"\"\nline # not comment\n\"\"\""),
            vec![
                (LexTokenKind::Keyword, "val"),
                (LexTokenKind::Ident, "s"),
                (LexTokenKind::Operator, "="),
                (LexTokenKind::Str, "\"\"\"\nline # not comment\n\"\"\""),
            ]
        );
    }

    #[test]
    fn raw_string_close_requires_matching_hashes() {
        let src = "r#\"\"\"body \"\"\" still body\"\"\"#";
        assert_eq!(kinds(src), vec![(LexTokenKind::Str, src)]);
    }

    #[test]
    fn unterminated_raw_string_runs_to_eof_without_panic() {
        let src = "r##\"\"\"never closed";
        assert_eq!(kinds(src), vec![(LexTokenKind::Str, src)]);
    }

    #[test]
    fn identifier_starting_with_r_is_not_a_raw_string() {
        assert_eq!(
            kinds("reconciler"),
            vec![(LexTokenKind::Ident, "reconciler")]
        );
    }

    #[test]
    fn two_char_operators_win_over_single() {
        use LexTokenKind::Operator;
        assert_eq!(
            kinds("== != ?? ++ =>"),
            vec![
                (Operator, "=="),
                (Operator, "!="),
                (Operator, "??"),
                (Operator, "++"),
                (Operator, "=>"),
            ]
        );
    }

    #[test]
    fn spans_are_sorted_nonempty_and_on_char_boundaries() {
        let src = "val 你好 = \"héllo ${x} 🎉\" # done\nr#\"\"\"raw\"\"\"# 3.14 ??";
        let toks = lex_tokens(src);
        let mut prev_end = 0;
        for t in &toks {
            assert!(t.span.start < t.span.end, "empty span: {t:?}");
            assert!(t.span.start >= prev_end, "overlap: {t:?}");
            assert!(src.is_char_boundary(t.span.start));
            assert!(src.is_char_boundary(t.span.end));
            prev_end = t.span.end;
        }
        assert!(prev_end <= src.len());
    }

    #[test]
    fn deep_interpolation_nesting_is_bounded_not_recursive_blowup() {
        let mut src = String::new();
        for _ in 0..200 {
            src.push_str("\"${");
        }
        src.push('x');
        for _ in 0..200 {
            src.push_str("}\"");
        }
        // Must terminate without stack overflow; token content is
        // best-effort beyond MAX_INTERPOLATION_DEPTH.
        let _ = lex_tokens(&src);
    }
}
