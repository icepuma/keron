//! chumsky-based parser for keron source.

mod block;
mod depth;
mod expr;
mod fn_decl;
mod match_expr;
mod pattern;
mod reconcile;
mod string;
mod struct_decl;
#[cfg(test)]
mod tests;
mod type_alias;
mod types;
mod use_decl;
mod util;

use chumsky::error::{RichPattern, RichReason};
use chumsky::input::InputRef;
use chumsky::prelude::*;

use crate::{
    ast::{Expr, Item, Program, Spanned, ValDecl},
    diagnostic::Diagnostic,
};

use self::{
    expr::expr,
    fn_decl::fn_decl,
    reconcile::reconcile_decl,
    struct_decl::struct_decl,
    type_alias::type_alias_decl,
    types::type_annotation,
    use_decl::use_decl,
    util::{Extra, ident, pad, span_to_range, spanned},
};
use crate::lex::{MultilineClose, is_multiline_close, multiline_open};

/// Parse keron source into a [`Program`].
///
/// # Errors
/// Returns one or more [`Diagnostic`]s when the source has syntax
/// errors. Item-level recovery means a file with several broken
/// top-level items reports one diagnostic per broken item, not just
/// the first.
pub fn parse(src: &str) -> Result<Program, Vec<Diagnostic>> {
    let (program, diagnostics) = parse_recovering(src);
    if diagnostics.is_empty() {
        Ok(program)
    } else {
        Err(diagnostics)
    }
}

/// Parse keron source with error recovery, returning the partial AST
/// alongside all diagnostics.
///
/// This is the entry point for editor tooling: a buffer that is
/// mid-edit still yields every top-level item that parses, so
/// features like outline, hover, and completion keep working while
/// the user types. Valid input yields the full program and an empty
/// diagnostics vector — the AST is byte-identical to what [`parse`]
/// returns.
#[must_use]
pub fn parse_recovering(src: &str) -> (Program, Vec<Diagnostic>) {
    // Reject pathologically nested input before chumsky recurses on the
    // native stack, so a malicious manifest gets a clean diagnostic
    // instead of aborting the process with a stack overflow.
    if let Err(diagnostics) = depth::enforce_nesting_limit(src) {
        return (Program { items: Vec::new() }, diagnostics);
    }
    let result = program().parse(src);
    let diagnostics = result.errors().map(rich_to_diagnostic).collect();
    let program = result
        .into_output()
        .unwrap_or(Program { items: Vec::new() });
    (program, diagnostics)
}

/// Build a [`Diagnostic`] from a chumsky [`Rich`] error.
///
/// chumsky's default `Display` dumps every leading-char alternative it
/// considered, including synthetic ones — `any` from the whitespace
/// rule, `something else` when nothing matched, `'#'` from the
/// comment-lead char, and `'"true"'`-style double-quoted keyword
/// identifiers from `text::keyword`. We rebuild the message from
/// [`RichReason`] so the user sees only meaningful alternatives.
fn rich_to_diagnostic(r: &Rich<'_, char>) -> Diagnostic {
    let span = *r.span();
    let range = span.start()..span.end();
    let (message, help) = match r.reason() {
        RichReason::Custom(msg) => (msg.clone(), custom_help(msg)),
        RichReason::ExpectedFound { expected, found } => {
            let cleaned = cleaned_alternatives(expected);
            let msg = format_expected_found(found.as_deref(), &cleaned);
            let help = expected_help(&cleaned, found.is_none());
            (msg, help)
        }
    };
    let mut d = Diagnostic::new(range, message);
    if let Some(h) = help {
        d = d.with_help(h);
    }
    d
}

