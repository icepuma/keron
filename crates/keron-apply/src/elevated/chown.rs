//! Ownership-transfer step. The elevated child calls
//! [`set_owner`] after each successful filesystem write so the
//! resulting symlink / file / directory is owned by the user that
//! invoked the unprivileged parent, *not* by root / Administrator.
//!
//! On Unix this is `lchown` from `std::os::unix::fs` — a single
//! safe syscall that sets the path's own owner without following
//! symlinks. For regular files and directories it behaves
//! identically to `chown`; for symlinks it sets the link's owner
//! rather than the target's. The previous two-step
//! `symlink_metadata` + `chown` / `lchown` branch was redundant.
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
    // `lchown` is correct for both leaves: on a symlink it sets the
    // link's owner (not the target's), and on a regular file or
    // directory it behaves exactly like `chown`. One safe syscall;
    // no `symlink_metadata` probe needed.
    std::os::unix::fs::lchown(path, Some(uid), Some(gid))
        .with_context(|| format!("lchown `{}` -> {uid}:{gid}", path.display()))
}

#[cfg(not(unix))]
fn set_owner_posix(_path: &Path, _uid: u32, _gid: u32) -> Result<()> {
    anyhow::bail!("POSIX owner ids received on a non-Unix host; this is a payload bug")
}

