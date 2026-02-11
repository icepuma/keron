use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::DiscoveryError;

/// Recursively discover manifest files (`*.lua`) under a root directory.
///
/// # Errors
///
/// Returns an error if `root` is invalid, directory walking fails, or a manifest
/// path cannot be canonicalized.
pub fn discover_manifests(root: &Path) -> std::result::Result<Vec<PathBuf>, DiscoveryError> {
    if !root.exists() {
        return Err(DiscoveryError::RootDoesNotExist {
            root: root.to_path_buf(),
        });
    }
    if !root.is_dir() {
        return Err(DiscoveryError::RootIsNotDirectory {
            root: root.to_path_buf(),
        });
    }

    let mut manifests = Vec::new();

    for entry in WalkDir::new(root) {
        let entry = match entry {
            Ok(value) => value,
            Err(source) => return Err(DiscoveryError::Walk { source }),
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

        let canonical =
            fs::canonicalize(entry.path()).map_err(|source| DiscoveryError::CanonicalizePath {
                path: entry.path().to_path_buf(),
                source,
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
