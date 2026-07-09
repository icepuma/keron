//! Property tests for the AST pretty-printer.
//!
//! Drives [`keron_lang::format`] across the entire parse-corpus and
//! asserts two invariants per fixture:
//!
//! 1. **Idempotence**: `format(format(src)) == format(src)`. Running
//!    the formatter twice must produce identical output — a stable
//!    fixed point.
//! 2. **AST round-trip**: `parse(format(src))` succeeds and produces
//!    the same AST as `parse(src)` (modulo source spans, which
//!    necessarily change after reformatting).
//!
//! Together these prove the formatter is a faithful canonicalizer:
//! it never alters the program's meaning and converges to a unique
//! canonical form regardless of starting formatting.

use std::fs;
use std::path::{Path, PathBuf};

use keron_lang::{Program, format, parse};

fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/parse");
    let mut out: Vec<PathBuf> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension().and_then(|e| e.to_str()) == Some("keron")).then_some(path)
        })
        .collect();
    out.sort();
    out
}

/// Strip span info so we can compare ASTs across two parses where
/// byte offsets differ (the second parse is over the formatted source,
/// which has different column/line positions).
fn ast_shape(program: &Program) -> String {
    // Debug-format prints spans; rewrite them to a constant marker.
    let raw = format!("{program:#?}");
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        // Match `span: <num>..<num>` and elide the numbers.
        if raw[i..].starts_with("span: ") {
            out.push_str("span: <elided>");
            i += "span: ".len();
            let bytes = raw.as_bytes();
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            continue;
        }
        let c = raw[i..].chars().next().expect("i is before string end");
        out.push(c);
        i += c.len_utf8();
    }
    out
}

#[test]
fn ast_shape_walk_is_unicode_safe() {
    let program = parse("val café = \"naïve ☕\"").expect("source parses");
    let shape = ast_shape(&program);
    assert!(shape.contains("café"));
    assert!(shape.contains("naïve ☕"));
}

#[test]
fn formatter_is_idempotent_over_parse_corpus() {
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for path in corpus_files() {
        let src = fs::read_to_string(&path).expect("read fixture");
        let once = match format(&src) {
            Ok(s) => s,
            Err(diags) => {
                failures.push((path.clone(), format!("first format failed: {diags:?}")));
                continue;
            }
        };
        let twice = match format(&once) {
            Ok(s) => s,
            Err(diags) => {
                failures.push((
                    path.clone(),
                    format!("second format failed; first run yielded:\n{once}\nerr: {diags:?}"),
                ));
                continue;
            }
        };
        if once != twice {
            failures.push((
                path.clone(),
                format!("not idempotent:\n--- once ---\n{once}--- twice ---\n{twice}"),
            ));
        }
    }
    if !failures.is_empty() {
        for (path, msg) in &failures {
            eprintln!("\n=== {} ===\n{msg}", path.display());
        }
        panic!("{} fixture(s) failed idempotence", failures.len());
    }
}

/// Returns every `#...` comment in `src` as a list of trimmed
/// strings, in source order. Used to assert that no comment goes
/// missing across a format pass.
fn collect_comment_texts(src: &str) -> Vec<String> {
    let (_, map) = keron_lang::parse_with_comments(src).expect("must parse");
    map.comments
        .into_iter()
        .map(|(c, _)| c.text.trim().to_string())
        .collect()
}

#[test]
fn formatter_preserves_every_comment_across_corpus() {
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for path in corpus_files() {
        let src = fs::read_to_string(&path).expect("read fixture");
        let before = collect_comment_texts(&src);
        if before.is_empty() {
            continue;
        }
        let formatted = match format(&src) {
            Ok(s) => s,
            Err(diags) => {
                failures.push((path.clone(), format!("format failed: {diags:?}")));
                continue;
            }
        };
        let after = collect_comment_texts(&formatted);
        if before != after {
            failures.push((
                path.clone(),
                format!(
                    "comment set changed:\n--- before ({}) ---\n{:?}\n--- after ({}) ---\n{:?}",
                    before.len(),
                    before,
                    after.len(),
                    after,
                ),
            ));
        }
    }
    if !failures.is_empty() {
        for (path, msg) in &failures {
            eprintln!("\n=== {} ===\n{msg}", path.display());
        }
        panic!("{} fixture(s) lost or reordered comments", failures.len());
    }
}

#[test]
fn formatter_preserves_ast_modulo_spans() {
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for path in corpus_files() {
        let src = fs::read_to_string(&path).expect("read fixture");
        let Ok(before) = parse(&src) else {
            // intentionally-broken fixtures live under errors/
            continue;
        };
        let formatted = match format(&src) {
            Ok(s) => s,
            Err(diags) => {
                failures.push((path.clone(), format!("format failed: {diags:?}")));
                continue;
            }
        };
        let after = match parse(&formatted) {
            Ok(p) => p,
            Err(diags) => {
                failures.push((
                    path.clone(),
                    format!("formatted output failed to re-parse:\n{formatted}\nerr: {diags:?}"),
                ));
                continue;
            }
        };
        let before_shape = ast_shape(&before);
        let after_shape = ast_shape(&after);
        if before_shape != after_shape {
            failures.push((
                path.clone(),
                format!(
                    "AST differs after format:\n--- formatted source ---\n{formatted}\n--- before ---\n{before_shape}\n--- after ---\n{after_shape}"
                ),
            ));
        }
    }
    if !failures.is_empty() {
        for (path, msg) in &failures {
            eprintln!("\n=== {} ===\n{msg}", path.display());
        }
        panic!("{} fixture(s) failed AST round-trip", failures.len());
    }
}
