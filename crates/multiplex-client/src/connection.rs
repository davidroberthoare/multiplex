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

use multiplex_media::{fades, MediaEngine, MediaEvent, MediaKind};
use multiplex_shared::clock_sync::{correction_for, Correction, CorrectionParams, OffsetFilter};
use multiplex_shared::protocol::{
    ClientMsg, ControllerMsg, Envelope, Hello, Layer as WireLayer, LoadCue, MediaFileStatus,
    MediaPushProgress, MediaPushResult, MediaReport, MediaReportEntry, Ready, Status, SyncReply,
    UpdateApplyResult, UpdatePushResult, PROTOCOL_VERSION,
};
use multiplex_shared::show::{
    parse_hex_color, CueKind, DropoutPolicy, EndAction, Poster, DEFAULT_FADE_MS,
};
use multiplex_shared::{hashing, transfer};

use crate::state::{PlaybackState, SharedState};
use crate::update;

/// Keep drift hard-seeks this far from a clip's end so we always land on real
/// media rather than triggering EOS.
const SEEK_MARGIN_MS: u64 = 250;

/// If a layer's position doesn't increase by at least this much over one
/// second while the controller expects it to, the clip has ended (or is stuck)
/// and drift correction should back off.
const PLATEAU_THRESHOLD_MS: u64 = 100;

/// Per-layer state for the plateau detector.
#[derive(Default)]
struct DriftState {
    prev_actual: Option<u64>,
    /// Ticks where the position hasn't advanced past PLATEAU_THRESHOLD.
    stagnant_ticks: u32,
    /// Once detected, skip this layer until a new PLAY_AT.
    ended: bool,
    /// When master_start_utc_ms changes we know it's a fresh cue.
    prev_master_start: Option<u64>,
}

pub struct ConnectionConfig {
    pub controller_url: String,
    pub client_id: String,
    pub name: String,
    pub media_root: PathBuf,
}

fn media_layer(l: WireLayer) -> multiplex_media::Layer {
    match l {
        WireLayer::A => multiplex_media::Layer::A,
        WireLayer::B => multiplex_media::Layer::B,
    }
}

fn other_layer(l: WireLayer) -> WireLayer {
    match l {
        WireLayer::A => WireLayer::B,
        WireLayer::B => WireLayer::A,
    }
}

