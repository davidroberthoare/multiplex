//! CueMesh2 client binary.
//!
//! Connects to a controller over WebSocket and drives the local two-layer
//! GStreamer pipeline. Auto-reconnects with exponential backoff; keeps the
//! pipeline running independently of the network task.
//!
//! Env vars:
//!   `CUEMESH_CONTROLLER` — controller URL (default `ws://127.0.0.1:9420`)
//!   `CUEMESH_NAME`       — human-readable client name (default hostname)
//!   `CUEMESH_MEDIA_ROOT` — where this client's media lives
//!                          (default `~/cuemesh_media`)
//!
//! See `CLAUDE.md` at the workspace root for the design brief.

use cuemesh2_client::{connection, discovery, state, ui};
use cuemesh2_media::MediaEngine;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let controller_url = std::env::var("CUEMESH_CONTROLLER")
        .unwrap_or_else(|_| "ws://127.0.0.1:9420".to_string());
    let name = std::env::var("CUEMESH_NAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "cuemesh-client".into());
    let client_id = uuid::Uuid::new_v4().to_string();
    let media_root = std::env::var("CUEMESH_MEDIA_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| {
            dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join("cuemesh_media")
        });

    let engine = MediaEngine::new()?;
    let state = state::shared();
    {
        let mut s = state.lock().unwrap();
        s.client_id = client_id.clone();
        s.name = name.clone();
        s.controller_addr = controller_url.clone();
        s.media_root = media_root.clone();
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    discovery::spawn_browser(state.clone());

    let conn_state = state.clone();
    let conn_engine = engine.clone();
    rt.spawn(async move {
        connection::run(
            connection::ConnectionConfig {
                controller_url,
                client_id,
                name,
                media_root,
            },
            conn_state,
            conn_engine,
        )
        .await;
    });

    let ui_state = state.clone();
    let _rt_guard = rt.enter();
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "CueMesh2 Client",
        native_options,
        Box::new(move |_cc| Ok(Box::new(ui::ClientApp::new(ui_state)))),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;
    Ok(())
}

