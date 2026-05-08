//! Load a `.keron` file or a directory of them into a single source
//! buffer for the parser. Directory loading concatenates files in
//! sorted order (matches Terraform/OpenTofu module semantics).

#![allow(clippy::redundant_pub_crate)]

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Debug)]
pub(crate) struct LoadedSource {
    pub(crate) text: String,
    #[allow(dead_code)] // surfaced once we map errors back to per-file spans
    pub(crate) files: Vec<PathBuf>,
}

pub(crate) fn load(path: &Path) -> Result<LoadedSource> {
    let meta = fs::metadata(path)
        .with_context(|| format!("reading `{}`", path.display()))?;

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
    let text = fs::read_to_string(path)
        .with_context(|| format!("reading `{}`", path.display()))?;
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
        .filter(|p| {
            p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("keron")
        })
        .collect();
    files.sort();

    if files.is_empty() {
        bail!("no .keron files in `{}`", path.display());
    }

    let mut text = String::new();
    for f in &files {
        let chunk = fs::read_to_string(f)
            .with_context(|| format!("reading `{}`", f.display()))?;
        text.push_str(&chunk);
        if !chunk.ends_with('\n') {
            text.push('\n');
        }
    }

    Ok(LoadedSource { text, files })
}
