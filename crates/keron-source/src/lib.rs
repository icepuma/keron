#![allow(clippy::multiple_crate_versions)]

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;

use anyhow::{Context, Result, bail};
use tempfile::{Builder, TempDir};
use url::{Url, form_urlencoded};

#[derive(Debug)]
struct GitSource {
    repo_url: Url,
    subdir: PathBuf,
    reference: Option<String>,
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
pub fn resolve_apply_source(input: &str) -> Result<ResolvedSource> {
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
    input.contains("://") && Url::parse(input).is_ok()
}

fn clone_into_tempdir(display_target: &str, source: &GitSource) -> Result<ResolvedSource> {
    let checkout_guard = Builder::new()
        .prefix("keron-remote-")
        .tempdir()
        .context("failed to create temporary clone directory")?;

    let checkout_root = checkout_guard.path().join("repo");
    let mut prepared = gix::prepare_clone(source.repo_url.as_str(), &checkout_root)
        .with_context(|| format!("failed to prepare clone from {}", source.repo_url))?;

    if let Some(reference) = source.reference.as_deref() {
        prepared = prepared
            .with_ref_name(Some(reference))
            .map_err(|_| anyhow::anyhow!("invalid git ref name: {reference}"))?;
    }

    let should_interrupt = AtomicBool::new(false);
    let (mut checkout, _) = prepared
        .fetch_then_checkout(gix::progress::Discard, &should_interrupt)
        .with_context(|| format!("failed to fetch remote repository {}", source.repo_url))?;

    let _ = checkout
        .main_worktree(gix::progress::Discard, &should_interrupt)
        .with_context(|| format!("failed to check out repository {}", source.repo_url))?;

    let manifest_root = resolve_manifest_root(&checkout_root, &source.subdir)?;
    Ok(ResolvedSource::remote(
        manifest_root,
        display_target.to_string(),
        checkout_guard,
    ))
}

fn parse_git_source(input: &str) -> Result<GitSource> {
    let (raw_base, raw_query) = split_source_and_query(input);
    let (repo_url_raw, subdir_raw) = split_repo_and_subdir(raw_base)?;

    let mut repo_url = Url::parse(&repo_url_raw)
        .with_context(|| format!("invalid repository URL: {repo_url_raw}"))?;
    validate_public_repo_url(&repo_url)?;
    repo_url.set_query(None);
    repo_url.set_fragment(None);

    let reference = parse_reference(raw_query)?;
    let subdir = normalize_relative_subdir(&subdir_raw)?;

    Ok(GitSource {
        repo_url,
        subdir,
        reference,
    })
}

fn split_source_and_query(input: &str) -> (&str, Option<&str>) {
    match input.split_once('?') {
        Some((base, query)) => (base, Some(query)),
        None => (input, None),
    }
}

fn split_repo_and_subdir(raw_base: &str) -> Result<(String, String)> {
    let Some(scheme_offset) = raw_base.find("://") else {
        bail!("remote source must include a URL scheme")
    };

    let offset_after_scheme = scheme_offset + 3;
    if let Some(marker_rel) = raw_base[offset_after_scheme..].find("//") {
        let marker = offset_after_scheme + marker_rel;
        let repo = raw_base[..marker].to_string();
        let subdir = raw_base[marker + 2..].to_string();
        return Ok((repo, subdir));
    }

    let parsed = Url::parse(raw_base).with_context(|| format!("invalid source URL: {raw_base}"))?;
    if parsed.scheme() != "git" {
        return Ok((raw_base.to_string(), String::new()));
    }

    let segments: Vec<String> = parsed
        .path_segments()
        .ok_or_else(|| anyhow::anyhow!("git source path cannot be empty"))?
        .filter(|segment| !segment.is_empty())
        .map(ToOwned::to_owned)
        .collect();
    if segments.is_empty() {
        bail!("git source must include a repository path")
    }

    let repo_segment_count = segments
        .iter()
        .position(|segment| {
            Path::new(segment)
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("git"))
        })
        .map_or(if segments.len() > 2 { 2 } else { 1 }, |index| index + 1);

    let mut repo_url = parsed;
    repo_url.set_path(&format!("/{}", segments[..repo_segment_count].join("/")));
    let subdir = segments[repo_segment_count..].join("/");
    Ok((repo_url.to_string(), subdir))
}

