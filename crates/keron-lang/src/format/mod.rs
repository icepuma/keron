//! AST-driven pretty-printer for keron source.
//!
//! Public entry: [`format`] takes a source string, parses it (via
//! [`crate::parse_with_comments`]), and returns canonical formatted
//! output. The formatter is opinionated:
//!
//! - 2-space indentation, no tabs.
//! - Single trailing newline.
//! - Inline form when a node + its children fit in the remaining
//!   columns (target ≤ 100); otherwise the node spreads across
//!   multiple lines in block form.
//! - Source-level parens that don't change precedence are dropped;
//!   the emitter re-inserts parens only where required for parse
//!   equivalence.
//! - Comments are round-tripped via the trivia extractor; see
//!   [`crate::trivia`].
//!
//! The emitter itself lives in [`emitter`]; this module is the
//! public surface and the orchestrator.

mod emitter;
mod precedence;
mod string_lit;

use crate::diagnostic::Diagnostic;
use crate::trivia::extract_comments;

/// Parse `src` and return its canonical formatted form. Returns the
/// parser's diagnostics unchanged when `src` is not a valid keron
/// program.
///
/// # Errors
///
/// Returns the parser's diagnostics when `src` does not parse.
pub fn format(src: &str) -> Result<String, Vec<Diagnostic>> {
    let program = crate::parser::parse(src)?;
    let comments = extract_comments(src, &program);
    Ok(emitter::format_program(&program, &comments))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(src: &str) -> String {
        format(src).expect("test source must parse")
    }

    #[test]
    fn empty_program_is_empty_string() {
        assert_eq!(fmt(""), "");
    }

    #[test]
    fn idempotent_on_already_canonical_simple_val() {
        let src = "val x: Int = 1\n";
        assert_eq!(fmt(src), src);
    }
}
