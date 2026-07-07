//! Operator window: cue list with GO/NEXT/PREV, transport, client roster
//! with preflight results and media push, log view.

use std::path::PathBuf;
use std::time::Duration;

use cuemesh2_shared::protocol::{
    ControllerMsg, FadeCmd, Layer, LoadCue, MediaFileStatus, PlayAt,
};
use cuemesh2_shared::show::ShowFile;

use crate::editor::{EditorAction, EditorState};
use crate::preflight;
use crate::server::{broadcast, now_utc_ms};
use crate::state::SharedState;

pub struct ControllerApp {
    state: SharedState,
    testscreen_on: bool,
    editor: EditorState,
}

impl ControllerApp {
    pub fn new(state: SharedState) -> Self {
        let app = Self {
            state,
            testscreen_on: false,
            editor: EditorState::default(),
        };
        // Auto-load the show named by CUEMESH_SHOW so headless-ish setups
        // (and operators with a fixed show) skip the open dialog entirely.
        if let Ok(p) = std::env::var("CUEMESH_SHOW") {
            app.load_show_from_path(PathBuf::from(p));
        }
        app
    }

    fn load_show_from_path(&self, path: PathBuf) {
        match ShowFile::load(&path) {
            Ok(sf) => {
                {
                    let mut s = self.state.lock().unwrap();
                    s.show = Some(sf);
                    s.show_path = Some(path.clone());
                    s.selected_cue_idx = Some(0);
                    s.run = Default::default();
                    s.local_media = None;
                    s.push_log(format!("loaded show {}", path.display()));
                }
                // Every connected client needs the new cue list.
                if let Some(msg) = crate::server::show_sync_msg(&self.state) {
                    broadcast(&self.state, msg);
                }
            }
            Err(e) => {
                self.state.lock().unwrap().push_log(format!("failed to load show: {e}"));
            }
        }
    }

    /// Fire the selected cue: load it on the idle layer, start it with lead
    /// time, crossfading from whatever is on air. Advances the selection.
    fn go_selected(&self) {
        let plan = {
            let mut s = self.state.lock().unwrap();
            let Some(show) = &s.show else { return };
            let Some(idx) = s.selected_cue_idx else { return };
            let Some(cue) = show.cues.get(idx).cloned() else { return };

            let outgoing = s.run.playing_cue_idx.and_then(|i| show.cues.get(i).cloned());
            let n = show.cues.len();
            let lead_ms = show.show.sync.start_lead_ms.max(250) as u64;
            let target_layer = match s.run.active_layer {
                Some(Layer::A) => Layer::B,
                Some(Layer::B) => Layer::A,
                None => Layer::A,
            };
            // Crossfade when something is on air. The duration prefers the
            // outgoing cue's crossfade_to_next_ms, falls back to the incoming
            // fade-in, and never goes below 40ms (a near-cut, but glitch-free).
            let crossfade_ms = outgoing
                .as_ref()
                .map(|out| out.crossfade_to_next_ms.max(cue.fade_in_ms).max(40));

            s.run.playing_cue_idx = Some(idx);
            s.run.active_layer = Some(target_layer);
            s.selected_cue_idx = Some((idx + 1).min(n.saturating_sub(1)));
            s.push_log(format!("GO cue {} on layer {:?}", cue.id, target_layer));
            (cue, target_layer, crossfade_ms, lead_ms)
        };
        let (cue, layer, crossfade_ms, lead_ms) = plan;

        broadcast(
            &self.state,
            ControllerMsg::LoadCue(LoadCue {
                cue_id: cue.id.clone(),
                layer,
                file: cue.file.clone(),
                kind: cue.kind,
                start_ms: None,
                end_ms: None,
                fade_in_ms: cue.fade_in_ms,
                fade_out_ms: cue.fade_out_ms,
                crossfade_to_next_ms: cue.crossfade_to_next_ms,
            }),
        );
        broadcast(
            &self.state,
            ControllerMsg::PlayAt(PlayAt {
                layer,
                master_start_utc_ms: now_utc_ms() + lead_ms,
                fade_in_ms: cue.fade_in_ms,
                crossfade_ms,
            }),
        );
    }

    fn move_selection(&self, delta: i64) {
        let mut s = self.state.lock().unwrap();
        let Some(show) = &s.show else { return };
        let n = show.cues.len();
        if n == 0 {
            return;
        }
        let cur = s.selected_cue_idx.unwrap_or(0) as i64;
        s.selected_cue_idx = Some((cur + delta).clamp(0, n as i64 - 1) as usize);
    }

    fn blackout(&self) {
        let duration_ms = {
            let mut s = self.state.lock().unwrap();
            s.run = Default::default();
            s.show
                .as_ref()
                .map(|sh| sh.show.settings.default_fade_ms)
                .unwrap_or(1500)
        };
        broadcast(&self.state, ControllerMsg::Fade(FadeCmd { duration_ms }));
    }

