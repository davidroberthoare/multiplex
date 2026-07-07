//! WebSocket client with reconnect loop, plus the command dispatcher that
//! turns incoming `ControllerMsg` values into media-engine calls.
//!
//! Design invariants (see CLAUDE.md "Resilience"):
//! - The media engine never waits on the network; a lost controller applies
//!   the show's dropout policy and playback of the current cue continues.
//! - Reconnection runs in the background with exponential backoff.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex as AsyncMutex};
use tokio_tungstenite::tungstenite::Message as WsMsg;

use cuemesh2_media::{fades, MediaEngine, MediaEvent, MediaKind};
use cuemesh2_shared::clock_sync::{correction_for, Correction, CorrectionParams, OffsetFilter};
use cuemesh2_shared::protocol::{
    ClientMsg, ControllerMsg, Envelope, Hello, Layer as WireLayer, MediaFileStatus,
    MediaPushProgress, MediaPushResult, MediaReport, MediaReportEntry, Ready, Status, SyncReply,
    PROTOCOL_VERSION,
};
use cuemesh2_shared::show::{CueKind, DropoutPolicy};
use cuemesh2_shared::{hashing, transfer};

use crate::state::{PlaybackState, SharedState};

pub struct ConnectionConfig {
    pub controller_url: String,
    pub client_id: String,
    pub name: String,
    pub media_root: PathBuf,
}

fn media_layer(l: WireLayer) -> cuemesh2_media::Layer {
    match l {
        WireLayer::A => cuemesh2_media::Layer::A,
        WireLayer::B => cuemesh2_media::Layer::B,
    }
}

fn other_layer(l: WireLayer) -> WireLayer {
    match l {
        WireLayer::A => WireLayer::B,
        WireLayer::B => WireLayer::A,
    }
}

pub async fn run(cfg: ConnectionConfig, state: SharedState, engine: MediaEngine) {
    spawn_media_event_pump(engine.clone(), state.clone());
    let cfg = Arc::new(cfg);
    let mut backoff_ms = 500u64;
    loop {
        // The UI may have picked a different controller (mDNS or manual).
        let url = {
            let mut s = state.lock().unwrap();
            let url = s.desired_url.clone().unwrap_or_else(|| cfg.controller_url.clone());
            s.controller_addr = url.clone();
            url
        };
        let was_connected = match connect_once(&url, &cfg, &state, &engine).await {
            Ok(_) => {
                log(&state, "connection closed");
                backoff_ms = 500;
                true
            }
            Err(e) => {
                log(&state, format!("connection error: {e}"));
                let was = state.lock().unwrap().connected;
                was
            }
        };
        state.lock().unwrap().connected = false;

        if was_connected {
            apply_dropout_policy(&state, &engine);
        }

        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        backoff_ms = (backoff_ms * 2).min(10_000);
    }
}

/// On losing the controller, do what the show file says (default: continue).
fn apply_dropout_policy(state: &SharedState, engine: &MediaEngine) {
    let (policy, fade_ms) = {
        let s = state.lock().unwrap();
        match &s.show {
            Some(show) => (show.dropout_policy, show.default_fade_ms),
            None => (DropoutPolicy::Continue, 1500),
        }
    };
    log(state, format!("controller lost; dropout policy: {policy:?}"));
    match policy {
        DropoutPolicy::Continue => {}
        DropoutPolicy::Freeze => engine.pause_all(),
        DropoutPolicy::Black => {
            let dur = Duration::from_millis(fade_ms as u64);
            fades::fade(engine, cuemesh2_media::Layer::A, 0.0, dur);
            fades::fade(engine, cuemesh2_media::Layer::B, 0.0, dur);
            let engine = engine.clone();
            let state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(dur).await;
                engine.stop_all();
                state.lock().unwrap().playback.state = PlaybackState::Black;
            });
        }
    }
}

