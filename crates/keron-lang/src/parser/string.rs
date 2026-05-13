//! String literal parser with `${expr}` interpolation.
//!
//! Expression strings have three forms:
//!
//! - `"..."` is a single-line cooked string.
//! - `""" ... """` is a cooked multiline string.
//! - `r""" ... """` and `r#""" ... """#` are raw multiline strings.
//!
//! Multiline openers must be followed immediately by a newline. The
//! closing delimiter must appear on its own line, and its leading
//! whitespace is stripped from every non-empty content line.

use chumsky::{input::InputRef, prelude::*};

use crate::ast::{Expr, Literal, Spanned, StringPart};

use super::util::{Extra, pad, span_to_range};

type ParserInput<'src, 'parse> = InputRef<'src, 'parse, &'src str, Extra<'src>>;
type ParseResult<'src, T> = Result<T, Rich<'src, char>>;

enum RawPart {
    Text(String),
    EscapedText(String),
    Expr(Box<Spanned<Expr>>),
}

pub(super) fn string_expr<'src, P>(
    expr: P,
) -> impl Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let raw_start = just('r')
        .then(just('#').repeated().ignored())
        .then(just("\"\"\""))
        .ignored()
        .rewind();
    let cooked_start = just('"').ignored().rewind();

    choice((raw_start, cooked_start))
        .ignore_then(custom(move |inp| parse_string_expr(inp, expr.clone())))
}

pub(super) fn plain_string<'src>() -> impl Parser<'src, &'src str, String, Extra<'src>> + Clone {
    let escape = just('\\').ignore_then(choice((
        just('"').to('"'),
        just('\\').to('\\'),
        just('n').to('\n'),
        just('r').to('\r'),
        just('t').to('\t'),
        just('$').to('$'),
    )));
    let bare_dollar = just('$').and_is(just("${").not()).to('$');
    let normal =
        any().filter(|c: &char| *c != '"' && *c != '\\' && *c != '$' && !is_line_break(*c));
    let ch = choice((escape, bare_dollar, normal));
    ch.repeated()
        .collect::<String>()
        .delimited_by(just('"'), just('"'))
}

fn parse_string_expr<'src, P>(
    inp: &mut ParserInput<'src, '_>,
    expr: P,
) -> ParseResult<'src, Spanned<Expr>>
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let checkpoint = inp.save();
    let start = inp.cursor();

    match raw_multiline_hashes(inp)? {
        RawStart::Matched(hashes) => {
            let (text, _) = parse_multiline_text(inp, Some(hashes), |_, _| {
                unreachable!("raw multiline strings do not parse interpolation")
            })?;
            let span = span_to_range(inp.span_since(&start));
            return Ok(Spanned {
                node: Expr::Literal(Literal::String(text)),
                span,
            });
        }
        RawStart::NotRaw => {}
    }

    if consume_str(inp, "\"\"\"") {
        consume_required_newline(inp, "multiline string opener must be followed by a newline")?;
        let (_, raw_parts) =
            parse_multiline_text(inp, None, |inp, text| parse_interpolation(inp, &expr, text))?;
        let span = span_to_range(inp.span_since(&start));
        return Ok(Spanned {
            node: collapse(cooked_multiline_parts(raw_parts)),
            span,
        });
    }

    let quote_checkpoint = inp.save();
    if matches!(inp.next(), Some('"')) {
        let parts = parse_single_line_parts(inp, &expr)?;
        let span = span_to_range(inp.span_since(&start));
        return Ok(Spanned {
            node: collapse(parts),
            span,
        });
    }
    inp.rewind(quote_checkpoint);

    inp.rewind(checkpoint);
    Err(Rich::custom(
        inp.span_since(&start),
        "expected string literal",
    ))
}

fn parse_single_line_parts<'src, P>(
    inp: &mut ParserInput<'src, '_>,
    expr: &P,
) -> ParseResult<'src, Vec<StringPart>>
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let mut parts = Vec::new();
    loop {
        let Some(c) = inp.next() else {
            return Err(error_at_current(inp, "unterminated string literal"));
        };
        match c {
            '"' => return Ok(parts),
            '\\' => push_text_char(&mut parts, parse_escape(inp)?),
            '$' => {
                let checkpoint = inp.save();
                if matches!(inp.next(), Some('{')) {
                    let inner = inp.parse(expr.clone().padded_by(pad()).then_ignore(just('}')))?;
                    parts.push(StringPart::Expr {
                        expr: Box::new(inner),
                        indent: None,
                    });
                } else {
                    inp.rewind(checkpoint);
                    push_text_char(&mut parts, '$');
                }
            }
            c if is_line_break(c) => {
                return Err(error_at_current(
                    inp,
                    "single-line string literal cannot contain a raw newline",
                ));
            }
            c => push_text_char(&mut parts, c),
        }
    }
}

