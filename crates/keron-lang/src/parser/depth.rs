//! Up-front nesting-depth guard for untrusted source.
//!
//! The chumsky grammar parses nested delimiters (`(`/`[`/`{`/`${`) and
//! prefix-operator chains (`-`/`!`) via `recursive(...)` on the native
//! call stack. A pathologically nested manifest — e.g. a cloned
//! dotfiles repo containing `((((…))))` 100k deep — would overflow the
//! stack and abort the *process* (SIGABRT) before any diagnostic is
//! produced, bypassing the documented exit-code contract. Manifests
//! are untrusted input (see the eval-time-IO threat model), so this
//! pass rejects absurd nesting with a clean diagnostic first.
//!
//! The scanner is string-aware: delimiters inside string literals are
//! content, not structure, and `${…}` interpolation re-enters code
//! mode (where its own delimiters count again). It is deliberately a
//! *bound*, not a parser — divergence from the real grammar can only
//! make it count slightly more or less nesting, never accept invalid
//! source, since the real parser still runs afterwards.

use crate::diagnostic::Diagnostic;

/// Maximum structural nesting depth (`(`/`[`/`{`) and maximum run of
/// consecutive unary prefix operators (`-`/`!`). Far above any
/// realistic hand-written or generated config, well below the native
/// stack-overflow point of the recursive-descent grammar.
const MAX_NESTING_DEPTH: usize = 256;

/// One frame of the scanner's context stack. `Delim` frames count
/// toward the structural depth; `Interp` is tracked only so its `}` is
/// matched back to the enclosing string; string frames suppress
/// delimiter counting until the string closes.
///
/// `Delim` and `Interp` open a fresh code context: a parenthesized or
/// bracketed sub-expression, or an interpolation body, is parsed by a
/// *new* recursion of the grammar's `expr`, so the right-associative
/// operator spines below re-count from zero. Each carries the enclosing
/// frame's spine counts to restore on close.
enum Frame {
    /// An open `(`, `[`, or `{`.
    Delim { saved: SpineRuns },
    /// An open `${` interpolation; its matching `}` resumes the string
    /// frame beneath it.
    Interp { saved: SpineRuns },
    /// A single-line cooked string `"…"` (interpolation + escapes).
    Str,
    /// A cooked multiline string `"""…"""` (interpolation + escapes).
    MultiStr,
    /// A raw multiline string `r#*"""…"""#*` — opaque, no interpolation
    /// and no escapes; the `usize` is the hash count to close on.
    RawStr(usize),
}

/// Depth of the right-associative operator spines the recursive-descent
/// grammar parses on the native stack *without* opening a delimiter:
/// `??` (`coalesce_chain`) and `**` (`unary_chain`'s `power`). Both are
/// `recursive(...)` combinators whose RHS recurses per operator, so a
/// flat `a ?? a ?? … ?? a` chain — no brackets, so the delimiter guard
/// never fires — recurses one frame per `??` and overflows the process
/// stack. Counting them here rejects the bomb with a clean diagnostic.
#[derive(Clone, Copy, Default)]
struct SpineRuns {
    coalesce: usize,
    power: usize,
}

