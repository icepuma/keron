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
enum Frame {
    /// An open `(`, `[`, or `{`.
    Delim,
    /// An open `${` interpolation; its matching `}` resumes the string
    /// frame beneath it.
    Interp,
    /// A single-line cooked string `"…"` (interpolation + escapes).
    Str,
    /// A cooked multiline string `"""…"""` (interpolation + escapes).
    MultiStr,
    /// A raw multiline string `r#*"""…"""#*` — opaque, no interpolation
    /// and no escapes; the `usize` is the hash count to close on.
    RawStr(usize),
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
    let mut i = 0;

    while i < bytes.len() {
        match stack.last() {
            Some(Frame::Str) => i = scan_cooked(bytes, i, &mut stack, false),
            Some(Frame::MultiStr) => i = scan_cooked(bytes, i, &mut stack, true),
            Some(&Frame::RawStr(hashes)) => i = scan_raw(bytes, i, hashes, &mut stack),
            // Code context: top is a delimiter, an interpolation, or the
            // stack is empty.
            _ => {
                let c = bytes[i];
                if c == b'-' || c == b'!' {
                    unary_run += 1;
                    if unary_run > MAX_NESTING_DEPTH {
                        return Err(vec![too_deep(i, "unary operator")]);
                    }
                    i += 1;
                    continue;
                }
                // Whitespace does not reset a prefix-operator run
                // (`! ! x` is still two prefixes); every other token does.
                if !c.is_ascii_whitespace() {
                    unary_run = 0;
                }
                i = scan_code(bytes, i, &mut stack, &mut depth)?;
            }
        }
    }
    Ok(())
}

/// Scan one step of code context. Updates `stack`/`depth`; returns the
/// next byte index.
fn scan_code(
    bytes: &[u8],
    i: usize,
    stack: &mut Vec<Frame>,
    depth: &mut usize,
) -> Result<usize, Vec<Diagnostic>> {
    match bytes[i] {
        b'#' => {
            // Line comment to end of line.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            Ok(j)
        }
        b'"' => {
            if bytes[i..].starts_with(b"\"\"\"") {
                stack.push(Frame::MultiStr);
                Ok(i + 3)
            } else {
                stack.push(Frame::Str);
                Ok(i + 1)
            }
        }
        b'r' if raw_open_hashes(bytes, i).is_some() => {
            let hashes = raw_open_hashes(bytes, i).expect("checked by guard");
            stack.push(Frame::RawStr(hashes));
            // Advance past `r` + hashes + `"""`.
            Ok(i + 1 + hashes + 3)
        }
        b'(' | b'[' | b'{' => {
            stack.push(Frame::Delim);
            *depth += 1;
            if *depth > MAX_NESTING_DEPTH {
                return Err(vec![too_deep(i, "nesting")]);
            }
            Ok(i + 1)
        }
        b')' | b']' => {
            if matches!(stack.last(), Some(Frame::Delim)) {
                stack.pop();
                *depth = depth.saturating_sub(1);
            }
            Ok(i + 1)
        }
        b'}' => {
            match stack.last() {
                Some(Frame::Delim) => {
                    stack.pop();
                    *depth = depth.saturating_sub(1);
                }
                // Closes a `${…}`; depth was never charged for `Interp`.
                Some(Frame::Interp) => {
                    stack.pop();
                }
                _ => {}
            }
            Ok(i + 1)
        }
        _ => Ok(i + 1),
    }
}

/// Scan inside a cooked string (single- or multi-line). Handles
/// escapes, `${` interpolation re-entry, and the closing quote(s).
fn scan_cooked(bytes: &[u8], i: usize, stack: &mut Vec<Frame>, multiline: bool) -> usize {
    let c = bytes[i];
    if c == b'\\' {
        // Escape: skip the next byte.
        return i + 2;
    }
    if c == b'$' && bytes.get(i + 1) == Some(&b'{') {
        // Re-enter code mode for the interpolation body. Its own
        // `(`/`[`/`{` are charged to `depth` in `scan_code`.
        stack.push(Frame::Interp);
        return i + 2;
    }
    if multiline {
        if bytes[i..].starts_with(b"\"\"\"") {
            stack.pop();
            return i + 3;
        }
    } else if c == b'"' {
        stack.pop();
        return i + 1;
    }
    i + 1
}

/// Scan inside a raw multiline string: opaque until `"""` followed by
/// at least `hashes` `#` characters.
fn scan_raw(bytes: &[u8], i: usize, hashes: usize, stack: &mut Vec<Frame>) -> usize {
    if bytes[i..].starts_with(b"\"\"\"") {
        let after = i + 3;
        let closing = bytes[after..].iter().take_while(|&&b| b == b'#').count();
        if closing >= hashes {
            stack.pop();
            return after + hashes;
        }
    }
    i + 1
}

/// If `bytes[i..]` begins with a raw multiline opener `r` + `#`* +
/// `"""`, return the hash count. The `r` of an ordinary identifier
/// (`reconcile`, a `val r`) is never immediately followed by `"""`, so
/// this is unambiguous.
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
}
