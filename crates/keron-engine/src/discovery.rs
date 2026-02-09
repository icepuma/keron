use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;

/// Recursively discover manifest files (`*.lua`) under a root directory.
///
/// # Errors
///
/// Returns an error if `root` is invalid, directory walking fails, or a manifest
/// path cannot be canonicalized.
pub fn discover_manifests(root: &Path) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        bail!("manifest root does not exist: {}", root.display());
    }
    if !root.is_dir() {
        bail!("manifest root must be a directory: {}", root.display());
    }

    let mut manifests = Vec::new();

    for entry in WalkDir::new(root) {
        let entry = match entry {
            Ok(value) => value,
            Err(error) => {
                return Err(error).context("failed while walking manifest directory");
            }
        };

        if !entry.file_type().is_file() {
            continue;
        }

        let Some(extension) = entry.path().extension() else {
            continue;
        };

        if extension != "lua" {
            continue;
        }

        let canonical = fs::canonicalize(entry.path()).with_context(|| {
            format!(
                "failed to canonicalize manifest path: {}",
                entry.path().display()
            )
        })?;
        manifests.push(canonical);
    }

    manifests.sort();
    Ok(manifests)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use std::fs;

    use super::discover_manifests;

    #[test]
    fn finds_only_lua_manifests_in_sorted_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        fs::create_dir_all(root.join("a")).expect("mkdir");
        fs::create_dir_all(root.join("z/nested")).expect("mkdir");
        fs::write(root.join("z/nested/two.lua"), "").expect("write");
        fs::write(root.join("a/one.lua"), "").expect("write");
        fs::write(root.join("a/ignore.txt"), "").expect("write");

        let manifests = discover_manifests(root).expect("discover");
        assert_eq!(manifests.len(), 2);
        assert!(manifests[0].ends_with("a/one.lua"));
        assert!(manifests[1].ends_with("z/nested/two.lua"));
    }
}