enum RawStart {
    Matched(usize),
    NotRaw,
}

fn raw_multiline_hashes<'src>(inp: &mut ParserInput<'src, '_>) -> ParseResult<'src, RawStart> {
    let checkpoint = inp.save();
    if !matches!(inp.next(), Some('r')) {
        inp.rewind(checkpoint);
        return Ok(RawStart::NotRaw);
    }

    let mut hashes = 0usize;
    loop {
        let hash_checkpoint = inp.save();
        if matches!(inp.next(), Some('#')) {
            hashes += 1;
        } else {
            inp.rewind(hash_checkpoint);
            break;
        }
    }

    if !consume_str(inp, "\"\"\"") {
        inp.rewind(checkpoint);
        return Ok(RawStart::NotRaw);
    }

    consume_required_newline(
        inp,
        "raw multiline string opener must be followed by a newline",
    )?;
    Ok(RawStart::Matched(hashes))
}

fn parse_multiline_text<'src, F>(
    inp: &mut ParserInput<'src, '_>,
    raw_hashes: Option<usize>,
    mut interpolation: F,
) -> ParseResult<'src, (String, Vec<RawPart>)>
where
    F: FnMut(&mut ParserInput<'src, '_>, &mut Vec<RawPart>) -> ParseResult<'src, ()>,
{
    let mut raw_parts = Vec::new();
    let mut raw_text = String::new();
    let mut line_start = true;

    loop {
        if line_start && let Some(close_indent) = consume_multiline_close(inp, raw_hashes) {
            trim_final_newline(&mut raw_text, &mut raw_parts);
            let text = strip_multiline_indent_from_text(&raw_text, &close_indent, inp)?;
            let parts = strip_multiline_indent_from_parts(raw_parts, &close_indent, inp)?;
            return Ok((text, parts));
        }

        let Some(c) = inp.next() else {
            return Err(error_at_current(
                inp,
                "unterminated multiline string literal",
            ));
        };

        match c {
            '\r' => {
                let checkpoint = inp.save();
                if !matches!(inp.next(), Some('\n')) {
                    inp.rewind(checkpoint);
                }
                push_raw_text_char(&mut raw_parts, '\n');
                raw_text.push('\n');
                line_start = true;
            }
            '\n' => {
                push_raw_text_char(&mut raw_parts, '\n');
                raw_text.push('\n');
                line_start = true;
            }
            '\\' => {
                if raw_hashes.is_some() {
                    push_raw_text_char(&mut raw_parts, c);
                    raw_text.push(c);
                } else {
                    let escaped = parse_escape(inp)?;
                    push_raw_escaped_char(&mut raw_parts, escaped);
                    raw_text.push(if is_line_break(escaped) {
                        '\u{0}'
                    } else {
                        escaped
                    });
                }
                line_start = false;
            }
            '$' => {
                let checkpoint = inp.save();
                if raw_hashes.is_none() && matches!(inp.next(), Some('{')) {
                    interpolation(inp, &mut raw_parts)?;
                    raw_text.push('\u{0}');
                } else {
                    inp.rewind(checkpoint);
                    push_raw_text_char(&mut raw_parts, c);
                    raw_text.push(c);
                }
                line_start = false;
            }
            c => {
                push_raw_text_char(&mut raw_parts, c);
                raw_text.push(c);
                line_start = false;
            }
        }
    }
}

fn parse_interpolation<'src, P>(
    inp: &mut ParserInput<'src, '_>,
    expr: &P,
    parts: &mut Vec<RawPart>,
) -> ParseResult<'src, ()>
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let inner = inp.parse(expr.clone().padded_by(pad()).then_ignore(just('}')))?;
    parts.push(RawPart::Expr(Box::new(inner)));
    Ok(())
}

