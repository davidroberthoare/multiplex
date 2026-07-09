//! Client-side update staging and apply.
//!
//! An update arrives over the WebSocket as a signed binary (see
//! `cuemesh2_shared::update`). It is **staged** next to the running
//! executable (`<exe>.new` plus a `<exe>.new.meta` JSON sidecar) and only
//! **applied** — atomic self-replace, then re-exec — on an explicit
//! operator `APPLY_UPDATE`, or at the next clean startup. Verification
//! (size, SHA-256, ed25519 signature, target triple, downgrade guard,
//! GStreamer floor) runs at stage time and again before every apply, since
//! the staged file sat on disk unattended in between.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use serde::{Deserialize, Serialize};

use cuemesh2_shared::protocol::UpdatePushBegin;
use cuemesh2_shared::{hashing, update};

/// This binary's compile-time target triple (emitted by `build.rs`).
pub const TARGET_TRIPLE: &str = env!("TARGET_TRIPLE");

/// This binary's semver.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything needed to re-verify a staged binary later.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StagedMeta {
    pub version: String,
    pub size: u64,
    pub sha256_hex: String,
    pub signature_b64: String,
}

/// Path of the staged replacement binary: `<current_exe>.new`.
pub fn staged_bin_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let mut name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .context("executable has no utf-8 file name")?
        .to_owned();
    name.push_str(".new");
    Ok(exe.with_file_name(name))
}

fn meta_path_of(bin: &Path) -> PathBuf {
    let mut name = bin
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("update")
        .to_owned();
    name.push_str(".meta");
    bin.with_file_name(name)
}

/// Checks that don't need the file contents: triple, downgrade guard,
/// GStreamer floor. Run before accepting a transfer so a doomed push fails
/// fast instead of after streaming megabytes.
pub fn precheck(begin: &UpdatePushBegin, gst_runtime: (u32, u32)) -> Result<()> {
    if begin.target_triple != TARGET_TRIPLE {
        bail!(
            "wrong platform: update is {}, this client is {}",
            begin.target_triple,
            TARGET_TRIPLE
        );
    }
    if !update::is_newer(&begin.version, APP_VERSION) {
        bail!(
            "not newer: update is {}, running {}",
            begin.version,
            APP_VERSION
        );
    }
    if let Some(floor) = &begin.min_gstreamer {
        let Some((maj, min)) = parse_major_minor(floor) else {
            bail!("malformed min_gstreamer {floor:?}");
        };
        if gst_runtime < (maj, min) {
            bail!(
                "needs GStreamer {maj}.{min}, runtime is {}.{} — manual reinstall required",
                gst_runtime.0,
                gst_runtime.1
            );
        }
    }
    Ok(())
}

fn parse_major_minor(v: &str) -> Option<(u32, u32)> {
    let (maj, rest) = v.split_once('.')?;
    let min = rest.split('.').next()?;
    Some((maj.trim().parse().ok()?, min.trim().parse().ok()?))
}

/// Verify a fully-received binary against its metadata: size, SHA-256, and
/// the release signature. `meta.version` freshness is the caller's concern
/// (checked in `precheck` at stage time and `take_staged` at apply time).
fn verify_file(bin: &Path, meta: &StagedMeta) -> Result<()> {
    let data = std::fs::read(bin).context("read staged binary")?;
    if data.len() as u64 != meta.size {
        bail!("size mismatch: got {} want {}", data.len(), meta.size);
    }
    let sha = hashing::to_hex(&hashing::sha256_bytes(&data));
    if sha != meta.sha256_hex {
        bail!("sha256 mismatch");
    }
    update::verify_signature(&update::release_pubkey_b64(), &data, &meta.signature_b64)
        .context("signature verification failed")?;
    Ok(())
}

/// Move a verified download into the staged slot and write its sidecar.
/// `tmp` must already have passed `verify_file`-equivalent checks; this
/// re-runs them against the final path to close the gap between "verified"
/// and "staged".
pub fn stage(tmp: &Path, meta: &StagedMeta) -> Result<()> {
    let bin = staged_bin_path()?;
    std::fs::rename(tmp, &bin).context("move into staged slot")?;
    verify_file(&bin, meta)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
    }
    let sidecar = meta_path_of(&bin);
    std::fs::write(&sidecar, serde_json::to_vec_pretty(meta)?)?;
    Ok(())
}

