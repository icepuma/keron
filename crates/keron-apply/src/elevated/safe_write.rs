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
//! Cross-platform note: only Unix is supported here. `cfg(windows)`
//! callers use the existing `fs::*` path; the Windows elevation
//! flow has its own ACL-based safety story.

#![cfg(unix)]

use std::ffi::{CString, OsStr};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};
use std::path::Path;

use anyhow::{Context, Result, bail};

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

    fn raw(&self) -> RawFd {
        std::os::unix::io::AsRawFd::as_raw_fd(&self.fd)
    }
}

/// Create a symlink at `leaf` inside `parent`, pointing at `target`.
/// `symlinkat(2)` does not follow `leaf`, so the leaf itself can't
/// be subverted. `target` is stored verbatim (it's the link's
/// payload; the kernel only resolves it when something dereferences
/// the link).
pub fn symlink_at(parent: &ParentDir, leaf: &OsStr, target: &Path) -> Result<()> {
    let target_c = cstring(target.as_os_str())?;
    let leaf_c = cstring(leaf)?;
    // SAFETY: FFI; both CStrings outlive the call, `parent.raw()`
    // is an open directory fd this struct owns until drop.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::symlinkat(target_c.as_ptr(), parent.raw(), leaf_c.as_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error()).with_context(|| {
            format!(
                "symlinkat `{}` -> `{}` in elevated parent",
                os_str_lossy(leaf),
                target.display()
            )
        });
    }
    Ok(())
}

/// Create a new regular file at `leaf` inside `parent` with the
/// given mode. Uses `O_CREAT | O_EXCL | O_NOFOLLOW | O_WRONLY` so:
///
/// - `O_EXCL` refuses if the leaf already exists (prevents
///   clobbering a victim file the attacker pre-created).
/// - `O_NOFOLLOW` refuses if the leaf itself is a symlink.
/// - The directory fd argument keeps the ancestor walk in scope.
pub fn create_file_at(parent: &ParentDir, leaf: &OsStr, mode: u32) -> Result<std::fs::File> {
    let leaf_c = cstring(leaf)?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: FFI; `leaf_c` outlives the call. The returned fd is
    // wrapped in `File` on success or freed via close-on-error if
    // we drop before `File::from_raw_fd`.
    #[allow(unsafe_code)]
    let fd = unsafe { libc::openat(parent.raw(), leaf_c.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error()).with_context(|| {
            format!(
                "openat `{}` in elevated parent (create new, no follow)",
                os_str_lossy(leaf),
            )
        });
    }
    // SAFETY: we own the fd; `File::from_raw_fd` takes ownership.
    #[allow(unsafe_code)]
    let file = unsafe { std::fs::File::from_raw_fd(fd) };
    Ok(file)
}