/// An in-flight controller→client file transfer.
struct IncomingTransfer {
    rel_path: PathBuf,
    tmp_path: PathBuf,
    file: std::fs::File,
    expected_size: u64,
    expected_sha256_hex: String,
    received: u64,
    last_progress_sent: u64,
}

type Transfers = Arc<AsyncMutex<HashMap<u64, IncomingTransfer>>>;

async fn connect_once(
    url: &str,
    cfg: &Arc<ConnectionConfig>,
    state: &SharedState,
    engine: &MediaEngine,
) -> Result<()> {
    log(state, format!("connecting to {url}"));
    let (ws, _resp) = tokio_tungstenite::connect_async(url).await?;
    state.lock().unwrap().connected = true;
    log(state, "connected");

    let (mut sink, mut source) = ws.split();
    let (out_tx, mut out_rx) = mpsc::channel::<ClientMsg>(64);

    // Send HELLO.
    let hello = Envelope::new(
        now_utc_ms(),
        ClientMsg::Hello(Hello {
            client_id: cfg.client_id.clone(),
            name: cfg.name.clone(),
            protocol_version: PROTOCOL_VERSION,
        }),
    );
    sink.send(WsMsg::Text(serde_json::to_string(&hello)?)).await?;

    // Writer task.
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            let env = Envelope::new(now_utc_ms(), msg);
            let text = match serde_json::to_string(&env) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if sink.send(WsMsg::Text(text)).await.is_err() {
                break;
            }
        }
    });

    // Periodic status/heartbeat + playback drift correction.
    let status = spawn_status_loop(state.clone(), engine.clone(), out_tx.clone());

    // Reader loop.
    let transfers: Transfers = Arc::new(AsyncMutex::new(HashMap::new()));
    let reader_state = state.clone();
    let reader_engine = engine.clone();
    let reader_out = out_tx.clone();
    let reader_cfg = cfg.clone();
    let reader = tokio::spawn(async move {
        while let Some(next) = source.next().await {
            let msg = match next {
                Ok(m) => m,
                Err(e) => {
                    log(&reader_state, format!("read error: {e}"));
                    break;
                }
            };
            match msg {
                WsMsg::Text(t) => {
                    let env: Envelope<ControllerMsg> = match serde_json::from_str(&t) {
                        Ok(e) => e,
                        Err(e) => {
                            log(&reader_state, format!("bad json: {e}"));
                            continue;
                        }
                    };
                    handle_controller_msg(
                        env,
                        &reader_cfg,
                        &reader_state,
                        &reader_engine,
                        &reader_out,
                        &transfers,
                    )
                    .await;
                }
                WsMsg::Binary(bytes) => {
                    handle_chunk(&bytes, &reader_state, &reader_out, &transfers).await;
                }
                WsMsg::Close(_) => break,
                _ => {}
            }
        }
    });

    // Any task finishing means the connection is done.
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
        _ = status => {}
    }
    Ok(())
}

