use std::path::Path;
use std::process::Command;

use crate::error::SecretError;

#[derive(Debug)]
pub struct SecretProvider {
    pub binary: &'static str,
    pub args: Vec<String>,
}

pub fn parse_secret_uri(uri: &str) -> Result<SecretProvider, SecretError> {
    let Some((scheme, path)) = uri.split_once("://") else {
        return Err(SecretError::InvalidUri {
            uri: uri.to_string(),
        });
    };

    match scheme {
        "op" => {
            if path.is_empty() {
                return Err(SecretError::InvalidSchemePath {
                    scheme: "op",
                    expected: "a path (e.g. op://vault/item/field)",
                    uri: uri.to_string(),
                });
            }
            Ok(SecretProvider {
                binary: "op",
                args: vec!["read".to_string(), uri.to_string()],
            })
        }
        "bw" => {
            let parts: Vec<&str> = path.split('/').collect();
            match parts.as_slice() {
                [item] if !item.is_empty() => Ok(SecretProvider {
                    binary: "bw",
                    args: vec![
                        "get".to_string(),
                        "password".to_string(),
                        (*item).to_string(),
                    ],
                }),
                [item, field] if !item.is_empty() && !field.is_empty() => Ok(SecretProvider {
                    binary: "bw",
                    args: vec!["get".to_string(), (*field).to_string(), (*item).to_string()],
                }),
                _ => Err(SecretError::InvalidSchemePath {
                    scheme: "bw",
                    expected: "bw://item or bw://item/field",
                    uri: uri.to_string(),
                }),
            }
        }
        "pp" => {
            let parts: Vec<&str> = path.split('/').collect();
            if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
                return Err(SecretError::InvalidSchemePath {
                    scheme: "pp",
                    expected: "pp://vault/item/field",
                    uri: uri.to_string(),
                });
            }
            let pass_uri = format!("pass://{path}");
            Ok(SecretProvider {
                binary: "pass-cli",
                args: vec!["item".to_string(), "view".to_string(), pass_uri],
            })
        }
        _ => Err(SecretError::UnsupportedScheme {
            scheme: scheme.to_string(),
            uri: uri.to_string(),
        }),
    }
}

pub fn resolve_secret(uri: &str) -> Result<String, SecretError> {
    let provider = parse_secret_uri(uri)?;

    let resolved_path = which::which(provider.binary).map_err(|_| SecretError::CliMissing {
        uri: uri.to_string(),
        binary: provider.binary,
    })?;

    run_secret_command(&resolved_path, provider.binary, &provider.args, uri)
}

fn run_secret_command(
    binary_path: &Path,
    provider_binary: &str,
    args: &[String],
    uri: &str,
) -> Result<String, SecretError> {
    let output = Command::new(binary_path)
        .args(args)
        .output()
        .map_err(|source| SecretError::CommandSpawn {
            provider_binary: provider_binary.to_string(),
            binary_path: binary_path.to_path_buf(),
            uri: uri.to_string(),
            source,
        })?;

    if !output.status.success() {
        return Err(SecretError::CommandFailed {
            provider_binary: provider_binary.to_string(),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        });
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{parse_secret_uri, resolve_secret, run_secret_command};

    #[test]
    fn parse_secret_uri_op_scheme() {
        let provider = parse_secret_uri("op://vault/item/field").expect("parse");
        assert_eq!(provider.binary, "op");
        assert_eq!(provider.args, vec!["read", "op://vault/item/field"]);
    }

    #[test]
    fn parse_secret_uri_bw_with_field() {
        let provider = parse_secret_uri("bw://my-login/username").expect("parse");
        assert_eq!(provider.binary, "bw");
        assert_eq!(provider.args, vec!["get", "username", "my-login"]);
    }

    #[test]
    fn parse_secret_uri_bw_default_field() {
        let provider = parse_secret_uri("bw://my-login").expect("parse");
        assert_eq!(provider.binary, "bw");
        assert_eq!(provider.args, vec!["get", "password", "my-login"]);
    }

    #[test]
    fn parse_secret_uri_pp_scheme() {
        let provider = parse_secret_uri("pp://v/i/f").expect("parse");
        assert_eq!(provider.binary, "pass-cli");
        assert_eq!(provider.args, vec!["item", "view", "pass://v/i/f"]);
    }

    #[test]
    fn parse_secret_uri_rejects_missing_scheme() {
        let error = parse_secret_uri("just-a-string").expect_err("should fail");
        assert!(
            error.to_string().contains("invalid URI"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_secret_uri_rejects_unknown_scheme() {
        let error = parse_secret_uri("vault://foo").expect_err("should fail");
        assert!(
            error.to_string().contains("unsupported scheme"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_secret_uri_bw_rejects_empty_item() {
        let error = parse_secret_uri("bw:///field").expect_err("should fail");
        assert!(
            error.to_string().contains("bw://"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn parse_secret_uri_pp_requires_full_path() {
        let error = parse_secret_uri("pp://vault/item").expect_err("should fail");
        assert!(
            error.to_string().contains("pp://"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn resolve_secret_errors_when_cli_missing() {
        let error = resolve_secret("op://vault/item/field").expect_err("should fail");
        assert!(
            error.to_string().contains("requires the \"op\" CLI"),
            "unexpected error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_secret_command_uses_resolved_binary_path() {
        let shell = which::which("sh").expect("sh should exist");
        let output = run_secret_command(
            &shell,
            "sh",
            &["-c".to_string(), "printf secret-value".to_string()],
            "op://vault/item/field",
        )
        .expect("shell command should succeed");
        assert_eq!(output, "secret-value");
    }

    #[cfg(windows)]
    #[test]
    fn run_secret_command_uses_resolved_binary_path() {
        let shell = which::which("cmd").expect("cmd should exist");
        let output = run_secret_command(
            &shell,
            "cmd",
            &["/C".to_string(), "echo secret-value".to_string()],
            "op://vault/item/field",
        )
        .expect("cmd should succeed");
        assert_eq!(output, "secret-value");
    }
}