pub async fn run(cfg: ConnectionConfig, state: SharedState, engine: MediaEngine) {
    let cfg = Arc::new(cfg);
    spawn_media_event_pump(engine.clone(), state.clone(), cfg.clone());
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
        let policy = s.show.as_ref().map(|sh| sh.dropout_policy).unwrap_or(DropoutPolicy::Continue);
        (policy, DEFAULT_FADE_MS)
    };
    log(state, format!("controller lost; dropout policy: {policy:?}"));
    match policy {
        DropoutPolicy::Continue => {}
        DropoutPolicy::Freeze => engine.pause_all(),
        DropoutPolicy::Black => {
            let dur = Duration::from_millis(fade_ms as u64);
            fades::fade(engine, multiplex_media::Layer::A, 0.0, dur);
            fades::fade(engine, multiplex_media::Layer::B, 0.0, dur);
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

/// Where a finished transfer's bytes end up, and how they're verified.
enum TransferDest {
    /// A media file, moved under the media root after a SHA-256 check.
    Media { rel_path: PathBuf, sha256_hex: String },
    /// A client-binary update, staged next to the executable after SHA-256
    /// plus release-signature verification. Never applied here.
    Update { meta: update::StagedMeta },
}

/// An in-flight controller→client file transfer.
struct IncomingTransfer {
    dest: TransferDest,
    tmp_path: PathBuf,
    file: std::fs::File,
    expected_size: u64,
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
            app_version: update::APP_VERSION.into(),
            target_triple: update::TARGET_TRIPLE.into(),
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
        // Debug kill switch: MULTIPLEX_DRIFT=off measures and reports drift but
        // never touches playback, to isolate the corrector when hunting
        // smoothness problems.
        let corrections_enabled = std::env::var("MULTIPLEX_DRIFT")
            .map(|v| !v.eq_ignore_ascii_case("off"))
            .unwrap_or(true);
        if !corrections_enabled {
            tracing::warn!("MULTIPLEX_DRIFT=off — drift is reported but never corrected");
        }
        // Rate currently applied per layer, to avoid re-seeking every tick.
        let mut applied_rate: HashMap<WireLayer, f64> = HashMap::new();
        // Per-layer plateau detector (end-of-media fallback when duration is
        // unreliable after a flush seek).
        let mut drift_state: HashMap<WireLayer, DriftState> = HashMap::new();
        loop {
            interval.tick().await;

            let (pb, offset, params) = {
                let mut s = state.lock().unwrap();
                s.playback.position_ms = engine
                    .position_ms(multiplex_media::Layer::A)
                    .or_else(|| engine.position_ms(multiplex_media::Layer::B))
                    .unwrap_or(0);
                s.playback.layer_a_alpha = engine.alpha(multiplex_media::Layer::A) as f32;
                s.playback.layer_b_alpha = engine.alpha(multiplex_media::Layer::B) as f32;
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

                    let in_ms = info.in_ms;
                    let looping = info.loops;

                    // Plateau detector: if the position has stopped advancing
                    // while the controller expects it to, the clip has ended.
                    // Looping clips legitimately jump backwards at the seam, so
                    // the "regressed → ended" heuristic must not apply to them.
                    let ds = drift_state.entry(wire).or_default();
                    // Reset when a new PLAY_AT (new master_start_utc_ms)
                    // gives us a fresh cue to track.
                    if ds.prev_master_start != Some(start) {
                        *ds = DriftState::default();
                        ds.prev_master_start = Some(start);
                    }
                    if ds.ended {
                        continue;
                    }
                    if !looping {
                        if let Some(prev) = ds.prev_actual {
                            // Position went backwards after a seek — the clip has
                            // ended (seek past EOS reset to keyframe at 0).
                            if actual < prev {
                                tracing::info!(
                                    ?wire,
                                    prev,
                                    actual,
                                    "drift: position regressed after seek — clip ended"
                                );
                                ds.ended = true;
                                continue;
                            }
                            // Much less progress than expected means we're stuck
                            // near the end of the clip (appsink throttled or
                            // decoder hit the final keyframe).
                            if actual < prev + PLATEAU_THRESHOLD_MS {
                                ds.stagnant_ticks += 1;
                                if ds.stagnant_ticks >= 2 {
                                    tracing::info!(
                                        ?wire,
                                        prev,
                                        actual,
                                        stagnant = ds.stagnant_ticks,
                                        "drift: layer appears ended — disabling correction"
                                    );
                                    ds.ended = true;
                                    continue;
                                }
                            } else {
                                ds.stagnant_ticks = 0;
                            }
                        }
                        ds.prev_actual = Some(actual);
                    }

                    // A clip with no seekable timeline (a still image via
                    // imagefreeze) or whose duration isn't known yet can't be
                    // meaningfully synced — don't touch it.
                    let Some(duration) = engine.duration_ms(ml).filter(|d| *d > 0) else {
                        continue;
                    };
                    // The effective end is the out-point, clamped to the media.
                    let effective_end = info.out_ms.unwrap_or(duration).min(duration);
                    if effective_end <= in_ms {
                        continue;
                    }
                    // Controller "now" through our filtered offset, as elapsed
                    // time since this cue nominally started.
                    let controller_now = now_utc_ms() as i64 - offset;
                    let elapsed = controller_now - start as i64;
                    if elapsed < 0 {
                        continue; // cue hasn't nominally started yet
                    }
                    let elapsed = elapsed as u64;

                    // Map master time onto the in/out window: where the media
                    // *should* be now, plus the drift against where it is
                    // (wrapping across the loop seam for looping cues).
                    let (expected_media, drift) = if looping {
                        let loop_len = effective_end - in_ms;
                        let em = in_ms + (elapsed % loop_len);
                        let mut d = actual as i64 - em as i64;
                        if d > loop_len as i64 / 2 {
                            d -= loop_len as i64;
                        } else if d < -(loop_len as i64 / 2) {
                            d += loop_len as i64;
                        }
                        (em, d)
                    } else {
                        let em = in_ms + elapsed;
                        // Past the out-point: the engine EOSes on its own and the
                        // on-end action fires; nothing to correct here.
                        if em + SEEK_MARGIN_MS >= effective_end {
                            continue;
                        }
                        (em, actual as i64 - em as i64)
                    };
                    {
                        let mut s = state.lock().unwrap();
                        s.last_drift_ms = Some(drift);
                    }
                    let _ = out
                        .send(ClientMsg::Drift(multiplex_shared::protocol::Drift {
                            drift_ms: drift,
                            filtered_offset_ms: offset,
                            sample_count: 0,
                        }))
                        .await;
                    if !corrections_enabled {
                        continue;
                    }
                    match correction_for(drift, &params) {
                        Correction::Hold => {
                            // Ease back to nominal speed once we're close.
                            if applied_rate.get(&wire).copied().unwrap_or(1.0) != 1.0
                                && drift.abs() < 10
                            {
                                tracing::info!(?wire, drift, "drift: easing rate back to 1.0");
                                let _ = engine.set_rate(ml, 1.0);
                                applied_rate.insert(wire, 1.0);
                            }
                        }
                        Correction::Rate(rate) => {
                            // Only re-apply when meaningfully different, so a
                            // drift wobble around the deadband doesn't toggle
                            // the rate every tick.
                            let cur = applied_rate.get(&wire).copied().unwrap_or(1.0);
                            if drift.abs() > 25 && (rate as f64 - cur).abs() > 0.005 {
                                tracing::info!(?wire, drift, rate, "drift: applying rate correction");
                                if engine.set_rate(ml, rate as f64).is_ok() {
                                    applied_rate.insert(wire, rate as f64);
                                }
                            }
                        }
                        Correction::HardSeek(_) => {
                            // Land on real media inside the [in, out] window.
                            let target = expected_media
                                .clamp(in_ms, effective_end.saturating_sub(SEEK_MARGIN_MS));
                            tracing::info!(?wire, drift, target, "hard seek to correct drift");
                            // Accurate, not keyframe-snapped: a KEY_UNIT seek
                            // can land a whole GOP short of the target, which
                            // the plateau detector then reads as "position
                            // regressed → clip ended" and sync goes dead for
                            // the rest of the cue.
                            let _ = engine.seek_ms_accurate(ml, target);
                            if applied_rate.get(&wire).copied().unwrap_or(1.0) != 1.0 {
                                let _ = engine.set_rate(ml, 1.0);
                                applied_rate.insert(wire, 1.0);
                            }
                            // Reset the plateau detector without assuming the
                            // seek landed exactly on target — let the next
                            // tick observe the real position fresh.
                            ds.prev_actual = None;
                            ds.stagnant_ticks = 0;
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

/// Preroll a cue onto its layer without playing it (shared by LOAD_CUE and
/// STANDBY — the two are behaviourally identical; only the intent and the log
/// line differ). Leaves the layer at alpha 0 and reports READY so the
/// controller knows a subsequent PLAY_AT will start instantly.
fn preload_cue(
    c: LoadCue,
    why: &str,
    cfg: &Arc<ConnectionConfig>,
    state: &SharedState,
    engine: &MediaEngine,
    outbound: &mpsc::Sender<ClientMsg>,
) {
    let ml = media_layer(c.layer);
    engine.set_alpha(ml, 0.0);
    let load_result = match c.kind {
        CueKind::Video | CueKind::Image => {
            let full = cfg.media_root.join(&c.file);
            log(state, format!("{why} {} → layer {:?}  file={}", c.cue_id, c.layer, full.display()));
            let kind = if c.kind == CueKind::Video { MediaKind::Video } else { MediaKind::Image };
            engine.load(ml, &full, kind)
        }
        CueKind::Color => {
            let hex = c.color.clone().unwrap_or_else(|| "#000000".into());
            let rgb = parse_hex_color(&hex);
            log(state, format!("{why} {} → layer {:?}  color={hex}", c.cue_id, c.layer));
            engine.load_color(ml, rgb)
        }
    };
    match load_result {
        Ok(_) => {
            let in_ms = c.start_ms.unwrap_or(0);
            // Apply the in/out/loop window (video only; stills/colour have no
            // meaningful timeline). Skip the seek entirely for a plain
            // whole-clip cue so we don't pay a needless flush at load.
            if c.kind == CueKind::Video && (in_ms > 0 || c.end_ms.is_some() || c.loops) {
                if let Err(e) = engine.set_bounds(ml, in_ms, c.end_ms, c.loops) {
                    log(state, format!("{why} set_bounds failed: {e}"));
                }
            }
            {
                let mut s = state.lock().unwrap();
                let info = s.layer_mut(c.layer);
                info.cue_id = Some(c.cue_id.clone());
                info.master_start_utc_ms = None;
                info.playing = false;
                info.in_ms = in_ms;
                info.out_ms = c.end_ms;
                info.loops = c.loops;
                info.on_end = c.on_end;
                info.fade_ms = c.fade_in_ms;
                s.playback.state = PlaybackState::Ready;
            }
            let _ = outbound.try_send(ClientMsg::Ready(Ready {
                cue_id: c.cue_id,
                layer: c.layer,
            }));
        }
        Err(e) => {
            log(state, format!("{why} load failed: {e}"));
            state.lock().unwrap().playback.state = PlaybackState::Error;
        }
    }
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
            // (Re)load the show's background poster; it shows through whenever
            // no cue is visible.
            apply_poster(state, engine, cfg);
        }
        ControllerMsg::LoadCue(c) => preload_cue(c, "LOAD_CUE", cfg, state, engine, outbound),
        ControllerMsg::Standby(c) => preload_cue(c, "STANDBY", cfg, state, engine, outbound),
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
            fades::fade(engine, multiplex_media::Layer::A, 0.0, dur);
            fades::fade(engine, multiplex_media::Layer::B, 0.0, dur);
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
        ControllerMsg::ShowTestscreen => match engine.load_testscreen(multiplex_media::Layer::A) {
            Ok(_) => {
                engine.set_alpha(multiplex_media::Layer::A, 1.0);
                engine.set_alpha(multiplex_media::Layer::B, 0.0);
                state.lock().unwrap().testscreen_on = true;
                log(state, "testscreen on");
            }
            Err(e) => log(state, format!("testscreen failed: {e}")),
        },
        ControllerMsg::HideTestscreen => {
            engine.stop(multiplex_media::Layer::A);
            state.lock().unwrap().testscreen_on = false;
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
                        status: check_file(&root, &spec.rel_path, spec.size),
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
        ControllerMsg::UpdatePushBegin(begin) => {
            let gst = multiplex_media::gstreamer_runtime_version();
            if let Err(e) = begin_update_transfer(transfers, &begin, gst).await {
                log(state, format!("update push v{} rejected: {e:#}", begin.version));
                let _ = outbound.try_send(ClientMsg::UpdatePushResult(UpdatePushResult {
                    transfer_id: begin.transfer_id,
                    version: begin.version,
                    ok: false,
                    error: Some(format!("{e:#}")),
                }));
            } else {
                log(
                    state,
                    format!("receiving update v{} ({} bytes)", begin.version, begin.size),
                );
            }
        }
        ControllerMsg::UpdatePushEnd(end) => {
            finish_transfer(cfg, state, outbound, transfers, end.transfer_id).await;
        }
        ControllerMsg::ApplyUpdate => {
            apply_update(state, outbound).await;
        }
    }
}

/// Operator-confirmed apply: refuse unless nothing is (or is about to be) on
/// air, re-verify the staged binary, then self-replace and re-exec.
async fn apply_update(state: &SharedState, outbound: &mpsc::Sender<ClientMsg>) {
    let pb_state = state.lock().unwrap().playback.state;
    let idle = matches!(
        pb_state,
        PlaybackState::Idle | PlaybackState::Black | PlaybackState::Error
    );
    let refuse = |why: String| {
        log(state, format!("apply update refused: {why}"));
        let _ = outbound.try_send(ClientMsg::UpdateApplyResult(UpdateApplyResult {
            ok: false,
            error: Some(why),
        }));
    };
    if !idle {
        return refuse(format!("busy — client is {pb_state:?}"));
    }
    let staged = match update::take_staged() {
        Ok(Some(bin)) => bin,
        Ok(None) => return refuse("no update staged".into()),
        Err(e) => return refuse(format!("staged update invalid: {e:#}")),
    };
    log(state, "applying staged update — restarting");
    let _ = outbound.try_send(ClientMsg::UpdateApplyResult(UpdateApplyResult {
        ok: true,
        error: None,
    }));
    // Give the writer task a moment to flush the result before this process
    // image disappears; the controller also learns the outcome from the
    // reconnect HELLO either way.
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Err(e) = update::apply_and_restart(&staged) {
        log(state, format!("update apply failed: {e:#}"));
        let _ = outbound.try_send(ClientMsg::UpdateApplyResult(UpdateApplyResult {
            ok: false,
            error: Some(format!("{e:#}")),
        }));
    }
}

/// Compare one on-disk file against the controller's expectation.
///
/// Filename + size only, not a content hash: hashing every media file on
/// every preflight doesn't scale with library size. A file that's actually
/// pushed still gets a SHA-256 integrity check after transfer (see
/// `finish_transfer`), so corruption in transit is still caught.
fn check_file(root: &Path, rel: &Path, want_size: u64) -> MediaFileStatus {
    let full = root.join(rel);
    let Ok(meta) = std::fs::metadata(&full) else {
        return MediaFileStatus::Missing;
    };
    let size = meta.len();
    if size == want_size {
        MediaFileStatus::Ok
    } else {
        MediaFileStatus::Mismatch { size }
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
    begin: &multiplex_shared::protocol::MediaPushBegin,
) -> Result<()> {
    sanitize_rel_path(&begin.rel_path)?;
    std::fs::create_dir_all(&cfg.media_root)?;
    let tmp_path = cfg
        .media_root
        .join(format!(".multiplex-incoming-{}", begin.transfer_id));
    let file = std::fs::File::create(&tmp_path)?;
    transfers.lock().await.insert(
        begin.transfer_id,
        IncomingTransfer {
            dest: TransferDest::Media {
                rel_path: begin.rel_path.clone(),
                sha256_hex: begin.sha256_hex.clone(),
            },
            tmp_path,
            file,
            expected_size: begin.size,
            received: 0,
            last_progress_sent: 0,
        },
    );
    Ok(())
}

/// Accept an incoming binary-update transfer. Prechecks (platform, version,
/// GStreamer floor) run here so a doomed push fails before any bytes move.
/// The download lands next to the executable so the later rename into the
/// staged slot stays on one filesystem.
async fn begin_update_transfer(
    transfers: &Transfers,
    begin: &multiplex_shared::protocol::UpdatePushBegin,
    gst_runtime: (u32, u32),
) -> Result<()> {
    update::precheck(begin, gst_runtime)?;
    let staged = update::staged_bin_path()?;
    let tmp_path = staged.with_file_name(format!(".multiplex-update-{}", begin.transfer_id));
    let file = std::fs::File::create(&tmp_path)?;
    transfers.lock().await.insert(
        begin.transfer_id,
        IncomingTransfer {
            dest: TransferDest::Update {
                meta: update::StagedMeta {
                    version: begin.version.clone(),
                    size: begin.size,
                    sha256_hex: begin.sha256_hex.clone(),
                    signature_b64: begin.signature_b64.clone(),
                },
            },
            tmp_path,
            file,
            expected_size: begin.size,
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
    // Update pushes skip it: binaries are small and get a single verdict.
    if matches!(t.dest, TransferDest::Media { .. })
        && (t.received - t.last_progress_sent >= 4 * 1024 * 1024 || t.received == t.expected_size)
    {
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
    let Some(t) = transfers.lock().await.remove(&transfer_id) else {
        log(state, format!("END for unknown transfer {transfer_id}"));
        return;
    };
    let media_root = cfg.media_root.clone();
    let result = tokio::task::spawn_blocking(move || complete_transfer(t, &media_root)).await;
    // Unpack the nested Result (join error vs. verify error).
    let outcome = match result {
        Ok(inner) => inner,
        Err(e) => Err((None, anyhow::anyhow!("verify task panicked: {e}"))),
    };

    let msg = match outcome {
        Ok(TransferDone::Media { rel_path }) => {
            log(state, format!("media received: {}", rel_path.display()));
            ClientMsg::MediaPushResult(MediaPushResult {
                transfer_id,
                rel_path,
                ok: true,
                error: None,
            })
        }
        Ok(TransferDone::Update { version }) => {
            log(state, format!("update v{version} staged and verified — awaiting apply"));
            ClientMsg::UpdatePushResult(UpdatePushResult {
                transfer_id,
                version,
                ok: true,
                error: None,
            })
        }
        Err((dest, e)) => {
            let error = format!("{e:#}");
            log(state, format!("push failed: {error}"));
            match dest {
                Some(TransferDest::Update { meta }) => {
                    ClientMsg::UpdatePushResult(UpdatePushResult {
                        transfer_id,
                        version: meta.version,
                        ok: false,
                        error: Some(error),
                    })
                }
                Some(TransferDest::Media { rel_path, .. }) => {
                    ClientMsg::MediaPushResult(MediaPushResult {
                        transfer_id,
                        rel_path,
                        ok: false,
                        error: Some(error),
                    })
                }
                None => ClientMsg::MediaPushResult(MediaPushResult {
                    transfer_id,
                    rel_path: PathBuf::new(),
                    ok: false,
                    error: Some(error),
                }),
            }
        }
    };
    let _ = outbound.try_send(msg);
}

enum TransferDone {
    Media { rel_path: PathBuf },
    Update { version: String },
}

/// Flush, verify, and move a finished download to its destination.
/// Blocking (hashing + rename); run on the blocking pool. Returns the
/// destination alongside errors so the caller can report on the right
/// channel.
fn complete_transfer(
    mut t: IncomingTransfer,
    media_root: &Path,
) -> std::result::Result<TransferDone, (Option<TransferDest>, anyhow::Error)> {
    let cleanup_tmp = |tmp: &Path| {
        let _ = std::fs::remove_file(tmp);
    };
    if let Err(e) = t.file.flush() {
        cleanup_tmp(&t.tmp_path);
        return Err((Some(t.dest), e.into()));
    }
    drop(t.file);
    if t.received != t.expected_size {
        cleanup_tmp(&t.tmp_path);
        return Err((
            Some(t.dest),
            anyhow::anyhow!("size mismatch: got {} want {}", t.received, t.expected_size),
        ));
    }
    match t.dest {
        TransferDest::Media { rel_path, sha256_hex } => {
            let sha = match hashing::sha256_file(&t.tmp_path) {
                Ok(h) => hashing::to_hex(&h),
                Err(e) => {
                    cleanup_tmp(&t.tmp_path);
                    return Err((Some(TransferDest::Media { rel_path, sha256_hex }), e.into()));
                }
            };
            if sha != sha256_hex {
                cleanup_tmp(&t.tmp_path);
                return Err((
                    Some(TransferDest::Media { rel_path, sha256_hex }),
                    anyhow::anyhow!("sha256 mismatch after transfer"),
                ));
            }
            let final_path = media_root.join(&rel_path);
            let moved = final_path
                .parent()
                .map(std::fs::create_dir_all)
                .transpose()
                .and_then(|_| std::fs::rename(&t.tmp_path, &final_path));
            match moved {
                Ok(_) => Ok(TransferDone::Media { rel_path }),
                Err(e) => {
                    cleanup_tmp(&t.tmp_path);
                    Err((
                        Some(TransferDest::Media { rel_path, sha256_hex }),
                        anyhow::anyhow!("move into place failed: {e}"),
                    ))
                }
            }
        }
        TransferDest::Update { meta } => {
            // stage() re-verifies size, SHA-256, and the release signature.
            match update::stage(&t.tmp_path, &meta) {
                Ok(()) => Ok(TransferDone::Update { version: meta.version }),
                Err(e) => {
                    cleanup_tmp(&t.tmp_path);
                    update::discard_staged();
                    Err((Some(TransferDest::Update { meta }), e))
                }
            }
        }
    }
}

/// (Re)load or clear the show's idle poster on the engine's background layer,
/// per the current show. The poster sits below the cue layers, so it appears
/// automatically whenever both are transparent — no idle bookkeeping needed.
/// Called on connect / every SHOW_SYNC (show load or update).
fn apply_poster(state: &SharedState, engine: &MediaEngine, cfg: &Arc<ConnectionConfig>) {
    let poster: Option<Poster> = state.lock().unwrap().show.as_ref().and_then(|sh| sh.poster.clone());
    match poster {
        Some(p) => {
            let full = cfg.media_root.join(&p.file);
            let kind = if p.kind == CueKind::Video {
                MediaKind::Video
            } else {
                MediaKind::Image
            };
            match engine.load_poster(&full, kind) {
                Ok(_) => log(state, format!("idle poster loaded ({})", full.display())),
                Err(e) => log(state, format!("poster load failed: {e}")),
            }
        }
        None => engine.stop_poster(),
    }
}

/// Forward pipeline events (errors, EOS) into the UI log and layer state.
fn spawn_media_event_pump(engine: MediaEngine, state: SharedState, _cfg: Arc<ConnectionConfig>) {
    let mut rx = engine.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(MediaEvent::Eos(layer)) => {
                    let wire = match layer {
                        multiplex_media::Layer::A => WireLayer::A,
                        multiplex_media::Layer::B => WireLayer::B,
                    };
                    let on_end = state.lock().unwrap().layer_mut(wire).on_end;
                    log(&state, format!("engine: EOS on layer {layer:?} → {on_end:?}"));
                    match on_end {
                        EndAction::Freeze => {
                            // Hold the last frame: leave the producer and alpha
                            // as-is, just stop tracking it for drift.
                            let mut s = state.lock().unwrap();
                            let info = s.layer_mut(wire);
                            info.playing = false;
                            info.master_start_utc_ms = None;
                        }
                        EndAction::Cut => {
                            // Drop the layer; the background poster (or black)
                            // shows through automatically.
                            engine.stop(layer);
                            *state.lock().unwrap().layer_mut(wire) = Default::default();
                        }
                        EndAction::Fade => {
                            let fade_ms =
                                state.lock().unwrap().layer_mut(wire).fade_ms.max(1);
                            let dur = Duration::from_millis(fade_ms as u64);
                            fades::fade(&engine, layer, 0.0, dur);
                            let engine2 = engine.clone();
                            let state2 = state.clone();
                            tokio::spawn(async move {
                                tokio::time::sleep(dur + Duration::from_millis(50)).await;
                                engine2.stop(layer);
                                *state2.lock().unwrap().layer_mut(wire) = Default::default();
                            });
                        }
                    }
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
        let dir = std::env::temp_dir().join("multiplex_check_file_test");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("good.bin"), b"abc").unwrap();

        assert_eq!(check_file(&dir, Path::new("good.bin"), 3), MediaFileStatus::Ok);
        assert_eq!(
            check_file(&dir, Path::new("absent.bin"), 3),
            MediaFileStatus::Missing
        );
        match check_file(&dir, Path::new("good.bin"), 999) {
            MediaFileStatus::Mismatch { size } => assert_eq!(size, 3),
            other => panic!("expected mismatch, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