    fn stop_all(&self) {
        self.state.lock().unwrap().run = Default::default();
        broadcast(&self.state, ControllerMsg::Stop);
    }

    /// Enter the editor seeded from the running show (or a blank one).
    fn open_editor(&mut self, blank: bool) {
        let (show, path) = {
            let s = self.state.lock().unwrap();
            (s.show.clone(), s.show_path.clone())
        };
        if blank {
            self.editor.enter(None, None);
        } else {
            self.editor.enter(show.as_ref(), path.as_deref());
        }
    }

    /// Push an edited show into the running state and re-sync every client.
    /// Resets run position (cue indices may have changed) and clears preflight.
    fn apply_show(&self, show: ShowFile) {
        let empty = show.cues.is_empty();
        {
            let mut s = self.state.lock().unwrap();
            s.show = Some(show);
            s.selected_cue_idx = if empty { None } else { Some(0) };
            s.run = Default::default();
            s.local_media = None;
            s.push_log("show updated from editor");
        }
        // Guard dropped above: `show_sync_msg`/`broadcast` re-lock `state`.
        if let Some(msg) = crate::server::show_sync_msg(&self.state) {
            broadcast(&self.state, msg);
        }
    }

    /// Render the editor and act on its result. Returns whether edit mode is
    /// still active (so the caller can skip run-mode panels).
    fn editor_panel(&mut self, ctx: &egui::Context) {
        let mut action = EditorAction::None;
        egui::CentralPanel::default().show(ctx, |ui| {
            action = self.editor.ui(ui);
        });
        match action {
            EditorAction::None => {}
            EditorAction::Apply => {
                let show = self.editor.build();
                match show.validate() {
                    Ok(()) => {
                        self.apply_show(show);
                        self.editor.set_status("applied to running show");
                    }
                    Err(e) => self.editor.set_status(format!("invalid: {e}")),
                }
            }
            EditorAction::Save => match self.editor.save_path() {
                None => self.editor.set_status("set a save path first"),
                Some(path) => {
                    let show = self.editor.build();
                    match show.save(&path) {
                        Ok(()) => {
                            self.state.lock().unwrap().show_path = Some(path.clone());
                            self.apply_show(show);
                            self.editor.set_status(format!("saved to {}", path.display()));
                        }
                        Err(e) => self.editor.set_status(format!("save failed: {e}")),
                    }
                }
            },
            EditorAction::Close => self.editor.open = false,
        }
    }
}

fn fmt_ms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}", s / 60, s % 60)
}