fn cleaned_alternatives(expected: &[RichPattern<'_, char>]) -> Vec<String> {
    let mut out: Vec<String> = expected.iter().filter_map(format_pattern).collect();
    out.sort();
    out.dedup();
    out
}

/// Render one expected-token entry for the user, returning `None` for
/// alternatives that exist only as a side-effect of the grammar and
/// would confuse the reader (e.g. `RichPattern::Any` from `pad()`,
/// `'#'` from the comment-lead rule).
fn format_pattern(p: &RichPattern<'_, char>) -> Option<String> {
    // Drop everything not explicitly handled below. Two notable
    // "expected" patterns we silently swallow:
    //   - `RichPattern::Any`: produced by `pad()`'s `any().filter(...)`
    //     whitespace rule. "expected any" is meaningless noise.
    //   - `RichPattern::SomethingElse`: chumsky's "no alternative
    //     recorded" fallback — never user-facing.
    // The wildcard also covers future variants since `RichPattern` is
    // `#[non_exhaustive]`.
    match p {
        RichPattern::EndOfInput => Some("end of input".into()),
        RichPattern::Label(l) => Some(l.to_string()),
        RichPattern::Identifier(i) => {
            // `text::keyword("if")` is recorded as `Identifier("\"if\"")`
            // — strip the Debug-formatted quotes so we render `` `if` ``.
            let s = i.trim_matches('"');
            Some(format!("`{s}`"))
        }
        RichPattern::Token(t) => {
            let c: char = **t;
            // `#` is only ever the comment lead; suggesting it as a
            // valid token at an error site is misleading.
            if c == '#' {
                None
            } else {
                Some(format!("`{c}`"))
            }
        }
        _ => None,
    }
}

fn format_expected_found(found: Option<&char>, expected: &[String]) -> String {
    let mut msg = found.map_or_else(
        || String::from("unexpected end of input"),
        |c| format!("unexpected `{c}`"),
    );
    match expected {
        [] => {}
        [one] => {
            msg.push_str(", expected ");
            msg.push_str(one);
        }
        [a, b] => {
            msg.push_str(", expected ");
            msg.push_str(a);
            msg.push_str(" or ");
            msg.push_str(b);
        }
        many => {
            msg.push_str(", expected ");
            for x in &many[..many.len() - 1] {
                msg.push_str(x);
                msg.push_str(", ");
            }
            msg.push_str("or ");
            msg.push_str(&many[many.len() - 1]);
        }
    }
    msg
}

fn expected_help(alternatives: &[String], at_eof: bool) -> Option<String> {
    // Pick a hint for the most common single-alternative cases. Each
    // arm answers "what do I write here?" rather than restating the
    // alternative; the message already says "expected X".
    if let [only] = alternatives {
        return Some(match only.as_str() {
            "expression" => {
                if at_eof {
                    "an expression is required here — write a value, name, or call.".into()
                } else {
                    "write an expression (literal, name, call, or `(…)`).".into()
                }
            }
            "type" => {
                "write a type — one of `String`, `Int`, `Boolean`, `Double`, `Void`, `List<T>`, `Map<K, V>`, or a named type.".into()
            }
            "identifier" => "write a name (lowercase letter or `_`, then letters / digits / `_`).".into(),
            "block" => "wrap the body in `{ … }`.".into(),
            "pattern" => "write a `match` pattern: `_`, a literal, a name, or `Struct { field, … }`.".into(),
            _ => return None,
        });
    }
    None
}

fn custom_help(msg: &str) -> Option<String> {
    // `Rich::custom` carries the message as plain text; pattern-match
    // the few stable ones we emit ourselves to attach a help line.
    if msg.contains("reserved keyword") {
        Some("rename the binding to a non-keyword identifier.".into())
    } else if msg.contains("named-argument LHS must be an identifier") {
        Some("use the form `name = value` in call arguments.".into())
    } else if msg.contains("struct pattern names must start with an uppercase letter") {
        Some(
            "capitalize the leading letter — struct names are uppercase, binds are lowercase."
                .into(),
        )
    } else if msg.contains("bare pattern bindings must start with a lowercase letter") {
        Some("use a lowercase name (e.g. `x`) or `_` for a wildcard.".into())
    } else {
        None
    }
}

fn program<'src>() -> impl Parser<'src, &'src str, Program, Extra<'src>> {
    item()
        .map(Some)
        .recover_with(via_parser(item_recovery()))
        .repeated()
        .collect::<Vec<_>>()
        .map(|items: Vec<Option<Item>>| Program {
            items: items.into_iter().flatten().collect(),
        })
        .padded_by(pad())
        .then_ignore(end())
}

/// Recovery for a broken top-level item: emit the item's error, then
/// skip forward to the next plausible item start so the items after
/// it still parse.
///
/// The synchronization point is a top-level keyword at column zero —
/// i.e. immediately after a newline. The scanner tracks multiline
/// strings so source-looking examples inside string bodies do not
/// become phantom recovered items. Requiring column zero avoids
/// re-syncing on an indented declaration inside a broken block.
fn item_recovery<'src>() -> impl Parser<'src, &'src str, Option<Item>, Extra<'src>> {
    custom(recover_item)
}

fn recover_item<'src>(
    inp: &mut InputRef<'src, '_, &'src str, Extra<'src>>,
) -> Result<Option<Item>, Rich<'src, char>> {
    let start = inp.cursor();
    let mut consumed = false;
    let mut line = String::new();
    let mut multiline: Option<MultilineClose> = None;

    loop {
        let before = inp.save();
        let Some(c) = inp.next() else {
            if consumed {
                return Ok(None);
            }
            return Err(Rich::custom(
                inp.span_since(&start),
                "unable to recover at end of input",
            ));
        };
        consumed = true;
        if c != '\n' {
            line.push(c);
            continue;
        }

        multiline = match multiline {
            Some(close) if is_multiline_close(&line, close) => None,
            Some(close) => Some(close),
            None => multiline_open(&line),
        };
        line.clear();

        if multiline.is_none() && next_is_sync_keyword(inp) {
            inp.rewind(before);
            return Ok(None);
        }
    }
}

