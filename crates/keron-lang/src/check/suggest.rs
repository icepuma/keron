//! Nearest-name suggestions for "unknown X" diagnostics.
//!
//! A hand-rolled bounded Levenshtein. The distance bound follows
//! rustc's heuristic: a
//! suggestion is only offered when the edit distance is at most a
//! third of the queried name's length (minimum 1), which keeps
//! `spilt` → `split` while suppressing noise like `x` → `os`.

/// Suggestions are optional diagnostic polish, so hostile identifiers
/// or namespaces must not turn them into an allocation/CPU amplifier.
const MAX_SUGGEST_CHARS: usize = 64;
const MAX_SUGGEST_CANDIDATES: usize = 1024;

/// The closest candidate to `name` within the acceptance bound, or
/// `None` when nothing is plausibly a typo of it. Ties resolve to the
/// candidate encountered first, so pass a deterministically ordered
/// iterator when snapshot stability matters.
pub(super) fn nearest<'a, I>(candidates: I, name: &str) -> Option<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let name_len = bounded_char_count(name)?;
    let max_dist = (name_len / 3).max(1);
    let candidates: Vec<&str> = candidates
        .into_iter()
        .take(MAX_SUGGEST_CANDIDATES + 1)
        .collect();
    if candidates.len() > MAX_SUGGEST_CANDIDATES {
        return None;
    }
    let mut best: Option<(usize, &str)> = None;
    for candidate in candidates {
        let Some(candidate_len) = bounded_char_count(candidate) else {
            continue;
        };
        if name_len.abs_diff(candidate_len) > max_dist {
            continue;
        }
        if candidate == name {
            continue;
        }
        // A case-only mismatch is always the intended name, however
        // many characters differ (`MacOS` → `Macos`).
        if candidate.eq_ignore_ascii_case(name) {
            return Some(candidate.to_string());
        }
        let d = levenshtein(name, candidate);
        if d <= max_dist && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, candidate));
        }
    }
    best.map(|(_, c)| c.to_string())
}

fn bounded_char_count(s: &str) -> Option<usize> {
    let len = s.chars().take(MAX_SUGGEST_CHARS + 1).count();
    (len <= MAX_SUGGEST_CHARS).then_some(len)
}

/// Optimal-string-alignment distance: Levenshtein plus adjacent
/// transpositions as a single edit, so the classic `spilt` → `split`
/// typo costs 1 instead of 2 (rustc's suggestion metric does the
/// same).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    // Full matrix; identifiers are short, so the O(len(a) * len(b))
    // cost (and allocation) is negligible.
    let cols = b.len() + 1;
    let mut m = vec![0usize; (a.len() + 1) * cols];
    for (j, cell) in m.iter_mut().enumerate().take(b.len() + 1) {
        *cell = j;
    }
    for i in 1..=a.len() {
        m[i * cols] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut best = (m[(i - 1) * cols + j - 1] + cost)
                .min(m[(i - 1) * cols + j] + 1)
                .min(m[i * cols + j - 1] + 1);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                best = best.min(m[(i - 2) * cols + j - 2] + 1);
            }
            m[i * cols + j] = best;
        }
    }
    m[a.len() * cols + b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_close_names() {
        assert_eq!(
            nearest(["split", "join", "trim"], "spilt").as_deref(),
            Some("split")
        );
        assert_eq!(
            nearest(["Macos", "Linux", "Windows"], "MacOs").as_deref(),
            Some("Macos")
        );
        // Case-only mismatches always suggest, even past the edit
        // bound.
        assert_eq!(
            nearest(["Macos", "Linux", "Windows"], "MACOS").as_deref(),
            Some("Macos")
        );
    }

    #[test]
    fn stays_quiet_when_nothing_is_close() {
        assert_eq!(nearest(["split", "join"], "reconcile"), None);
        // Short names get a bound of 1 edit, so unrelated one-letter
        // names don't produce absurd suggestions.
        assert_eq!(nearest(["os"], "x"), None);
    }

    #[test]
    fn exact_match_is_not_a_suggestion() {
        assert_eq!(nearest(["split"], "split"), None);
    }

    #[test]
    fn hostile_identifier_length_skips_suggestion_work() {
        let long = "é".repeat(MAX_SUGGEST_CHARS + 1);
        assert_eq!(nearest(["split"], &long), None);
        assert_eq!(nearest([long.as_str()], "spilt"), None);
    }

    #[test]
    fn hostile_namespace_size_skips_suggestion_work() {
        let candidates: Vec<String> = (0..=MAX_SUGGEST_CANDIDATES)
            .map(|i| format!("name_{i}"))
            .collect();
        assert_eq!(
            nearest(candidates.iter().map(String::as_str), "name_1"),
            None
        );
    }
}
