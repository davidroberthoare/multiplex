//! CueMesh2 client library: controller connection, media dispatch, status UI.
//! The `cuemesh2-client` binary is a thin wrapper; the split exists so
//! integration tests can exercise the connection logic directly.

pub mod connection;
pub mod discovery;
pub mod state;
pub mod ui;