fn cooked_multiline_parts(raw_parts: Vec<RawPart>) -> Vec<StringPart> {
    let mut out = Vec::new();
    let lines = split_raw_parts_into_lines(raw_parts);
    for (idx, line) in lines.into_iter().enumerate() {
        if idx > 0 {
            push_text_char(&mut out, '\n');
        }
        let mut indent = Some(String::new());
        for part in line {
            match part {
                RawPart::Text(text) => {
                    if let Some(prefix) = &mut indent {
                        if text.chars().all(is_indent_char) {
                            prefix.push_str(&text);
                        } else {
                            indent = None;
                        }
                    }
                    push_text_str(&mut out, &text);
                }
                RawPart::EscapedText(text) => {
                    indent = None;
                    push_text_str(&mut out, &text);
                }
                RawPart::Expr(expr) => {
                    out.push(StringPart::Expr {
                        expr,
                        indent: indent.clone(),
                    });
                    indent = None;
                }
            }
        }
    }
    out
}

fn split_raw_parts_into_lines(parts: Vec<RawPart>) -> Vec<Vec<RawPart>> {
    let mut lines = vec![Vec::new()];
    for part in parts {
        match part {
            RawPart::Text(text) => split_text_part(&mut lines, &text),
            RawPart::EscapedText(text) => lines
                .last_mut()
                .expect("line vector is never empty")
                .push(RawPart::EscapedText(text)),
            RawPart::Expr(expr) => lines
                .last_mut()
                .expect("line vector is never empty")
                .push(RawPart::Expr(expr)),
        }
    }
    lines
}

fn split_text_part(lines: &mut Vec<Vec<RawPart>>, text: &str) {
    let mut segment = String::new();
    for c in text.chars() {
        if c == '\n' {
            lines
                .last_mut()
                .expect("line vector is never empty")
                .push(RawPart::Text(std::mem::take(&mut segment)));
            lines.push(Vec::new());
        } else {
            segment.push(c);
        }
    }
    lines
        .last_mut()
        .expect("line vector is never empty")
        .push(RawPart::Text(segment));
}

fn strip_multiline_indent_from_text<'src>(
    text: &str,
    close_indent: &str,
    inp: &mut ParserInput<'src, '_>,
) -> ParseResult<'src, String> {
    let mut out = String::new();
    for (idx, line) in text.split('\n').enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if line.chars().all(is_indent_char) {
            continue;
        }
        let Some(stripped) = line.strip_prefix(close_indent) else {
            return Err(indent_error(inp));
        };
        out.push_str(stripped);
    }
    Ok(out)
}

fn strip_multiline_indent_from_parts<'src>(
    parts: Vec<RawPart>,
    close_indent: &str,
    inp: &mut ParserInput<'src, '_>,
) -> ParseResult<'src, Vec<RawPart>> {
    let mut out = Vec::new();
    let lines = split_raw_parts_into_lines(parts);
    for (idx, line) in lines.into_iter().enumerate() {
        if idx > 0 {
            push_raw_text_char(&mut out, '\n');
        }
        if raw_line_is_blank(&line) {
            continue;
        }
        for part in strip_line_indent(line, close_indent, inp)? {
            push_raw_part(&mut out, part);
        }
    }
    Ok(out)
}

fn raw_line_is_blank(line: &[RawPart]) -> bool {
    line.iter()
        .all(|p| matches!(p, RawPart::Text(text) if text.chars().all(is_indent_char)))
}

fn strip_line_indent<'src>(
    line: Vec<RawPart>,
    close_indent: &str,
    inp: &mut ParserInput<'src, '_>,
) -> ParseResult<'src, Vec<RawPart>> {
    if close_indent.is_empty() {
        return Ok(line);
    }

    let mut rest = close_indent;
    let mut out = Vec::new();
    for part in line {
        if rest.is_empty() {
            push_raw_part(&mut out, part);
            continue;
        }
        match part {
            RawPart::Text(text) => {
                if let Some(tail) = text.strip_prefix(rest) {
                    rest = "";
                    push_raw_text_str(&mut out, tail);
                } else if let Some(next_rest) = rest.strip_prefix(&text) {
                    rest = next_rest;
                } else {
                    return Err(indent_error(inp));
                }
            }
            RawPart::EscapedText(_) | RawPart::Expr(_) => return Err(indent_error(inp)),
        }
    }

    if rest.is_empty() {
        Ok(out)
    } else {
        Err(indent_error(inp))
    }
}

fn consume_multiline_close(
    inp: &mut ParserInput<'_, '_>,
    raw_hashes: Option<usize>,
) -> Option<String> {
    let checkpoint = inp.save();
    let mut indent = String::new();
    loop {
        let char_checkpoint = inp.save();
        match inp.next() {
            Some(c) if is_indent_char(c) => indent.push(c),
            _ => {
                inp.rewind(char_checkpoint);
                break;
            }
        }
    }

    if !consume_str(inp, "\"\"\"") {
        inp.rewind(checkpoint);
        return None;
    }

    if let Some(hashes) = raw_hashes {
        for _ in 0..hashes {
            if !matches!(inp.next(), Some('#')) {
                inp.rewind(checkpoint);
                return None;
            }
        }
    }

    let end_checkpoint = inp.save();
    match inp.next() {
        None | Some('\n' | '\r') => {
            inp.rewind(end_checkpoint);
            Some(indent)
        }
        _ => {
            inp.rewind(checkpoint);
            None
        }
    }
}