/// Once a second: report status + heartbeat, and correct playback drift on
/// whichever layer is running a synchronized cue.
fn spawn_status_loop(
    state: SharedState,
    engine: MediaEngine,
    out: mpsc::Sender<ClientMsg>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(1000));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Rate currently applied per layer, to avoid re-seeking every tick.
        let mut applied_rate: HashMap<WireLayer, f64> = HashMap::new();
        loop {
            interval.tick().await;

            let (pb, offset, params) = {
                let mut s = state.lock().unwrap();
                s.playback.position_ms = engine
                    .position_ms(cuemesh2_media::Layer::A)
                    .or_else(|| engine.position_ms(cuemesh2_media::Layer::B))
                    .unwrap_or(0);
                s.playback.layer_a_alpha = engine.alpha(cuemesh2_media::Layer::A) as f32;
                s.playback.layer_b_alpha = engine.alpha(cuemesh2_media::Layer::B) as f32;
                let params = s
                    .show
                    .as_ref()
                    .map(|sh| CorrectionParams {
                        rate_min: sh.sync.correction.rate_min,
                        rate_max: sh.sync.correction.rate_max,
                        hard_seek_threshold_ms: sh.sync.correction.hard_seek_threshold_ms,
                        saturation_ms: 200,
                    })
                    .unwrap_or_default();
                (s.playback.clone(), s.clock_offset_ms, params)
            };

            // Drift correction against the master clock.
            if let Some(offset) = offset {
                for (wire, info) in [(WireLayer::A, &pb.layer_a), (WireLayer::B, &pb.layer_b)] {
                    let (true, Some(start)) = (info.playing, info.master_start_utc_ms) else {
                        continue;
                    };
                    let ml = media_layer(wire);
                    let Some(actual) = engine.position_ms(ml) else { continue };
                    // Controller "now" through our filtered offset.
                    let controller_now = now_utc_ms() as i64 - offset;
                    let expected = controller_now - start as i64;
                    if expected < 0 {
                        continue; // cue hasn't nominally started yet
                    }
                    let drift = actual as i64 - expected;
                    {
                        let mut s = state.lock().unwrap();
                        s.last_drift_ms = Some(drift);
                    }
                    let _ = out
                        .send(ClientMsg::Drift(cuemesh2_shared::protocol::Drift {
                            drift_ms: drift,
                            filtered_offset_ms: offset,
                            sample_count: 0,
                        }))
                        .await;
                    match correction_for(drift, &params) {
                        Correction::Hold => {
                            // Ease back to nominal speed once we're close.
                            if applied_rate.get(&wire).copied().unwrap_or(1.0) != 1.0
                                && drift.abs() < 10
                            {
                                let _ = engine.set_rate(ml, 1.0);
                                applied_rate.insert(wire, 1.0);
                            }
                        }
                        Correction::Rate(rate) => {
                            // Only re-seek when meaningfully different; a rate
                            // change is a flushing seek and not free.
                            let cur = applied_rate.get(&wire).copied().unwrap_or(1.0);
                            if drift.abs() > 25 && (rate as f64 - cur).abs() > 0.005
                                && engine.set_rate(ml, rate as f64).is_ok() {
                                    applied_rate.insert(wire, rate as f64);
                                }
                        }
                        Correction::HardSeek(_) => {
                            let target = expected.max(0) as u64;
                            tracing::info!(?wire, drift, target, "hard seek to correct drift");
                            let _ = engine.seek_ms(ml, target);
                            let _ = engine.set_rate(ml, 1.0);
                            applied_rate.insert(wire, 1.0);
                        }
                    }
                }
            }

            let status_msg = ClientMsg::Status(Status {
                state: pb.state.into(),
                current_cue_id: pb.current_cue_id,
                position_ms: pb.position_ms,
                rate: 1.0,
                layer_a_alpha: pb.layer_a_alpha,
                layer_b_alpha: pb.layer_b_alpha,
            });
            if out.send(status_msg).await.is_err() {
                break;
            }
            if out.send(ClientMsg::Heartbeat).await.is_err() {
                break;
            }
        }
    })
}

