//! Controller-side auto-update: local update bundle, fleet push, and
//! controller self-update.
//!
//! Two independent, operator-triggered actions (see CLAUDE.md):
//!
//! 1. **Update controller** — when online, fetch the latest signed release
//!    (manifest + own binary + all client binaries), verify everything,
//!    stage the controller binary, and cache the client bundle into the
//!    local `updates/` directory. The operator then confirms the restart.
//!    A theatre with no internet skips this and drops a bundle into
//!    `updates/` by hand (USB stick).
//! 2. **Update fleet** — stream the right per-triple client binary from the
//!    local bundle to each out-of-date client over the existing WebSocket.
//!    Clients stage + verify; the operator then applies per client (or
//!    fleet-wide), which clients only honour while idle.
//!
//! The controller re-verifies every artifact's signature before pushing or
//! staging, so a tampered `updates/` directory is caught here, not on N
//! clients.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _, Result};
use cuemesh2_shared::protocol::{ControllerMsg, UpdatePushBegin, UpdatePushEnd};
use cuemesh2_shared::update::{self, Artifact, UpdateManifest};
use cuemesh2_shared::{hashing, transfer};

use crate::preflight::next_transfer_id;
use crate::server::{client_queue, log};
use crate::state::{ClientUpdate, Outgoing, SelfUpdate, SharedState};

/// This binary's compile-time target triple (emitted by `build.rs`).
pub const TARGET_TRIPLE: &str = env!("TARGET_TRIPLE");

/// This binary's semver.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Where release downloads come from. GitHub's `releases/latest/download/`
/// redirect serves flat asset names, which is why manifest `file` fields are
/// flat. Override with `CUEMESH_UPDATE_URL` (e.g. an intranet mirror).
fn release_base_url() -> String {
    std::env::var("CUEMESH_UPDATE_URL")
        .unwrap_or_else(|_| "https://github.com/drhmedia/cuemesh2/releases/latest/download".into())
}

/// The local update bundle: `updates/` next to the controller binary, or
/// `CUEMESH_UPDATE_BUNDLE`.
pub fn bundle_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CUEMESH_UPDATE_BUNDLE") {
        return PathBuf::from(dir);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("updates")))
        .unwrap_or_else(|| PathBuf::from("updates"))
}

/// (Re)load the local bundle manifest into state. Quiet when there simply is
/// no bundle; loud when a bundle exists but doesn't parse.
pub fn load_local_manifest(state: &SharedState) {
    let dir = bundle_dir();
    if !dir.join("manifest.toml").exists() {
        state.lock().unwrap().update_manifest = None;
        return;
    }
    match UpdateManifest::load(&dir) {
        Ok(m) => {
            log(
                &state.clone(),
                format!("update bundle v{} loaded from {}", m.version, dir.display()),
            );
            state.lock().unwrap().update_manifest = Some(m);
        }
        Err(e) => {
            state.lock().unwrap().update_manifest = None;
            log(
                state,
                format!("update bundle at {} is unusable: {e}", dir.display()),
            );
        }
    }
}

/// The bundle artifact that would upgrade this client, if any: right triple,
/// strictly newer version, not already mid-update.
pub fn available_for(manifest: &UpdateManifest, row: &crate::state::ClientRow) -> Option<Artifact> {
    if row.target_triple.is_empty() {
        return None;
    }
    if !update::is_newer(&manifest.version, &row.app_version) {
        return None;
    }
    manifest.clients.get(&row.target_triple).cloned()
}

/// Read one artifact from the local bundle and verify hash + signature.
/// Returns the bytes; every code path that ships or stages a binary goes
/// through here first.
fn read_verified(dir: &Path, artifact: &Artifact) -> Result<Vec<u8>> {
    let path = dir.join(&artifact.file);
    let data = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
    let sha = hashing::to_hex(&hashing::sha256_bytes(&data));
    if sha != artifact.sha256 {
        bail!(
            "{}: sha256 mismatch — bundle is corrupt or tampered",
            artifact.file
        );
    }
    update::verify_signature(&update::release_pubkey_b64(), &data, &artifact.signature)
        .with_context(|| format!("{}: signature verification failed", artifact.file))?;
    Ok(data)
}

