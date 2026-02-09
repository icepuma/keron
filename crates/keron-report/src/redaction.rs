use std::collections::BTreeSet;

pub fn redact_sensitive(text: &str, sensitive_values: &BTreeSet<String>) -> String {
    let mut sorted: Vec<&str> = sensitive_values
        .iter()
        .filter(|v| v.len() >= 3)
        .map(String::as_str)
        .collect();
    sorted.sort_by_key(|value| std::cmp::Reverse(value.len()));

    let mut result = text.to_string();
    for value in sorted {
        result = result.replace(value, "[REDACTED]");
    }
    result
}