async fn handle_controller_msg(
    env: Envelope<ControllerMsg>,
    cfg: &Arc<ConnectionConfig>,
    state: &SharedState,
    engine: &MediaEngine,
    outbound: &mpsc::Sender<ClientMsg>,
    transfers: &Transfers,
) {
    match env.msg {
        ControllerMsg::HelloAck(a) => {
            log(state, format!("controller: {} (v{})", a.controller_name, a.protocol_version));
        }
        ControllerMsg::ShowSync(show) => {
            log(
                state,
                format!("show sync: \"{}\" ({} cues, dropout {:?})", show.title, show.cues.len(), show.dropout_policy),
            );
            state.lock().unwrap().show = Some(show);
        }
        ControllerMsg::LoadCue(c) => {
            let ml = media_layer(c.layer);
            let full = cfg.media_root.join(&c.file);
            log(
                state,
                format!("LOAD_CUE {} → layer {:?}  file={}", c.cue_id, c.layer, full.display()),
            );
            let kind = match c.kind {
                CueKind::Video => MediaKind::Video,
                CueKind::Image => MediaKind::Image,
            };
            engine.set_alpha(ml, 0.0);
            match engine.load(ml, &full, kind) {
                Ok(_) => {
                    {
                        let mut s = state.lock().unwrap();
                        let info = s.layer_mut(c.layer);
                        info.cue_id = Some(c.cue_id.clone());
                        info.master_start_utc_ms = None;
                        info.playing = false;
                        s.playback.state = PlaybackState::Ready;
                    }
                    let _ = outbound.try_send(ClientMsg::Ready(Ready {
                        cue_id: c.cue_id,
                        layer: c.layer,
                    }));
                }
                Err(e) => {
                    log(state, format!("load failed: {e}"));
                    state.lock().unwrap().playback.state = PlaybackState::Error;
                }
            }
        }
        ControllerMsg::PlayAt(p) => {
            let ml = media_layer(p.layer);
            // Convert master (controller-UTC) start into local time using the
            // filtered clock offset, so machines with wrong wall clocks still
            // start together.
            let offset = state.lock().unwrap().clock_offset_ms.unwrap_or(0);
            let local_deadline = (p.master_start_utc_ms as i64 + offset).max(0) as u64;
            let now = now_utc_ms();
            let delay = local_deadline.saturating_sub(now);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            match engine.play(ml) {
                Ok(_) => {
                    {
                        let mut s = state.lock().unwrap();
                        s.playback.state = PlaybackState::Playing;
                        let info = s.layer_mut(p.layer);
                        info.master_start_utc_ms = Some(p.master_start_utc_ms);
                        info.playing = true;
                        s.playback.current_cue_id = info.cue_id.clone();
                        // The outgoing layer is no longer authoritative.
                        if p.crossfade_ms.is_some() {
                            let other = s.layer_mut(other_layer(p.layer));
                            other.playing = false;
                            other.master_start_utc_ms = None;
                        }
                    }
                    if let Some(cf) = p.crossfade_ms {
                        let dur = Duration::from_millis(cf as u64);
                        let out_ml = media_layer(other_layer(p.layer));
                        fades::crossfade(engine, out_ml, ml, dur);
                        // Retire the outgoing producer once it is invisible.
                        let engine = engine.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(dur + Duration::from_millis(100)).await;
                            engine.stop(out_ml);
                        });
                    } else if p.fade_in_ms > 0 {
                        fades::fade(engine, ml, 1.0, Duration::from_millis(p.fade_in_ms as u64));
                    } else {
                        engine.set_alpha(ml, 1.0);
                    }
                }
                Err(e) => log(state, format!("play failed: {e}")),
            }
        }
        ControllerMsg::SeekTo(sk) => {
            if let Err(e) = engine.seek_ms(media_layer(sk.layer), sk.position_ms) {
                log(state, format!("seek failed: {e}"));
            }
        }
        ControllerMsg::SetRate(r) => {
            if let Err(e) = engine.set_rate(media_layer(r.layer), r.rate as f64) {
                log(state, format!("set_rate failed: {e}"));
            }
        }
        ControllerMsg::Pause => {
            engine.pause_all();
            let mut s = state.lock().unwrap();
            s.playback.state = PlaybackState::Paused;
            // Drift correction must not fight a deliberate pause.
            s.playback.layer_a.playing = false;
            s.playback.layer_b.playing = false;
        }
        ControllerMsg::Resume => {
            for wire in [WireLayer::A, WireLayer::B] {
                let loaded = engine.is_loaded(media_layer(wire));
                let had_started = state
                    .lock()
                    .unwrap()
                    .layer_mut(wire)
                    .master_start_utc_ms
                    .is_some();
                if loaded && had_started {
                    let _ = engine.play(media_layer(wire));
                    // Note: master_start is stale after a pause; drop it so
                    // drift correction doesn't hard-seek us forward.
                    let mut s = state.lock().unwrap();
                    let info = s.layer_mut(wire);
                    info.playing = true;
                    info.master_start_utc_ms = None;
                    s.playback.state = PlaybackState::Playing;
                }
            }
        }
        ControllerMsg::Fade(cmd) => {
            let dur = Duration::from_millis(cmd.duration_ms as u64);
            fades::fade(engine, cuemesh2_media::Layer::A, 0.0, dur);
            fades::fade(engine, cuemesh2_media::Layer::B, 0.0, dur);
            let engine_clone = engine.clone();
            let state_clone = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(dur).await;
                engine_clone.stop_all();
                let mut s = state_clone.lock().unwrap();
                s.playback.state = PlaybackState::Black;
                s.playback.layer_a = Default::default();
                s.playback.layer_b = Default::default();
                s.playback.current_cue_id = None;
            });
        }
        ControllerMsg::Stop => {
            engine.stop_all();
            let mut s = state.lock().unwrap();
            s.playback.state = PlaybackState::Black;
            s.playback.layer_a = Default::default();
            s.playback.layer_b = Default::default();
            s.playback.current_cue_id = None;
        }
        ControllerMsg::ShowTestscreen => match engine.load_testscreen(cuemesh2_media::Layer::A) {
            Ok(_) => {
                engine.set_alpha(cuemesh2_media::Layer::A, 1.0);
                engine.set_alpha(cuemesh2_media::Layer::B, 0.0);
                log(state, "testscreen on");
            }
            Err(e) => log(state, format!("testscreen failed: {e}")),
        },
        ControllerMsg::HideTestscreen => {
            engine.stop(cuemesh2_media::Layer::A);
            log(state, "testscreen off");
        }
        ControllerMsg::RequestStatus | ControllerMsg::ReadyCheck => {
            // Status is sent on our own cadence.
        }
        ControllerMsg::Sync(ping) => {
            let t2 = now_utc_ms();
            if let Some(measured) = ping.last_offset_ms {
                // Median-filter controller-measured offsets. The filter lives
                // in state so it survives reconnects within a session.
                let mut s = state.lock().unwrap();
                let filter = s.offset_filter.get_or_insert_with(|| OffsetFilter::new(8));
                filter.push(measured);
                s.clock_offset_ms = s.offset_filter.as_ref().and_then(|f| f.median());
            }
            let t3 = now_utc_ms();
            let _ = outbound.try_send(ClientMsg::SyncReply(SyncReply {
                token: ping.token,
                t1_utc_ms: ping.t1_utc_ms,
                t2_local_ms: t2,
                t3_local_ms: t3,
            }));
        }
        ControllerMsg::MediaCheck(check) => {
            let root = cfg.media_root.clone();
            let out = outbound.clone();
            let state = state.clone();
            tokio::task::spawn_blocking(move || {
                let entries: Vec<MediaReportEntry> = check
                    .files
                    .iter()
                    .map(|spec| MediaReportEntry {
                        rel_path: spec.rel_path.clone(),
                        status: check_file(&root, &spec.rel_path, spec.size, &spec.sha256_hex),
                    })
                    .collect();
                let n_ok = entries.iter().filter(|e| e.status == MediaFileStatus::Ok).count();
                log(&state, format!("media check: {}/{} ok", n_ok, entries.len()));
                let _ = out.try_send(ClientMsg::MediaReport(MediaReport { entries }));
            });
        }
        ControllerMsg::MediaPushBegin(begin) => {
            if let Err(e) = begin_transfer(cfg, transfers, &begin).await {
                log(state, format!("media push {} rejected: {e}", begin.rel_path.display()));
                let _ = outbound.try_send(ClientMsg::MediaPushResult(MediaPushResult {
                    transfer_id: begin.transfer_id,
                    rel_path: begin.rel_path,
                    ok: false,
                    error: Some(e.to_string()),
                }));
            } else {
                log(
                    state,
                    format!(
                        "receiving {} ({} bytes)",
                        begin.rel_path.display(),
                        begin.size
                    ),
                );
            }
        }
        ControllerMsg::MediaPushEnd(end) => {
            finish_transfer(cfg, state, outbound, transfers, end.transfer_id).await;
        }
    }
}

