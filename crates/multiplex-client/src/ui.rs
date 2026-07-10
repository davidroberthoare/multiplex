//! Client egui window: a chromeless, resizable box that is *just* the video
//! canvas. Text appears in only two situations: (1) startup / disconnected —
//! a small grey status line at the bottom; (2) testscreen — a large centred
//! block naming this client. Controller selection is automatic: the first
//! mDNS-discovered controller is adopted while offline (manual override via
//! `CUEMESH_CONTROLLER`).
//!
//! There is no window chrome to trigger native OS fullscreen from (no menu
//! bar, no title bar), so `F` or `F11` toggles it directly — safe to claim
//! unconditionally since nothing in this UI ever takes text input.

use std::time::Duration;

use cuemesh2_media::MediaEngine;

use crate::state::SharedState;

pub struct ClientApp {
    state: SharedState,
    engine: MediaEngine,
    video_texture: Option<egui::TextureHandle>,
}

impl ClientApp {
    pub fn new(state: SharedState, engine: MediaEngine) -> Self {
        Self {
            state,
            engine,
            video_texture: None,
        }
    }
}

impl eframe::App for ClientApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Repaints are driven by the engine's frame-notify callback (see
        // main.rs); this slow tick only keeps the status line fresh when no
        // video is flowing.
        ctx.request_repaint_after(Duration::from_millis(250));

        // Fullscreen toggle. `key_pressed` fires regardless of held
        // modifiers, so plain F also covers macOS's conventional
        // Cmd+Ctrl+F chord without a separate check.
        let toggle_fullscreen =
            ctx.input(|i| i.key_pressed(egui::Key::F11) || i.key_pressed(egui::Key::F));
        if toggle_fullscreen {
            let is_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!is_fullscreen));
        }

        // ── Pick up a new composited video frame, if one landed ──────────
        if let Some(rgba) = self.engine.latest_frame() {
            let canvas = self.engine.canvas();
            let w = canvas.width as usize;
            let h = canvas.height as usize;
            if rgba.len() >= w * h * 4 {
                let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                let opts = egui::TextureOptions::default();
                // Update the existing texture in place; allocating a fresh GPU
                // texture every frame causes visible jank.
                match &mut self.video_texture {
                    Some(tex) => tex.set(color_image, opts),
                    None => {
                        self.video_texture =
                            Some(ctx.load_texture("cuemesh2-video", color_image, opts));
                    }
                }
            }
        }

        // ── Snapshot shared state ────────────────────────────────────────
        let (name, id, addr, connected, testscreen_on) = {
            let mut s = self.state.lock().unwrap();
            // Auto-adopt the first discovered controller while offline and no
            // explicit target has been chosen yet, so no in-window connect UI
            // is needed on a normal LAN.
            if !s.connected && s.desired_url.is_none() {
                if let Some(url) = s.discovered.values().next().cloned() {
                    s.desired_url = Some(url);
                }
            }
            (
                s.name.clone(),
                s.client_id.clone(),
                s.controller_addr.clone(),
                s.connected,
                s.testscreen_on,
            )
        };

        // ── The whole window is the canvas: black background + video ─────
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                let rect = ui.max_rect();
                let painter = ui.painter();

                // Paint the video frame, letterboxed to fit.
                if let Some(tex) = &self.video_texture {
                    let img = tex.size_vec2();
                    let scale = (rect.width() / img.x).min(rect.height() / img.y);
                    let size = img * scale;
                    let pos = rect.center() - size / 2.0;
                    let dst = egui::Rect::from_min_size(pos, size);
                    painter.image(
                        tex.id(),
                        dst,
                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                        egui::Color32::WHITE,
                    );
                }

                // 1. Testscreen: large centred client identity on a black
                // panel so it stays legible over the colour-bar pattern.
                if testscreen_on {
                    let short = &id[..8.min(id.len())];
                    let name_g = painter.layout_no_wrap(
                        name.clone(),
                        egui::FontId::proportional(56.0),
                        egui::Color32::WHITE,
                    );
                    let id_g = painter.layout_no_wrap(
                        format!("id {short}"),
                        egui::FontId::proportional(22.0),
                        egui::Color32::from_gray(200),
                    );
                    let gap = 10.0;
                    let w = name_g.rect.width().max(id_g.rect.width());
                    let h = name_g.rect.height() + gap + id_g.rect.height();
                    let pad = egui::vec2(32.0, 22.0);
                    let bg = egui::Rect::from_center_size(rect.center(), egui::vec2(w, h) + pad * 2.0);
                    painter.rect_filled(bg, 12.0, egui::Color32::from_rgba_unmultiplied(0, 0, 0, 220));
                    let name_pos =
                        egui::pos2(rect.center().x - name_g.rect.width() / 2.0, bg.top() + pad.y);
                    let id_pos = egui::pos2(
                        rect.center().x - id_g.rect.width() / 2.0,
                        name_pos.y + name_g.rect.height() + gap,
                    );
                    painter.galley(name_pos, name_g, egui::Color32::WHITE);
                    painter.galley(id_pos, id_g, egui::Color32::from_gray(200));
                }

                // 2. Startup / disconnected: small grey status line, bottom.
                if !connected {
                    let msg = format!("CueMesh2 · {name} · connecting to {addr} …");
                    painter.text(
                        egui::pos2(rect.left() + 10.0, rect.bottom() - 8.0),
                        egui::Align2::LEFT_BOTTOM,
                        msg,
                        egui::FontId::proportional(13.0),
                        egui::Color32::from_gray(130),
                    );
                }
            });
    }
}
