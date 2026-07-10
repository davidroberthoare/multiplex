//! Wire protocol for MultiPlex.
//!
//! All messages are JSON envelopes over WebSocket. Envelope shape:
//!
//! ```json
//! { "type": "<MSG_TYPE>", "ts_utc_ms": 1234567890123, "payload": { ... } }
//! ```
//!
//! [`ControllerMsg`] is what the controller sends to clients; [`ClientMsg`] is
//! what clients send back. Each direction is a serde-tagged enum so adding a
//! variant is a one-line change.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::show::{Cue, CueKind, DropoutPolicy, EndAction, Poster, SyncConfig};

/// Envelope wrapping any protocol message with the sender's UTC timestamp.
///
/// Serializes flat: `{"ts_utc_ms": ..., "type": ..., "payload": ...}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope<M> {
    pub ts_utc_ms: u64,
    #[serde(flatten)]
    pub msg: M,
}

impl<M> Envelope<M> {
    pub fn new(ts_utc_ms: u64, msg: M) -> Self {
        Self { ts_utc_ms, msg }
    }
}

/// One of the two video layers the client compositor exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Layer {
    A,
    B,
}

/// Playback state reported by clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClientState {
    Idle,
    Loading,
    Ready,
    Playing,
    Paused,
    Error,
    Black,
}

// ─── Controller → Client ──────────────────────────────────────────────────

/// Messages sent from controller to client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ControllerMsg {
    HelloAck(HelloAck),
    /// Push the parts of the show file clients need (dropout policy, sync
    /// params, cue list). Sent on show load and to every client that joins.
    ShowSync(ShowSync),
    LoadCue(LoadCue),
    /// Preload a cue onto the layer it will next play on, ahead of GO, so the
    /// following PLAY_AT starts instantly instead of stalling on a cold
    /// decode. Same payload and client behaviour as LOAD_CUE — the distinct
    /// variant lets the operator UI drive speculative preloads when a cue is
    /// selected (or auto-advanced) without conflating them with an explicit,
    /// operator-forced load.
    Standby(LoadCue),
    PlayAt(PlayAt),
    SeekTo(SeekTo),
    SetRate(SetRate),
    /// Freeze all layers in place (no fades).
    Pause,
    /// Resume whatever was frozen by PAUSE.
    Resume,
    /// Fade all layers to black over the given duration, then stop.
    /// The controller reads its show's `default_fade_ms` and fills this in;
    /// clients don't need the show file to honour the command.
    Fade(FadeCmd),
    /// Cut all layers to black immediately, stop pipelines.
    Stop,
    ShowTestscreen,
    HideTestscreen,
    RequestStatus,
    Sync(SyncPing),
    ReadyCheck,
    /// Ask the client to hash these files under its media root and report.
    MediaCheck(MediaCheck),
    /// Announce an incoming file transfer; binary chunks follow.
    MediaPushBegin(MediaPushBegin),
    /// All chunks for this transfer have been sent.
    MediaPushEnd(MediaPushEnd),
    /// Announce an incoming client-binary update; binary chunks follow.
    /// The client stages and verifies but does not apply.
    UpdatePushBegin(UpdatePushBegin),
    /// All chunks for this update transfer have been sent.
    UpdatePushEnd(UpdatePushEnd),
    /// Operator-confirmed apply of a previously staged update. The client
    /// swaps its binary and re-execs only if idle; otherwise it refuses.
    ApplyUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAck {
    pub controller_name: String,
    pub protocol_version: u32,
}

/// The subset of the show file every client needs to run cues.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowSync {
    pub title: String,
    pub dropout_policy: DropoutPolicy,
    pub sync: SyncConfig,
    /// Idle poster, if the show defines one. Clients load it on connect.
    #[serde(default)]
    pub poster: Option<Poster>,
    pub cues: Vec<Cue>,
}

/// Preroll a cue onto a specific layer without playing it.
///
/// `file` is **relative to the media root**; each side resolves it against
/// its own root (controller: the show's `media_root`; client: its configured
/// `MULTIPLEX_MEDIA_ROOT`). Absolute paths would only work single-box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadCue {
    pub cue_id: String,
    pub layer: Layer,
    #[serde(default)]
    pub file: PathBuf,
    pub kind: CueKind,
    /// Solid colour (`#RRGGBB`) for `color` cues; ignored otherwise.
    #[serde(default)]
    pub color: Option<String>,
    /// In-point (ms into the file). `None`/`0` starts at the beginning.
    #[serde(default)]
    pub start_ms: Option<u64>,
    /// Out-point (ms into the file). `None` plays to the natural end.
    #[serde(default)]
    pub end_ms: Option<u64>,
    /// Loop between in/out until replaced.
    #[serde(default)]
    pub loops: bool,
    /// What to do at the out-point / natural end.
    #[serde(default)]
    pub on_end: EndAction,
    #[serde(default)]
    pub fade_in_ms: u32,
}