/// Compare one on-disk file against the controller's expectation.
fn check_file(root: &Path, rel: &Path, want_size: u64, want_sha_hex: &str) -> MediaFileStatus {
    let full = root.join(rel);
    let Ok(meta) = std::fs::metadata(&full) else {
        return MediaFileStatus::Missing;
    };
    let size = meta.len();
    // Hash even when sizes differ so the report carries the actual hash.
    let sha_hex = match hashing::sha256_file(&full) {
        Ok(h) => hashing::to_hex(&h),
        Err(_) => return MediaFileStatus::Missing,
    };
    if size == want_size && sha_hex == want_sha_hex {
        MediaFileStatus::Ok
    } else {
        MediaFileStatus::Mismatch { size, sha256_hex: sha_hex }
    }
}

/// Reject traversal and absolute paths; a hostile controller must not be able
/// to write outside the media root.
fn sanitize_rel_path(rel: &Path) -> Result<()> {
    if rel.is_absolute() {
        anyhow::bail!("absolute path");
    }
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        anyhow::bail!("path traversal");
    }
    Ok(())
}

async fn begin_transfer(
    cfg: &Arc<ConnectionConfig>,
    transfers: &Transfers,
    begin: &cuemesh2_shared::protocol::MediaPushBegin,
) -> Result<()> {
    sanitize_rel_path(&begin.rel_path)?;
    std::fs::create_dir_all(&cfg.media_root)?;
    let tmp_path = cfg
        .media_root
        .join(format!(".cuemesh-incoming-{}", begin.transfer_id));
    let file = std::fs::File::create(&tmp_path)?;
    transfers.lock().await.insert(
        begin.transfer_id,
        IncomingTransfer {
            rel_path: begin.rel_path.clone(),
            tmp_path,
            file,
            expected_size: begin.size,
            expected_sha256_hex: begin.sha256_hex.clone(),
            received: 0,
            last_progress_sent: 0,
        },
    );
    Ok(())
}

