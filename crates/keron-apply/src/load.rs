//! Load a `.keron` file or a directory of them into a single source
//! buffer for the parser. Directory loading concatenates files in
//! sorted order (matches Terraform/OpenTofu module semantics).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub struct LoadedSource {
    pub text: String,
    #[allow(dead_code)] // surfaced once we map errors back to per-file spans
    pub files: Vec<PathBuf>,
}

pub fn load(path: &Path) -> Result<LoadedSource> {
    let meta = fs::metadata(path).with_context(|| format!("reading `{}`", path.display()))?;

    if meta.is_file() {
        load_file(path)
    } else if meta.is_dir() {
        load_dir(path)
    } else {
        bail!(
            "`{}` is neither a regular file nor a directory",
            path.display()
        );
    }
}

fn load_file(path: &Path) -> Result<LoadedSource> {
    if path.extension().and_then(|e| e.to_str()) != Some("keron") {
        bail!("`{}` is not a .keron file", path.display());
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading `{}`", path.display()))?;
    Ok(LoadedSource {
        text,
        files: vec![path.to_path_buf()],
    })
}

fn load_dir(path: &Path) -> Result<LoadedSource> {
    let mut files: Vec<PathBuf> = fs::read_dir(path)
        .with_context(|| format!("reading directory `{}`", path.display()))?
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("keron"))
        .collect();
    files.sort();

    if files.is_empty() {
        bail!("no .keron files in `{}`", path.display());
    }

    let mut text = String::new();
    for f in &files {
        let chunk = fs::read_to_string(f).with_context(|| format!("reading `{}`", f.display()))?;
        text.push_str(&chunk);
        if !chunk.ends_with('\n') {
            text.push('\n');
        }
    }

    Ok(LoadedSource { text, files })
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

    #[test]
    fn load_file_accepts_keron_extension() {
        let d = TempDir::new("file-ok");
        let p = d.path.join("entry.keron");
        fs::write(&p, "val n: Int = 1\n").unwrap();
        let got = load(&p).unwrap();
        assert_eq!(got.text, "val n: Int = 1\n");
        assert_eq!(got.files, vec![p]);
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
    fn load_dir_concatenates_keron_files_in_sorted_order() {
        let d = TempDir::new("dir-sort");
        fs::write(d.path.join("b.keron"), "val b: Int = 2\n").unwrap();
        fs::write(d.path.join("a.keron"), "val a: Int = 1\n").unwrap();
        let got = load(&d.path).unwrap();
        // `a.keron` must come before `b.keron`.
        let a_pos = got.text.find("val a").unwrap();
        let b_pos = got.text.find("val b").unwrap();
        assert!(a_pos < b_pos, "got: {}", got.text);
    }

    #[test]
    fn load_dir_skips_non_keron_files() {
        let d = TempDir::new("dir-skip-ext");
        fs::write(d.path.join("a.keron"), "val a: Int = 1\n").unwrap();
        fs::write(d.path.join("b.txt"), "should not appear").unwrap();
        let got = load(&d.path).unwrap();
        assert!(!got.text.contains("should not appear"), "got: {}", got.text);
    }

    #[test]
    fn load_dir_skips_subdirectories_named_with_keron_extension() {
        // Subdirs are filtered by `p.is_file()`. Mutating `&& → ||`
        // would let them through and cause a read failure. Mutating
        // `== → !=` would invert the filter and exclude the legit
        // file too.
        let d = TempDir::new("dir-skip-subdir");
        fs::write(d.path.join("a.keron"), "val a: Int = 1\n").unwrap();
        // Cargo would never name a directory like this in practice,
        // but the filter must still hold.
        fs::create_dir(d.path.join("nested.keron")).unwrap();
        let got = load(&d.path).unwrap();
        assert!(got.text.contains("val a: Int = 1"), "got: {}", got.text);
    }

    #[test]
    fn load_dir_pads_chunks_without_trailing_newline() {
        // Concatenation rule: each chunk must end with `\n`. If the
        // file's last char isn't a newline, one is appended. Without
        // the `!`, every chunk gets a stray newline appended.
        let d = TempDir::new("dir-pad");
        // First file deliberately ends without a newline.
        fs::write(d.path.join("a.keron"), "val a: Int = 1").unwrap();
        fs::write(d.path.join("b.keron"), "val b: Int = 2\n").unwrap();
        let got = load(&d.path).unwrap();
        // We expect `val a: Int = 1\nval b: Int = 2\n`. The middle
        // newline was synthesized; without the guard the file ends
        // up missing the boundary or doubling it.
        assert_eq!(got.text, "val a: Int = 1\nval b: Int = 2\n");
    }

    #[test]
    fn load_dir_does_not_double_newline_when_chunk_ends_with_newline() {
        let d = TempDir::new("dir-nodouble");
        fs::write(d.path.join("a.keron"), "val a: Int = 1\n").unwrap();
        let got = load(&d.path).unwrap();
        assert_eq!(got.text, "val a: Int = 1\n");
        assert!(
            !got.text.ends_with("\n\n"),
            "got double newline: {}",
            got.text
        );
    }

    #[test]
    fn load_dir_errors_when_no_keron_files() {
        let d = TempDir::new("dir-empty");
        fs::write(d.path.join("a.txt"), "").unwrap();
        let err = load(&d.path).unwrap_err();
        assert!(err.to_string().contains("no .keron files"));
    }
}
