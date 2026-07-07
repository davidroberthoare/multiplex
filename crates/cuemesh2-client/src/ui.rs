//! Client egui window: video display with status overlay and connect screen.

use std::time::Duration;

use cuemesh2_media::MediaEngine;

use crate::state::SharedState;

pub struct ClientApp {
    state: SharedState,
    manual_url: String,
    engine: MediaEngine,
    video_texture: Option<egui::TextureHandle>,
}

impl ClientApp {
    pub fn new(state: SharedState, engine: MediaEngine) -> Self {
        let manual_url = state.lock().unwrap().controller_addr.clone();
        Self {
            state,
            manual_url,
            engine,
            video_texture: None,
        }
    }
}

impl eframe::App for ClientApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Repaints are driven by the engine's frame-notify callback (see
        // main.rs); this slow tick only keeps the overlay/status fresh when
        // no video is flowing.
        ctx.request_repaint_after(Duration::from_millis(250));

        // ── Pick up a new composited video frame, if one landed ──────────
        if let Some(rgba) = self.engine.latest_frame() {
            let canvas = self.engine.canvas();
            let w = canvas.width as usize;
            let h = canvas.height as usize;
            if rgba.len() >= w * h * 4 {
                let color_image = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                let opts = egui::TextureOptions::default();
                // Update the existing texture in place; allocating a fresh
                // GPU texture every frame causes visible jank.
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
        let (name, id, addr, connected, pb, _offset, drift, show_title, _media_root, discovered, _log_tail) = {
            let s = self.state.lock().unwrap();
            (
                s.name.clone(),
                s.client_id.clone(),
                s.controller_addr.clone(),
                s.connected,
                s.playback.clone(),
                s.clock_offset_ms,
                s.last_drift_ms,
                s.show.as_ref().map(|sh| sh.title.clone()),
                s.media_root.clone(),
                s.discovered.clone(),
                s.log_lines.iter().rev().take(80).cloned().collect::<Vec<_>>(),
            )
        };

        // ── Top bar ──────────────────────────────────────────────────────
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("CueMesh2 Client");
                ui.separator();
                ui.label(format!("{name}  ({id})"));
                ui.separator();
                ui.label(format!(
                    "controller: {addr}   {}",
                    if connected { "● online" } else { "○ offline" }
                ));
            });
        });

        // ── Central area: video background + overlay ─────────────────────
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(egui::Color32::BLACK))
            .show(ctx, |ui| {
                let available = ui.available_size();

                // Paint the video frame filling the available space.
                if let Some(tex) = &self.video_texture {
                    let img_size = tex.size_vec2();
                    let scale = (available.x / img_size.x).min(available.y / img_size.y);
                    let final_size = img_size * scale;
                    // Center within the available area.
                    let x_off = (available.x - final_size.x).max(0.0) / 2.0;
                    let y_off = (available.y - final_size.y).max(0.0) / 2.0;
                    let rect = egui::Rect::from_min_size(
                        ui.min_rect().min + egui::vec2(x_off, y_off),
                        final_size,
                    );
                    ui.put(rect, |ui: &mut egui::Ui| -> egui::Response {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(final_size))
                    });
                }

                // Overlay: connect screen when offline.
                if !connected {
                    let overlay = egui::Frame::none()
                        .fill(egui::Color32::from_black_alpha(180))
                        .rounding(6.0)
                        .inner_margin(egui::Margin::symmetric(16.0, 8.0));
                    let rect = egui::Rect::from_min_size(
                        ui.min_rect().min + egui::vec2(16.0, 8.0),
                        egui::vec2(360.0, available.y - 16.0),
                    );
                    ui.put(rect, |ui: &mut egui::Ui| -> egui::Response {
                        overlay
                            .show(ui, |ui| {
                                ui.heading("Connect");
                                if discovered.is_empty() {
                                    ui.label("(searching for controllers on the LAN…)");
                                }
                                for (name, url) in &discovered {
                                    let pretty = name.split("._cuemesh").next().unwrap_or(name);
                                    if ui.button(format!("{pretty}  ({url})")).clicked() {
                                        self.state.lock().unwrap().desired_url =
                                            Some(url.clone());
                                    }
                                }
                                ui.horizontal(|ui| {
                                    ui.label("Manual:");
                                    ui.text_edit_singleline(&mut self.manual_url);
                                    if ui.button("Connect").clicked()
                                        && !self.manual_url.trim().is_empty()
                                    {
                                        self.state.lock().unwrap().desired_url =
                                            Some(self.manual_url.trim().to_string());
                                    }
                                });
                            })
                            .response
                    });
                }

                // Overlay: minimal status in the bottom-right corner.
                let status_text = format!(
                    "{} state: {:?}  {:?}  pos: {} ms  α A={:.2} B={:.2}  drift: {}",
                    show_title.unwrap_or_default(),
                    pb.state,
                    pb.current_cue_id.unwrap_or_else(|| "—".into()),
                    pb.position_ms,
                    pb.layer_a_alpha,
                    pb.layer_b_alpha,
                    drift.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into()),
                );
                let bottom_left = egui::pos2(
                    ui.min_rect().min.x + 8.0,
                    ui.min_rect().max.y - 20.0,
                );
                let status_rect = egui::Rect::from_min_size(
                    bottom_left,
                    egui::vec2(available.x - 16.0, 18.0),
                );
                ui.put(status_rect, |ui: &mut egui::Ui| -> egui::Response {
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&status_text)
                                .color(egui::Color32::WHITE)
                                .size(13.0),
                        )
                        .selectable(false),
                    )
                });
            });
    }
}