/// Stream the bundle's client binary to one client. Runs as a task; the
/// outcome lands in the roster via UPDATE_PUSH_RESULT.
pub fn push_update_to(state: &SharedState, client_id: String) {
    let state = state.clone();
    tokio::spawn(async move {
        let (version, triple, artifact) = {
            let s = state.lock().unwrap();
            let Some(m) = s.update_manifest.as_ref() else {
                return;
            };
            let Some(row) = s.clients.get(&client_id) else {
                return;
            };
            let Some(a) = available_for(m, row) else {
                return;
            };
            (m.version.clone(), row.target_triple.clone(), a)
        };
        let Some(queue) = client_queue(&state, &client_id) else {
            return;
        };

        let dir = bundle_dir();
        let read_artifact = artifact.clone();
        let loaded = tokio::task::spawn_blocking(move || read_verified(&dir, &read_artifact)).await;
        let data = match loaded {
            Ok(Ok(d)) => d,
            Ok(Err(e)) => {
                log(&state, format!("update push → {client_id} aborted: {e:#}"));
                return;
            }
            Err(e) => {
                log(&state, format!("update push → {client_id} aborted: {e}"));
                return;
            }
        };

        {
            let mut s = state.lock().unwrap();
            if let Some(row) = s.clients.get_mut(&client_id) {
                row.update = ClientUpdate::Pushing;
            }
        }
        log(
            &state,
            format!(
                "pushing update v{version} ({} bytes) → {client_id}",
                data.len()
            ),
        );

        let transfer_id = next_transfer_id();
        let begin = ControllerMsg::UpdatePushBegin(UpdatePushBegin {
            transfer_id,
            target_triple: triple,
            version: version.clone(),
            size: data.len() as u64,
            sha256_hex: artifact.sha256.clone(),
            signature_b64: artifact.signature.clone(),
            min_gstreamer: artifact.min_gstreamer.clone(),
        });
        let sent = async {
            queue.send(Outgoing::Msg(begin)).await?;
            for chunk in data.chunks(transfer::CHUNK_SIZE) {
                queue
                    .send(Outgoing::Chunk(transfer::encode_chunk(transfer_id, chunk)))
                    .await?;
            }
            queue
                .send(Outgoing::Msg(ControllerMsg::UpdatePushEnd(UpdatePushEnd {
                    transfer_id,
                })))
                .await?;
            Ok::<_, tokio::sync::mpsc::error::SendError<Outgoing>>(())
        }
        .await;
        if sent.is_err() {
            // Queue died — the client dropped; its row is gone or will be.
            log(
                &state,
                format!("update push → {client_id}: connection lost"),
            );
        }
    });
}

/// Push to every connected client the bundle can upgrade.
pub fn update_fleet(state: &SharedState) {
    let targets: Vec<String> = {
        let s = state.lock().unwrap();
        let Some(m) = s.update_manifest.as_ref() else {
            return;
        };
        s.clients
            .values()
            .filter(|row| available_for(m, row).is_some())
            .filter(|row| !matches!(row.update, ClientUpdate::Pushing | ClientUpdate::Applying))
            .map(|row| row.client_id.clone())
            .collect()
    };
    if targets.is_empty() {
        log(
            state,
            "update fleet: every connected client is already current",
        );
        return;
    }
    for id in targets {
        push_update_to(state, id);
    }
}

/// Tell one client to apply its staged update (it refuses unless idle).
pub fn send_apply(state: &SharedState, client_id: &str) {
    if let Some(queue) = client_queue(state, client_id) {
        let _ = queue.try_send(Outgoing::Msg(ControllerMsg::ApplyUpdate));
    }
}