/// Start the preloaded cue at a synchronized wall-clock time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayAt {
    pub layer: Layer,
    pub master_start_utc_ms: u64,
    /// Ramp this layer's alpha 0→1 over this many ms at start (0 = cut).
    #[serde(default)]
    pub fade_in_ms: u32,
    /// If set, simultaneously ramp the *other* layer to 0 over the same
    /// number of ms and stop it when the ramp lands — i.e. a crossfade.
    /// Overrides `fade_in_ms` for the incoming layer.
    #[serde(default)]
    pub crossfade_ms: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeekTo {
    pub layer: Layer,
    pub position_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRate {
    pub layer: Layer,
    pub rate: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FadeCmd {
    pub duration_ms: u32,
}

/// One file the controller expects the client to have.
///
/// Preflight is a filename + size check, not a content hash: hashing every
/// media file on every client on every preflight doesn't scale with library
/// size (large/many videos made this the dominant cost). A file that's
/// present with the right name and size is treated as ok; genuine content
/// corruption is still caught where it matters — the SHA-256 check on actual
/// transferred bytes in [`MediaPushBegin`] / the client's post-transfer
/// verification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFileSpec {
    /// Path relative to the media root.
    pub rel_path: PathBuf,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaCheck {
    pub files: Vec<MediaFileSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPushBegin {
    /// Correlates the binary chunks and the END/RESULT messages.
    pub transfer_id: u64,
    pub rel_path: PathBuf,
    pub size: u64,
    pub sha256_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPushEnd {
    pub transfer_id: u64,
}

/// Header for a controller→client binary update transfer.
///
/// The chunks that follow reuse the media-push binary framing
/// ([`crate::transfer`]); only the destination and verification differ: the
/// client writes to a staging path next to its own executable and checks the
/// ed25519 `signature_b64` against the release public key baked into the
/// binary, on top of the usual size + SHA-256 check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePushBegin {
    /// Correlates the binary chunks and the END/RESULT messages.
    pub transfer_id: u64,
    /// Target triple this binary was built for; the client refuses a
    /// mismatch with its own compile-time triple.
    pub target_triple: String,
    /// Semver of the pushed binary; the client refuses downgrades.
    pub version: String,
    pub size: u64,
    pub sha256_hex: String,
    /// ed25519 signature over the binary bytes, base64 (standard alphabet).
    pub signature_b64: String,
    /// Minimum GStreamer runtime ("major.minor") the pushed binary needs;
    /// the client refuses when its installed runtime is older, since the
    /// updater swaps only the MultiPlex binary, never the bundled runtime.
    #[serde(default)]
    pub min_gstreamer: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePushEnd {
    pub transfer_id: u64,
}

/// Controller-driven NTP-style sync ping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncPing {
    /// Controller's UTC time when this ping was sent.
    pub t1_utc_ms: u64,
    /// Opaque token echoed back in the reply.
    pub token: u64,
    /// The controller's most recent RTT-corrected measurement of this
    /// client's clock offset (client_local − controller_utc, ms). The client
    /// medians these and uses the result to convert `master_start_utc_ms`
    /// into local time and to measure playback drift. `None` until the first
    /// SYNC_REPLY has been processed.
    #[serde(default)]
    pub last_offset_ms: Option<i64>,
}

// ─── Client → Controller ──────────────────────────────────────────────────

/// Messages sent from client to controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ClientMsg {
    Hello(Hello),
    Ready(Ready),
    Status(Status),
    Drift(Drift),
    Heartbeat,
    Log(LogEntry),
    SyncReply(SyncReply),
    /// Reply to MEDIA_CHECK: per-file verification results.
    MediaReport(MediaReport),
    /// Periodic progress while receiving a MEDIA_PUSH (for the operator UI).
    MediaPushProgress(MediaPushProgress),
    /// Final verdict on a MEDIA_PUSH after hash verification.
    MediaPushResult(MediaPushResult),
    /// Final verdict on an UPDATE_PUSH: staged + verified, or why not.
    UpdatePushResult(UpdatePushResult),
    /// Outcome of an APPLY_UPDATE (refused when not idle).
    UpdateApplyResult(UpdateApplyResult),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub client_id: String,
    pub name: String,
    pub protocol_version: u32,
    /// Client binary semver (`CARGO_PKG_VERSION`). Empty from pre-update
    /// clients; the controller treats that as "version unknown".
    #[serde(default)]
    pub app_version: String,
    /// Compile-time target triple, so the controller can pick the right
    /// update artifact. Empty from pre-update clients.
    #[serde(default)]
    pub target_triple: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ready {
    pub cue_id: String,
    pub layer: Layer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub state: ClientState,
    pub current_cue_id: Option<String>,
    pub position_ms: u64,
    pub rate: f32,
    pub layer_a_alpha: f32,
    pub layer_b_alpha: f32,
}

/// How one expected file looks on the client's disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum MediaFileStatus {
    /// Present with matching size.
    Ok,
    /// Not present at all.
    Missing,
    /// Present but a different size than expected.
    Mismatch { size: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaReportEntry {
    pub rel_path: PathBuf,
    #[serde(flatten)]
    pub status: MediaFileStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaReport {
    pub entries: Vec<MediaReportEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPushProgress {
    pub transfer_id: u64,
    pub received_bytes: u64,
    pub total_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaPushResult {
    pub transfer_id: u64,
    pub rel_path: PathBuf,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePushResult {
    pub transfer_id: u64,
    /// Version the transfer claimed to carry.
    pub version: String,
    /// True when the binary is staged and fully verified (size, SHA-256,
    /// signature, triple, downgrade guard).
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateApplyResult {
    /// True when the client is about to swap its binary and re-exec.
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Drift {
    pub drift_ms: i64,
    pub filtered_offset_ms: i64,
    pub sample_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub level: LogLevel,
    pub message: String,
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncReply {
    pub token: u64,
    pub t1_utc_ms: u64,
    pub t2_local_ms: u64,
    pub t3_local_ms: u64,
}

/// Current protocol version. Bump when wire format changes in a breaking way.
pub const PROTOCOL_VERSION: u32 = 3;

/// TCP port the controller listens on.
pub const DEFAULT_PORT: u16 = 9420;

/// mDNS service type advertised by the controller.
pub const MDNS_SERVICE_TYPE: &str = "_multiplex._tcp.local.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_pause_roundtrip() {
        let env = Envelope::new(42, ControllerMsg::Pause);
        let json = serde_json::to_string(&env).unwrap();
        assert_eq!(json, r#"{"ts_utc_ms":42,"type":"PAUSE"}"#);
        let back: Envelope<ControllerMsg> = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.msg, ControllerMsg::Pause));
        assert_eq!(back.ts_utc_ms, 42);
    }

    #[test]
    fn controller_fade_roundtrip() {
        let env = Envelope::new(1, ControllerMsg::Fade(FadeCmd { duration_ms: 1500 }));
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"FADE""#));
        let back: Envelope<ControllerMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ControllerMsg::Fade(f) => assert_eq!(f.duration_ms, 1500),
            _ => panic!(),
        }
    }

    #[test]
    fn controller_load_cue_roundtrip() {
        let env = Envelope::new(
            100,
            ControllerMsg::LoadCue(LoadCue {
                cue_id: "cue-1".into(),
                layer: Layer::A,
                file: PathBuf::from("intro.mp4"),
                kind: CueKind::Video,
                color: None,
                start_ms: Some(2_500),
                end_ms: Some(30_000),
                loops: true,
                on_end: EndAction::Freeze,
                fade_in_ms: 500,
            }),
        );
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope<ControllerMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ControllerMsg::LoadCue(c) => {
                assert_eq!(c.cue_id, "cue-1");
                assert_eq!(c.layer, Layer::A);
                assert_eq!(c.kind, CueKind::Video);
                assert_eq!(c.start_ms, Some(2_500));
                assert_eq!(c.end_ms, Some(30_000));
                assert!(c.loops);
                assert_eq!(c.on_end, EndAction::Freeze);
                assert_eq!(c.fade_in_ms, 500);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn standby_roundtrip() {
        let env = Envelope::new(
            7,
            ControllerMsg::Standby(LoadCue {
                cue_id: "cue-9".into(),
                layer: Layer::B,
                file: PathBuf::from("next.mp4"),
                kind: CueKind::Video,
                color: None,
                start_ms: None,
                end_ms: None,
                loops: false,
                on_end: EndAction::default(),
                fade_in_ms: 500,
            }),
        );
        let json = serde_json::to_string(&env).unwrap();
        assert!(json.contains(r#""type":"STANDBY""#));
        let back: Envelope<ControllerMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ControllerMsg::Standby(c) => {
                assert_eq!(c.cue_id, "cue-9");
                assert_eq!(c.layer, Layer::B);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn play_at_crossfade_roundtrip() {
        let env = Envelope::new(
            5,
            ControllerMsg::PlayAt(PlayAt {
                layer: Layer::B,
                master_start_utc_ms: 123,
                fade_in_ms: 0,
                crossfade_ms: Some(1500),
            }),
        );
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope<ControllerMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ControllerMsg::PlayAt(p) => {
                assert_eq!(p.layer, Layer::B);
                assert_eq!(p.crossfade_ms, Some(1500));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn play_at_defaults_are_backwards_lenient() {
        // A minimal PLAY_AT without the new fields must still parse.
        let json = r#"{"ts_utc_ms":1,"type":"PLAY_AT","payload":{"layer":"A","master_start_utc_ms":42}}"#;
        let back: Envelope<ControllerMsg> = serde_json::from_str(json).unwrap();
        match back.msg {
            ControllerMsg::PlayAt(p) => {
                assert_eq!(p.fade_in_ms, 0);
                assert_eq!(p.crossfade_ms, None);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn show_sync_roundtrip() {
        let env = Envelope::new(
            9,
            ControllerMsg::ShowSync(ShowSync {
                title: "T".into(),
                dropout_policy: DropoutPolicy::Freeze,
                sync: SyncConfig::default(),
                poster: Some(Poster {
                    kind: CueKind::Image,
                    file: PathBuf::from("poster.jpg"),
                }),
                cues: vec![Cue {
                    id: "c1".into(),
                    name: "One".into(),
                    kind: CueKind::Image,
                    file: PathBuf::from("a.jpg"),
                    color: None,
                    fade_in_ms: 0,
                    in_ms: 0,
                    out_ms: None,
                    loops: false,
                    on_end: EndAction::default(),
                    notes: None,
                }],
            }),
        );
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope<ControllerMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ControllerMsg::ShowSync(s) => {
                assert_eq!(s.title, "T");
                assert_eq!(s.dropout_policy, DropoutPolicy::Freeze);
                assert_eq!(s.poster.as_ref().map(|p| p.file.clone()), Some(PathBuf::from("poster.jpg")));
                assert_eq!(s.cues.len(), 1);
                assert_eq!(s.cues[0].kind, CueKind::Image);
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn media_check_and_report_roundtrip() {
        let check = Envelope::new(
            1,
            ControllerMsg::MediaCheck(MediaCheck {
                files: vec![MediaFileSpec {
                    rel_path: PathBuf::from("a.mp4"),
                    size: 100,
                }],
            }),
        );
        let json = serde_json::to_string(&check).unwrap();
        let back: Envelope<ControllerMsg> = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.msg, ControllerMsg::MediaCheck(_)));

        let report = Envelope::new(
            2,
            ClientMsg::MediaReport(MediaReport {
                entries: vec![
                    MediaReportEntry {
                        rel_path: PathBuf::from("a.mp4"),
                        status: MediaFileStatus::Ok,
                    },
                    MediaReportEntry {
                        rel_path: PathBuf::from("b.mp4"),
                        status: MediaFileStatus::Mismatch { size: 5 },
                    },
                ],
            }),
        );
        let json = serde_json::to_string(&report).unwrap();
        let back: Envelope<ClientMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ClientMsg::MediaReport(r) => {
                assert_eq!(r.entries.len(), 2);
                assert_eq!(r.entries[0].status, MediaFileStatus::Ok);
                assert!(matches!(r.entries[1].status, MediaFileStatus::Mismatch { .. }));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn media_push_messages_roundtrip() {
        let begin = Envelope::new(
            1,
            ControllerMsg::MediaPushBegin(MediaPushBegin {
                transfer_id: 77,
                rel_path: PathBuf::from("clip.mp4"),
                size: 1024,
                sha256_hex: "ff".into(),
            }),
        );
        let json = serde_json::to_string(&begin).unwrap();
        let back: Envelope<ControllerMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ControllerMsg::MediaPushBegin(b) => assert_eq!(b.transfer_id, 77),
            _ => panic!("wrong variant"),
        }

        let result = Envelope::new(
            2,
            ClientMsg::MediaPushResult(MediaPushResult {
                transfer_id: 77,
                rel_path: PathBuf::from("clip.mp4"),
                ok: false,
                error: Some("hash mismatch".into()),
            }),
        );
        let json = serde_json::to_string(&result).unwrap();
        let back: Envelope<ClientMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ClientMsg::MediaPushResult(r) => {
                assert!(!r.ok);
                assert_eq!(r.error.as_deref(), Some("hash mismatch"));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn client_hello_roundtrip() {
        let env = Envelope::new(
            0,
            ClientMsg::Hello(Hello {
                client_id: "abc".into(),
                name: "Stage Left".into(),
                protocol_version: PROTOCOL_VERSION,
                app_version: "0.1.0".into(),
                target_triple: "x86_64-unknown-linux-gnu".into(),
            }),
        );
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope<ClientMsg> = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.msg, ClientMsg::Hello(_)));
    }

    #[test]
    fn hello_without_version_fields_still_parses() {
        // A v2 client's HELLO has no app_version/target_triple.
        let json = r#"{"ts_utc_ms":1,"type":"HELLO","payload":{"client_id":"a","name":"n","protocol_version":2}}"#;
        let back: Envelope<ClientMsg> = serde_json::from_str(json).unwrap();
        match back.msg {
            ClientMsg::Hello(h) => {
                assert!(h.app_version.is_empty());
                assert!(h.target_triple.is_empty());
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn update_push_messages_roundtrip() {
        let begin = Envelope::new(
            1,
            ControllerMsg::UpdatePushBegin(UpdatePushBegin {
                transfer_id: 9,
                target_triple: "aarch64-unknown-linux-gnu".into(),
                version: "0.2.0".into(),
                size: 4096,
                sha256_hex: "ab".into(),
                signature_b64: "c2ln".into(),
                min_gstreamer: None,
            }),
        );
        let json = serde_json::to_string(&begin).unwrap();
        assert!(json.contains(r#""type":"UPDATE_PUSH_BEGIN""#));
        let back: Envelope<ControllerMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ControllerMsg::UpdatePushBegin(b) => {
                assert_eq!(b.transfer_id, 9);
                assert_eq!(b.version, "0.2.0");
            }
            _ => panic!("wrong variant"),
        }

        let apply = Envelope::new(2, ControllerMsg::ApplyUpdate);
        let json = serde_json::to_string(&apply).unwrap();
        assert_eq!(json, r#"{"ts_utc_ms":2,"type":"APPLY_UPDATE"}"#);

        let result = Envelope::new(
            3,
            ClientMsg::UpdatePushResult(UpdatePushResult {
                transfer_id: 9,
                version: "0.2.0".into(),
                ok: false,
                error: Some("signature verification failed".into()),
            }),
        );
        let json = serde_json::to_string(&result).unwrap();
        let back: Envelope<ClientMsg> = serde_json::from_str(&json).unwrap();
        match back.msg {
            ClientMsg::UpdatePushResult(r) => {
                assert!(!r.ok);
                assert!(r.error.unwrap().contains("signature"));
            }
            _ => panic!("wrong variant"),
        }

        let applied = Envelope::new(
            4,
            ClientMsg::UpdateApplyResult(UpdateApplyResult {
                ok: false,
                error: Some("busy — not idle".into()),
            }),
        );
        let json = serde_json::to_string(&applied).unwrap();
        let back: Envelope<ClientMsg> = serde_json::from_str(&json).unwrap();
        assert!(matches!(back.msg, ClientMsg::UpdateApplyResult(r) if !r.ok));
    }

    #[test]
    fn sync_reply_roundtrip() {
        let env = Envelope::new(
            999,
            ClientMsg::SyncReply(SyncReply {
                token: 7,
                t1_utc_ms: 1_000,
                t2_local_ms: 500,
                t3_local_ms: 505,
            }),
        );
        let s = serde_json::to_string(&env).unwrap();
        let back: Envelope<ClientMsg> = serde_json::from_str(&s).unwrap();
        if let ClientMsg::SyncReply(r) = back.msg {
            assert_eq!(r.token, 7);
            assert_eq!(r.t3_local_ms, 505);
        } else {
            panic!();
        }
    }
}
