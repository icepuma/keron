//! Deterministic diagnostic rendering for snapshot fixtures.
//!
//! Output is `error: <msg>` followed by a `--> line:col` pointer and a
//! single-line caret excerpt. ANSI codes are not emitted; column counts
//! are byte-based (UTF-8 fixtures are intentional). This is the contract
//! that `errors/*` snapshots pin.

use std::fmt::Write as _;

use keron_lang::Diagnostic;

pub fn diagnostics(src: &str, diags: &[Diagnostic]) -> String {
    let mut out = String::new();
    for (i, d) in diags.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        render_one(&mut out, src, d);
    }
    out
}

fn render_one(out: &mut String, src: &str, d: &Diagnostic) {
    let (line, col, line_text) = locate(src, d.span.start);
    let len = d.span.end.saturating_sub(d.span.start).max(1);
    let gutter = line.to_string();
    let pad = " ".repeat(gutter.len());
    let caret_indent = " ".repeat(col.saturating_sub(1));
    let carets = "^".repeat(len);

    writeln!(out, "error: {}", d.message).expect("write to String");
    writeln!(out, "{pad} --> {line}:{col}").expect("write to String");
    writeln!(out, "{pad} |").expect("write to String");
    writeln!(out, "{gutter} | {line_text}").expect("write to String");
    writeln!(out, "{pad} | {caret_indent}{carets}").expect("write to String");
}

fn locate(src: &str, byte: usize) -> (usize, usize, &str) {
    let clamped = byte.min(src.len());
    let prefix = &src[..clamped];
    let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
    let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
    let line_end = src[line_start..]
        .find('\n')
        .map_or(src.len(), |i| line_start + i);
    let col = clamped - line_start + 1;
    (line, col, &src[line_start..line_end])
}