async fn handle_chunk(
    frame: &[u8],
    state: &SharedState,
    outbound: &mpsc::Sender<ClientMsg>,
    transfers: &Transfers,
) {
    let Some((id, data)) = transfer::decode_chunk(frame) else {
        log(state, "dropped malformed binary frame");
        return;
    };
    let mut guard = transfers.lock().await;
    let Some(t) = guard.get_mut(&id) else {
        log(state, format!("chunk for unknown transfer {id}"));
        return;
    };
    if let Err(e) = t.file.write_all(data) {
        log(state, format!("transfer {id} write failed: {e}"));
        return;
    }
    t.received += data.len() as u64;
    // Progress roughly every 4 MiB — enough for a UI bar, cheap on the wire.
    if t.received - t.last_progress_sent >= 4 * 1024 * 1024 || t.received == t.expected_size {
        t.last_progress_sent = t.received;
        let _ = outbound.try_send(ClientMsg::MediaPushProgress(MediaPushProgress {
            transfer_id: id,
            received_bytes: t.received,
            total_bytes: t.expected_size,
        }));
    }
}

async fn finish_transfer(
    cfg: &Arc<ConnectionConfig>,
    state: &SharedState,
    outbound: &mpsc::Sender<ClientMsg>,
    transfers: &Transfers,
    transfer_id: u64,
) {
    let Some(mut t) = transfers.lock().await.remove(&transfer_id) else {
        log(state, format!("END for unknown transfer {transfer_id}"));
        return;
    };
    let result = tokio::task::spawn_blocking(move || -> Result<(PathBuf, PathBuf)> {
        t.file.flush()?;
        drop(t.file);
        if t.received != t.expected_size {
            let _ = std::fs::remove_file(&t.tmp_path);
            anyhow::bail!("size mismatch: got {} want {}", t.received, t.expected_size);
        }
        let sha_hex = hashing::to_hex(&hashing::sha256_file(&t.tmp_path)?);
        if sha_hex != t.expected_sha256_hex {
            let _ = std::fs::remove_file(&t.tmp_path);
            anyhow::bail!("sha256 mismatch after transfer");
        }
        Ok((t.tmp_path, t.rel_path))
    })
    .await;

    // Unpack the nested Result (join error vs. verify error).
    let verified: Result<(PathBuf, PathBuf)> = match result {
        Ok(inner) => inner,
        Err(e) => Err(anyhow::anyhow!("verify task panicked: {e}")),
    };

    let (ok, rel_path, error) = match verified {
        Ok((tmp, rel)) => {
            let final_path = cfg.media_root.join(&rel);
            let moved = final_path
                .parent()
                .map(std::fs::create_dir_all)
                .transpose()
                .and_then(|_| std::fs::rename(&tmp, &final_path).map(Some));
            match moved {
                Ok(_) => {
                    log(state, format!("media received: {}", rel.display()));
                    (true, rel, None)
                }
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    (false, rel, Some(format!("move into place failed: {e}")))
                }
            }
        }
        Err(e) => (false, PathBuf::new(), Some(e.to_string())),
    };
    if let Some(err) = &error {
        log(state, format!("media push failed: {err}"));
    }
    let _ = outbound.try_send(ClientMsg::MediaPushResult(MediaPushResult {
        transfer_id,
        rel_path,
        ok,
        error,
    }));
}

