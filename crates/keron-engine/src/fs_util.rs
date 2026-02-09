use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = out.pop();
            }
            Component::Normal(segment) => out.push(segment),
            Component::RootDir => out.push(component.as_os_str()),
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
        }
    }

    out
}

pub fn path_exists_including_dangling_symlink(path: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn symlink_points_to(dest: &Path, src: &Path) -> std::io::Result<bool> {
    let metadata = std::fs::symlink_metadata(dest)?;
    if !metadata.file_type().is_symlink() {
        return Ok(false);
    }

    let target = std::fs::read_link(dest)?;
    let resolved_target = if target.is_absolute() {
        normalize_path(&target)
    } else {
        let parent = dest.parent().ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidInput,
                format!("symlink destination has no parent: {}", dest.display()),
            )
        })?;
        normalize_path(&parent.join(target))
    };

    Ok(resolved_target == normalize_path(src))
}
