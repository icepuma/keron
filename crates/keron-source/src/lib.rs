#![allow(clippy::multiple_crate_versions)]

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;

use tempfile::{Builder, TempDir};
use url::Url;

mod error;

pub use error::SourceError;

type SourceResult<T> = std::result::Result<T, SourceError>;

#[derive(Debug)]
struct GitSource {
    repo_url: Url,
    subdir: PathBuf,
    reference: String,
}

/// Resolved manifest source for `keron apply`.
#[derive(Debug)]
pub struct ResolvedSource {
    /// Local directory that contains manifest files.
    pub manifest_root: PathBuf,
    /// User-facing source label for report rendering.
    pub display_target: String,
    // Keep temporary checkouts alive for the full apply lifecycle.
    checkout_guard: Option<TempDir>,
}

#[allow(clippy::missing_const_for_fn)]
impl ResolvedSource {
    fn local(manifest_root: PathBuf, display_target: String) -> Self {
        Self {
            manifest_root,
            display_target,
            checkout_guard: None,
        }
    }

    fn remote(manifest_root: PathBuf, display_target: String, checkout_guard: TempDir) -> Self {
        Self {
            manifest_root,
            display_target,
            checkout_guard: Some(checkout_guard),
        }
    }

    #[must_use]
    pub fn is_remote(&self) -> bool {
        self.checkout_guard.is_some()
    }
}

/// Resolve an `apply` source into a local manifest root.
///
/// Local paths are returned as-is. Remote sources are cloned into a temporary
/// checkout and cleaned up automatically after use.
///
/// # Errors
///
/// Returns an error if source parsing fails, a remote clone/checkout fails, or
/// the requested manifest directory does not exist in the cloned repository.
pub fn resolve_apply_source(input: &str) -> SourceResult<ResolvedSource> {
    if !looks_like_remote_source(input) {
        return Ok(ResolvedSource::local(
            PathBuf::from(input),
            input.to_string(),
        ));
    }

    let parsed = parse_git_source(input)?;
    clone_into_tempdir(input, &parsed)
}

fn looks_like_remote_source(input: &str) -> bool {
    (input.contains("://") && Url::parse(input).is_ok()) || looks_like_scp_style_remote(input)
}

fn looks_like_scp_style_remote(input: &str) -> bool {
    if input.contains("://") {
        return false;
    }

    let Some((left, right)) = input.split_once(':') else {
        return false;
    };
    if right.is_empty() || right.starts_with('/') {
        return false;
    }

    let Some((user, host)) = left.rsplit_once('@') else {
        return false;
    };

    !user.is_empty() && !host.is_empty()
}

fn clone_into_tempdir(display_target: &str, source: &GitSource) -> SourceResult<ResolvedSource> {
    let checkout_guard = Builder::new()
        .prefix("keron-remote-")
        .tempdir()
        .map_err(|source| SourceError::TempDirCreate { source })?;

    let checkout_root = checkout_guard.path().join("repo");
    let mut prepared =
        gix::prepare_clone(source.repo_url.as_str(), &checkout_root).map_err(|error| {
            SourceError::PrepareClone {
                repo_url: source.repo_url.to_string(),
                source: Box::new(error),
            }
        })?;

    prepared = prepared
        .with_ref_name(Some(source.reference.as_str()))
        .map_err(|_| SourceError::InvalidGitRefName {
            reference: source.reference.clone(),
        })?;

    let should_interrupt = AtomicBool::new(false);
    let (mut checkout, _) = prepared
        .fetch_then_checkout(gix::progress::Discard, &should_interrupt)
        .map_err(|error| SourceError::FetchRemote {
            repo_url: source.repo_url.to_string(),
            source: Box::new(error),
        })?;

    let _ = checkout
        .main_worktree(gix::progress::Discard, &should_interrupt)
        .map_err(|error| SourceError::CheckoutRemote {
            repo_url: source.repo_url.to_string(),
            source: Box::new(error),
        })?;

    let manifest_root = resolve_manifest_root(&checkout_root, &source.subdir)?;
    Ok(ResolvedSource::remote(
        manifest_root,
        display_target.to_string(),
        checkout_guard,
    ))
}

fn parse_git_source(input: &str) -> SourceResult<GitSource> {
    let (mut repo_url, subdir_raw) = if looks_like_scp_style_remote(input) {
        parse_scp_style_source(input)?
    } else {
        let source_url = Url::parse(input).map_err(|source| SourceError::InvalidSourceUrl {
            input: input.to_string(),
            source,
        })?;
        validate_public_repo_url(&source_url)?;

        if source_url.query().is_some() {
            return Err(SourceError::QueryParametersNotSupported);
        }

        if uses_legacy_subdir_delimiter(input)? {
            return Err(SourceError::LegacySubdirDelimiter {
                example: "https://host/org/repo/manifests",
            });
        }

        split_repo_and_subdir(&source_url)?
    };
    validate_public_repo_url(&repo_url)?;
    repo_url.set_query(None);
    repo_url.set_fragment(None);

    let subdir = normalize_relative_subdir(&subdir_raw)?;

    Ok(GitSource {
        repo_url,
        subdir,
        reference: "main".to_string(),
    })
}

