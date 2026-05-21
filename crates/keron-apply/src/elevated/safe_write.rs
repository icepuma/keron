//! `openat`-based path-walking primitives for the elevated apply.
//!
//! The elevated child runs as root and must not be tricked into
//! writing through a symlink an unprivileged peer planted in an
//! intermediate directory. `crates/keron-apply/src/elevated/child.rs`
//! used to rely on a `symlink_metadata` ancestor scan, which is
//! defense-in-depth but leaves a TOCTOU window between the check
//! and the actual write.
//!
//! This module closes that window: every ancestor directory is
//! opened with `O_DIRECTORY | O_NOFOLLOW`, and the leaf write
//! happens via `symlinkat(2)` / `openat(2)` relative to the
//! resulting directory file descriptor. A concurrent swap of an
//! ancestor to a symlink results in `ELOOP` from the next
//! `openat`, not a redirected write.
//!
//! The syscalls go through `rustix`'s typed wrappers, which return
//! `OwnedFd` directly and accept `&Path` arguments. No `unsafe` is
//! needed in this file — the FFI surface is fully encapsulated by
//! the wrapper crate.
//!
//! Cross-platform note: only Unix is supported here. `cfg(windows)`
//! callers use the existing `fs::*` path; the Windows elevation
//! flow has its own ACL-based safety story.

#![cfg(unix)]

use std::ffi::OsStr;
use std::io;
use std::os::unix::io::OwnedFd;
use std::path::Path;

use anyhow::{Context, Result, bail};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;

/// Owned parent-directory file descriptor; closes on drop.
#[derive(Debug)]
pub struct ParentDir {
    fd: OwnedFd,
}

impl ParentDir {
    /// Walk `parent` component by component, opening each one with
    /// `O_DIRECTORY | O_NOFOLLOW` so an attacker who plants a symlink
    /// at any intermediate position is rejected with `ELOOP` instead
    /// of redirecting the elevated write. Missing components are
    /// created with `mkdirat` (mode 0755) and immediately re-opened.
    ///
    /// `parent` must be absolute; relative paths are not used in the
    /// elevated apply flow.
    pub fn open(parent: &Path) -> Result<Self> {
        if !parent.is_absolute() {
            bail!(
                "elevated apply requires absolute paths, got `{}`",
                parent.display()
            );
        }
        let mut current = open_root().context("opening filesystem root for elevated walk")?;
        for component in parent.components() {
            match component {
                std::path::Component::RootDir | std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    bail!(
                        "elevated apply refuses to walk `..` in `{}` (paths must be canonical)",
                        parent.display()
                    );
                }
                std::path::Component::Prefix(_) => {
                    bail!("elevated apply does not support Windows path prefixes")
                }
                std::path::Component::Normal(name) => {
                    current = open_or_create_subdir(&current, name).with_context(|| {
                        format!(
                            "opening `{}` while walking elevated parent `{}`",
                            os_str_lossy(name),
                            parent.display()
                        )
                    })?;
                }
            }
        }
        Ok(Self { fd: current })
    }
}

/// Create a symlink at `leaf` inside `parent`, pointing at `target`.
/// `symlinkat(2)` does not follow `leaf`, so the leaf itself can't
/// be subverted. `target` is stored verbatim (it's the link's
/// payload; the kernel only resolves it when something dereferences
/// the link).
pub fn symlink_at(parent: &ParentDir, leaf: &OsStr, target: &Path) -> Result<()> {
    rustix::fs::symlinkat(target, &parent.fd, leaf).with_context(|| {
        format!(
            "symlinkat `{}` -> `{}` in elevated parent",
            os_str_lossy(leaf),
            target.display()
        )
    })
}

/// Create a new regular file at `leaf` inside `parent` with the
/// given mode. Uses `O_CREAT | O_EXCL | O_NOFOLLOW | O_WRONLY` so:
///
/// - `O_EXCL` refuses if the leaf already exists (prevents
///   clobbering a victim file the attacker pre-created).
/// - `O_NOFOLLOW` refuses if the leaf itself is a symlink.
/// - The directory fd argument keeps the ancestor walk in scope.
pub fn create_file_at(parent: &ParentDir, leaf: &OsStr, mode: u32) -> Result<std::fs::File> {
    let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let fd = rustix::fs::openat(&parent.fd, leaf, flags, raw_mode(mode)).with_context(|| {
        format!(
            "openat `{}` in elevated parent (create new, no follow)",
            os_str_lossy(leaf),
        )
    })?;
    Ok(std::fs::File::from(fd))
}

/// Build a `Mode` from a `u32` of mode bits. `rustix::fs::RawMode`
/// is the platform's `mode_t` — `u32` on Linux, `u16` on macOS —
/// so we mask to the standard permission + setuid/setgid/sticky
/// bits (12 bits, fits in `u16`) before narrowing. The mask
/// documents that only the low file-mode bits are meaningful here
/// and silences the truncation lint without an `#[allow]`.
#[allow(clippy::cast_possible_truncation)]
const fn raw_mode(mode: u32) -> Mode {
    Mode::from_bits_truncate((mode & 0o7777) as rustix::fs::RawMode)
}

