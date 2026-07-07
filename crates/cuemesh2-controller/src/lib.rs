//! CueMesh2 controller library: WebSocket hub, cue sequencing, preflight,
//! and the operator UI. The `cuemesh2-controller` binary is a thin wrapper;
//! the split exists so integration tests can drive the server directly.

pub mod discovery;
pub mod editor;
pub mod preflight;
pub mod server;
pub mod state;
pub mod sync;
pub mod ui;
pub mod util;
