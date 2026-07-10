//! Client-side state shared between the network task and the egui window.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use cuemesh2_shared::protocol::{ClientState, ShowSync};
use cuemesh2_shared::show::EndAction;

#[derive(Debug, Default)]
pub struct AppState {
    pub client_id: String,
    pub name: String,
    pub controller_addr: String,
    pub connected: bool,
    pub media_root: PathBuf,
    /// Controllers found via mDNS: instance fullname → ws URL.
    pub discovered: std::collections::HashMap<String, String>,
    /// URL the user picked in the UI; the reconnect loop switches to it on
    /// its next attempt (it does not sever a live connection).
    pub desired_url: Option<String>,
    /// Show metadata pushed by the controller (dropout policy, sync params,
    /// cue list). None until the first SHOW_SYNC arrives.
    pub show: Option<ShowSync>,
    /// Median-filtered clock offset (client_local − controller_utc, ms),
    /// as measured by the controller and echoed in SYNC pings.
    pub clock_offset_ms: Option<i64>,
    /// Rolling median over recent controller-measured offsets.
    pub offset_filter: Option<cuemesh2_shared::clock_sync::OffsetFilter>,
    /// Last playback drift measurement (positive = ahead of master).
    pub last_drift_ms: Option<i64>,
    pub playback: ClientPlayback,
    /// True while the controller's test pattern is displayed; drives the
    /// centred client-id overlay.
    pub testscreen_on: bool,
    pub log_lines: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ClientPlayback {
    pub state: PlaybackState,
    pub current_cue_id: Option<String>,
    pub position_ms: u64,
    pub layer_a: LayerInfo,
    pub layer_b: LayerInfo,
    pub layer_a_alpha: f32,
    pub layer_b_alpha: f32,
}

/// What the client believes is on one of its two layers.
#[derive(Debug, Clone, Default)]
pub struct LayerInfo {
    pub cue_id: Option<String>,
    /// Wall-clock start (controller UTC ms) of the running cue, for drift.
    pub master_start_utc_ms: Option<u64>,
    pub playing: bool,
    /// In-point (ms into the media) that `master_start_utc_ms` maps to.
    pub in_ms: u64,
    /// Out-point (ms into the media); `None` = natural end.
    pub out_ms: Option<u64>,
    /// Whether this cue loops between in/out.
    pub loops: bool,
    /// What to do when the cue reaches its out-point / natural end.
    pub on_end: EndAction,
    /// The cue's fade duration, used for an `on_end == Fade` fade-out.
    pub fade_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackState {
    #[default]
    Idle,
    Loading,
    Ready,
    Playing,
    Paused,
    Error,
    Black,
}

impl From<PlaybackState> for ClientState {
    fn from(v: PlaybackState) -> Self {
        match v {
            PlaybackState::Idle => ClientState::Idle,
            PlaybackState::Loading => ClientState::Loading,
            PlaybackState::Ready => ClientState::Ready,
            PlaybackState::Playing => ClientState::Playing,
            PlaybackState::Paused => ClientState::Paused,
            PlaybackState::Error => ClientState::Error,
            PlaybackState::Black => ClientState::Black,
        }
    }
}

impl AppState {
    pub fn layer_mut(&mut self, layer: cuemesh2_shared::protocol::Layer) -> &mut LayerInfo {
        match layer {
            cuemesh2_shared::protocol::Layer::A => &mut self.playback.layer_a,
            cuemesh2_shared::protocol::Layer::B => &mut self.playback.layer_b,
        }
    }

    pub fn push_log(&mut self, line: impl Into<String>) {
        const CAP: usize = 200;
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
