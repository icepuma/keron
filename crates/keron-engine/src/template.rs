use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use crate::secrets::resolve_secret;

fn lock_sensitive_values(
    sensitive: &Arc<Mutex<BTreeSet<String>>>,
) -> tera::Result<std::sync::MutexGuard<'_, BTreeSet<String>>> {
    sensitive
        .lock()
        .map_err(|_| tera::Error::msg("internal error: sensitive value collector lock poisoned"))
}

pub fn render_template_string(
    template: &str,
    vars: &BTreeMap<String, String>,
) -> tera::Result<(String, BTreeSet<String>)> {
    let mut tera = tera::Tera::default();
    tera.add_raw_template("__inline__", template)?;

    let sensitive: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));

    {
        let sensitive = Arc::clone(&sensitive);
        tera.register_function("env", move |args: &HashMap<String, tera::Value>| {
            let name = args
                .get("name")
                .and_then(tera::Value::as_str)
                .ok_or_else(|| tera::Error::msg("env() requires a `name` string argument"))?;

            std::env::var(name).map_or_else(
                |_| {
                    Err(tera::Error::msg(format!(
                        "env(name=\"{name}\") is not set in the current environment"
                    )))
                },
                |value| {
                    lock_sensitive_values(&sensitive)?.insert(value.clone());
                    Ok(tera::Value::String(value))
                },
            )
        });
    }

    {
        let sensitive = Arc::clone(&sensitive);
        tera.register_function("secret", move |args: &HashMap<String, tera::Value>| {
            let uri = args
                .get("uri")
                .and_then(tera::Value::as_str)
                .ok_or_else(|| tera::Error::msg("secret() requires a `uri` string argument"))?;

            let value = resolve_secret(uri).map_err(tera::Error::msg)?;
            lock_sensitive_values(&sensitive)?.insert(value.clone());
            Ok(tera::Value::String(value))
        });
    }

    let context = tera::Context::from_serialize(vars)?;
    let rendered = tera.render("__inline__", &context)?;
    let sensitive_values = lock_sensitive_values(&sensitive)?.clone();
    Ok((rendered, sensitive_values))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::collections::BTreeMap;

    use super::render_template_string;

    #[test]
    fn render_template_env_function() {
        let path = std::env::var("PATH").expect("PATH should be set");
        let vars = BTreeMap::new();
        let (rendered, sensitive) =
            render_template_string("path={{ env(name=\"PATH\") }}", &vars).expect("render");
        assert_eq!(rendered, format!("path={path}"));
        assert!(
            sensitive.contains(&path),
            "expected sensitive set to contain PATH value"
        );
    }

    #[test]
    fn render_template_env_unknown_errors() {
        use std::error::Error;

        let vars = BTreeMap::new();
        let result = render_template_string("{{ env(name=\"KERON_MISSING_XYZ\") }}", &vars);
        let error = result.expect_err("should fail");
        let mut full = error.to_string();
        let mut current: &dyn Error = &error;
        while let Some(source) = current.source() {
            full.push_str(": ");
            full.push_str(&source.to_string());
            current = source;
        }
        assert!(
            full.contains("KERON_MISSING_XYZ"),
            "unexpected error chain: {full}"
        );
    }
}