impl eframe::App for ControllerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 30fps refresh; cheap for our data volume.
        ctx.request_repaint_after(Duration::from_millis(33));

        // Keyboard: space = GO, arrows = move selection. Only in run mode and
        // when no widget (e.g. an editor text field) wants the keyboard.
        if !self.editor.open && !ctx.wants_keyboard_input() {
            ctx.input(|i| {
                if i.key_pressed(egui::Key::Space) {
                    self.go_selected();
                }
                if i.key_pressed(egui::Key::ArrowDown) {
                    self.move_selection(1);
                }
                if i.key_pressed(egui::Key::ArrowUp) {
                    self.move_selection(-1);
                }
            });
        }

        let (show_summary, cues, playing_idx, selected, clients, preflight_running, log_tail) = {
            let s = self.state.lock().unwrap();
            let show_summary = match &s.show {
                Some(sf) => format!("{}  ({} cues)", sf.show.title, sf.cues.len()),
                None => "(no show loaded)".into(),
            };
            let cues: Vec<_> = match &s.show {
                Some(sf) => sf
                    .cues
                    .iter()
                    .map(|c| (c.id.clone(), c.name.clone(), c.crossfade_to_next_ms))
                    .collect(),
                None => vec![],
            };
            let clients: Vec<_> = s.clients.values().cloned().collect();
            let tail: Vec<_> = s.log_lines.iter().rev().take(80).cloned().collect();
            (
                show_summary,
                cues,
                s.run.playing_cue_idx,
                s.selected_cue_idx,
                clients,
                s.preflight_running,
                tail,
            )
        };

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("CueMesh2 Controller");
                ui.separator();
                ui.label(show_summary);
                ui.separator();
                if ui.button("Open show…").clicked() {
                    let path = std::env::var("CUEMESH_SHOW").ok().map(PathBuf::from).or_else(|| {
                        Some(
                            std::env::current_dir()
                                .unwrap_or_default()
                                .join("examples/example_show.cuemesh.toml"),
                        )
                    });
                    if let Some(p) = path {
                        self.load_show_from_path(p);
                    }
                }
                ui.separator();
                if ui
                    .add_enabled(!preflight_running, egui::Button::new("Preflight"))
                    .clicked()
                {
                    preflight::start_preflight(&self.state);
                }
                if preflight_running {
                    ui.spinner();
                }
                ui.separator();
                let ts_label = if self.testscreen_on { "Hide testscreen" } else { "Testscreen" };
                if ui.button(ts_label).clicked() {
                    self.testscreen_on = !self.testscreen_on;
                    broadcast(
                        &self.state,
                        if self.testscreen_on {
                            ControllerMsg::ShowTestscreen
                        } else {
                            ControllerMsg::HideTestscreen
                        },
                    );
                }
                ui.separator();
                if !self.editor.open {
                    if ui.button("New show").clicked() {
                        self.open_editor(true);
                    }
                    if ui.button("Edit show").clicked() {
                        self.open_editor(false);
                    }
                }
            });
        });

        // Edit mode takes over the body; run-mode panels are hidden.
        if self.editor.open {
            self.editor_panel(ctx);
            return;
        }

        egui::SidePanel::left("cues").min_width(300.0).show(ctx, |ui| {
            ui.heading("Cues");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, (id, name, crossfade)) in cues.iter().enumerate() {
                    let is_sel = selected == Some(i);
                    let on_air = playing_idx == Some(i);
                    let marker = if on_air { "▶" } else { " " };
                    let xf = if *crossfade > 0 { " ⤳" } else { "" };
                    let label = format!("{marker} {i:>3}  {id}  —  {name}{xf}");
                    if ui.selectable_label(is_sel, label).clicked() {
                        self.state.lock().unwrap().selected_cue_idx = Some(i);
                    }
                }
            });
        });

        egui::SidePanel::right("clients").min_width(320.0).show(ctx, |ui| {
            ui.heading("Clients");
            ui.separator();
            if clients.is_empty() {
                ui.label("(no clients connected)");
            }
            let now = now_utc_ms();
            for c in &clients {
                ui.group(|ui| {
                    let stale = now.saturating_sub(c.last_heartbeat_ms) > 3000;
                    let dot = if stale { "🔴" } else { "🟢" };
                    ui.label(format!("{dot} {}  ({})", c.name, &c.client_id[..8.min(c.client_id.len())]));
                    ui.label(format!("addr: {}   state: {:?}", c.addr, c.state));
                    if let Some(cue) = &c.current_cue {
                        ui.label(format!("cue: {cue}  @ {}", fmt_ms(c.position_ms)));
                    }
                    ui.label(format!(
                        "offset: {}   drift: {}",
                        c.offset_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into()),
                        c.last_drift_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into()),
                    ));
                    if !c.preflight.is_empty() {
                        let ok = c
                            .preflight
                            .values()
                            .filter(|s| **s == MediaFileStatus::Ok)
                            .count();
                        let total = c.preflight.len();
                        ui.label(format!("media: {ok}/{total} ok"));
                        for (path, status) in &c.preflight {
                            if *status != MediaFileStatus::Ok {
                                let what = match status {
                                    MediaFileStatus::Missing => "missing",
                                    MediaFileStatus::Mismatch { .. } => "mismatch",
                                    MediaFileStatus::Ok => unreachable!(),
                                };
                                ui.label(format!("   ⚠ {} ({what})", path.display()));
                            }
                        }
                        if let Some((path, received, total)) = &c.push_progress {
                            let frac = if *total > 0 {
                                *received as f32 / *total as f32
                            } else {
                                0.0
                            };
                            ui.add(
                                egui::ProgressBar::new(frac)
                                    .text(format!("{} {:.0}%", path.display(), frac * 100.0)),
                            );
                        } else if ok < total && ui.button("Push missing media").clicked() {
                            preflight::push_missing_to(&self.state, c.client_id.clone());
                        }
                    }
                });
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                let go = egui::Button::new(egui::RichText::new("  GO  ").size(24.0).strong());
                if ui.add(go).clicked() {
                    self.go_selected();
                }
                if ui.button("PREV").clicked() {
                    self.move_selection(-1);
                }
                if ui.button("NEXT").clicked() {
                    self.move_selection(1);
                }
                ui.separator();
                if ui.button("PAUSE").clicked() {
                    broadcast(&self.state, ControllerMsg::Pause);
                }
                if ui.button("RESUME").clicked() {
                    broadcast(&self.state, ControllerMsg::Resume);
                }
                ui.separator();
                if ui.button("BLACKOUT (fade)").clicked() {
                    self.blackout();
                }
                if ui.button("STOP (cut)").clicked() {
                    self.stop_all();
                }
            });
            ui.label("space = GO   ↑/↓ = select cue");
            ui.separator();
            ui.heading("Log");
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for line in log_tail.iter().rev() {
                    ui.monospace(line);
                }
            });
        });
    }
}