/// Reject source whose structural nesting or unary-prefix run exceeds
/// [`MAX_NESTING_DEPTH`].
///
/// # Errors
/// Returns a single [`Diagnostic`] pointing at the offending byte when
/// either limit is exceeded.
pub(super) fn enforce_nesting_limit(src: &str) -> Result<(), Vec<Diagnostic>> {
    let bytes = src.as_bytes();
    let mut stack: Vec<Frame> = Vec::new();
    let mut depth: usize = 0;
    let mut unary_run: usize = 0;
    let mut spine = SpineRuns::default();
    let mut i = 0;

    while i < bytes.len() {
        // Multiline string closes match the parser only at line start
        // (`consume_multiline_close`), so the scanner must know whether
        // it is there. Recomputed each step from the previous byte, so
        // it stays correct no matter how far a scan step advanced.
        let at_line_start = i == 0 || bytes[i - 1] == b'\n';
        match stack.last() {
            Some(Frame::Str) => i = scan_single_line_cooked(bytes, i, &mut stack, &mut spine),
            Some(Frame::MultiStr) => {
                i = scan_multiline(bytes, i, at_line_start, None, &mut stack, &mut spine);
            }
            Some(&Frame::RawStr(hashes)) => {
                i = scan_multiline(
                    bytes,
                    i,
                    at_line_start,
                    Some(hashes),
                    &mut stack,
                    &mut spine,
                );
            }
            // Code context: top is a delimiter, an interpolation, or the
            // stack is empty.
            _ => {
                let c = bytes[i];
                // Inter-token trivia the parser's `pad()` swallows —
                // `#` line comments and *Unicode* whitespace — must not
                // reset the unary-prefix run, or `-# c\n-# c\n…` and
                // `-\u{00A0}-\u{00A0}…` (each a real `neg` recursion in
                // the grammar) would never accumulate a count here.
                if c == b'#' {
                    i = skip_line_comment(bytes, i);
                    continue;
                }
                if let Some(len) = leading_whitespace_len(src, i) {
                    i += len;
                    continue;
                }
                if c == b'-' || c == b'!' {
                    unary_run += 1;
                    if unary_run > MAX_NESTING_DEPTH {
                        return Err(vec![too_deep(i, "unary operator")]);
                    }
                    i += 1;
                    continue;
                }
                // Right-associative operator spines (`??`, `**`) recurse
                // on the parser stack once per operator with no enclosing
                // delimiter, so they need their own bound.
                if bytes[i..].starts_with(b"??") {
                    spine.coalesce += 1;
                    if spine.coalesce > MAX_NESTING_DEPTH {
                        return Err(vec![too_deep(i, "`??` chain")]);
                    }
                    unary_run = 0;
                    i += 2;
                    continue;
                }
                if bytes[i..].starts_with(b"**") {
                    spine.power += 1;
                    if spine.power > MAX_NESTING_DEPTH {
                        return Err(vec![too_deep(i, "`**` chain")]);
                    }
                    unary_run = 0;
                    i += 2;
                    continue;
                }
                // Any other real token breaks the prefix-operator run.
                unary_run = 0;
                i = scan_code(src, bytes, i, &mut stack, &mut depth, &mut spine)?;
            }
        }
    }
    Ok(())
}

/// Length in bytes of the whitespace character starting at `i`, if any
/// — using `char::is_whitespace` so it matches `pad()`'s Unicode
/// whitespace rule (`\u{00A0}`, `\u{2028}`, …), not just ASCII.
fn leading_whitespace_len(src: &str, i: usize) -> Option<usize> {
    // `src.get(i..)` (not `src[i..]`) so a byte index that landed inside
    // a multi-byte char — the byte scan advances one byte at a time
    // through code context — yields `None` instead of panicking.
    let c = src.get(i..)?.chars().next()?;
    c.is_whitespace().then(|| c.len_utf8())
}

/// Advance past a `#` line comment to (but not including) the newline,
/// mirroring `pad()`'s `# …` rule.
fn skip_line_comment(bytes: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < bytes.len() && bytes[j] != b'\n' {
        j += 1;
    }
    j
}

/// Top-level keywords that unambiguously begin a new item. When the
/// delimiter stack is empty, encountering one of these ends the
/// previous top-level expression, so its right-associative spine counts
/// reset — a manifest with hundreds of `env(…) ?? "default"` statements
/// must not accumulate toward the per-expression limit. These keywords
/// never appear at frame-zero *inside* an expression (only `if`/`for`/
/// `match`, which are not reset points, do), so resetting here can never
/// hide a genuine single-expression bomb.
const ITEM_KEYWORDS: [&[u8]; 5] = [b"val", b"fn", b"struct", b"type", b"reconcile"];