/// Forward pipeline events (errors, EOS) into the UI log and layer state.
fn spawn_media_event_pump(engine: MediaEngine, state: SharedState) {
    let mut rx = engine.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(MediaEvent::Eos(layer)) => {
                    log(&state, format!("engine: EOS on layer {layer:?}"));
                    // A finished cue leaves the layer showing its last frame;
                    // retire the producer and mark the layer idle. The
                    // operator (or the next GO) decides what happens next.
                    engine.stop(layer);
                    let wire = match layer {
                        cuemesh2_media::Layer::A => WireLayer::A,
                        cuemesh2_media::Layer::B => WireLayer::B,
                    };
                    let mut s = state.lock().unwrap();
                    *s.layer_mut(wire) = Default::default();
                }
                Ok(MediaEvent::Error { layer, source, message }) => {
                    log(&state, format!("engine ERROR layer {layer:?} [{source}]: {message}"));
                    let mut s = state.lock().unwrap();
                    s.playback.state = PlaybackState::Error;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log(&state, format!("engine event stream lagged, dropped {n}"))
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn log(state: &SharedState, line: impl Into<String>) {
    let line = line.into();
    tracing::info!("{line}");
    state.lock().unwrap().push_log(line);
}

fn now_utc_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_rejects_traversal_and_absolute() {
        assert!(sanitize_rel_path(Path::new("ok/clip.mp4")).is_ok());
        assert!(sanitize_rel_path(Path::new("clip.mp4")).is_ok());
        assert!(sanitize_rel_path(Path::new("/etc/passwd")).is_err());
        assert!(sanitize_rel_path(Path::new("../outside.mp4")).is_err());
        assert!(sanitize_rel_path(Path::new("a/../../outside.mp4")).is_err());
    }

    #[test]
    fn check_file_statuses() {
        let dir = std::env::temp_dir().join("cuemesh2_check_file_test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("good.bin"), b"abc").unwrap();

        // Known sha256 of "abc".
        let sha_abc = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(
            check_file(&dir, Path::new("good.bin"), 3, sha_abc),
            MediaFileStatus::Ok
        );
        assert_eq!(
            check_file(&dir, Path::new("absent.bin"), 3, sha_abc),
            MediaFileStatus::Missing
        );
        match check_file(&dir, Path::new("good.bin"), 3, "0000") {
            MediaFileStatus::Mismatch { size, sha256_hex } => {
                assert_eq!(size, 3);
                assert_eq!(sha256_hex, sha_abc);
            }
            other => panic!("expected mismatch, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
