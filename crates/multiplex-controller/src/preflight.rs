//! Preflight media verification and controller→client media push.
//!
//! Flow: the operator hits "Preflight" → we stat every unique file the show
//! references (locally, off the UI thread) → broadcast `MEDIA_CHECK` with
//! each file's relative path + size → each client stats its own copies and
//! replies `MEDIA_REPORT` → the roster shows ok/missing/mismatch per client →
//! "Push missing" streams the bad files to that client over the existing
//! WebSocket as framed binary chunks.
//!
//! This is a filename + size check, not a content hash: hashing every media
//! file on every client on every preflight doesn't scale with library size.
//! A file actually being pushed still gets a SHA-256 computed at push time
//! and verified by the client after transfer, so corruption in transit is
//! still caught — preflight just no longer pays that cost for files that are
//! already correctly in place.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use multiplex_shared::protocol::{
    ControllerMsg, MediaCheck, MediaFileSpec, MediaFileStatus, MediaPushBegin, MediaPushEnd,
};
use multiplex_shared::show::ShowFile;
use multiplex_shared::{hashing, transfer};

use crate::server::{broadcast, client_queue, log};
use crate::state::{Outgoing, SharedState};
use crate::util::expand_tilde;

/// Monotonic transfer ids, unique within this controller process.
static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a transfer id (shared with the update pusher so media and update
/// chunks can never collide on the wire).
pub fn next_transfer_id() -> u64 {
    NEXT_TRANSFER_ID.fetch_add(1, Ordering::Relaxed)
}

/// Stat every unique file the show references. Returns the specs for files
/// that exist locally plus the rel paths that are missing on the controller
/// itself (those can't be checked or pushed and deserve a loud log line).
pub fn scan_show_media(show: &ShowFile, media_root: &Path) -> (Vec<MediaFileSpec>, Vec<PathBuf>) {
    let mut seen = HashSet::new();
    let mut specs = Vec::new();
    let mut missing = Vec::new();
    for cue in &show.cues {
        if !seen.insert(cue.file.clone()) {
            continue;
        }
        let full = media_root.join(&cue.file);
        match std::fs::metadata(&full) {
            Ok(meta) => specs.push(MediaFileSpec { rel_path: cue.file.clone(), size: meta.len() }),
            Err(_) => missing.push(cue.file.clone()),
        }
    }
    (specs, missing)
}

/// Kick off a full preflight: scan locally, then ask every client to verify.
/// Returns immediately; results arrive as MEDIA_REPORT messages.
pub fn start_preflight(state: &SharedState) {
    {
        let mut s = state.lock().unwrap();
        if s.preflight_running || s.show.is_none() {
            return;
        }
        s.preflight_running = true;
        // Stale reports would read as fresh results.
        for row in s.clients.values_mut() {
            row.preflight.clear();
        }
    }
    let state = state.clone();
    tokio::spawn(async move {
        let (show, media_root) = {
            let s = state.lock().unwrap();
            let Some(show) = s.show.clone() else { return };
            let root = expand_tilde(&show.show.media_root);
            (show, root)
        };
        let result =
            tokio::task::spawn_blocking(move || scan_show_media(&show, &media_root)).await;
        let (specs, missing) = result.unwrap_or_default();
        for rel in &missing {
            log(&state, format!("preflight: {} missing on CONTROLLER", rel.display()));
        }
        {
            let mut s = state.lock().unwrap();
            s.local_media = Some(specs.clone());
            s.preflight_running = false;
        }
        log(&state, format!("preflight: scanned {} files, checking clients…", specs.len()));
        broadcast(&state, ControllerMsg::MediaCheck(MediaCheck { files: specs }));
    });
}

/// Stream every file the last preflight marked missing/mismatched on
/// `client_id`. Sequential per client; chunks ride the client's outbound
/// queue with backpressure (`send().await`, not `try_send`).
pub fn push_missing_to(state: &SharedState, client_id: String) {
    let state = state.clone();
    tokio::spawn(async move {
        let Some(queue) = client_queue(&state, &client_id) else { return };
        let (to_send, media_root) = {
            let s = state.lock().unwrap();
            let Some(specs) = s.local_media.clone() else { return };
            let Some(row) = s.clients.get(&client_id) else { return };
            let root = s
                .show
                .as_ref()
                .map(|sh| expand_tilde(&sh.show.media_root))
                .unwrap_or_default();
            let to_send: Vec<MediaFileSpec> = specs
                .into_iter()
                .filter(|spec| {
                    !matches!(row.preflight.get(&spec.rel_path), Some(MediaFileStatus::Ok))
                })
                .collect();
            (to_send, root)
        };
        if to_send.is_empty() {
            log(&state, format!("push: nothing to send to {client_id}"));
            return;
        }
        for spec in to_send {
            let transfer_id = next_transfer_id();
            {
                let mut s = state.lock().unwrap();
                if let Some(row) = s.clients.get_mut(&client_id) {
                    row.push_progress = Some((spec.rel_path.clone(), 0, spec.size));
                }
            }
            log(
                &state,
                format!("pushing {} ({} bytes) → {client_id}", spec.rel_path.display(), spec.size),
            );
            if let Err(e) = push_one(&queue, &media_root, &spec, transfer_id).await {
                log(&state, format!("push {} failed: {e}", spec.rel_path.display()));
                let mut s = state.lock().unwrap();
                if let Some(row) = s.clients.get_mut(&client_id) {
                    row.push_progress = None;
                }
                break; // queue is probably dead; the client will re-report
            }
        }
    });
}

async fn push_one(
    queue: &tokio::sync::mpsc::Sender<Outgoing>,
    media_root: &Path,
    spec: &MediaFileSpec,
    transfer_id: u64,
) -> anyhow::Result<()> {
    use tokio::io::AsyncReadExt;

    // Preflight no longer hashes every file up front, so the file actually
    // being pushed gets hashed here — the client uses this to verify the
    // transfer landed intact.
    let full = media_root.join(&spec.rel_path);
    let hash = tokio::task::spawn_blocking(move || hashing::sha256_file(&full)).await??;

    queue
        .send(Outgoing::Msg(ControllerMsg::MediaPushBegin(MediaPushBegin {
            transfer_id,
            rel_path: spec.rel_path.clone(),
            size: spec.size,
            sha256_hex: hashing::to_hex(&hash),
        })))
        .await?;

    let mut file = tokio::fs::File::open(media_root.join(&spec.rel_path)).await?;
    let mut buf = vec![0u8; transfer::CHUNK_SIZE];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        queue
            .send(Outgoing::Chunk(transfer::encode_chunk(transfer_id, &buf[..n])))
            .await?;
    }

    queue
        .send(Outgoing::Msg(ControllerMsg::MediaPushEnd(MediaPushEnd { transfer_id })))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use multiplex_shared::show::{Cue, CueKind};

    fn cue(id: &str, file: &str) -> Cue {
        Cue {
            id: id.into(),
            name: id.into(),
            kind: CueKind::Video,
            file: PathBuf::from(file),
            color: None,
            fade_in_ms: 0,
            in_ms: 0,
            out_ms: None,
            loops: false,
            on_end: multiplex_shared::show::EndAction::default(),
            notes: None,
        }
    }

    #[test]
    fn scan_show_media_dedupes_and_reports_missing() {
        let dir = std::env::temp_dir().join("multiplex_preflight_test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("a.mp4"), b"content-a").unwrap();

        let mut show = ShowFile::new_empty("T");
        show.cues.push(cue("c1", "a.mp4"));
        show.cues.push(cue("c2", "a.mp4")); // duplicate file
        show.cues.push(cue("c3", "gone.mp4")); // missing locally

        let (specs, missing) = scan_show_media(&show, &dir);
        assert_eq!(specs.len(), 1, "duplicate files scanned once");
        assert_eq!(specs[0].rel_path, PathBuf::from("a.mp4"));
        assert_eq!(specs[0].size, 9);
        assert_eq!(missing, vec![PathBuf::from("gone.mp4")]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
