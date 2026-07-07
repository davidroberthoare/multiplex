//! Central controller state shared between the network tasks and the egui thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use cuemesh2_shared::protocol::{ClientState, ControllerMsg, Layer, MediaFileSpec, MediaFileStatus};
use cuemesh2_shared::show::ShowFile;

/// What the per-client writer task can send: JSON envelopes or raw binary
/// media chunks (already framed by `cuemesh2_shared::transfer`).
#[derive(Debug, Clone)]
pub enum Outgoing {
    Msg(ControllerMsg),
    Chunk(Vec<u8>),
}

/// A connected client from the controller's point of view.
#[derive(Debug, Clone)]
pub struct ClientRow {
    pub client_id: String,
    pub name: String,
    pub addr: String,
    pub state: ClientState,
    pub current_cue: Option<String>,
    pub position_ms: u64,
    /// RTT-corrected clock offset from the last SYNC_REPLY (client − controller).
    pub offset_ms: Option<i64>,
    /// Playback drift the client last reported about itself.
    pub last_drift_ms: Option<i64>,
    pub last_heartbeat_ms: u64,
    /// Per-file verification results from the last MEDIA_CHECK.
    pub preflight: HashMap<PathBuf, MediaFileStatus>,
    /// (rel_path, received, total) while a push to this client is running.
    pub push_progress: Option<(PathBuf, u64, u64)>,
    /// Outbound queue to the WebSocket task for this client.
    pub outbound: mpsc::Sender<Outgoing>,
}

/// Where we are in the running show.
#[derive(Debug, Clone, Default)]
pub struct RunState {
    /// Cue index currently on air, if any.
    pub playing_cue_idx: Option<usize>,
    /// Layer that cue went out on; the next GO uses the other layer.
    pub active_layer: Option<Layer>,
    /// (cue_id, layer) currently pre-loaded on clients via STANDBY, so GO can
    /// skip the LOAD_CUE and fire PLAY_AT alone. Cleared when consumed by GO
    /// or invalidated by a show change.
    pub standby: Option<(String, Layer)>,
    /// The next STANDBY target layer is busy finishing a crossfade-out until
    /// this controller-UTC ms; preloading onto it before then would cut the
    /// outgoing video. 0 = free now.
    pub idle_free_utc_ms: u64,
}

#[derive(Debug, Default)]
pub struct AppState {
    pub show: Option<ShowFile>,
    pub show_path: Option<PathBuf>,
    pub selected_cue_idx: Option<usize>,
    pub run: RunState,
    pub clients: HashMap<String, ClientRow>,
    pub blacklist: Vec<String>,
    /// Local hashes of the show's media (rel_path → spec), filled by the
    /// preflight task. None = not yet computed for the current show.
    pub local_media: Option<Vec<MediaFileSpec>>,
    /// True while the preflight hashing task runs (drives a UI spinner).
    pub preflight_running: bool,
    /// Log lines shown in the UI. Bounded — oldest entries drop.
    pub log_lines: Vec<String>,
}

impl AppState {
    pub fn push_log(&mut self, line: impl Into<String>) {
        const CAP: usize = 500;
        self.log_lines.push(line.into());
        if self.log_lines.len() > CAP {
            let drop = self.log_lines.len() - CAP;
            self.log_lines.drain(..drop);
        }
    }
}

pub type SharedState = Arc<Mutex<AppState>>;

pub fn shared() -> SharedState {
    Arc::new(Mutex::new(AppState::default()))
}
