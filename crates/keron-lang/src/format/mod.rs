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
    fn nested_block_comment_stays_inside_block() {
        let src = "fn f(): Int {\n  # documents x\n  val x = 1\n  x\n}\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn field_receiver_parentheses_preserve_coalesce_meaning() {
        let src = concat!(
            "struct Point { x: Int }\n",
            "val maybe: Point? = null\n",
            "val fallback: Point = Point(1)\n",
            "val n: Int = (maybe ?? fallback).x\n",
        );
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn statement_blocks_inside_lists_do_not_inline_with_semicolons() {
        let src = "val xs: List<Int> = [if true {\n  val x = 1\n  x\n} else {\n  2\n}]\n";
        let out = fmt(src);
        assert!(
            !out.contains(';'),
            "formatter must not emit parser-invalid semicolon separators: {out}",
        );
        crate::parse(&out).expect("formatted output should parse");
    }

    #[test]
    fn cooked_multiline_string_style_is_preserved() {
        let src = "val s: String = \"\"\"\nhello\n\"\"\"\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn raw_multiline_string_style_is_preserved() {
        let src = "val s: String = r#\"\"\"\n${HOME}\n\"\"\"#\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn cooked_multiline_interpolation_style_is_preserved() {
        let src = "val name = \"keron\"\nval s: String = \"\"\"\nhello ${name}\n\"\"\"\n";
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

    /// The formatter writes files in place, so its output must always
    /// re-parse. A cooked string holding a non-ASCII character used to
    /// be re-emitted as `\u{...}`, an escape the parser rejects —
    /// silently corrupting the manifest. Guard the whole round trip.
    #[test]
    fn non_ascii_cooked_string_round_trips_and_reparses() {
        let src = "val s = \"café ☕ \u{0007}\"\n";
        let once = fmt(src);
        crate::parse(&once).expect("formatted output must re-parse");
        assert_eq!(fmt(&once), once, "must be idempotent");
    }

    /// `(-a) ** b` must not be flattened to `-a ** b`, which parses as
    /// `-(a ** b)` — a different value. `**` is the one operator that
    /// binds tighter than a leading unary.
    #[test]
    fn unary_left_of_power_keeps_parentheses() {
        let src = "val x = (-2) ** 2\n";
        let once = fmt(src);
        crate::parse(&once).expect("formatted output must re-parse");
        assert!(
            once.contains("(-2) ** 2"),
            "unary LHS of ** must keep parens, got: {once}"
        );
        assert_eq!(fmt(&once), once, "must be idempotent");
    }

    /// An import path is the parser's *decoded* string; a `"` or `\`
    /// in it must be re-escaped, or the emitted `from "…"` is
    /// unparseable.
    #[test]
    fn use_path_with_quote_round_trips() {
        let src = "from \"./a\\\"b.keron\" use x\n";
        let once = fmt(src);
        crate::parse(&once).expect("formatted use path must re-parse");
        assert_eq!(fmt(&once), once, "must be idempotent");
    }

    /// A param default that is an `if` with a statement body must not be
    /// inlined as `{ a; b }` — the block grammar has no `;` separator.
    #[test]
    fn param_default_with_statement_block_stays_block_form() {
        let src = concat!(
            "fn f(x: Int = if true {\n",
            "  val a = 1\n",
            "  a\n",
            "} else {\n",
            "  2\n",
            "}): Int {\n",
            "  x\n",
            "}\n",
        );
        let out = fmt(src);
        assert!(
            !out.contains("; "),
            "statement-block default must not inline with `;`: {out}"
        );
        crate::parse(&out).expect("formatted output must re-parse");
        assert_eq!(fmt(&out), out, "must be idempotent");
    }

    /// `else if` must stay flat, not become an `else { if … }` pyramid
    /// that adds an indentation level per chain link.
    #[test]
    fn else_if_chain_stays_flat() {
        let src = "val x = if a {\n  1\n} else if b {\n  2\n} else {\n  3\n}\n";
        let once = fmt(src);
        crate::parse(&once).expect("formatted output must re-parse");
        assert!(
            once.contains("} else if b {"),
            "else if must stay flat, got:\n{once}"
        );
        assert_eq!(fmt(&once), once, "must be idempotent");
    }

    /// A reconcile whose single step is a map literal must keep the
    /// `reconcile { … }` block wrapper — `reconcile {"a": 1}` re-parses
    /// as the block form and fails on the map's `:`.
    #[test]
    fn reconcile_with_leading_map_keeps_block_wrapper() {
        let src = "reconcile { {\"a\": 1} }\n";
        let out = fmt(src);
        crate::parse(&out).expect("formatted output must re-parse");
        assert_eq!(fmt(&out), out, "must be idempotent");
    }

    /// A comment-only file must not gain a leading blank line, and a
    /// comment right after the last item must not gain a blank line the
    /// source didn't have.
    #[test]
    fn trailing_comments_do_not_gain_spurious_blank_lines() {
        assert_eq!(fmt("# header only\n"), "# header only\n");
        assert_eq!(fmt("val a = 1\n# note\n"), "val a = 1\n# note\n");
        // An authored blank line before the comment is preserved.
        assert_eq!(fmt("val a = 1\n\n# note\n"), "val a = 1\n\n# note\n");
    }

    /// A multi-line interpolated string nested inside a call argument
    /// must keep its verbatim block form — collapsing it to a single
    /// line drops the interpolation `indent` the evaluator re-applies.
    /// The block-form separator/close must also stay off the `"""` line.
    #[test]
    fn nested_multiline_string_is_not_collapsed() {
        let src = "val name = \"x\"\nval s = f(\n  \"\"\"\n  hello ${name}\n  \"\"\"\n)\n";
        let once = fmt(src);
        crate::parse(&once).expect("formatted output must re-parse");
        assert!(
            once.contains("hello ${name}"),
            "nested multiline interpolation must stay multiline, got:\n{once}"
        );
        assert_eq!(fmt(&once), once, "must be idempotent");
    }

    /// A multi-line string as a list element must round-trip: the
    /// block-form comma cannot follow the close on the same line.
    #[test]
    fn multiline_string_list_element_round_trips() {
        let src = "val xs = [\n  1,\n  \"\"\"\n  hi\n  \"\"\"\n]\n";
        let once = fmt(src);
        crate::parse(&once).expect("formatted output must re-parse");
        assert_eq!(fmt(&once), once, "must be idempotent");
    }

    /// An identifier ending in `r` immediately before a `#"""` comment
    /// used to be misread as a raw-string opener, so the comment
    /// extractor swallowed that comment and every later one — and the
    /// formatter then deleted them. All comments must survive.
    #[test]
    fn identifier_ending_in_r_before_hash_comment_keeps_comments() {
        let src = "val bar = 1 #\"\"\"\n# important note\nval y = 2\n";
        let out = fmt(src);
        crate::parse(&out).expect("formatted output must re-parse");
        assert!(
            out.contains("# important note"),
            "later comment lost:\n{out}"
        );
    }

    /// A comment trailing a match arm used to wedge the comment cursor
    /// and drop every later comment in the file. It must survive and
    /// the whole file's comments must be preserved.
    #[test]
    fn comment_on_match_arm_is_preserved() {
        let src = concat!(
            "val r = match x {\n",
            "  1 => \"a\",\n",
            "  _ => \"b\", # fallback\n",
            "}\n",
            "\n",
            "# documents y\n",
            "val y = 2\n",
        );
        let out = fmt(src);
        crate::parse(&out).expect("formatted output must re-parse");
        assert!(out.contains("# fallback"), "arm comment lost:\n{out}");
        assert!(out.contains("# documents y"), "later comment lost:\n{out}");
    }
}