fn uses_legacy_subdir_delimiter(input: &str) -> SourceResult<bool> {
    let Some(scheme_offset) = input.find("://") else {
        return Err(SourceError::MissingRemoteScheme);
    };

    let offset_after_scheme = scheme_offset + 3;
    let after_scheme = &input[offset_after_scheme..];
    Ok(after_scheme.contains("//"))
}

fn parse_scp_style_source(input: &str) -> SourceResult<(Url, String)> {
    let Some((left, right)) = input.split_once(':') else {
        return Err(SourceError::InvalidScpStyleFormat);
    };
    let Some((user, host)) = left.rsplit_once('@') else {
        return Err(SourceError::InvalidScpStyleFormat);
    };
    if user.is_empty() || host.is_empty() || right.is_empty() {
        return Err(SourceError::InvalidScpStyleFormat);
    }

    if right.contains("//") {
        return Err(SourceError::LegacySubdirDelimiter {
            example: "git@host:org/repo.git/manifests",
        });
    }

    let normalized = format!("ssh://{user}@{host}/{}", right.trim_start_matches('/'));
    let source_url =
        Url::parse(&normalized).map_err(|source| SourceError::InvalidScpStyleSource {
            input: input.to_string(),
            source,
        })?;

    split_repo_and_subdir(&source_url)
}

fn split_repo_and_subdir(parsed: &Url) -> SourceResult<(Url, String)> {
    let segments: Vec<String> = parsed
        .path_segments()
        .ok_or(SourceError::EmptySourcePath)?
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if segments.is_empty() {
        return Err(SourceError::MissingRepositoryPath);
    }

    let repo_segment_count = segments
        .iter()
        .position(|segment| {
            Path::new(segment)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("git"))
        })
        .map_or(if segments.len() >= 2 { 2 } else { 1 }, |index| index + 1);

    let mut repo_url = parsed.clone();
    repo_url.set_path(&format!("/{}", segments[..repo_segment_count].join("/")));
    let subdir = segments[repo_segment_count..].join("/");
    Ok((repo_url, subdir))
}

fn validate_public_repo_url(url: &Url) -> SourceResult<()> {
    let scheme = url.scheme();
    match scheme {
        "https" | "git" | "ssh" => {}
        "http" => return Err(SourceError::HttpSchemeNotSupported),
        "file" => return Err(SourceError::FileSchemeNotSupported),
        _ => {
            return Err(SourceError::UnsupportedRepositoryScheme {
                scheme: scheme.to_string(),
            });
        }
    }

    if url.host_str().is_none() {
        return Err(SourceError::MissingRepositoryHost);
    }

    if url.password().is_some() {
        return Err(SourceError::PasswordNotSupported);
    }

    if scheme != "ssh" && !url.username().is_empty() {
        return Err(SourceError::AuthenticatedUrlsNotSupported);
    }

    Ok(())
}

fn normalize_relative_subdir(raw_subdir: &str) -> SourceResult<PathBuf> {
    let trimmed = raw_subdir.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(PathBuf::new());
    }

    let mut normalized = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => normalized.push(segment),
            Component::ParentDir => return Err(SourceError::ParentDirNotAllowed),
            Component::RootDir | Component::Prefix(_) => {
                return Err(SourceError::AbsoluteSubdirNotAllowed);
            }
        }
    }

    Ok(normalized)
}