fn open_root() -> io::Result<OwnedFd> {
    rustix::fs::open(
        "/",
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(io::Error::from)
}

fn open_or_create_subdir(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    // Probe the entry without following any symlink so we can
    // distinguish "regular dir" (open with NOFOLLOW) from "symlink"
    // (only allowed if root-owned; co-resident attackers can't
    // tamper with what root owns). Anything else — a regular file,
    // a non-root symlink, an unreadable entry — is refused.
    let stat = match rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return mkdir_and_open(parent, name),
        Err(e) => return Err(e.into()),
    };

    let file_type = FileType::from_raw_mode(stat.st_mode);
    if file_type == FileType::Symlink {
        // Symlink: follow only if the link itself is root-owned.
        // macOS ships `/var -> /private/var`, `/tmp -> /private/tmp`,
        // `/etc -> /private/etc`; these are owned by uid 0 and a
        // co-resident user cannot replace them. A user-owned
        // symlink in the path IS the attack we're defending against.
        if stat.st_uid != 0 {
            return Err(io::Error::other(format!(
                "elevated apply refuses to walk through non-root symlink `{}` (uid {})",
                name.to_string_lossy(),
                stat.st_uid,
            )));
        }
        // Open following the symlink. After the open, verify the
        // resolved target is itself root-owned so a one-step
        // root→user redirect can't slip through.
        let follow_flags = OFlags::DIRECTORY | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(parent, name, follow_flags, Mode::empty())
            .map_err(io::Error::from)?;
        let target_stat = rustix::fs::fstat(&fd).map_err(io::Error::from)?;
        if target_stat.st_uid != 0 {
            return Err(io::Error::other(format!(
                "elevated apply refuses: root symlink `{}` resolves to non-root target (uid {})",
                name.to_string_lossy(),
                target_stat.st_uid,
            )));
        }
        return Ok(fd);
    }
    if file_type != FileType::Directory {
        return Err(io::Error::other(format!(
            "elevated apply refuses: ancestor `{}` is neither a directory nor a symlink",
            name.to_string_lossy()
        )));
    }

    // Regular directory.
    let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::openat(parent, name, flags, Mode::empty()).map_err(io::Error::from)
}

fn mkdir_and_open(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    // Component doesn't exist yet — create it and re-open. The
    // mode is the conventional 0755 for directories; the
    // elevated child will chown each created leaf back to the
    // calling user afterwards, but intermediate dirs keep their
    // existing ownership and mode (`mkdirat` honors umask).
    //
    // Tolerate `EEXIST`: another process may have created the dir
    // between our stat and mkdir. The re-open below will see it.
    match rustix::fs::mkdirat(parent, name, raw_mode(0o755)) {
        Ok(()) | Err(Errno::EXIST) => {}
        Err(e) => return Err(e.into()),
    }
    let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::openat(parent, name, flags, Mode::empty()).map_err(io::Error::from)
}

fn os_str_lossy(s: &OsStr) -> String {
    s.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn fresh_dir(tag: &str) -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("keron-safewrite-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn open_walks_existing_directory() {
        let root = fresh_dir("walk-existing");
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let parent = ParentDir::open(&nested).expect("ok");
        // Drop closes fd; smoke check the meta is intact.
        drop(parent);
        assert!(nested.is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_creates_missing_intermediates() {
        let root = fresh_dir("walk-missing");
        let leaf_parent = root.join("new").join("dir");
        assert!(!leaf_parent.exists());
        let _parent = ParentDir::open(&leaf_parent).expect("ok");
        assert!(leaf_parent.is_dir());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn open_refuses_symlinked_ancestor() {
        let root = fresh_dir("walk-symlink");
        let real = root.join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // Walking through `link` (a symlink to `real`) must fail.
        let err = ParentDir::open(&link.join("inside")).expect_err("symlink ancestor");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("opening")
                || msg.contains("ELOOP")
                || msg.contains("symbolic")
                || msg.contains("not a directory"),
            "expected symlink-refusal message, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn symlink_at_creates_link_inside_parent() {
        let root = fresh_dir("symlinkat");
        let parent_path = root.join("p");
        std::fs::create_dir_all(&parent_path).unwrap();
        let target = root.join("target");
        std::fs::write(&target, "hi").unwrap();
        let parent = ParentDir::open(&parent_path).unwrap();
        symlink_at(&parent, OsStr::new("link"), &target).unwrap();
        let link = parent_path.join("link");
        let meta = std::fs::symlink_metadata(&link).unwrap();
        assert!(meta.file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "hi");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_file_at_creates_with_mode() {
        let root = fresh_dir("openat");
        let parent_path = root.join("p");
        std::fs::create_dir_all(&parent_path).unwrap();
        let parent = ParentDir::open(&parent_path).unwrap();
        let mut file = create_file_at(&parent, OsStr::new("leaf"), 0o600).unwrap();
        file.write_all(b"payload").unwrap();
        drop(file);
        let path = parent_path.join("leaf");
        let mode = std::fs::metadata(&path).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o600, "mode: {mode:o}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "payload");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_file_at_refuses_existing_leaf() {
        let root = fresh_dir("openat-existing");
        let parent_path = root.join("p");
        std::fs::create_dir_all(&parent_path).unwrap();
        std::fs::write(parent_path.join("leaf"), "old").unwrap();
        let parent = ParentDir::open(&parent_path).unwrap();
        let err =
            create_file_at(&parent, OsStr::new("leaf"), 0o600).expect_err("leaf already exists");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("exists") || msg.contains("openat"),
            "expected exists error, got: {msg}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn create_file_at_refuses_symlink_leaf() {
        let root = fresh_dir("openat-symlink-leaf");
        let parent_path = root.join("p");
        std::fs::create_dir_all(&parent_path).unwrap();
        let target = root.join("target");
        std::fs::write(&target, "victim").unwrap();
        std::os::unix::fs::symlink(&target, parent_path.join("leaf")).unwrap();
        let parent = ParentDir::open(&parent_path).unwrap();
        let err = create_file_at(&parent, OsStr::new("leaf"), 0o600)
            .expect_err("symlink leaf must not open");
        let _ = err;
        // Victim untouched.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "victim");
        let _ = std::fs::remove_dir_all(&root);
    }
}
