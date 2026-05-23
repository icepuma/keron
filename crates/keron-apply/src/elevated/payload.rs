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

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    expected: PayloadExpectation,
}

impl TempPayload {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub const fn expected(&self) -> &PayloadExpectation {
        &self.expected
    }
}

impl std::fmt::Debug for TempPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TempPayload")
            .field("path", &self.path)
            .field("expected", &self.expected)
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
    let expected = PayloadExpectation::capture(&path, &json)
        .with_context(|| format!("capturing identity for `{}`", path.display()))?;
    Ok(TempPayload { path, expected })
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadExpectation {
    pub digest_hex: String,
    pub identity: PayloadIdentity,
}

impl PayloadExpectation {
    fn capture(path: &Path, bytes: &[u8]) -> Result<Self> {
        Ok(Self {
            digest_hex: digest_hex(bytes),
            identity: PayloadIdentity::capture(path)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadIdentity {
    #[cfg(unix)]
    Unix {
        dev: u64,
        ino: u64,
        uid: u32,
        gid: u32,
        mode: u32,
        len: u64,
    },
    #[cfg(windows)]
    Windows { len: u64, modified_nanos: u128 },
}

impl PayloadIdentity {
    #[must_use]
    pub fn encode(&self) -> String {
        match self {
            #[cfg(unix)]
            Self::Unix {
                dev,
                ino,
                uid,
                gid,
                mode,
                len,
            } => format!("unix:{dev}:{ino}:{uid}:{gid}:{mode:o}:{len}"),
            #[cfg(windows)]
            Self::Windows {
                len,
                modified_nanos,
            } => {
                format!("windows:{len}:{modified_nanos}")
            }
        }
    }

    /// Decode the identity argument passed to the elevated child.
    ///
    /// # Errors
    ///
    /// Returns an error when the encoded value has an unknown platform
    /// tag, the wrong number of fields, or malformed numeric fields.
    pub fn decode(s: &str) -> Result<Self> {
        let parts = s.split(':').collect::<Vec<_>>();
        match parts.as_slice() {
            #[cfg(unix)]
            ["unix", dev, ino, uid, gid, mode, len] => Ok(Self::Unix {
                dev: dev.parse().context("parsing payload device id")?,
                ino: ino.parse().context("parsing payload inode")?,
                uid: uid.parse().context("parsing payload uid")?,
                gid: gid.parse().context("parsing payload gid")?,
                mode: u32::from_str_radix(mode, 8).context("parsing payload mode")?,
                len: len.parse().context("parsing payload length")?,
            }),
            #[cfg(windows)]
            ["windows", len, modified_nanos] => Ok(Self::Windows {
                len: len.parse().context("parsing payload length")?,
                modified_nanos: modified_nanos
                    .parse()
                    .context("parsing payload modification time")?,
            }),
            _ => bail!("invalid elevated payload identity `{s}`"),
        }
    }

    fn capture(path: &Path) -> Result<Self> {
        let meta = fs::metadata(path).context("stat elevated payload")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            Ok(Self::Unix {
                dev: meta.dev(),
                ino: meta.ino(),
                uid: meta.uid(),
                gid: meta.gid(),
                mode: meta.permissions().mode(),
                len: meta.len(),
            })
        }
        #[cfg(windows)]
        {
            use std::time::UNIX_EPOCH;
            let modified = meta
                .modified()
                .context("reading elevated payload modification time")?
                .duration_since(UNIX_EPOCH)
                .context("elevated payload modification time predates Unix epoch")?;
            Ok(Self::Windows {
                len: meta.len(),
                modified_nanos: modified.as_nanos(),
            })
        }
    }
}

pub fn read_verified(path: &Path, expected: &PayloadExpectation) -> Result<Vec<u8>> {
    let before = PayloadIdentity::capture(path)?;
    verify_identity(&before, expected)?;
    let bytes =
        fs::read(path).with_context(|| format!("reading elevated payload `{}`", path.display()))?;
    let after = PayloadIdentity::capture(path)?;
    verify_identity(&after, expected)?;
    let actual = digest_hex(&bytes);
    if actual != expected.digest_hex {
        bail!(
            "elevated payload digest mismatch: expected {}, got {actual}",
            expected.digest_hex
        );
    }
    Ok(bytes)
}

fn verify_identity(actual: &PayloadIdentity, expected: &PayloadExpectation) -> Result<()> {
    if actual != &expected.identity {
        bail!(
            "elevated payload metadata changed: expected {}, got {}",
            expected.identity.encode(),
            actual.encode()
        );
    }
    #[cfg(unix)]
    {
        let PayloadIdentity::Unix { mode, .. } = actual;
        let mode = *mode;
        // libc::S_IF* is u16 on macOS, u32 on Linux; the From is a no-op
        // on Linux but required for portability.
        #[allow(clippy::useless_conversion)]
        let file_type = mode & u32::from(libc::S_IFMT);
        #[allow(clippy::useless_conversion)]
        let s_ifreg = u32::from(libc::S_IFREG);
        if file_type != s_ifreg {
            bail!("elevated payload is not a regular file");
        }
        if mode & 0o077 != 0 {
            bail!(
                "elevated payload permissions are too broad (mode {:o}); expected private 0600",
                mode & 0o777
            );
        }
    }
    Ok(())
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
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
// Symmetric shape with `capture_owner_windows` which is fallible;
// keep the Result so `capture_owner` dispatches both arms uniformly.
#[allow(clippy::unnecessary_wraps)]
fn capture_owner_unix() -> Result<OwnerId> {
    // Platform FFI for the elevated-rights flow: the calling user is
    // the effective uid/gid of this (unprivileged) parent process.
    // Don't infer ownership from CWD — `cd /etc; keron apply ...`
    // would otherwise chown every created file to root. Don't read
    // SUDO_UID/SUDO_GID either: an attacker who can plant env vars
    // in the unprivileged parent (shell rc, wrapper script,
    // `env SUDO_UID=0 keron apply`) would otherwise direct the
    // root child to chown user files to arbitrary uids. The legit
    // SUDO_UID use case (direct `sudo keron apply`) is already
    // refused by `refuse_direct_elevation`.
    #[allow(unsafe_code)]
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    Ok(OwnerId::Posix { uid, gid })
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
            requires_force: false,
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

    #[test]
    fn rand_suffix_returns_nonzero_high_entropy_value() {
        // rand_suffix encodes nanos-since-epoch, which is on the order
        // of 1e18 in 2026 — orders of magnitude larger than the
        // function-body mutations `-> 0` and `-> 1`. One observation is
        // enough to distinguish.
        let s = rand_suffix();
        assert!(
            s > 1_000_000_000,
            "rand_suffix should encode nanos since epoch, got {s}",
        );
    }

    #[test]
    fn rand_suffix_varies_across_calls() {
        // Belt-and-braces: rand_suffix is meant to be probabilistically
        // unique, so 64 consecutive calls must produce at least two
        // distinct values. Catches both `-> 0` and `-> 1` mutations,
        // which would emit the same constant every call.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..64 {
            seen.insert(rand_suffix());
        }
        assert!(
            seen.len() > 1,
            "rand_suffix must vary across calls; got {} unique value(s)",
            seen.len(),
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