/// The staged version, if a staged update exists (sidecar only — cheap; used
/// for status reporting, not for trust).
pub fn staged_version() -> Option<String> {
    let bin = staged_bin_path().ok()?;
    let meta: StagedMeta = serde_json::from_slice(&std::fs::read(meta_path_of(&bin)).ok()?).ok()?;
    bin.exists().then_some(meta.version)
}

/// Fully re-verify the staged update and return its path if it's good and
/// still newer than the running binary. On any failure the staged files are
/// deleted (they're either corrupt or stale) and the error says why.
pub fn take_staged() -> Result<Option<PathBuf>> {
    let bin = staged_bin_path()?;
    if !bin.exists() {
        return Ok(None);
    }
    let result = (|| -> Result<()> {
        let meta: StagedMeta = serde_json::from_slice(
            &std::fs::read(meta_path_of(&bin)).context("read staged metadata")?,
        )
        .context("parse staged metadata")?;
        if !update::is_newer(&meta.version, APP_VERSION) {
            bail!(
                "staged {} is not newer than running {}",
                meta.version,
                APP_VERSION
            );
        }
        verify_file(&bin, &meta)
    })();
    match result {
        Ok(()) => Ok(Some(bin)),
        Err(e) => {
            discard_staged();
            Err(e)
        }
    }
}

/// Delete the staged binary and sidecar, if present.
pub fn discard_staged() {
    if let Ok(bin) = staged_bin_path() {
        let _ = std::fs::remove_file(meta_path_of(&bin));
        let _ = std::fs::remove_file(&bin);
    }
}

/// Swap the staged binary into place and restart as the new version.
/// Only returns on error.
pub fn apply_and_restart(staged: &Path) -> Result<()> {
    self_replace::self_replace(staged).context("self-replace")?;
    discard_staged();
    restart_self()
}

/// Re-exec the (now replaced) current executable with the same arguments.
/// On unix this replaces the process image; on Windows it spawns a child and
/// exits, because a running PE cannot exec over itself.
fn restart_self() -> Result<()> {
    let exe = std::env::current_exe().context("current_exe")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let err = std::process::Command::new(&exe).args(&args).exec();
        bail!("exec failed: {err}");
    }
    #[cfg(not(unix))]
    {
        std::process::Command::new(&exe)
            .args(&args)
            .spawn()
            .context("spawn replacement")?;
        std::process::exit(0);
    }
}

/// Startup hook: if a valid staged update is waiting, apply it before doing
/// anything else. Invalid/stale staged files are discarded with a warning.
pub fn apply_staged_at_startup() {
    match take_staged() {
        Ok(Some(bin)) => {
            tracing::info!("applying staged update at startup");
            if let Err(e) = apply_and_restart(&bin) {
                tracing::warn!("staged update apply failed: {e:#} — continuing on current version");
            }
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("discarded staged update: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(version: &str, triple: &str, min_gst: Option<&str>) -> UpdatePushBegin {
        UpdatePushBegin {
            transfer_id: 1,
            target_triple: triple.into(),
            version: version.into(),
            size: 0,
            sha256_hex: String::new(),
            signature_b64: String::new(),
            min_gstreamer: min_gst.map(Into::into),
        }
    }

    #[test]
    fn precheck_rejects_wrong_triple() {
        let err = precheck(&begin("99.0.0", "wasm32-unknown-unknown", None), (1, 24))
            .unwrap_err()
            .to_string();
        assert!(err.contains("wrong platform"), "{err}");
    }

    #[test]
    fn precheck_rejects_downgrade_and_same_version() {
        for v in ["0.0.1", APP_VERSION] {
            let err = precheck(&begin(v, TARGET_TRIPLE, None), (1, 24))
                .unwrap_err()
                .to_string();
            assert!(err.contains("not newer"), "{err}");
        }
    }

    #[test]
    fn precheck_enforces_gstreamer_floor() {
        assert!(precheck(&begin("99.0.0", TARGET_TRIPLE, Some("1.18")), (1, 24)).is_ok());
        let err = precheck(&begin("99.0.0", TARGET_TRIPLE, Some("1.26")), (1, 24))
            .unwrap_err()
            .to_string();
        assert!(err.contains("manual reinstall"), "{err}");
        assert!(precheck(&begin("99.0.0", TARGET_TRIPLE, Some("nonsense")), (1, 24)).is_err());
    }

    #[test]
    fn parse_major_minor_variants() {
        assert_eq!(parse_major_minor("1.18"), Some((1, 18)));
        assert_eq!(parse_major_minor("1.24.13"), Some((1, 24)));
        assert_eq!(parse_major_minor("2"), None);
        assert_eq!(parse_major_minor("a.b"), None);
    }
}
