//! Bolero-driven fuzz tests. The contract: neither `parse` nor `check`
//! may panic on any input — well-formed source returns `Ok`, malformed
//! source returns `Err(Vec<Diagnostic>)`.
//!
//! In `cargo test`/`cargo nextest`, bolero samples random inputs (fast,
//! no fuzzer dependency). To run with coverage-guided libfuzzer:
//!     `cargo bolero test parse_never_panics --engine libfuzzer`

use keron_lang::{check, parse};

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
            && let Ok(prog) = parse(s)
        {
            let _ = check(&prog);
        }
    });
}