fn validate_public_repo_url(url: &Url) -> Result<()> {
    let scheme = url.scheme();
    match scheme {
        "https" | "http" | "git" => {}
        "file" => {
            bail!(
                "file:// repositories are not supported; only public network repositories are allowed"
            )
        }
        _ => bail!(
            "unsupported repository scheme \"{scheme}\"; expected https://, http://, or git://"
        ),
    }

    if url.host_str().is_none() {
        bail!("repository URL must include a host")
    }

    if !url.username().is_empty() || url.password().is_some() {
        bail!(
            "authenticated repository URLs are not supported; only public repositories are allowed"
        )
    }

    Ok(())
}

fn parse_reference(raw_query: Option<&str>) -> Result<Option<String>> {
    let Some(query) = raw_query else {
        return Ok(None);
    };

    let mut reference = None;
    for (key, value) in form_urlencoded::parse(query.as_bytes()) {
        match key.as_ref() {
            "ref" => {
                if value.is_empty() {
                    bail!("ref query parameter cannot be empty")
                }
                if reference.is_some() {
                    bail!("source query may only include one ref parameter")
                }
                reference = Some(value.into_owned());
            }
            _ => bail!("unsupported source query parameter: {key}"),
        }
    }

    Ok(reference)
}

fn normalize_relative_subdir(raw_subdir: &str) -> Result<PathBuf> {
    let trimmed = raw_subdir.trim_matches('/');
    if trimmed.is_empty() {
        return Ok(PathBuf::new());
    }

    let mut normalized = PathBuf::new();
    for component in Path::new(trimmed).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(segment) => normalized.push(segment),
            Component::ParentDir => {
                bail!("manifest subdirectory cannot contain '..'")
            }
            Component::RootDir | Component::Prefix(_) => {
                bail!("manifest subdirectory must be relative")
            }
        }
    }

    Ok(normalized)
}

fn resolve_manifest_root(checkout_root: &Path, subdir: &Path) -> Result<PathBuf> {
    let candidate = if subdir.as_os_str().is_empty() {
        checkout_root.to_path_buf()
    } else {
        checkout_root.join(subdir)
    };

    if !candidate.is_dir() {
        let subdir_display = if subdir.as_os_str().is_empty() {
            "."
        } else {
            subdir.to_str().unwrap_or("<non-utf8 path>")
        };
        bail!("manifest directory \"{subdir_display}\" does not exist in cloned repository")
    }

    let checkout_root = fs::canonicalize(checkout_root).with_context(|| {
        format!(
            "failed to canonicalize checkout root {}",
            checkout_root.display()
        )
    })?;
    let candidate = fs::canonicalize(&candidate).with_context(|| {
        format!(
            "failed to canonicalize manifest directory {}",
            candidate.display()
        )
    })?;

    if !candidate.starts_with(&checkout_root) {
        bail!("manifest directory escapes cloned repository checkout")
    }

    Ok(candidate)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{parse_git_source, resolve_apply_source};

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
            parse_git_source("file:///tmp/repo//manifests").expect_err("must reject file URLs");
        assert!(
            error
                .to_string()
                .contains("file:// repositories are not supported")
        );
    }

    #[test]
    fn rejects_authenticated_sources() {
        let error = parse_git_source("https://user:pass@example.com/repo.git//manifests")
            .expect_err("must reject authenticated URLs");
        assert!(
            error
                .to_string()
                .contains("authenticated repository URLs are not supported")
        );
    }

    #[test]
    fn parses_canonical_source_subdir_and_ref() {
        let parsed = parse_git_source("https://example.com/repo.git//manifests/dev?ref=main")
            .expect("parse canonical source");
        assert_eq!(parsed.repo_url.as_str(), "https://example.com/repo.git");
        assert_eq!(parsed.subdir, std::path::PathBuf::from("manifests/dev"));
        assert_eq!(parsed.reference.as_deref(), Some("main"));
    }

    #[test]
    fn parses_git_scheme_compatibility_source() {
        let parsed =
            parse_git_source("git://example.com/repo/manifests").expect("parse git source");
        assert_eq!(parsed.repo_url.as_str(), "git://example.com/repo");
        assert_eq!(parsed.subdir, std::path::PathBuf::from("manifests"));
    }

    #[test]
    fn rejects_subdir_traversal() {
        let error = parse_git_source("https://example.com/repo.git//../manifests")
            .expect_err("must reject traversal");
        assert!(
            error
                .to_string()
                .contains("manifest subdirectory cannot contain '..'"),
            "unexpected error: {error}"
        );
    }
}