fn next_is_sync_keyword<'src>(inp: &mut InputRef<'src, '_, &'src str, Extra<'src>>) -> bool {
    const LONGEST_KEYWORD: usize = "reconcile".len();

    let checkpoint = inp.save();
    let mut word = String::with_capacity(LONGEST_KEYWORD);
    let mut chars = 0usize;
    while let Some(c) = inp.next() {
        if !unicode_ident::is_xid_continue(c) {
            break;
        }
        chars += 1;
        if chars > LONGEST_KEYWORD {
            inp.rewind(checkpoint);
            return false;
        }
        word.push(c);
    }
    inp.rewind(checkpoint);
    matches!(
        word.as_str(),
        "val" | "fn" | "struct" | "type" | "reconcile" | "from" | "if" | "for" | "match"
    )
}

fn item<'src>() -> impl Parser<'src, &'src str, Item, Extra<'src>> {
    let e = expr();
    // Top-level expression statements are restricted to expressions
    // beginning with `if`, `for`, or `match` — the three constructs
    // that can produce a `Void` value (and so the only ones whose
    // top-level use as a statement is meaningful; a Void `match`
    // dispatching `reconcile`s per OS is as legitimate an effect as
    // the equivalent `if` ladder). Gating this with a `peek` for the
    // leading keyword also keeps normal declarations' error messages
    // crisp: errors inside `val x = …` aren't merged with a generic
    // "expression here" alternative.
    let void_stmt = choice((
        text::keyword("if").rewind().ignored(),
        text::keyword("for").rewind().ignored(),
        text::keyword("match").rewind().ignored(),
    ))
    .ignore_then(e.clone())
    .map(Item::ExprStmt);
    choice((
        use_decl().map(Item::Use),
        struct_decl(e.clone()).map(Item::Struct),
        type_alias_decl().map(Item::TypeAlias),
        val_decl(e.clone()).map(Item::Val),
        fn_decl(e.clone()).map(Item::Fn),
        reconcile_decl(e).map(Item::Reconcile),
        void_stmt,
    ))
    .padded_by(pad())
}

pub(super) fn val_decl<'src, P>(
    expr: P,
) -> impl Parser<'src, &'src str, ValDecl, Extra<'src>> + Clone
where
    P: Parser<'src, &'src str, Spanned<Expr>, Extra<'src>> + Clone + 'src,
{
    let kw_val = text::keyword("val").padded_by(pad());
    let colon = just(':').padded_by(pad());
    let eq = just('=').padded_by(pad());
    let annotation = colon.ignore_then(spanned(type_annotation())).or_not();

    kw_val
        .ignore_then(spanned(ident()))
        .then(annotation)
        .then_ignore(eq)
        .then(expr)
        .map_with(|((name, ty), value), e| ValDecl {
            name,
            ty,
            value,
            span: span_to_range(e.span()),
        })
}