/// Scan one step of code context. Updates `stack`/`depth`/`spine`;
/// returns the next byte index.
fn scan_code(
    src: &str,
    bytes: &[u8],
    i: usize,
    stack: &mut Vec<Frame>,
    depth: &mut usize,
    spine: &mut SpineRuns,
) -> Result<usize, Vec<Diagnostic>> {
    match bytes[i] {
        b'"' => {
            if bytes[i..].starts_with(b"\"\"\"") && opener_has_newline(bytes, i + 3) {
                stack.push(Frame::MultiStr);
                Ok(i + 3)
            } else {
                stack.push(Frame::Str);
                Ok(i + 1)
            }
        }
        // A raw-string opener only when the `r` doesn't continue an
        // identifier: `bar#"""` lexes as the identifier `bar` plus the
        // comment `#"""`, not a raw string — matching how the tokenizer
        // reads it. Without the preceding-byte check the guard would
        // flip into (phantom) raw-string mode and stop counting the
        // delimiters after it.
        b'r' if !crate::lex::prev_char_is_ident(src, i)
            && raw_open_hashes(bytes, i)
                .is_some_and(|hashes| opener_has_newline(bytes, i + 1 + hashes + 3)) =>
        {
            let hashes = raw_open_hashes(bytes, i).expect("checked by guard");
            stack.push(Frame::RawStr(hashes));
            // Advance past `r` + hashes + `"""`.
            Ok(i + 1 + hashes + 3)
        }
        b'(' | b'[' | b'{' => {
            stack.push(Frame::Delim { saved: *spine });
            *spine = SpineRuns::default();
            *depth += 1;
            if *depth > MAX_NESTING_DEPTH {
                return Err(vec![too_deep(i, "nesting")]);
            }
            Ok(i + 1)
        }
        b')' | b']' => {
            if let Some(Frame::Delim { saved }) = stack.last() {
                *spine = *saved;
                stack.pop();
                *depth = depth.saturating_sub(1);
            }
            Ok(i + 1)
        }
        b'}' => {
            match stack.last() {
                // `Delim` was charged to `depth`; `Interp` never was.
                Some(Frame::Delim { saved }) => {
                    let saved = *saved;
                    *depth = depth.saturating_sub(1);
                    *spine = saved;
                    stack.pop();
                }
                Some(Frame::Interp { saved }) => {
                    *spine = *saved;
                    stack.pop();
                }
                _ => {}
            }
            Ok(i + 1)
        }
        b if b.is_ascii_alphabetic()
            && stack.is_empty()
            && !crate::lex::prev_char_is_ident(src, i) =>
        {
            // At the top level, reset the spine counters when an item
            // keyword starts a new statement. Read the whole word so a
            // longer identifier that merely starts with `val…` (e.g.
            // `valid`) is not mistaken for the `val` keyword.
            let start = i;
            let mut j = i;
            while let Some(c) = src.get(j..).and_then(|rest| rest.chars().next()) {
                if !unicode_ident::is_xid_continue(c) {
                    break;
                }
                j += c.len_utf8();
            }
            let word = &bytes[start..j];
            if ITEM_KEYWORDS.contains(&word) {
                *spine = SpineRuns::default();
            }
            Ok(j)
        }
        _ => Ok(i + 1),
    }
}

fn opener_has_newline(bytes: &[u8], after: usize) -> bool {
    matches!(bytes.get(after), Some(b'\n' | b'\r'))
}

/// Scan one byte inside a single-line cooked string (`"…"`): honor `\`
/// escapes, `${` interpolation re-entry, and the closing `"`.
fn scan_single_line_cooked(
    bytes: &[u8],
    i: usize,
    stack: &mut Vec<Frame>,
    spine: &mut SpineRuns,
) -> usize {
    let c = bytes[i];
    if c == b'\n' || c == b'\r' {
        stack.pop();
        return i + 1;
    }
    if c == b'\\' {
        return match bytes.get(i + 1) {
            Some(b'\n' | b'\r') => {
                stack.pop();
                i + 2
            }
            Some(_) => i + 2,
            None => {
                stack.pop();
                i + 1
            }
        };
    }
    if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
        // Re-enter code mode for the interpolation body — a fresh `expr`
        // recursion, so its right-associative spines re-count from zero.
        // Its own `(`/`[`/`{` are charged to `depth` in `scan_code`.
        stack.push(Frame::Interp { saved: *spine });
        *spine = SpineRuns::default();
        return i + 2;
    }
    if c == b'"' {
        stack.pop();
        return i + 1;
    }
    i + 1
}

