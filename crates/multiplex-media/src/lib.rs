//! Two-layer GStreamer video engine for CueMesh2 clients.
//!
//! A persistent display pipeline composites two `intervideosrc` channels;
//! each layer is fed by an independent producer pipeline (file decoder,
//! image loop, or test pattern) that can load, preroll, play, seek, and stop
//! without ever stalling the on-screen output. Layer alphas are set on the
//! compositor's sink pads, so no separate `alpha` element is needed.
//!
//! See `CLAUDE.md` at the workspace root for the design brief.

pub mod fades;
pub mod pipeline;

pub use pipeline::{Canvas, MediaEngine, MediaError, MediaEvent, MediaKind};

pub use cuemesh2_shared::protocol::Layer;

/// The GStreamer runtime's (major, minor) version. Used by the auto-updater
/// to refuse a client binary that needs a newer runtime than is installed.
pub fn gstreamer_runtime_version() -> (u32, u32) {
    let (major, minor, _micro, _nano) = gstreamer::version();
    (major, minor)
}
