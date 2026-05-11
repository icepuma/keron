//! Ownership-transfer step. The elevated child calls
//! [`set_owner`] after each successful filesystem write so the
//! resulting symlink / file / directory is owned by the user that
//! invoked the unprivileged parent, *not* by root / Administrator.
//!
//! On Unix this is `lchown` for symlinks (so we set the link's
//! owner, not the target's) and `chown` for regular files and
//! directories — both from `std::os::unix::fs` so we don't need to
//! add the `nix` or `libc` crate.
//!
//! On Windows we open a handle with `FILE_FLAG_OPEN_REPARSE_POINT`
//! and call `SetSecurityInfo` on the handle. The flag is critical:
//! without it `CreateFileW` follows symlinks and we'd set the
//! ownership on the link's target, which is exactly the home-manager
//! `/root` bug. See `windows::set_file_owner` for the full FFI
//! sequence.

use std::path::Path;

use anyhow::Result;

use crate::elevated::payload::OwnerId;

/// Set the owner of `path` to whoever ran the unprivileged parent.
/// Symlinks get `lchown` semantics (link owner, not target). On
/// Unix this is the only API path used. On Windows we route through
/// [`windows::set_file_owner`] which opens an explicit handle.
///
/// # Errors
/// Errors when the underlying syscall fails. The child treats a
/// chown failure as fatal but completes any in-flight write first
/// (the file already exists, only its ownership is wrong).
pub fn set_owner(path: &Path, owner: &OwnerId) -> Result<()> {
    match owner {
        OwnerId::Posix { uid, gid } => set_owner_posix(path, *uid, *gid),
        #[cfg(windows)]
        OwnerId::Windows { sid } => windows::set_file_owner(path, sid),
        #[cfg(not(windows))]
        OwnerId::Windows { .. } => {
            anyhow::bail!("Windows owner SID received on a non-Windows host; this is a payload bug")
        }
    }
}

#[cfg(unix)]
fn set_owner_posix(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use anyhow::Context;
    let meta = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting `{}` for ownership transfer", path.display()))?;
    if meta.file_type().is_symlink() {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))
            .with_context(|| format!("lchown `{}` -> {uid}:{gid}", path.display()))?;
    } else {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))
            .with_context(|| format!("chown `{}` -> {uid}:{gid}", path.display()))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn set_owner_posix(_path: &Path, _uid: u32, _gid: u32) -> Result<()> {
    anyhow::bail!("POSIX owner ids received on a non-Unix host; this is a payload bug")
}

#[cfg(windows)]
pub mod windows {
    //! Windows-side ownership transfer + UAC re-exec primitives.
    //!
    //! All `unsafe` blocks below are scoped per-call-site with a
    //! one-line *why* comment, per the workspace `unsafe_code =
    //! "deny"` policy.

    use std::ffi::c_void;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr;

    use anyhow::{Result, bail};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSidToSidW, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, OWNER_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows_sys::Win32::UI::Shell::{
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
    };

    const WRITE_OWNER: u32 = 0x0008_0000;
    const SW_HIDE: i32 = 0;

