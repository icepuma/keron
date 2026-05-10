//! Discover and read `.keron` files for the resolver.
//!
//! `load(path)` accepts either:
//! - a single `.keron` file, returning one [`LoadedFile`];
//! - a directory, returning every `.keron` file found recursively,
//!   sorted alphanumerically by relative path against `path`.
//!
//! Each file is one independent module; the loader does not
//! concatenate. Cross-file references must go through explicit `use`
//! imports — that's what makes per-file scope enforceable end-to-end.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub struct LoadedFile {
    /// Canonicalized absolute path. Used as the `ModuleId::File` key.
    pub path: PathBuf,
    pub text: String,
}

#[derive(Debug)]
pub struct LoadedSource {
    pub files: Vec<LoadedFile>,
}

pub fn load(path: &Path) -> Result<LoadedSource> {
    let meta = fs::metadata(path).with_context(|| format!("reading `{}`", path.display()))?;

    if meta.is_file() {
        Ok(LoadedSource {
            files: vec![load_file(path)?],
        })
    } else if meta.is_dir() {
        let files = load_dir(path)?;
        if files.is_empty() {
            bail!("no .keron files in `{}`", path.display());
        }
        Ok(LoadedSource { files })
    } else {
        bail!(
            "`{}` is neither a regular file nor a directory",
            path.display()
        );
    }
}

fn load_file(path: &Path) -> Result<LoadedFile> {
    if path.extension().and_then(|e| e.to_str()) != Some("keron") {
        bail!("`{}` is not a .keron file", path.display());
    }
    let canonical =
        fs::canonicalize(path).with_context(|| format!("canonicalizing `{}`", path.display()))?;
    let text = fs::read_to_string(&canonical)
        .with_context(|| format!("reading `{}`", canonical.display()))?;
    Ok(LoadedFile {
        path: canonical,
        text,
    })
}

