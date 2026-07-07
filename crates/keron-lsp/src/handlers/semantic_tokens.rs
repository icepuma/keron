//! `textDocument/semanticTokens/full` — syntax highlighting.
//!
//! Two layers: the error-tolerant lexical scanner
//! (`keron_lang::lex_tokens`) colors keywords / strings / numbers /
//! comments / operators on *any* buffer, and — when the buffer
//! currently parses — an AST overlay classifies identifiers precisely
//! (function vs type vs parameter vs property). Mid-edit buffers keep
//! lexical colors and fall back to name-based identifier
//! classification, so highlighting never flickers off while typing.

use std::collections::HashMap;

use keron_lang::{Expr, Item, LexTokenKind, Program, lex_tokens};
use lsp_types::{SemanticToken, SemanticTokenType, SemanticTokens, SemanticTokensParams};

use crate::analysis::node_at::walk_exprs;
use crate::state::ServerState;

/// The legend advertised in the server capabilities. Index positions
/// are the `token_type` values encoded below — keep the two in sync
/// by construction (both read this constant).
pub const TOKEN_TYPES: [SemanticTokenType; 10] = [
    SemanticTokenType::KEYWORD,
    SemanticTokenType::COMMENT,
    SemanticTokenType::STRING,
    SemanticTokenType::NUMBER,
    SemanticTokenType::OPERATOR,
    SemanticTokenType::FUNCTION,
    SemanticTokenType::TYPE,
    SemanticTokenType::VARIABLE,
    SemanticTokenType::PARAMETER,
    SemanticTokenType::PROPERTY,
];

const KEYWORD: u32 = 0;
const COMMENT: u32 = 1;
const STRING: u32 = 2;
const NUMBER: u32 = 3;
const OPERATOR: u32 = 4;
const FUNCTION: u32 = 5;
const TYPE: u32 = 6;
const VARIABLE: u32 = 7;
const PARAMETER: u32 = 8;
const PROPERTY: u32 = 9;

pub fn handle(state: &ServerState, params: &SemanticTokensParams) -> Option<SemanticTokens> {
    let (_, doc) = state.doc_by_uri(&params.text_document.uri)?;
    let text = &doc.text;
    let overlay = doc
        .last_good
        .as_ref()
        // AST spans are only valid against the exact text they were
        // parsed from; skip the overlay mid-edit.
        .filter(|lg| lg.text == *text)
        .map(|lg| IdentClassifier::new(&lg.program));

    let mut spans: Vec<(usize, usize, u32)> = Vec::new();
    for token in lex_tokens(text) {
        let token_type = match token.kind {
            LexTokenKind::Comment => COMMENT,
            LexTokenKind::Str => STRING,
            LexTokenKind::Number => NUMBER,
            LexTokenKind::Keyword => KEYWORD,
            LexTokenKind::Operator => OPERATOR,
            LexTokenKind::BuiltinType => TYPE,
            LexTokenKind::Punct => continue,
            LexTokenKind::Ident => {
                let name = &text[token.span.clone()];
                overlay.as_ref().map_or(VARIABLE, |c| {
                    c.classify(token.span.start, token.span.end, name)
                })
            }
        };
        spans.push((token.span.start, token.span.end, token_type));
    }
    Some(SemanticTokens {
        result_id: None,
        data: encode(text, &spans),
    })
}

/// Identifier classification from the parsed AST: an exact span map
/// for declaration/reference sites the walker visits, plus a name map
/// for everything else (type annotations, builtin references).
struct IdentClassifier {
    by_span: HashMap<(usize, usize), u32>,
    by_name: HashMap<String, u32>,
}