/// Scan one step inside a multiline string — cooked (`raw_hashes ==
/// None`, honoring `\` escapes and `${` interpolation) or raw
/// (`raw_hashes == Some(n)`, fully opaque). The close is recognized
/// **only at line start** and only as `indent* """ #{n} (newline|EOF)`,
/// exactly like the parser's `consume_multiline_close`; a mid-line
/// `"""` (legal string content) no longer ends the string early, which
/// used to desync the guard and hide every delimiter after it.
fn scan_multiline(
    bytes: &[u8],
    i: usize,
    at_line_start: bool,
    raw_hashes: Option<usize>,
    stack: &mut Vec<Frame>,
    spine: &mut SpineRuns,
) -> usize {
    if at_line_start && let Some(after) = multiline_close_end(bytes, i, raw_hashes) {
        stack.pop();
        return after;
    }
    // Content byte. Raw strings are opaque (no escapes, no
    // interpolation); cooked strings re-enter code mode on `${`.
    if raw_hashes.is_none() {
        let c = bytes[i];
        if c == b'\\' {
            return i + 2;
        }
        if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
            stack.push(Frame::Interp { saved: *spine });
            *spine = SpineRuns::default();
            return i + 2;
        }
    }
    i + 1
}

/// If a multiline-string close starts at `i` (assumed line start),
/// return the byte index just past the closing `"""#{n}`; else `None`.
/// Mirrors `consume_multiline_close`: optional leading indentation,
/// `"""`, exactly `raw_hashes` `#`, then a newline or EOF.
fn multiline_close_end(bytes: &[u8], i: usize, raw_hashes: Option<usize>) -> Option<usize> {
    let mut j = i;
    while matches!(bytes.get(j), Some(b' ' | b'\t')) {
        j += 1;
    }
    if !bytes[j..].starts_with(b"\"\"\"") {
        return None;
    }
    j += 3;
    for _ in 0..raw_hashes.unwrap_or(0) {
        if bytes.get(j) != Some(&b'#') {
            return None;
        }
        j += 1;
    }
    // Exactly `raw_hashes` hashes: an extra `#` here means the next byte
    // is not a line end, so this is not the close.
    match bytes.get(j) {
        None | Some(b'\n' | b'\r') => Some(j),
        _ => None,
    }
}

/// If `bytes[i..]` begins with a raw multiline opener `r` + `#`* +
/// `"""`, return the hash count. Callers must additionally confirm the
/// `r` does not continue an identifier (a `bar#"""` line is `bar` plus
/// a comment, not a raw string) — that preceding-byte check lives at
/// the call site in `scan_code`.
fn raw_open_hashes(bytes: &[u8], i: usize) -> Option<usize> {
    if bytes.get(i) != Some(&b'r') {
        return None;
    }
    let mut j = i + 1;
    let mut hashes = 0;
    while bytes.get(j) == Some(&b'#') {
        hashes += 1;
        j += 1;
    }
    if bytes[j..].starts_with(b"\"\"\"") {
        Some(hashes)
    } else {
        None
    }
}

