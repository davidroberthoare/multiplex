//! CueMesh2 controller binary.
//!
//! Hosts the WebSocket server (default port 9420), runs a periodic sync loop,
//! and drives the operator egui window.
//!
//! See `CLAUDE.md` at the workspace root for the design brief.

use std::net::SocketAddr;

use cuemesh2_controller::{discovery, server, state, sync, ui, update};
use cuemesh2_shared::protocol::DEFAULT_PORT;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let bind: SocketAddr = std::env::var("CUEMESH_BIND")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)));

    let state = state::shared();
    // Pick up any client-update bundle sitting next to the binary (placed by
    // a previous self-update, or by hand from a USB stick).
    update::load_local_manifest(&state);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let server_state = state.clone();
    rt.spawn(async move {
        if let Err(e) = server::run(server_state, bind).await {
            tracing::error!("server exited: {e}");
        }
    });
    discovery::advertise(bind.port());
    let sync_state = state.clone();
    rt.spawn(async move {
        sync::run(sync_state).await;
    });

    let native_options = eframe::NativeOptions::default();
    let ui_state = state.clone();
    // Keep the runtime alive for the lifetime of the UI.
    let _rt_guard = rt.enter();
    eframe::run_native(
        "CueMesh2 Controller",
        native_options,
        Box::new(move |cc| {
            // egui's default fonts have no symbol glyphs; install Phosphor so
            // the toolbar/table icons render.
            let mut fonts = egui::FontDefinitions::default();
            egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
            cc.egui_ctx.set_fonts(fonts);
            Ok(Box::new(ui::ControllerApp::new(ui_state)))
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe: {e}"))?;

    Ok(())
}