/// Tell every client with a staged update to apply.
pub fn apply_fleet(state: &SharedState) {
    let staged: Vec<String> = {
        let s = state.lock().unwrap();
        s.clients
            .values()
            .filter(|row| matches!(row.update, ClientUpdate::Staged(_)))
            .map(|row| row.client_id.clone())
            .collect()
    };
    for id in staged {
        send_apply(state, &id);
    }
}

// ─── Controller self-update ────────────────────────────────────────────────

fn set_self_update(state: &SharedState, v: SelfUpdate) {
    state.lock().unwrap().self_update = v;
}

/// Staged replacement for the controller binary: `<current_exe>.new`.
fn staged_controller_path() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current_exe")?;
    let mut name = exe
        .file_name()
        .and_then(|n| n.to_str())
        .context("executable has no utf-8 file name")?
        .to_owned();
    name.push_str(".new");
    Ok(exe.with_file_name(name))
}

/// "Update controller": fetch the latest release, verify, stage our own
/// binary, and cache the full client bundle for offline fleet pushes.
/// Ends in `SelfUpdate::ReadyToRestart` — the operator confirms the restart.
pub fn start_self_update(state: &SharedState) {
    {
        let s = state.lock().unwrap();
        if matches!(s.self_update, SelfUpdate::Working(_)) {
            return;
        }
    }
    let state = state.clone();
    tokio::spawn(async move {
        set_self_update(&state, SelfUpdate::Working("checking…".into()));
        match self_update_inner(&state).await {
            Ok(Some(version)) => {
                log(
                    &state,
                    format!("controller v{version} staged; restart to apply"),
                );
                set_self_update(&state, SelfUpdate::ReadyToRestart(version));
            }
            Ok(None) => {
                log(&state, format!("already up to date (v{APP_VERSION})"));
                set_self_update(&state, SelfUpdate::Idle);
            }
            Err(e) => {
                log(&state, format!("controller update failed: {e:#}"));
                set_self_update(&state, SelfUpdate::Failed(format!("{e:#}")));
            }
        }
    });
}

async fn fetch(client: &reqwest::Client, url: &str) -> Result<Vec<u8>> {
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url}: HTTP {}", resp.status());
    }
    Ok(resp
        .bytes()
        .await
        .with_context(|| format!("read {url}"))?
        .to_vec())
}