fn trim_final_newline(text: &mut String, parts: &mut Vec<RawPart>) {
    if text.ends_with('\n') {
        text.pop();
        pop_raw_text_char(parts);
    }
}

fn parse_escape<'src>(inp: &mut ParserInput<'src, '_>) -> ParseResult<'src, char> {
    match inp.next() {
        Some('"') => Ok('"'),
        Some('\\') => Ok('\\'),
        Some('n') => Ok('\n'),
        Some('r') => Ok('\r'),
        Some('t') => Ok('\t'),
        Some('$') => Ok('$'),
        Some(other) => Err(error_at_current(
            inp,
            format!("unknown string escape `\\{other}`"),
        )),
        None => Err(error_at_current(inp, "unterminated string escape")),
    }
}

fn consume_required_newline<'src>(
    inp: &mut ParserInput<'src, '_>,
    message: &str,
) -> ParseResult<'src, ()> {
    match inp.next() {
        Some('\n') => Ok(()),
        Some('\r') => {
            let checkpoint = inp.save();
            if !matches!(inp.next(), Some('\n')) {
                inp.rewind(checkpoint);
            }
            Ok(())
        }
        _ => Err(error_at_current(inp, message)),
    }
}

fn consume_str(inp: &mut ParserInput<'_, '_>, expected: &str) -> bool {
    let checkpoint = inp.save();
    for expected_char in expected.chars() {
        if !matches!(inp.next(), Some(c) if c == expected_char) {
            inp.rewind(checkpoint);
            return false;
        }
    }
    true
}

fn push_text_char(parts: &mut Vec<StringPart>, c: char) {
    match parts.last_mut() {
        Some(StringPart::Text(text)) => text.push(c),
        _ => parts.push(StringPart::Text(c.to_string())),
    }
}

fn push_text_str(parts: &mut Vec<StringPart>, s: &str) {
    if s.is_empty() {
        return;
    }
    match parts.last_mut() {
        Some(StringPart::Text(text)) => text.push_str(s),
        _ => parts.push(StringPart::Text(s.to_string())),
    }
}

fn push_raw_text_char(parts: &mut Vec<RawPart>, c: char) {
    parts.push(RawPart::Text(c.to_string()));
}

fn push_raw_text_str(parts: &mut Vec<RawPart>, s: &str) {
    parts.push(RawPart::Text(s.to_string()));
}

fn push_raw_escaped_char(parts: &mut Vec<RawPart>, c: char) {
    parts.push(RawPart::EscapedText(c.to_string()));
}

fn push_raw_part(parts: &mut Vec<RawPart>, part: RawPart) {
    match part {
        RawPart::Text(text) => push_raw_text_str(parts, &text),
        RawPart::EscapedText(text) => parts.push(RawPart::EscapedText(text)),
        RawPart::Expr(expr) => parts.push(RawPart::Expr(expr)),
    }
}

fn pop_raw_text_char(parts: &mut Vec<RawPart>) {
    let Some(RawPart::Text(text)) = parts.last_mut() else {
        return;
    };
    text.pop();
    if text.is_empty() {
        parts.pop();
    }
}

fn collapse(parts: Vec<StringPart>) -> Expr {
    if parts.iter().all(|p| matches!(p, StringPart::Text(_))) {
        let mut s = String::new();
        for p in parts {
            if let StringPart::Text(t) = p {
                s.push_str(&t);
            }
        }
        Expr::Literal(Literal::String(s))
    } else {
        Expr::Interpolation(parts)
    }
}

fn indent_error<'src>(inp: &mut ParserInput<'src, '_>) -> Rich<'src, char> {
    error_at_current(
        inp,
        "multiline string content must use the closing delimiter indentation",
    )
}

fn error_at_current<'src>(
    inp: &mut ParserInput<'src, '_>,
    message: impl Into<String>,
) -> Rich<'src, char> {
    let here = inp.cursor();
    Rich::custom(inp.span_since(&here), message)
}

const fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r')
}

const fn is_indent_char(c: char) -> bool {
    matches!(c, ' ' | '\t')
}
