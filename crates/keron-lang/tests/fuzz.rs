//! Bolero-driven fuzz tests. The contract: neither `parse` nor full check
//! may panic on any input — well-formed source returns `Ok`, malformed
//! source returns `Err(Vec<Diagnostic>)`.
//!
//! In `cargo test`/`cargo nextest`, bolero samples random inputs (fast,
//! no fuzzer dependency). To run with coverage-guided libfuzzer:
//!     `cargo bolero test parse_never_panics --engine libfuzzer`

use keron_lang::{
    check_module, format as format_source, lex_tokens, parse, parse_recovering,
    parse_with_comments, resolve_type_names,
};

#[path = "support/stdlib.rs"]
mod stdlib;

#[test]
fn parse_never_panics() {
    bolero::check!().for_each(|input: &[u8]| {
        if let Ok(s) = std::str::from_utf8(input) {
            let _ = parse(s);
        }
    });
}

#[test]
fn check_never_panics() {
    bolero::check!().for_each(|input: &[u8]| {
        if let Ok(s) = std::str::from_utf8(input)
            && let Ok(mut prog) = parse(s)
        {
            let imp = stdlib::imports();
            if resolve_type_names(&mut prog, &imp).is_ok() {
                let _ = check_module(&prog, &imp);
            }
        }
    });
}

#[test]
fn editor_and_formatter_surfaces_never_panic() {
    bolero::check!().for_each(|input: &[u8]| {
        let Ok(s) = std::str::from_utf8(input) else {
            return;
        };

        let tokens = lex_tokens(s);
        let mut previous_end = 0usize;
        for token in tokens {
            assert!(token.span.start < token.span.end);
            assert!(token.span.start >= previous_end);
            assert!(s.is_char_boundary(token.span.start));
            assert!(s.is_char_boundary(token.span.end));
            previous_end = token.span.end;
        }

        let _ = parse_recovering(s);
        if parse_with_comments(s).is_ok()
            && let Ok(formatted) = format_source(s)
        {
            assert!(parse(&formatted).is_ok());
        }
    });
}
