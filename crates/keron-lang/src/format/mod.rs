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
    Ok(emitter::format_program(src, &program, &comments))
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

    #[test]
    fn adjacent_top_level_vals_stay_adjacent() {
        let src = "val a = 1\nval b = 2\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn one_blank_line_between_top_level_vals_is_preserved() {
        let src = "val a = 1\n\nval b = 2\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn multiple_blank_lines_between_top_level_vals_collapse_to_one() {
        let src = "val a = 1\n\n\n\nval b = 2\n";
        let expected = "val a = 1\n\nval b = 2\n";
        assert_eq!(fmt(src), expected);
    }

    #[test]
    fn leading_comment_stays_attached_to_next_item_across_blank_line() {
        let src = "val a = 1\n\n# documents b\nval b = 2\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn multi_line_map_literal_stays_multi_line_even_when_inline_fits() {
        let src = "val m = {\n  \"a\": 1,\n  \"b\": 2,\n}\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn single_line_map_literal_stays_inline() {
        let src = "val m = {\"a\": 1, \"b\": 2}\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn multi_line_list_literal_stays_multi_line_even_when_inline_fits() {
        let src = "val xs = [\n  1,\n  2,\n  3,\n]\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn single_line_list_literal_stays_inline() {
        let src = "val xs = [1, 2, 3]\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn multi_line_call_args_stay_multi_line_even_when_inline_fits() {
        let src = "val t: Template = template(\n  source = \"a.tpl\",\n  target = \"/b\",\n  vars = {},\n)\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn blank_line_between_block_statements_is_preserved() {
        let src = "fn f(): Void {\n  val a = 1\n\n  val b = 2\n}\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn newline_preserving_pass_is_idempotent_on_combined_layout() {
        let src = concat!(
            "val a = 1\n",
            "val b = 2\n",
            "\n",
            "val m = {\n  \"x\": a,\n  \"y\": b,\n}\n",
            "\n",
            "reconcile symlink(source = \"s\", target = \"t\")\n",
        );
        let once = fmt(src);
        let twice = fmt(&once);
        assert_eq!(once, twice, "format must be idempotent on combined layout");
        assert_eq!(once, src, "well-formed source must round-trip unchanged");
    }
}