    fn to_wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Capture the current process's user SID as a `S-1-5-...`
    /// string. The unprivileged parent calls this and embeds the
    /// result in the payload so the elevated child knows whose SID
    /// to set as the owner of newly created files.
    ///
    /// # Errors
    /// Wraps any failing Win32 call with `io::Error::last_os_error`.
    pub fn current_user_sid_string() -> Result<String> {
        let mut token: HANDLE = ptr::null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle; FFI
        // call writes the actual token handle into `token`. We close
        // it before returning.
        #[allow(unsafe_code)]
        let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if ok == 0 {
            bail!("OpenProcessToken failed: {}", io::Error::last_os_error());
        }

        // Two-step `GetTokenInformation`: query size, then read.
        let mut needed: u32 = 0;
        // SAFETY: FFI; passing NULL and 0 length is the documented
        // probe pattern. We don't read the unset memory.
        #[allow(unsafe_code)]
        unsafe {
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
        }
        if needed == 0 {
            // SAFETY: closing the token we successfully opened.
            #[allow(unsafe_code)]
            unsafe {
                CloseHandle(token);
            }
            bail!(
                "GetTokenInformation probe returned 0 bytes: {}",
                io::Error::last_os_error()
            );
        }
        let mut buf = vec![0u8; needed as usize];
        // SAFETY: FFI fills `buf` with `TOKEN_USER` data; `needed`
        // came from the probe above, so the buffer is sized.
        #[allow(unsafe_code)]
        let ok = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast::<c_void>(),
                needed,
                &mut needed,
            )
        };
        // SAFETY: token is still live; close it now that we have the data.
        #[allow(unsafe_code)]
        unsafe {
            CloseHandle(token);
        }
        if ok == 0 {
            bail!(
                "GetTokenInformation read failed: {}",
                io::Error::last_os_error()
            );
        }

        // SAFETY: `TOKEN_USER` starts at offset 0 of the buffer
        // because we read it via `GetTokenInformation(TokenUser, ...)`.
        #[allow(unsafe_code)]
        let token_user: &TOKEN_USER = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        let sid_ptr: PSID = token_user.User.Sid;

        let mut sid_str: *mut u16 = ptr::null_mut();
        // SAFETY: FFI; `sid_ptr` came from the kernel a moment ago
        // so it's valid. The string `sid_str` writes to is owned by
        // LocalAlloc and we LocalFree it below.
        #[allow(unsafe_code)]
        let ok = unsafe { ConvertSidToStringSidW(sid_ptr, &mut sid_str) };
        if ok == 0 {
            bail!(
                "ConvertSidToStringSidW failed: {}",
                io::Error::last_os_error()
            );
        }
        // SAFETY: we walk the UTF-16 buffer until the NUL terminator
        // that `ConvertSidToStringSidW` is documented to produce.
        #[allow(unsafe_code)]
        let s = unsafe {
            let mut len = 0;
            while *sid_str.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(sid_str, len))
        };
        // SAFETY: free the buffer LocalAlloc'd by ConvertSidToStringSidW.
        #[allow(unsafe_code)]
        unsafe {
            LocalFree(sid_str as HLOCAL);
        }
        Ok(s)
    }

    /// Set the owner of `path` to the SID stored in `sid_str`.
    /// Routes through a `CreateFileW` handle with
    /// `FILE_FLAG_OPEN_REPARSE_POINT` so the call writes the link's
    /// owner, not the target's. Works for files, directories, and
    /// symlinks alike — `FILE_FLAG_BACKUP_SEMANTICS` lets us open a
    /// directory handle.
    ///
    /// # Errors
    /// Wraps Win32 errors with `io::Error::last_os_error`.
    pub fn set_file_owner(path: &Path, sid_str: &str) -> Result<()> {
        let wide_sid = to_wide(sid_str);
        let mut sid: PSID = ptr::null_mut();
        // SAFETY: FFI; `wide_sid` is a NUL-terminated UTF-16 slice
        // owned by us. On success `sid` points at LocalAlloc memory
        // we LocalFree below.
        #[allow(unsafe_code)]
        let ok = unsafe { ConvertStringSidToSidW(wide_sid.as_ptr(), &mut sid) };
        if ok == 0 {
            bail!(
                "ConvertStringSidToSidW failed for `{sid_str}`: {}",
                io::Error::last_os_error()
            );
        }

        let wide_path = to_wide(path);
        // SAFETY: FFI; `wide_path` is NUL-terminated. The handle we
        // get back is closed below.
        #[allow(unsafe_code)]
        let handle: HANDLE = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                WRITE_OWNER,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                ptr::null_mut(),
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            // SAFETY: clean up the SID before bailing.
            #[allow(unsafe_code)]
            unsafe {
                LocalFree(sid as HLOCAL);
            }
            bail!(
                "CreateFileW(`{}`, WRITE_OWNER) failed: {}",
                path.display(),
                io::Error::last_os_error()
            );
        }

        // SAFETY: handle is open, sid is valid; the DACL/SACL/group
        // pointers are NULL because we only want to set the owner.
        #[allow(unsafe_code)]
        let err = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                sid,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
            )
        };
        // SAFETY: close the handle and free the SID regardless of result.
        #[allow(unsafe_code)]
        unsafe {
            CloseHandle(handle);
            LocalFree(sid as HLOCAL);
        }
        if err != 0 {
            bail!(
                "SetSecurityInfo on `{}` failed with code {err}",
                path.display()
            );
        }
        Ok(())
    }

    /// UAC re-exec. Spawns `<exe> __apply-elevated <payload>` via
    /// `ShellExecuteExW` with the `runas` verb. Waits for the child
    /// to finish and returns its `ExitStatus`.
    ///
    /// stdin/stdout are not redirected — that's a `ShellExecuteExW`
    /// limitation; the elevated child's output goes to its own
    /// console (which is hidden via `SW_HIDE`). The unprivileged
    /// parent renders the final summary itself.
    ///
    /// # Errors
    /// Errors if `ShellExecuteExW` fails, if waiting fails, or if
    /// `GetExitCodeProcess` fails.
    pub fn shell_execute_runas(exe: &Path, payload: &Path) -> Result<std::process::ExitStatus> {
        use std::os::windows::process::ExitStatusExt;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, INFINITE, WaitForSingleObject,
        };

        let verb = to_wide("runas");
        let exe_w = to_wide(exe);
        let params: PathBuf = format!("__apply-elevated \"{}\"", payload.display()).into();
        let params_w = to_wide(&params);

        // SAFETY: `SHELLEXECUTEINFOW` contains an anonymous union;
        // zero-initialising the whole struct is the documented
        // pattern in Microsoft's Win32 reference. We immediately
        // overwrite every field we care about; the union member
        // ends up holding all-zero bits, which is the documented
        // "not used" sentinel for the verbs we invoke.
        #[allow(unsafe_code)]
        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = verb.as_ptr();
        info.lpFile = exe_w.as_ptr();
        info.lpParameters = params_w.as_ptr();
        info.nShow = SW_HIDE;
        // SAFETY: FFI; `info` is correctly initialized per its
        // documented contract (cbSize set, all string ptrs are
        // NUL-terminated UTF-16 we own, NULL where allowed).
        #[allow(unsafe_code)]
        let ok = unsafe { ShellExecuteExW(&mut info) };
        if ok == 0 {
            // SAFETY: GetLastError reads thread-local error state.
            #[allow(unsafe_code)]
            let code = unsafe { GetLastError() };
            bail!("ShellExecuteExW(runas) failed: GetLastError = {code}");
        }
        if info.hProcess.is_null() {
            bail!("ShellExecuteExW returned no process handle");
        }
        // SAFETY: hProcess is live; INFINITE blocks until the child exits.
        #[allow(unsafe_code)]
        unsafe {
            WaitForSingleObject(info.hProcess, INFINITE);
        }
        let mut code: u32 = 0;
        // SAFETY: hProcess still live; we read its exit code.
        #[allow(unsafe_code)]
        let ok = unsafe { GetExitCodeProcess(info.hProcess, &mut code) };
        // SAFETY: close the process handle regardless.
        #[allow(unsafe_code)]
        unsafe {
            CloseHandle(info.hProcess);
        }
        if ok == 0 {
            bail!("GetExitCodeProcess failed: {}", io::Error::last_os_error());
        }
        Ok(std::process::ExitStatus::from_raw(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn temp(tag: &str) -> PathBuf {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p =
            std::env::temp_dir().join(format!("keron-chown-test-{tag}-{}-{n}", std::process::id()));
        if p.exists() {
            let _ = fs::remove_dir_all(&p);
        }
        fs::create_dir_all(&p).unwrap();
        fs::canonicalize(p).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn set_owner_to_self_is_a_noop_on_a_regular_file() {
        // We can't chown to a different user without root, but we
        // *can* chown to our own uid/gid as a no-op and prove the
        // syscall is wired up correctly. Mutating `set_owner_posix`
        // to `Ok(())` would skip the syscall and still pass — but
        // mutating it to swap uid/gid would error (EPERM), which a
        // mutation test would catch.
        use std::os::unix::fs::MetadataExt;
        let d = temp("self");
        let file = d.join("payload");
        fs::write(&file, "hi").unwrap();
        let meta = fs::metadata(&file).unwrap();
        let owner = OwnerId::Posix {
            uid: meta.uid(),
            gid: meta.gid(),
        };
        set_owner(&file, &owner).expect("self-chown should succeed");
        let after = fs::metadata(&file).unwrap();
        assert_eq!(after.uid(), meta.uid());
        assert_eq!(after.gid(), meta.gid());
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn set_owner_to_self_is_a_noop_on_a_symlink() {
        use std::os::unix::fs::MetadataExt;
        let d = temp("self-symlink");
        let target = d.join("real");
        fs::write(&target, "hi").unwrap();
        let link = d.join("alias");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let meta = fs::symlink_metadata(&link).unwrap();
        let owner = OwnerId::Posix {
            uid: meta.uid(),
            gid: meta.gid(),
        };
        set_owner(&link, &owner).expect("self-lchown should succeed");
        let after = fs::symlink_metadata(&link).unwrap();
        assert_eq!(after.uid(), meta.uid());
        assert_eq!(after.gid(), meta.gid());
        let _ = fs::remove_dir_all(&d);
    }

    #[cfg(unix)]
    #[test]
    fn set_owner_rejects_windows_payload_on_unix() {
        let d = temp("wrong-platform");
        let file = d.join("x");
        fs::write(&file, "").unwrap();
        let err = set_owner(
            &file,
            &OwnerId::Windows {
                sid: "S-1-5-21-X".into(),
            },
        )
        .expect_err("Windows owner on unix must error");
        assert!(
            format!("{err:#}").contains("non-Windows host"),
            "got: {err:#}",
        );
        let _ = fs::remove_dir_all(&d);
    }
}
