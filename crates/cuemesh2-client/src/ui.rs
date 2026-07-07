//! Minimal client egui window: connection status, playback readout, log.

use std::time::Duration;

use crate::state::SharedState;

pub struct ClientApp {
    state: SharedState,
    manual_url: String,
}

impl ClientApp {
    pub fn new(state: SharedState) -> Self {
        let manual_url = state.lock().unwrap().controller_addr.clone();
        Self { state, manual_url }
    }
}

impl eframe::App for ClientApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        ctx.request_repaint_after(Duration::from_millis(100));

        let (name, id, addr, connected, pb, offset, drift, show_title, media_root, discovered, log_tail) = {
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

        egui::CentralPanel::default().show(ctx, |ui| {
            if !connected {
                ui.heading("Connect");
                if discovered.is_empty() {
                    ui.label("(searching for controllers on the LAN…)");
                }
                for (name, url) in &discovered {
                    let pretty = name.split("._cuemesh").next().unwrap_or(name);
                    if ui.button(format!("Connect to {pretty}  ({url})")).clicked() {
                        self.state.lock().unwrap().desired_url = Some(url.clone());
                    }
                }
                ui.horizontal(|ui| {
                    ui.label("Manual:");
                    ui.text_edit_singleline(&mut self.manual_url);
                    if ui.button("Connect").clicked() && !self.manual_url.trim().is_empty() {
                        self.state.lock().unwrap().desired_url =
                            Some(self.manual_url.trim().to_string());
                    }
                });
                ui.separator();
            }
            ui.heading("Playback");
            ui.label(format!("show:    {}", show_title.unwrap_or_else(|| "(none)".into())));
            ui.label(format!("media:   {}", media_root.display()));
            ui.label(format!("state:   {:?}", pb.state));
            ui.label(format!(
                "cue:     {}",
                pb.current_cue_id.unwrap_or_else(|| "(none)".into())
            ));
            ui.label(format!("pos:     {} ms", pb.position_ms));
            ui.label(format!(
                "alphas:  A={:.2}   B={:.2}",
                pb.layer_a_alpha, pb.layer_b_alpha
            ));
            ui.label(format!(
                "layer A: {}   layer B: {}",
                pb.layer_a.cue_id.as_deref().unwrap_or("—"),
                pb.layer_b.cue_id.as_deref().unwrap_or("—"),
            ));
            ui.label(format!(
                "clock offset: {}   drift: {}",
                offset.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into()),
                drift.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into()),
            ));
            ui.separator();
            ui.heading("Log");
            egui::ScrollArea::vertical().show(ui, |ui| {
                for line in log_tail.iter().rev() {
                    ui.monospace(line);
                }
            });
        });
    }
}
