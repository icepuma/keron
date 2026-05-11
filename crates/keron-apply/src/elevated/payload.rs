//! JSON-over-tempfile contract between the parent and the elevated
//! child. The parent serialises the elevated subset of the [`Plan`]
//! plus the calling user's identity, writes it to a 0600-mode file
//! in the system temp dir, and passes the path to the child via
//! `keron __apply-elevated <payload-path>`. The child reads it once,
//! applies in order, and `chown`s each created path back.
//!
//! Order is contractual: `changes` is applied verbatim in `Vec`
//! order. The parent's planner is the single source of truth for
//! sequencing; the child never re-sorts or parallelises.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::plan::{Plan, ResourceChange};

/// Identity of the user that invoked the unprivileged keron process.
/// The elevated child uses this to chown each created path back, so
/// the final filesystem state matches what an unprivileged user
/// would have produced if they had the rights.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum OwnerId {
    /// POSIX numeric ids. Captured by stat-ing the entry path the
    /// user passed to `keron apply <path>` (a file/dir we know they
    /// own) — `std::os::unix::fs::MetadataExt::uid()` /  `gid()`.
    Posix { uid: u32, gid: u32 },
    /// Windows owner SID in the `ConvertSidToStringSidW` form
    /// (`"S-1-5-21-..."`). Round-trips losslessly through
    /// `ConvertStringSidToSidW` in the elevated child.
    Windows { sid: String },
}

/// The wire payload. `changes` is applied in iteration order — see
/// the module doc for the ordering contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ElevatedPayload {
    /// Bumped when the wire format changes incompatibly. The child
    /// refuses to apply a payload with an unknown `version`.
    pub version: u32,
    pub owner: OwnerId,
    pub changes: Vec<ResourceChange>,
}

pub const PAYLOAD_VERSION: u32 = 1;

/// Owns the lifecycle of the tempfile: removes it on `Drop`. The
/// parent passes [`TempPayload::path`] to the elevated child and
/// keeps the handle alive until the child exits, so a child crash
/// can't leak the payload on disk.
pub struct TempPayload {
    path: PathBuf,
}

impl TempPayload {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for TempPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TempPayload")
            .field("path", &self.path)
            .finish()
    }
}

impl Drop for TempPayload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Serialise `plan` + `owner` to a fresh tempfile under
/// `std::env::temp_dir()`. The file is created mode 0600 on Unix so
/// no other local user can read the manifest while the elevated
/// child is starting up.
///
/// # Errors
/// Errors when the tempfile can't be created or JSON serialisation
/// fails.
pub fn write_payload(plan: &Plan, owner: &OwnerId) -> Result<TempPayload> {
    let dir = std::env::temp_dir();
    let path = dir.join(format!(
        "keron-elevated-{}-{}.json",
        std::process::id(),
        rand_suffix()
    ));
    let payload = ElevatedPayload {
        version: PAYLOAD_VERSION,
        owner: owner.clone(),
        changes: plan.changes.clone(),
    };
    let json =
        serde_json::to_vec_pretty(&payload).context("serializing elevated payload to JSON")?;
    write_secure(&path, &json)
        .with_context(|| format!("writing payload to `{}`", path.display()))?;
    Ok(TempPayload { path })
}