fn open_root() -> io::Result<OwnedFd> {
    let root = CString::new("/").expect("root path is valid");
    // SAFETY: FFI; root path is a static NUL-terminated string.
    #[allow(unsafe_code)]
    let fd = unsafe {
        libc::open(
            root.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: we own the fd.
    #[allow(unsafe_code)]
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_or_create_subdir(parent: &OwnedFd, name: &OsStr) -> io::Result<OwnedFd> {
    let cname = cstring_io(name)?;
    let parent_fd = std::os::unix::io::AsRawFd::as_raw_fd(parent);

    // Probe the entry without following any symlink so we can
    // distinguish "regular dir" (open with NOFOLLOW) from "symlink"
    // (only allowed if root-owned; co-resident attackers can't
    // tamper with what root owns). Anything else — a regular file,
    // a non-root symlink, an unreadable entry — is refused.
    let mut statbuf: libc::stat = unsafe_zeroed_stat();
    // SAFETY: FFI; cname outlives the call.
    #[allow(unsafe_code)]
    let rc = unsafe {
        libc::fstatat(
            parent_fd,
            cname.as_ptr(),
            &raw mut statbuf,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::NotFound {
            return Err(err);
        }
        return mkdir_and_open(parent_fd, &cname);
    }

    // `mode_t` is `u32` on Linux and `u16` on macOS; comparing
    // `st_mode` against `S_IF*` masks in the native type works on
    // both platforms without a `u32::from(...)` round-trip that
    // would be a self-cast on Linux.
    let mode = statbuf.st_mode;
    if mode & libc::S_IFMT == libc::S_IFLNK {
        // Symlink: follow only if the link itself is root-owned.
        // macOS ships `/var -> /private/var`, `/tmp -> /private/tmp`,
        // `/etc -> /private/etc`; these are owned by uid 0 and a
        // co-resident user cannot replace them. A user-owned
        // symlink in the path IS the attack we're defending against.
        if statbuf.st_uid != 0 {
            return Err(io::Error::other(format!(
                "elevated apply refuses to walk through non-root symlink `{}` (uid {})",
                name.to_string_lossy(),
                statbuf.st_uid,
            )));
        }
        // Open following the symlink. After the open, verify the
        // resolved target is itself root-owned so a one-step
        // root→user redirect can't slip through.
        let follow_flags = libc::O_DIRECTORY | libc::O_CLOEXEC;
        // SAFETY: FFI; cname outlives the call.
        #[allow(unsafe_code)]
        let fd = unsafe { libc::openat(parent_fd, cname.as_ptr(), follow_flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut target_stat: libc::stat = unsafe_zeroed_stat();
        // SAFETY: FFI; fd is open.
        #[allow(unsafe_code)]
        let rc = unsafe { libc::fstat(fd, &raw mut target_stat) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            // SAFETY: close the fd we just opened.
            #[allow(unsafe_code)]
            unsafe {
                libc::close(fd);
            }
            return Err(err);
        }
        if target_stat.st_uid != 0 {
            // SAFETY: close the fd we just opened.
            #[allow(unsafe_code)]
            unsafe {
                libc::close(fd);
            }
            return Err(io::Error::other(format!(
                "elevated apply refuses: root symlink `{}` resolves to non-root target (uid {})",
                name.to_string_lossy(),
                target_stat.st_uid,
            )));
        }
        // SAFETY: we own the fd.
        #[allow(unsafe_code)]
        return Ok(unsafe { OwnedFd::from_raw_fd(fd) });
    }
    if mode & libc::S_IFMT != libc::S_IFDIR {
        return Err(io::Error::other(format!(
            "elevated apply refuses: ancestor `{}` is neither a directory nor a symlink",
            name.to_string_lossy()
        )));
    }

    // Regular directory.
    let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: FFI; cname outlives the call.
    #[allow(unsafe_code)]
    let fd = unsafe { libc::openat(parent_fd, cname.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: we own the fd.
    #[allow(unsafe_code)]
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn mkdir_and_open(parent_fd: RawFd, cname: &CString) -> io::Result<OwnedFd> {
    // Component doesn't exist yet — create it and re-open. The
    // mode is the conventional 0755 for directories; the
    // elevated child will chown each created leaf back to the
    // calling user afterwards, but intermediate dirs keep their
    // existing ownership and mode (`mkdirat` honors umask).
    // SAFETY: FFI; cname outlives the call.
    #[allow(unsafe_code)]
    let rc = unsafe { libc::mkdirat(parent_fd, cname.as_ptr(), 0o755) };
    if rc != 0 {
        let mk_err = io::Error::last_os_error();
        // Tolerate races: another process may have created the
        // dir between our stat and mkdir. Re-open below will see it.
        if mk_err.kind() != io::ErrorKind::AlreadyExists {
            return Err(mk_err);
        }
    }
    let flags = libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: FFI; cname outlives the call.
    #[allow(unsafe_code)]
    let fd = unsafe { libc::openat(parent_fd, cname.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: we own the fd.
    #[allow(unsafe_code)]
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

const fn unsafe_zeroed_stat() -> libc::stat {
    // SAFETY: `libc::stat` is plain-old-data; zero bytes is a valid
    // (if meaningless) value, immediately overwritten by `fstatat`.
    #[allow(unsafe_code)]
    unsafe {
        std::mem::zeroed()
    }
}

fn cstring(name: &OsStr) -> Result<CString> {
    cstring_io(name).map_err(|e| anyhow::anyhow!(e))
}

fn cstring_io(name: &OsStr) -> io::Result<CString> {
    CString::new(name.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path component contains an interior NUL byte",
        )
    })
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