fn too_deep(byte: usize, what: &str) -> Diagnostic {
    Diagnostic::new(
        byte..byte + 1,
        format!(
            "{what} nested too deeply (limit {MAX_NESTING_DEPTH}); this is almost always a generated or malformed file"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) {
        assert!(
            enforce_nesting_limit(src).is_ok(),
            "expected within-limit, got error for {src:?}"
        );
    }

    fn rejected(src: &str) {
        assert!(
            enforce_nesting_limit(src).is_err(),
            "expected over-limit rejection for {src:?}"
        );
    }

    #[test]
    fn ordinary_source_is_accepted() {
        ok("val x: Int = (1 + 2) * 3");
        ok("val xs: List<List<Int>> = [[1], [2, 3]]");
        ok(r#"val m: Map<String, Int> = {"a": 1, "b": 2}"#);
        ok(r#"val s: String = "deep ((((parens)))) in a string is fine""#);
    }

    #[test]
    fn brackets_inside_strings_do_not_count() {
        // 1000 unclosed parens — but all inside a string literal.
        let inner = "(".repeat(1000);
        ok(&format!("val s: String = \"{inner}\""));
    }

    #[test]
    fn deeply_nested_parens_are_rejected() {
        let src = format!("val x = {}1{}", "(".repeat(300), ")".repeat(300));
        rejected(&src);
    }

    #[test]
    fn deeply_nested_brackets_are_rejected() {
        let src = format!("val x = {}{}", "[".repeat(300), "]".repeat(300));
        rejected(&src);
    }

    #[test]
    fn long_unary_prefix_run_is_rejected() {
        rejected(&format!("val x = {}5", "-".repeat(300)));
        rejected(&format!("val x = {}true", "!".repeat(300)));
    }

    #[test]
    fn binary_subtraction_chain_is_not_a_unary_run() {
        // `1-1-1-…`: each `-` is broken by an operand, so the run never
        // grows. This must stay accepted (it is parsed iteratively).
        let chain = "1".to_string() + &"-1".repeat(1000);
        ok(&format!("val x = {chain}"));
    }

    #[test]
    fn interpolation_delimiters_are_counted() {
        // `${(((…)))}` nests structurally even though it is written
        // inside a string.
        let src = format!(
            "val s: String = \"x ${{{}1{}}}\"",
            "(".repeat(300),
            ")".repeat(300)
        );
        rejected(&src);
    }

    #[test]
    fn nested_strings_in_interpolation_are_handled() {
        ok(r#"val s: String = "${ "inner" }""#);
    }

    #[test]
    fn raw_string_brackets_do_not_count() {
        let inner = "(".repeat(1000);
        ok(&format!("val s: String = r#\"\"\"\n{inner}\n\"\"\"#"));
    }

    #[test]
    fn long_coalesce_chain_is_rejected() {
        // `a ?? a ?? … ?? a` recurses one parser frame per `??` with no
        // enclosing delimiter — the delimiter guard never sees it.
        let chain = "a".to_string() + &" ?? a".repeat(300);
        rejected(&format!("val x = {chain}"));
    }

    #[test]
    fn long_power_chain_is_rejected() {
        let chain = "2".to_string() + &" ** 2".repeat(300);
        rejected(&format!("val x = {chain}"));
    }

    #[test]
    fn many_statements_each_with_one_coalesce_are_accepted() {
        // A realistic large manifest: hundreds of independent
        // `env(…) ?? "default"` statements. Each ends the previous
        // top-level expression, so the per-expression spine resets and
        // the flat total never trips the limit.
        use std::fmt::Write as _;
        let mut src = String::new();
        for n in 0..1000 {
            let _ = writeln!(src, "val v{n} = env(\"X\") ?? \"default\"");
        }
        ok(&src);
    }

    #[test]
    fn coalesce_run_resets_inside_parentheses() {
        // `((a ?? b)) ?? … ?? c`: the parenthesized `??` is a separate
        // recursion, so a modest chain that reuses parens must stay
        // accepted rather than summing across frames.
        let mut expr = "a".to_string();
        for _ in 0..200 {
            expr = format!("({expr} ?? a)");
        }
        ok(&format!("val x = {expr}"));
    }

    #[test]
    fn identifier_prefixed_with_keyword_does_not_reset() {
        // `valid` starts with `val` but is not the keyword; a spine that
        // continues across such an identifier must still be counted.
        // (Constructed so the only reset candidate is the bare word.)
        let chain = "valid".to_string() + &" ?? a".repeat(300);
        rejected(&format!("val x = {chain}"));
    }

    #[test]
    fn mid_line_triple_quote_in_multiline_string_does_not_desync() {
        // A cooked multiline string whose body contains a mid-line
        // `"""` is legal content — the parser closes only at line
        // start. The guard must stay in string mode, so the deep
        // delimiter nest *after* the string is still counted.
        let deep = format!("{}1{}", "(".repeat(300), ")".repeat(300));
        let src = format!("val s = \"\"\"\nx \"\"\" y\n\"\"\"\nval deep = {deep}\n");
        rejected(&src);
    }

    #[test]
    fn identifier_ending_in_r_before_hash_comment_is_not_a_raw_string() {
        // `bar#"""` is the identifier `bar` plus a comment `#"""`, not a
        // raw-string opener. The guard must not enter raw-string mode
        // and swallow the deeply-nested expression on the next line.
        let deep = format!("{}1{}", "[".repeat(300), "]".repeat(300));
        let src = format!("val x = [bar#\"\"\"\n]\nval deep = {deep}\n");
        rejected(&src);
    }

    #[test]
    fn unicode_identifier_suffix_does_not_reset_recursive_chain() {
        let chain = "éval".to_string() + &" ?? éval".repeat(300);
        rejected(&format!("val x = {chain}"));
    }

    #[test]
    fn keyword_prefix_of_unicode_identifier_does_not_reset_recursive_chain() {
        let chain = "valé".to_string() + &" ?? valé".repeat(300);
        rejected(&format!("val x = {chain}"));
    }

    #[test]
    fn contextual_from_identifier_does_not_reset_recursive_chain() {
        let chain = "from".to_string() + &" ?? from".repeat(300);
        rejected(&format!("val x = {chain}"));
    }

    #[test]
    fn unicode_identifier_ending_in_r_before_comment_is_not_a_raw_string() {
        let deep = format!("{}1{}", "(".repeat(300), ")".repeat(300));
        let src = format!("val x = ér#\"\"\"\nval deep = {deep}\n");
        rejected(&src);
    }

    #[test]
    fn unicode_whitespace_before_raw_opener_still_enters_raw_string() {
        let content = "(".repeat(1000);
        for ws in ['\u{00a0}', '\u{2003}'] {
            ok(&format!("val x = {ws}r#\"\"\"\n{content}\n\"\"\"#\n"));
        }
    }

    #[test]
    fn unterminated_single_line_string_does_not_hide_later_nesting() {
        let deep = format!("{}1{}", "(".repeat(300), ")".repeat(300));
        let src = format!("val x = \"unterminated\nval deep = {deep}\n");
        rejected(&src);
    }

    #[test]
    fn newline_escape_error_does_not_hide_later_nesting() {
        let deep = format!("{}1{}", "(".repeat(300), ")".repeat(300));
        let src = format!("val x = \"bad\\\nval deep = {deep}\n");
        rejected(&src);
    }

    #[test]
    fn malformed_multiline_openers_do_not_hide_later_nesting() {
        let deep = format!("{}1{}", "(".repeat(300), ")".repeat(300));
        rejected(&format!(
            "val x = \"\"\"not a multiline opener\nval deep = {deep}\n"
        ));
        rejected(&format!(
            "val x = r#\"\"\"not a raw opener\nval deep = {deep}\n"
        ));
    }

    #[test]
    fn unicode_whitespace_between_unary_operators_still_counts() {
        // `pad()` accepts Unicode whitespace between prefix operators,
        // so `-\u{00A0}-\u{00A0}…` is one deep `neg` chain in the
        // grammar; the guard must not let the non-breaking space reset
        // the run.
        let chain = "-\u{00A0}".repeat(300);
        rejected(&format!("val x = {chain}5"));
    }

    #[test]
    fn comments_between_unary_operators_still_count() {
        // `# comment` between prefix operators is trivia the parser
        // skips; it must not reset the unary run either.
        let chain = "-# c\n".repeat(300);
        rejected(&format!("val x = {chain}5"));
    }

    #[test]
    fn raw_string_with_mid_content_triple_quote_and_wrong_hashes_stays_open() {
        // Inside `r#"""…"""#`, a `"""` with the wrong hash count (or not
        // at line start) is content; the close needs line-start + exact
        // hashes + line end. Delimiters after the real close are counted.
        let deep = format!("{}1{}", "(".repeat(300), ")".repeat(300));
        let src = format!("val s = r#\"\"\"\n\"\"\" not a close\n\"\"\"#\nval deep = {deep}\n");
        rejected(&src);
    }
}
