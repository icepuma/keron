use keron_lang::{Diagnostic, Item, ValDecl};

use super::CheckedModule;

const MAX_SUGGEST_CHARS: usize = 64;
const MAX_SUGGEST_CANDIDATES: usize = 1024;

/// Build the import error for a missing export, including bounded typo help.
// Parent-only visibility keeps suggestion policy behind the resolver API.
#[allow(clippy::redundant_pub_crate)]
pub(super) fn missing_export_diagnostic(
    span: keron_lang::Span,
    module: &CheckedModule,
    name: &str,
) -> Diagnostic {
    if module.program.items.iter().any(|item| {
        matches!(
            item,
            Item::Val(ValDecl {
                name: n,
                ty: None,
                ..
            }) if n.node == name
        )
    }) {
        return Diagnostic::new(
            span,
            format!("module `{}` defines `{name}`", module.id.display()),
        )
        .with_help(format!(
            "imported vals need an explicit type annotation — add one to `val {name}` in `{}`",
            module.id.display()
        ));
    }
    let mut diagnostic = Diagnostic::new(
        span,
        format!("module `{}` does not export `{name}`", module.id.display()),
    );
    let mut exports: Vec<&str> = module
        .exported_fns
        .iter()
        .chain(module.exported_vals.iter())
        .chain(module.exported_types.keys())
        .map(String::as_str)
        .take(MAX_SUGGEST_CANDIDATES + 1)
        .collect();
    if exports.len() > MAX_SUGGEST_CANDIDATES {
        return diagnostic;
    }
    exports.sort_unstable();
    exports.dedup();
    if let Some(suggestion) = nearest_export(&exports, name) {
        diagnostic = diagnostic.with_help(format!("did you mean `{suggestion}`?"));
    }
    diagnostic
}

fn nearest_export(candidates: &[&str], name: &str) -> Option<String> {
    if candidates.len() > MAX_SUGGEST_CANDIDATES {
        return None;
    }
    let name_len = bounded_char_count(name)?;
    let max_dist = (name_len / 3).max(1);
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let Some(candidate_len) = bounded_char_count(candidate) else {
            continue;
        };
        if name_len.abs_diff(candidate_len) > max_dist || *candidate == name {
            continue;
        }
        let distance = levenshtein(name, candidate);
        if distance <= max_dist && best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, candidate));
        }
    }
    best.map(|(_, candidate)| candidate.to_string())
}

fn bounded_char_count(s: &str) -> Option<usize> {
    let len = s.chars().take(MAX_SUGGEST_CHARS + 1).count();
    (len <= MAX_SUGGEST_CHARS).then_some(len)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, a_char) in a.iter().enumerate() {
        let mut previous_diagonal = row[0];
        row[0] = i + 1;
        for (j, b_char) in b.iter().enumerate() {
            let cost = usize::from(a_char != b_char);
            let next = (previous_diagonal + cost)
                .min(row[j] + 1)
                .min(row[j + 1] + 1);
            previous_diagonal = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_suggestions_are_bounded_against_hostile_input() {
        assert_eq!(nearest_export(&["split"], "slit"), Some("split".into()));

        let long = "é".repeat(MAX_SUGGEST_CHARS + 1);
        assert_eq!(nearest_export(&["split"], &long), None);
        assert_eq!(nearest_export(&[long.as_str()], "spilt"), None);

        let names: Vec<String> = (0..=MAX_SUGGEST_CANDIDATES)
            .map(|index| format!("name_{index}"))
            .collect();
        let candidates: Vec<&str> = names.iter().map(String::as_str).collect();
        assert_eq!(nearest_export(&candidates, "name_1"), None);
    }
}