#[cfg(windows)]
pub mod windows {
    //! Windows-side ownership transfer + UAC re-exec primitives.
    //!
    //! Each function carries one `#[allow(unsafe_code)]` opt-out
    //! plus a top-of-function SAFETY block describing the
    //! invariants for *every* FFI call inside. That gives auditors
    //! one unit of review per logical operation instead of one per
    //! syscall — the same approach the rest of the codebase takes
    //! for sites that genuinely need FFI.
    //!
    //! Strings are marshalled via the `windows` crate's `PCWSTR`
    //! wrapper around our own `Vec<u16>` buffer (NUL-terminated),
    //! which keeps lifetimes obvious and avoids an HSTRING heap
    //! allocation for paths we already own.

    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use anyhow::{Result, bail};
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, HANDLE, HLOCAL, INVALID_HANDLE_VALUE, LocalFree,
    };
    use windows::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSidToSidW, SE_FILE_OBJECT, SetSecurityInfo,
    };
    use windows::Win32::Security::{
        GetTokenInformation, OWNER_SECURITY_INFORMATION, PSID, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::UI::Shell::{SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows::core::{PCWSTR, PWSTR};

    const WRITE_OWNER: u32 = 0x0008_0000;
    const SW_HIDE: i32 = 0;

    /// Encode `s` as a NUL-terminated UTF-16 buffer suitable for
    /// `PCWSTR(buf.as_ptr())`. The caller owns the `Vec<u16>` and
    /// must keep it alive for as long as the pointer is in use.
    fn to_wide(s: impl AsRef<std::ffi::OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Capture the current process's user SID as a `S-1-5-...`
    /// string. The unprivileged parent calls this and embeds the
    /// result in the payload so the elevated child knows whose SID
    /// to set as the owner of newly created files.
    ///
    /// # Errors
    /// Returns any failing Win32 call as `windows::core::Error`
    /// wrapped in `anyhow`.
    //
    // SAFETY (all unsafe blocks below):
    //   - `GetCurrentProcess()` returns a pseudo-handle that needs
    //     no closing; `OpenProcessToken` writes the real token into
    //     `token` which we `CloseHandle` before returning.
    //   - The first `GetTokenInformation` call is the documented
    //     size-probe pattern (NULL buffer / 0 length).
    //   - The second `GetTokenInformation` call reads into `buf`,
    //     which is sized by the probe.
    //   - `&*(buf.as_ptr() as *const TOKEN_USER)` is valid because
    //     `TokenUser` returns `TOKEN_USER` at offset 0 of the buffer.
    //   - `ConvertSidToStringSidW` writes a LocalAlloc'd UTF-16
    //     string we walk to the NUL terminator and `LocalFree`.
    #[allow(unsafe_code)]
    pub fn current_user_sid_string() -> Result<String> {
        let mut token = HANDLE::default();
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
            .map_err(|e| anyhow::anyhow!("OpenProcessToken failed: {e}"))?;

        // Two-step `GetTokenInformation`: query size, then read.
        let mut needed: u32 = 0;
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
        if needed == 0 {
            let _ = unsafe { CloseHandle(token) };
            bail!(
                "GetTokenInformation probe returned 0 bytes: {}",
                std::io::Error::last_os_error()
            );
        }
        let mut buf = vec![0u8; needed as usize];
        let read_res = unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast::<c_void>()),
                needed,
                &mut needed,
            )
        };
        let _ = unsafe { CloseHandle(token) };
        read_res.map_err(|e| anyhow::anyhow!("GetTokenInformation read failed: {e}"))?;

        let token_user: &TOKEN_USER = unsafe { &*(buf.as_ptr() as *const TOKEN_USER) };
        let sid_ptr: PSID = token_user.User.Sid;

        let mut sid_str = PWSTR::null();
        unsafe { ConvertSidToStringSidW(sid_ptr, &mut sid_str) }
            .map_err(|e| anyhow::anyhow!("ConvertSidToStringSidW failed: {e}"))?;
        let s = unsafe {
            let mut len = 0;
            while *sid_str.0.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(sid_str.0, len))
        };
        let _ = unsafe { LocalFree(Some(HLOCAL(sid_str.0.cast::<c_void>()))) };
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
    /// Returns Win32 errors via `windows::core::Error`.
    //
    // SAFETY (all unsafe blocks below):
    //   - `ConvertStringSidToSidW` writes a LocalAlloc'd SID we
    //     `LocalFree` before returning.
    //   - `CreateFileW` is called with our own NUL-terminated
    //     wide buffer; the returned handle is `CloseHandle`d.
    //   - `SetSecurityInfo` is called with the open handle and the
    //     freshly-allocated SID; DACL/SACL/group pointers are NULL.
    //   - `CloseHandle`/`LocalFree` run on the resource handles we
    //     created above, regardless of `SetSecurityInfo` outcome.
    #[allow(unsafe_code)]
    pub fn set_file_owner(path: &Path, sid_str: &str) -> Result<()> {
        let wide_sid = to_wide(sid_str);
        let mut sid = PSID::default();
        unsafe { ConvertStringSidToSidW(PCWSTR(wide_sid.as_ptr()), &mut sid) }
            .map_err(|e| anyhow::anyhow!("ConvertStringSidToSidW failed for `{sid_str}`: {e}"))?;

        let wide_path = to_wide(path);
        let handle = unsafe {
            CreateFileW(
                PCWSTR(wide_path.as_ptr()),
                WRITE_OWNER,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
                None,
            )
        };
        let handle = match handle {
            Ok(h) if h != INVALID_HANDLE_VALUE => h,
            _ => {
                let _ = unsafe { LocalFree(Some(HLOCAL(sid.0))) };
                bail!(
                    "CreateFileW(`{}`, WRITE_OWNER) failed: {}",
                    path.display(),
                    std::io::Error::last_os_error()
                );
            }
        };

        let err = unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                Some(sid),
                None,
                None,
                None,
            )
        };
        let _ = unsafe { CloseHandle(handle) };
        let _ = unsafe { LocalFree(Some(HLOCAL(sid.0))) };
        if err.is_err() {
            bail!(
                "SetSecurityInfo on `{}` failed with code {:?}",
                path.display(),
                err
            );
        }
        Ok(())
    }

    /// UAC re-exec. Spawns
    /// `<exe> __apply-elevated <payload> <digest> <identity>` via
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
    //
    // SAFETY (all unsafe blocks below):
    //   - `SHELLEXECUTEINFOW` contains an anonymous union;
    //     `mem::zeroed` is the documented init pattern. Every field
    //     we use is set explicitly before the call.
    //   - `ShellExecuteExW`, `WaitForSingleObject`,
    //     `GetExitCodeProcess`, and `CloseHandle` are invoked with
    //     `info.hProcess`, which is non-null on the success path
    //     (checked) and which we close before returning.
    //
    // `#[mutants::skip]` because the function calls Win32
    // `ShellExecuteExW`/`WaitForSingleObject`/`GetExitCodeProcess` —
    // there is no in-process test harness for that on macOS / Linux
    // (where mutants runs).
    #[cfg_attr(test, mutants::skip)]
    #[allow(unsafe_code)]
    pub fn shell_execute_runas(
        exe: &Path,
        payload: &Path,
        expected: &crate::elevated::payload::PayloadExpectation,
    ) -> Result<std::process::ExitStatus> {
        use std::os::windows::process::ExitStatusExt;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, INFINITE, WaitForSingleObject,
        };

        // The payload path is interpolated into the command line we
        // hand to `ShellExecuteExW`, surrounded by `"`. A path that
        // itself contains a `"` would break out of the quoting; one
        // that ends in `\` would cause the closing quote to be
        // escaped (Win32 argv parsing treats `\"` as a literal `"`
        // when preceded by an odd number of backslashes). Refuse
        // both rather than risk argv smuggling — the unprivileged
        // parent picks the path under `temp_dir`, so a poisoned
        // `TEMP`/`USERPROFILE` is the only way these characters can
        // appear and it would already indicate environment tampering.
        let display = payload.display().to_string();
        if display.chars().any(|c| c == '"' || c.is_control()) {
            bail!(
                "refusing to elevate: payload path `{}` contains a quote or control character",
                display
            );
        }
        if display.ends_with('\\') {
            bail!(
                "refusing to elevate: payload path `{}` ends with `\\` (would escape the closing quote)",
                display
            );
        }
        let identity = expected.identity.encode();
        for arg in [&expected.digest_hex, &identity] {
            if arg.chars().any(|c| c == '"' || c.is_control()) {
                bail!("refusing to elevate: payload verifier argument contains unsafe characters");
            }
        }

        let verb = to_wide("runas");
        let exe_w = to_wide(exe);
        let params: PathBuf = format!(
            "__apply-elevated \"{display}\" \"{}\" \"{identity}\"",
            expected.digest_hex
        )
        .into();
        let params_w = to_wide(&params);

        let mut info: SHELLEXECUTEINFOW = unsafe { std::mem::zeroed() };
        info.cbSize = std::mem::size_of::<SHELLEXECUTEINFOW>() as u32;
        info.fMask = SEE_MASK_NOCLOSEPROCESS;
        info.lpVerb = PCWSTR(verb.as_ptr());
        info.lpFile = PCWSTR(exe_w.as_ptr());
        info.lpParameters = PCWSTR(params_w.as_ptr());
        info.nShow = SW_HIDE;
        if unsafe { ShellExecuteExW(&mut info) }.is_err() {
            let code = unsafe { GetLastError() };
            bail!("ShellExecuteExW(runas) failed: GetLastError = {code:?}");
        }
        if info.hProcess.is_invalid() {
            bail!("ShellExecuteExW returned no process handle");
        }
        let _ = unsafe { WaitForSingleObject(info.hProcess, INFINITE) };
        let mut code: u32 = 0;
        let exit_res = unsafe { GetExitCodeProcess(info.hProcess, &mut code) };
        let _ = unsafe { CloseHandle(info.hProcess) };
        exit_res.map_err(|e| anyhow::anyhow!("GetExitCodeProcess failed: {e}"))?;
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
    fn set_owner_to_alien_uid_fails_when_not_root() {
        // The non-root test process cannot chown a file to a different
        // owner: the real `lchown(2)` returns EPERM. A function-body
        // mutation that swaps the implementation for `Ok(())` would
        // silently skip the syscall and pass — this test pins that the
        // real call is executed by demanding it fails.
        //
        // Skipped if the test happens to be running as root (CI runners
        // sometimes do): in that case EPERM is not the kernel's answer.
        #[allow(unsafe_code)]
        let euid = unsafe { libc::geteuid() };
        if euid == 0 {
            return;
        }
        let d = temp("alien");
        let file = d.join("payload");
        fs::write(&file, "hi").unwrap();
        // Pick a uid/gid that almost certainly isn't us. `nobody`/`nogroup`
        // is conventionally 65534 on Linux, 4294967294 (u32::MAX-1) on
        // macOS — in either case we are not them, so chown'ing to it
        // from an unprivileged process must EPERM.
        let alien = 65534;
        let owner = OwnerId::Posix {
            uid: alien,
            gid: alien,
        };
        let err = set_owner(&file, &owner).expect_err("unprivileged chown to alien uid must EPERM");
        assert!(
            format!("{err:#}").contains("lchown"),
            "expected lchown context in error, got: {err:#}",
        );
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