fn resolve_manifest_root(checkout_root: &Path, subdir: &Path) -> SourceResult<PathBuf> {
    let candidate = if subdir.as_os_str().is_empty() {
        checkout_root.to_path_buf()
    } else {
        checkout_root.join(subdir)
    };

    if !candidate.is_dir() {
        let subdir_display = if subdir.as_os_str().is_empty() {
            ".".to_string()
        } else {
            subdir.to_str().unwrap_or("<non-utf8 path>").to_string()
        };
        return Err(SourceError::MissingManifestDirectory {
            subdir: subdir_display,
        });
    }

    let checkout_root = fs::canonicalize(checkout_root).map_err(|source| {
        SourceError::CanonicalizeCheckoutRoot {
            path: checkout_root.to_path_buf(),
            source,
        }
    })?;
    let candidate = fs::canonicalize(&candidate).map_err(|source| {
        SourceError::CanonicalizeManifestDirectory {
            path: candidate.clone(),
            source,
        }
    })?;

    if !candidate.starts_with(&checkout_root) {
        return Err(SourceError::ManifestDirectoryEscapesCheckout);
    }

    Ok(candidate)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{normalize_relative_subdir, parse_git_source, resolve_apply_source};

    #[test]
    fn local_paths_remain_local_sources() {
        let resolved = resolve_apply_source("./examples/simple").expect("resolve local source");
        assert_eq!(
            resolved.manifest_root,
            std::path::PathBuf::from("./examples/simple")
        );
        assert!(!resolved.is_remote());
    }

    #[test]
    fn rejects_file_scheme_sources() {
        let error =
            parse_git_source("file:///tmp/repo/manifests").expect_err("must reject file URLs");
        assert!(
            error
                .to_string()
                .contains("file:// repositories are not supported")
        );
    }

    #[test]
    fn rejects_authenticated_sources() {
        let error = parse_git_source("https://user:pass@example.com/repo.git/manifests")
            .expect_err("must reject authenticated URLs");
        assert!(error.to_string().contains("passwords are not supported"));
    }

    #[test]
    fn parses_remote_source_subdir_with_main_ref() {
        let parsed =
            parse_git_source("https://example.com/repo.git/manifests/dev").expect("parse source");
        assert_eq!(parsed.repo_url.as_str(), "https://example.com/repo.git");
        assert_eq!(parsed.subdir, std::path::PathBuf::from("manifests/dev"));
        assert_eq!(parsed.reference, "main");
    }

    #[test]
    fn parses_https_owner_repo_without_subdir() {
        let parsed = parse_git_source("https://github.com/icepuma/dotfiles")
            .expect("parse owner/repo source");
        assert_eq!(
            parsed.repo_url.as_str(),
            "https://github.com/icepuma/dotfiles"
        );
        assert_eq!(parsed.subdir, std::path::PathBuf::new());
        assert_eq!(parsed.reference, "main");
    }

    #[test]
    fn parses_git_scheme_compatibility_source() {
        let parsed =
            parse_git_source("git://example.com/repo.git/manifests").expect("parse git source");
        assert_eq!(parsed.repo_url.as_str(), "git://example.com/repo.git");
        assert_eq!(parsed.subdir, std::path::PathBuf::from("manifests"));
    }

    #[test]
    fn parses_git_owner_repo_without_subdir() {
        let parsed =
            parse_git_source("git://github.com/icepuma/dotfiles").expect("parse owner/repo source");
        assert_eq!(
            parsed.repo_url.as_str(),
            "git://github.com/icepuma/dotfiles"
        );
        assert_eq!(parsed.subdir, std::path::PathBuf::new());
        assert_eq!(parsed.reference, "main");
    }

    #[test]
    fn parses_scp_style_owner_repo_without_subdir() {
        let parsed =
            parse_git_source("git@github.com:icepuma/dotfiles.git").expect("parse scp source");
        assert_eq!(
            parsed.repo_url.as_str(),
            "ssh://git@github.com/icepuma/dotfiles.git"
        );
        assert_eq!(parsed.subdir, std::path::PathBuf::new());
        assert_eq!(parsed.reference, "main");
    }

    #[test]
    fn parses_scp_style_with_subdir() {
        let parsed = parse_git_source("git@github.com:icepuma/dotfiles.git/manifests")
            .expect("parse scp source with subdir");
        assert_eq!(
            parsed.repo_url.as_str(),
            "ssh://git@github.com/icepuma/dotfiles.git"
        );
        assert_eq!(parsed.subdir, std::path::PathBuf::from("manifests"));
        assert_eq!(parsed.reference, "main");
    }

    #[test]
    fn rejects_legacy_double_slash_delimiter() {
        let error = parse_git_source("https://example.com/repo.git//manifests")
            .expect_err("must reject legacy delimiter");
        assert!(error.to_string().contains("\"//\" subdirectory delimiter"));
    }

    #[test]
    fn rejects_query_parameters() {
        let error = parse_git_source("https://example.com/repo.git/manifests?ref=dev")
            .expect_err("must reject query params");
        assert!(error.to_string().contains("always checks out ref \"main\""));
    }

    #[test]
    fn rejects_http_scheme_sources() {
        let error = parse_git_source("http://example.com/repo.git/manifests")
            .expect_err("must reject http URLs");
        assert!(
            error
                .to_string()
                .contains("http:// repositories are not supported")
        );
    }

    #[test]
    fn normalize_relative_subdir_rejects_traversal() {
        let error = normalize_relative_subdir("../manifests").expect_err("must reject traversal");
        assert!(
            error
                .to_string()
                .contains("manifest subdirectory cannot contain '..'"),
            "unexpected error: {error}"
        );
    }
}