#[cfg(unix)]
fn write_secure(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn write_secure(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn rand_suffix() -> u64 {
    // Time-based unique-enough suffix; collision means `create_new`
    // errs and the parent surfaces it. Avoids pulling in a RNG crate.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// Capture the calling user's identity for embedding in the payload.
/// On Unix we stat a known-good file (the `current_exe` is fine since
/// it's readable; cargo-installed binaries land in a user-owned
/// dir). On Windows we read the current process token's user SID.
///
/// # Errors
/// Errors when the underlying syscalls fail.
pub fn capture_owner() -> Result<OwnerId> {
    #[cfg(unix)]
    {
        capture_owner_unix()
    }
    #[cfg(windows)]
    {
        capture_owner_windows()
    }
    #[cfg(not(any(unix, windows)))]
    {
        anyhow::bail!("elevated rights flow is not supported on this platform")
    }
}

#[cfg(unix)]
fn capture_owner_unix() -> Result<OwnerId> {
    use std::os::unix::fs::MetadataExt;
    // Prefer SUDO_UID/GID if a script plumbed it in (direct-sudo is
    // refused at the entry point, but tests/oddities can set it).
    if let (Ok(uid), Ok(gid)) = (std::env::var("SUDO_UID"), std::env::var("SUDO_GID"))
        && let (Ok(uid), Ok(gid)) = (uid.parse(), gid.parse())
    {
        return Ok(OwnerId::Posix { uid, gid });
    }
    // No libc in the workspace, so stat the cwd as a stand-in for
    // `geteuid()` — the CWD belongs to the calling user in any
    // reasonable shell, and is correct even when the binary itself
    // is root-owned (system-wide install).
    let probe = std::env::current_dir().context("locating the calling user via cwd")?;
    let meta = fs::metadata(&probe)
        .with_context(|| format!("stat-ing `{}` for uid/gid", probe.display()))?;
    Ok(OwnerId::Posix {
        uid: meta.uid(),
        gid: meta.gid(),
    })
}

#[cfg(windows)]
fn capture_owner_windows() -> Result<OwnerId> {
    let sid = crate::elevated::chown::windows::current_user_sid_string()
        .context("capturing current process SID")?;
    Ok(OwnerId::Windows { sid })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Action, ResourceChange, ResourceKind, ResourceState};
    use proptest::prelude::*;
    use std::path::PathBuf;

    fn sample_change(addr: &str) -> ResourceChange {
        ResourceChange {
            address: addr.into(),
            kind: ResourceKind::Symlink,
            action: Action::Create,
            before: None,
            after: Some(ResourceState::Symlink {
                from: PathBuf::from(addr),
                to: PathBuf::from(format!("{addr}-target")),
            }),
            requires_elevation: true,
        }
    }

    #[test]
    fn payload_round_trips_through_json() {
        let plan = Plan {
            changes: vec![sample_change("/etc/a"), sample_change("/etc/b")],
        };
        let owner = OwnerId::Posix {
            uid: 1000,
            gid: 1000,
        };
        let tempfile = write_payload(&plan, &owner).unwrap();
        let bytes = fs::read(tempfile.path()).unwrap();
        let decoded: ElevatedPayload = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded.version, PAYLOAD_VERSION);
        assert_eq!(decoded.changes.len(), 2);
        assert_eq!(decoded.changes[0].address, "/etc/a");
        assert_eq!(decoded.changes[1].address, "/etc/b");
        let OwnerId::Posix { uid, gid } = decoded.owner else {
            panic!("expected Posix owner");
        };
        assert_eq!((uid, gid), (1000, 1000));
    }

    #[test]
    fn temp_payload_removes_file_on_drop() {
        let plan = Plan {
            changes: vec![sample_change("/etc/x")],
        };
        let owner = OwnerId::Posix {
            uid: 1000,
            gid: 1000,
        };
        let path = {
            let tempfile = write_payload(&plan, &owner).unwrap();
            tempfile.path().to_path_buf()
        };
        assert!(!path.exists(), "payload must be removed on drop");
    }

    #[cfg(unix)]
    #[test]
    fn payload_file_is_mode_0600_on_unix() {
        use std::os::unix::fs::MetadataExt;
        let plan = Plan {
            changes: vec![sample_change("/etc/y")],
        };
        let owner = OwnerId::Posix {
            uid: 1000,
            gid: 1000,
        };
        let tempfile = write_payload(&plan, &owner).unwrap();
        let mode = fs::metadata(tempfile.path()).unwrap().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "payload should be owner-only: {mode:o}"
        );
    }

    proptest! {
        #[test]
        fn payload_round_trip_preserves_order(
            count in 0usize..16,
            seed in any::<u32>(),
        ) {
            let changes: Vec<_> = (0..count)
                .map(|i| sample_change(&format!("/etc/r{i}-{seed}")))
                .collect();
            let plan = Plan { changes: changes.clone() };
            let owner = OwnerId::Posix { uid: 1000, gid: 1000 };
            let tempfile = write_payload(&plan, &owner).unwrap();
            let bytes = fs::read(tempfile.path()).unwrap();
            let decoded: ElevatedPayload = serde_json::from_slice(&bytes).unwrap();
            let decoded_addrs: Vec<_> = decoded.changes.iter().map(|c| c.address.clone()).collect();
            let original_addrs: Vec<_> = changes.iter().map(|c| c.address.clone()).collect();
            prop_assert_eq!(decoded_addrs, original_addrs);
        }
    }
}