async fn self_update_inner(state: &SharedState) -> Result<Option<String>> {
    let base = release_base_url();
    let http = reqwest::Client::builder()
        .user_agent(concat!("cuemesh2-controller/", env!("CARGO_PKG_VERSION")))
        .build()?;

    let manifest_bytes = fetch(&http, &format!("{base}/manifest.toml")).await?;
    let manifest = UpdateManifest::parse(std::str::from_utf8(&manifest_bytes)?)?;
    if !update::is_newer(&manifest.version, APP_VERSION) {
        return Ok(None);
    }
    let version = manifest.version.clone();
    let pubkey = update::release_pubkey_b64();

    // Our own binary first — no point caching a client bundle we can't run
    // alongside.
    let own = manifest.controllers.get(TARGET_TRIPLE).with_context(|| {
        format!("release v{version} has no controller build for {TARGET_TRIPLE}")
    })?;
    set_self_update(
        state,
        SelfUpdate::Working(format!("downloading controller v{version}…")),
    );
    let own_bytes = fetch(&http, &format!("{base}/{}", own.file)).await?;
    verify_bytes(&pubkey, &own_bytes, own)?;

    // Cache every client artifact so fleets update offline later.
    let dir = bundle_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let total = manifest.clients.len();
    for (i, (triple, artifact)) in manifest.clients.iter().enumerate() {
        set_self_update(
            state,
            SelfUpdate::Working(format!(
                "downloading client bundle {}/{total} ({triple})…",
                i + 1
            )),
        );
        let bytes = fetch(&http, &format!("{base}/{}", artifact.file)).await?;
        verify_bytes(&pubkey, &bytes, artifact)?;
        std::fs::write(dir.join(&artifact.file), &bytes)
            .with_context(|| format!("write {}", artifact.file))?;
    }
    std::fs::write(dir.join("manifest.toml"), &manifest_bytes).context("write manifest")?;
    load_local_manifest(state);

    // Stage our own replacement last, once everything else has landed.
    let staged = staged_controller_path()?;
    std::fs::write(&staged, &own_bytes).with_context(|| format!("write {}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(Some(version))
}

fn verify_bytes(pubkey: &str, data: &[u8], artifact: &Artifact) -> Result<()> {
    let sha = hashing::to_hex(&hashing::sha256_bytes(data));
    if sha != artifact.sha256 {
        bail!("{}: sha256 mismatch", artifact.file);
    }
    update::verify_signature(pubkey, data, &artifact.signature)
        .with_context(|| format!("{}: signature verification failed", artifact.file))?;
    Ok(())
}

/// Operator-confirmed restart into the staged controller binary.
pub fn restart_into_staged(state: &SharedState) {
    let staged = match staged_controller_path() {
        Ok(p) if p.exists() => p,
        _ => {
            log(state, "no staged controller update to restart into");
            return;
        }
    };
    log(state, "restarting into updated controller");
    if let Err(e) = self_replace::self_replace(&staged).context("self-replace") {
        log(state, format!("controller update apply failed: {e:#}"));
        set_self_update(state, SelfUpdate::Failed(format!("{e:#}")));
        return;
    }
    let _ = std::fs::remove_file(&staged);
    restart_self(state);
}

/// Re-exec the (now replaced) current executable with the same arguments.
fn restart_self(state: &SharedState) {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            log(state, format!("restart failed: {e}"));
            return;
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let err = std::process::Command::new(&exe).args(&args).exec();
        log(state, format!("restart exec failed: {err}"));
    }
    #[cfg(not(unix))]
    {
        match std::process::Command::new(&exe).args(&args).spawn() {
            Ok(_) => std::process::exit(0),
            Err(e) => log(state, format!("restart spawn failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ClientRow;
    use cuemesh2_shared::protocol::ClientState;
    use tokio::sync::mpsc;

    fn row(version: &str, triple: &str) -> ClientRow {
        let (tx, _rx) = mpsc::channel(1);
        ClientRow {
            client_id: "c1".into(),
            name: "n".into(),
            addr: "a".into(),
            app_version: version.into(),
            target_triple: triple.into(),
            update: Default::default(),
            state: ClientState::Idle,
            current_cue: None,
            position_ms: 0,
            offset_ms: None,
            last_drift_ms: None,
            last_heartbeat_ms: 0,
            preflight: Default::default(),
            push_progress: None,
            outbound: tx,
        }
    }

    fn manifest() -> UpdateManifest {
        UpdateManifest::parse(
            r#"
            version = "0.2.0"
            [clients.x86_64-unknown-linux-gnu]
            file = "cuemesh2-client-x86_64-unknown-linux-gnu"
            sha256 = "ab"
            signature = "c2ln"
            "#,
        )
        .unwrap()
    }

    #[test]
    fn available_only_for_older_matching_triple() {
        let m = manifest();
        assert!(available_for(&m, &row("0.1.0", "x86_64-unknown-linux-gnu")).is_some());
        // Unknown version (old client) still gets offered the update.
        assert!(available_for(&m, &row("", "x86_64-unknown-linux-gnu")).is_some());
        // Same or newer version: nothing to do.
        assert!(available_for(&m, &row("0.2.0", "x86_64-unknown-linux-gnu")).is_none());
        assert!(available_for(&m, &row("0.3.0", "x86_64-unknown-linux-gnu")).is_none());
        // No artifact for this platform.
        assert!(available_for(&m, &row("0.1.0", "aarch64-apple-darwin")).is_none());
        // Pre-update client that never reported a triple.
        assert!(available_for(&m, &row("0.1.0", "")).is_none());
    }
}