impl IdentClassifier {
    fn new(program: &Program) -> Self {
        let mut by_span: HashMap<(usize, usize), u32> = HashMap::new();
        let mut by_name: HashMap<String, u32> = HashMap::new();

        // Builtins first so module-local declarations override them.
        for module in keron_modules::stdlib::registry().values() {
            for name in module.fns.keys() {
                by_name.insert(name.clone(), FUNCTION);
            }
            for name in module.types.keys() {
                by_name.insert(name.clone(), TYPE);
            }
        }

        for item in &program.items {
            match item {
                Item::Fn(f) => {
                    by_span.insert(span_key(&f.name.span), FUNCTION);
                    by_name.insert(f.name.node.clone(), FUNCTION);
                    for p in &f.params {
                        by_span.insert(span_key(&p.name.span), PARAMETER);
                    }
                }
                Item::Val(v) => {
                    by_span.insert(span_key(&v.name.span), VARIABLE);
                }
                Item::Struct(s) => {
                    by_span.insert(span_key(&s.name.span), TYPE);
                    by_name.insert(s.name.node.clone(), TYPE);
                    for f in &s.fields {
                        by_span.insert(span_key(&f.name.span), PROPERTY);
                    }
                }
                Item::TypeAlias(t) => {
                    by_span.insert(span_key(&t.name.span), TYPE);
                    by_name.insert(t.name.node.clone(), TYPE);
                }
                Item::Use(_) | Item::Reconcile(_) | Item::ExprStmt(_) => {}
            }
        }

        walk_exprs(program, &mut |e| match &e.node {
            Expr::Call { callee, .. } => {
                by_span.insert(span_key(&callee.span), FUNCTION);
            }
            Expr::StructLiteral { name, fields } => {
                by_span.insert(span_key(&name.span), TYPE);
                for f in fields {
                    if f.value.is_some() {
                        by_span.insert(span_key(&f.name.span), PROPERTY);
                    }
                }
            }
            Expr::Field { field, .. } => {
                by_span.insert(span_key(&field.span), PROPERTY);
            }
            _ => {}
        });

        Self { by_span, by_name }
    }

    fn classify(&self, start: usize, end: usize, name: &str) -> u32 {
        self.by_span
            .get(&(start, end))
            .or_else(|| self.by_name.get(name))
            .copied()
            .unwrap_or(VARIABLE)
    }
}

const fn span_key(span: &keron_lang::Span) -> (usize, usize) {
    (span.start, span.end)
}

/// Delta-encode into the LSP wire format. Tokens spanning multiple
/// lines (multiline strings) are split per line, because not every
/// client supports multiline tokens.
fn encode(text: &str, spans: &[(usize, usize, u32)]) -> Vec<SemanticToken> {
    let index = crate::line_index::LineIndex::new(text);
    let mut data = Vec::new();
    let (mut prev_line, mut prev_char) = (0u32, 0u32);
    for &(start, end, token_type) in spans {
        for (seg_start, seg_end) in split_lines(text, start, end) {
            let start_pos = index.position(text, seg_start);
            let end_pos = index.position(text, seg_end);
            let length = end_pos.character.saturating_sub(start_pos.character);
            if length == 0 {
                continue;
            }
            let delta_line = start_pos.line - prev_line;
            let delta_start = if delta_line == 0 {
                start_pos.character - prev_char
            } else {
                start_pos.character
            };
            data.push(SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type,
                token_modifiers_bitset: 0,
            });
            (prev_line, prev_char) = (start_pos.line, start_pos.character);
        }
    }
    data
}

fn split_lines(text: &str, start: usize, end: usize) -> Vec<(usize, usize)> {
    let mut segments = Vec::new();
    let mut seg_start = start;
    for (i, b) in text[start..end].bytes().enumerate() {
        if b == b'\n' {
            segments.push((seg_start, start + i));
            seg_start = start + i + 1;
        }
    }
    segments.push((seg_start, end));
    segments.retain(|(s, e)| e > s);
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_handles_multiline_span() {
        let text = "ab\ncd\nef";
        assert_eq!(split_lines(text, 0, 8), vec![(0, 2), (3, 5), (6, 8)]);
        assert_eq!(split_lines(text, 0, 2), vec![(0, 2)]);
        // Span ending exactly on the newline produces no empty segment.
        assert_eq!(split_lines(text, 0, 3), vec![(0, 2)]);
    }

    #[test]
    fn classifier_prefers_span_over_name() {
        let program = keron_lang::parse("fn f(): Int { 1 }\nval f2: Int = f()\n").unwrap();
        let c = IdentClassifier::new(&program);
        // Callee reference classified as function via span map.
        let src = "fn f(): Int { 1 }\nval f2: Int = f()\n";
        let call = src.rfind("f()").unwrap();
        assert_eq!(c.classify(call, call + 1, "f"), FUNCTION);
        // Unknown identifier falls back to variable.
        assert_eq!(c.classify(999, 1000, "mystery"), VARIABLE);
        // Builtin known by name.
        assert_eq!(c.classify(999, 1000, "symlink"), FUNCTION);
    }
}