fn load_dir(root: &Path) -> Result<Vec<LoadedFile>> {
    // Walk gathers entry-relative paths; sort by relative path so the
    // ordering is the alphanumeric tree-walk the model promises (and is
    // independent of the OS's `read_dir` order).
    let mut paths: Vec<PathBuf> = Vec::new();
    walk(root, &mut paths)?;
    paths.sort_by(|a, b| {
        let ra = a.strip_prefix(root).unwrap_or(a);
        let rb = b.strip_prefix(root).unwrap_or(b);
        ra.cmp(rb)
    });

    let mut files: Vec<LoadedFile> = Vec::with_capacity(paths.len());
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for path in paths {
        // Canonicalize before reading so two entries pointing at the
        // same real file (e.g. via a symlinked subdir) collapse to one
        // module. Resolver also keys ModuleId::File by canonical path.
        let canonical = fs::canonicalize(&path)
            .with_context(|| format!("canonicalizing `{}`", path.display()))?;
        if !seen.insert(canonical.clone()) {
            continue;
        }
        let text = fs::read_to_string(&canonical)
            .with_context(|| format!("reading `{}`", canonical.display()))?;
        files.push(LoadedFile {
            path: canonical,
            text,
        });
    }
    Ok(files)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("reading directory `{}`", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("listing `{}`", dir.display()))?;
        let path = entry.path();
        let ft = entry
            .file_type()
            .with_context(|| format!("stat `{}`", path.display()))?;
        // `file_type()` does not follow symlinks: a symlink-to-dir
        // reports `is_symlink()` and we skip it. That keeps recursion
        // bounded without hand-rolled cycle detection.
        if ft.is_dir() {
            walk(&path, out)?;
        } else if ft.is_file() && path.extension().and_then(|e| e.to_str()) == Some("keron") {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(name: &str) -> Self {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let p =
                env::temp_dir().join(format!("keron-load-test-{name}-{}-{n}", std::process::id()));
            if p.exists() {
                fs::remove_dir_all(&p).ok();
            }
            fs::create_dir_all(&p).unwrap();
            Self { path: p }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn rel_names(src: &LoadedSource, root: &Path) -> Vec<String> {
        let canonical_root = fs::canonicalize(root).unwrap();
        src.files
            .iter()
            .map(|f| {
                f.path
                    .strip_prefix(&canonical_root)
                    .unwrap_or(&f.path)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    #[test]
    fn load_file_accepts_keron_extension() {
        let d = TempDir::new("file-ok");
        let p = d.path.join("entry.keron");
        fs::write(&p, "val n: Int = 1\n").unwrap();
        let got = load(&p).unwrap();
        assert_eq!(got.files.len(), 1);
        assert_eq!(got.files[0].text, "val n: Int = 1\n");
        assert_eq!(got.files[0].path, fs::canonicalize(&p).unwrap());
    }

    #[test]
    fn load_file_rejects_non_keron_extension() {
        let d = TempDir::new("file-bad-ext");
        let p = d.path.join("entry.txt");
        fs::write(&p, "").unwrap();
        let err = load(&p).unwrap_err();
        assert!(err.to_string().contains("not a .keron file"));
    }

    #[test]
    fn load_dir_returns_files_in_alphanumeric_order() {
        // Without sorting the loader would return whatever
        // `read_dir` decides (filesystem-dependent). Force the
        // out-of-order names so a missing/inverted sort is visible.
        let d = TempDir::new("dir-sort");
        fs::write(d.path.join("z.keron"), "val z: Int = 0\n").unwrap();
        fs::write(d.path.join("b.keron"), "val b: Int = 0\n").unwrap();
        fs::write(d.path.join("a.keron"), "val a: Int = 0\n").unwrap();
        let got = load(&d.path).unwrap();
        let names = rel_names(&got, &d.path);
        assert_eq!(
            names,
            vec!["a.keron", "b.keron", "z.keron"],
            "got: {names:?}"
        );
    }

    #[test]
    fn load_dir_recurses_into_subdirectories() {
        // The previous loader walked one level only. This fixture
        // pins the recursion: nested .keron files must show up too.
        let d = TempDir::new("dir-recurse");
        fs::create_dir_all(d.path.join("sub/deeper")).unwrap();
        fs::write(d.path.join("a.keron"), "val a: Int = 0\n").unwrap();
        fs::write(d.path.join("sub/b.keron"), "val b: Int = 0\n").unwrap();
        fs::write(d.path.join("sub/deeper/c.keron"), "val c: Int = 0\n").unwrap();
        let got = load(&d.path).unwrap();
        let names = rel_names(&got, &d.path);
        assert_eq!(
            names,
            vec!["a.keron", "sub/b.keron", "sub/deeper/c.keron"],
            "got: {names:?}",
        );
    }

    #[test]
    fn load_dir_alphanumeric_order_uses_full_relative_path() {
        // `a/x.keron < a/y.keron < b/x.keron` — the sort key is the
        // full relative path, not just the basename.
        let d = TempDir::new("dir-relpath-sort");
        fs::create_dir_all(d.path.join("a")).unwrap();
        fs::create_dir_all(d.path.join("b")).unwrap();
        fs::write(d.path.join("a/y.keron"), "val ay: Int = 0\n").unwrap();
        fs::write(d.path.join("a/x.keron"), "val ax: Int = 0\n").unwrap();
        fs::write(d.path.join("b/x.keron"), "val bx: Int = 0\n").unwrap();
        let got = load(&d.path).unwrap();
        let names = rel_names(&got, &d.path);
        assert_eq!(
            names,
            vec!["a/x.keron", "a/y.keron", "b/x.keron"],
            "got: {names:?}",
        );
    }

    #[test]
    fn load_dir_skips_non_keron_files() {
        let d = TempDir::new("dir-skip-ext");
        fs::write(d.path.join("a.keron"), "val a: Int = 1\n").unwrap();
        fs::write(d.path.join("b.txt"), "should not appear").unwrap();
        let got = load(&d.path).unwrap();
        assert_eq!(got.files.len(), 1);
        assert!(got.files[0].path.ends_with("a.keron"));
    }

    #[test]
    fn load_dir_skips_subdirectories_named_with_keron_extension() {
        // A directory whose name happens to end in `.keron` must not
        // be treated as a file. Mutating the file/dir filter would
        // surface here.
        let d = TempDir::new("dir-skip-subdir");
        fs::write(d.path.join("a.keron"), "val a: Int = 1\n").unwrap();
        fs::create_dir(d.path.join("nested.keron")).unwrap();
        let got = load(&d.path).unwrap();
        assert_eq!(got.files.len(), 1);
        assert!(got.files[0].path.ends_with("a.keron"));
    }

    #[test]
    fn load_dir_errors_when_no_keron_files() {
        let d = TempDir::new("dir-empty");
        fs::write(d.path.join("a.txt"), "").unwrap();
        let err = load(&d.path).unwrap_err();
        assert!(err.to_string().contains("no .keron files"));
    }
}
