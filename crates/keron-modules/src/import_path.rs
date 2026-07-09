use std::fs;
use std::path::{Path, PathBuf};

/// Resolve one source path from a `use` declaration using the module
/// loader's exact validation rules.
///
/// Relative imports must start with `./` or `../`; absolute imports
/// are accepted. The target must exist, canonicalize, be a regular
/// file, and carry the `.keron` extension. Editor integrations use
/// this function too so navigation cannot open a path the resolver
/// itself would reject.
///
/// # Errors
/// Returns a user-facing explanation when the path shape or target is
/// invalid.
pub fn resolve_import_path(raw: &str, base_dir: &Path) -> Result<PathBuf, String> {
    let raw_path = Path::new(raw);
    if raw.starts_with("./") || raw.starts_with("../") || raw_path.is_absolute() {
        let joined = base_dir.join(raw_path);
        let canonical =
            fs::canonicalize(&joined).map_err(|e| format!("could not resolve `{raw}`: {e}"))?;
        if canonical.extension().and_then(|e| e.to_str()) != Some("keron") {
            return Err(format!("`{raw}` is not a `.keron` file"));
        }
        if !canonical.is_file() {
            return Err(format!(
                "`{raw}` is not a regular file (a directory or special file cannot be a module)"
            ));
        }
        return Ok(canonical);
    }
    Err(format!(
        "import path must start with `./` or `../`, or be absolute; found `{raw}`"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_bare_name() {
        let err = resolve_import_path("helpers.keron", Path::new("/anywhere")).unwrap_err();
        assert!(err.contains("must start with"), "got: {err}");
    }

    #[test]
    fn rejects_relative_without_dot() {
        let err = resolve_import_path("foo/bar.keron", Path::new("/anywhere")).unwrap_err();
        assert!(err.contains("must start with"), "got: {err}");
    }

    #[test]
    fn relative_dot_resolves_against_base() {
        let dir = std::env::temp_dir().join("keron-resolve-path-rel");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hi.keron");
        fs::write(&target, "").unwrap();
        let got = resolve_import_path("./hi.keron", &dir).unwrap();
        let canonical = fs::canonicalize(&target).unwrap();
        assert_eq!(got, canonical);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn absolute_resolves_to_canonical_file() {
        let dir = std::env::temp_dir().join("keron-resolve-path-abs");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hi.keron");
        fs::write(&target, "").unwrap();
        let canonical = fs::canonicalize(&target).unwrap();
        let abs_str = canonical.to_string_lossy().into_owned();
        let got = resolve_import_path(&abs_str, Path::new("/")).unwrap();
        assert_eq!(got, canonical);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_absolute_resolves_to_canonical_file() {
        let dir = std::env::temp_dir().join("keron-resolve-path-windows-abs");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hi.keron");
        fs::write(&target, "").unwrap();
        let canonical = fs::canonicalize(&target).unwrap();
        let got =
            resolve_import_path(&canonical.to_string_lossy(), Path::new(r"C:\ignored")).unwrap();
        assert_eq!(got, canonical);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parent_dot_resolves_against_base() {
        let parent = std::env::temp_dir().join("keron-resolve-path-parent");
        let child = parent.join("nested");
        fs::create_dir_all(&child).unwrap();
        let target = parent.join("hi.keron");
        fs::write(&target, "").unwrap();
        let got = resolve_import_path("../hi.keron", &child).unwrap();
        let canonical = fs::canonicalize(&target).unwrap();
        assert_eq!(got, canonical);
        let _ = fs::remove_dir_all(&parent);
    }

    #[test]
    fn rejects_non_keron_extension() {
        let dir = std::env::temp_dir().join("keron-resolve-path-bad-ext");
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("hi.txt");
        fs::write(&target, "").unwrap();
        let err = resolve_import_path("./hi.txt", &dir).unwrap_err();
        assert!(err.contains("not a `.keron` file"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_keron_suffixed_directory() {
        let dir = std::env::temp_dir().join("keron-resolve-path-dir");
        fs::create_dir_all(&dir).unwrap();
        fs::create_dir_all(dir.join("sub.keron")).unwrap();
        let err = resolve_import_path("./sub.keron", &dir).unwrap_err();
        assert!(err.contains("not a regular file"), "got: {err}");
        let _ = fs::remove_dir_all(&dir);
    }
}
