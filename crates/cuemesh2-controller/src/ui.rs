//! Operator window: cue list with GO/NEXT/PREV, transport, client roster
//! with preflight results and media push, log view.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui_file_dialog::FileDialog;
use egui_phosphor::regular as icon;

use cuemesh2_shared::protocol::{ControllerMsg, FadeCmd, Layer, MediaFileStatus, PlayAt};
use cuemesh2_shared::show::{CueKind, ShowFile};

use crate::editor::{EditorAction, EditorState};
use crate::preflight;
use crate::server::{broadcast, load_cue_for, now_utc_ms};
use crate::state::{ClientUpdate, SelfUpdate, SharedState};
use crate::update;

/// A file dialog pre-filtered to CueMesh show files.
fn show_dialog() -> FileDialog {
    let filter: Arc<dyn Fn(&Path) -> bool + Send + Sync> =
        Arc::new(|p| p.to_string_lossy().to_ascii_lowercase().ends_with(".cuemesh.toml"));
    FileDialog::new()
        .add_file_filter("CueMesh shows (*.cuemesh.toml)", filter)
        .default_file_filter("CueMesh shows (*.cuemesh.toml)")
        .default_file_name("show.cuemesh.toml")
}

/// How long a cue selection must hold still before we preload it. Keeps
/// arrow-key scrolling from firing a cold decode on every intermediate cue.
const SELECTION_SETTLE_MS: u128 = 150;

/// The layer the next GO (and thus the next STANDBY) will target: the opposite
/// of whatever is currently on air, or A when nothing is playing.
fn idle_layer(active: Option<Layer>) -> Layer {
    match active {
        Some(Layer::A) => Layer::B,
        Some(Layer::B) => Layer::A,
        None => Layer::A,
    }
}

pub struct ControllerApp {
    state: SharedState,
    testscreen_on: bool,
    editor: EditorState,
    /// Last selection we observed, and when it last changed — drives the
    /// standby debounce.
    last_selected: Option<usize>,
    selected_since: Instant,
    /// "Open show…" picker.
    open_dialog: FileDialog,
    /// Editor "Save as…" picker.
    save_dialog: FileDialog,
}

impl ControllerApp {
    pub fn new(state: SharedState) -> Self {
        let app = Self {
            state,
            testscreen_on: false,
            editor: EditorState::default(),
            last_selected: None,
            selected_since: Instant::now(),
            open_dialog: show_dialog(),
            save_dialog: show_dialog(),
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

    /// Fire the selected cue: start it with lead time, crossfading from
    /// whatever is on air. If the cue was already pre-loaded on the target
    /// layer via STANDBY, this is just a PLAY_AT and starts instantly;
    /// otherwise it falls back to loading now (a cold decode that will stall).
    /// Advances the selection.
    fn go_selected(&self) {
        let plan = {
            let mut s = self.state.lock().unwrap();
            let Some(show) = &s.show else { return };
            let Some(idx) = s.selected_cue_idx else { return };
            let Some(cue) = show.cues.get(idx).cloned() else { return };

            let on_air = s.run.playing_cue_idx.and_then(|i| show.cues.get(i)).is_some();
            let n = show.cues.len();
            let lead_ms = show.show.sync.start_lead_ms.max(250) as u64;
            let target_layer = idle_layer(s.run.active_layer);
            // Crossfade when something is on air. The incoming cue's fade-in is
            // the single knob for both fade-from-black and crossfade duration;
            // never below 40ms (a near-cut, but glitch-free).
            let crossfade_ms = on_air.then(|| cue.fade_in_ms.max(40));

            // Did STANDBY already preload this exact cue on this layer?
            let preloaded = s.run.standby.as_ref() == Some(&(cue.id.clone(), target_layer));

            s.run.playing_cue_idx = Some(idx);
            s.run.active_layer = Some(target_layer);
            // Standby is consumed (or invalidated by an out-of-order GO).
            s.run.standby = None;
            // The layer the *next* GO targets is the one we're now fading
            // away from; hold the next STANDBY off it until the crossfade has
            // finished (loading resets a layer's alpha to 0, which would cut
            // the outgoing video). A cut (no crossfade) frees it immediately.
            s.run.idle_free_utc_ms = match crossfade_ms {
                Some(ms) => now_utc_ms() + lead_ms + ms as u64 + 300,
                None => 0,
            };
            s.selected_cue_idx = Some((idx + 1).min(n.saturating_sub(1)));
            s.push_log(format!(
                "GO cue {} on layer {target_layer:?}{}",
                cue.id,
                if preloaded { " (preloaded)" } else { " (cold load)" }
            ));
            (cue, target_layer, crossfade_ms, lead_ms, preloaded)
        };
        let (cue, layer, crossfade_ms, lead_ms, preloaded) = plan;

        // Only send LOAD_CUE when the media isn't already prerolled from a
        // STANDBY — otherwise we'd trigger a redundant cold decode and lose
        // the head start entirely.
        if !preloaded {
            broadcast(&self.state, ControllerMsg::LoadCue(load_cue_for(&cue, layer)));
        }
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

    /// Preload the selected cue onto the layer the next GO will use, so that
    /// GO starts instantly. Debounced (so scrolling doesn't thrash clients)
    /// and gated on the target layer being free (not mid-crossfade). Idempotent
    /// — re-preloads only when the desired (cue, layer) actually changes.
    fn maybe_standby(&self) {
        if self.selected_since.elapsed().as_millis() < SELECTION_SETTLE_MS {
            return;
        }
        let plan = {
            let s = self.state.lock().unwrap();
            let Some(show) = &s.show else { return };
            let Some(idx) = s.selected_cue_idx else { return };
            let Some(cue) = show.cues.get(idx).cloned() else { return };
            let target = idle_layer(s.run.active_layer);
            // Already pre-loaded on the right layer, or the layer is still
            // finishing a crossfade-out.
            if s.run.standby.as_ref() == Some(&(cue.id.clone(), target))
                || now_utc_ms() < s.run.idle_free_utc_ms
            {
                return;
            }
            (cue, target)
        };
        let (cue, target) = plan;
        {
            let mut s = self.state.lock().unwrap();
            s.run.standby = Some((cue.id.clone(), target));
            s.push_log(format!("STANDBY cue {} on layer {target:?}", cue.id));
        }
        broadcast(&self.state, ControllerMsg::Standby(load_cue_for(&cue, target)));
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
        self.state.lock().unwrap().run = Default::default();
        broadcast(
            &self.state,
            ControllerMsg::Fade(FadeCmd {
                duration_ms: cuemesh2_shared::show::DEFAULT_FADE_MS,
            }),
        );
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

    /// Render the editor and act on its result.
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
            // Save to the known path, or fall through to Save-as when unset.
            EditorAction::Save => match self.editor.save_path() {
                Some(path) => self.save_editor_to(path),
                None => self.save_dialog.save_file(),
            },
            EditorAction::SaveAs => self.save_dialog.save_file(),
            EditorAction::Close => self.editor.open = false,
        }
    }

    /// Build the draft, write it to `path`, remember the path, and push it live.
    fn save_editor_to(&mut self, path: PathBuf) {
        let show = self.editor.build();
        match show.save(&path) {
            Ok(()) => {
                self.editor.set_path(&path);
                self.state.lock().unwrap().show_path = Some(path.clone());
                self.apply_show(show);
                self.editor.set_status(format!("saved to {}", path.display()));
            }
            Err(e) => self.editor.set_status(format!("save failed: {e}")),
        }
    }

    /// Advance both file dialogs and act on a completed pick. Call last in the
    /// frame so the dialog window renders on top of the panels.
    fn drive_dialogs(&mut self, ctx: &egui::Context) {
        self.open_dialog.update(ctx);
        if let Some(path) = self.open_dialog.take_selected() {
            self.load_show_from_path(path);
        }
        self.save_dialog.update(ctx);
        if let Some(path) = self.save_dialog.take_selected() {
            self.save_editor_to(path);
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

        let (
            show_summary,
            cues,
            playing_idx,
            selected,
            clients,
            preflight_running,
            log_tail,
            update_manifest,
            self_update,
        ) = {
            let s = self.state.lock().unwrap();
            let show_summary = match &s.show {
                Some(sf) => format!("{}  ({} cues)", sf.show.title, sf.cues.len()),
                None => "(no show loaded)".into(),
            };
            let cues: Vec<_> = match &s.show {
                Some(sf) => sf
                    .cues
                    .iter()
                    .map(|c| (c.name.clone(), c.kind, c.fade_in_ms))
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
                s.update_manifest.clone(),
                s.self_update.clone(),
            )
        };

        // Track selection changes to debounce speculative preloading, then
        // (in run mode) preload the settled selection so GO is instant.
        if self.last_selected != selected {
            self.last_selected = selected;
            self.selected_since = Instant::now();
        }
        if !self.editor.open {
            self.maybe_standby();
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("CueMesh2 Controller");
                ui.separator();
                ui.label(show_summary);
                ui.separator();
                if ui.button(format!("{}  Open show", icon::FOLDER_OPEN)).clicked() {
                    self.open_dialog.select_file();
                }
                ui.separator();
                if ui
                    .add_enabled(
                        !preflight_running,
                        egui::Button::new(format!("{}  Preflight", icon::CHECK_CIRCLE)),
                    )
                    .clicked()
                {
                    preflight::start_preflight(&self.state);
                }
                if preflight_running {
                    ui.spinner();
                }
                ui.separator();
                let ts_label = if self.testscreen_on {
                    format!("{}  Hide testscreen", icon::EYE_SLASH)
                } else {
                    format!("{}  Testscreen", icon::EYE)
                };
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
                    if ui.button(format!("{}  New show", icon::FILE_PLUS)).clicked() {
                        self.open_editor(true);
                    }
                    if ui.button(format!("{}  Edit show", icon::PENCIL_SIMPLE)).clicked() {
                        self.open_editor(false);
                    }
                }
                ui.separator();
                // Two independent operator actions: update the controller
                // itself (needs internet, also refreshes the client bundle),
                // then optionally update the fleet from the local bundle.
                match &self_update {
                    SelfUpdate::Idle | SelfUpdate::Failed(_) => {
                        if ui
                            .button(format!("{}  Update controller", icon::DOWNLOAD_SIMPLE))
                            .on_hover_text(format!(
                                "v{} — check the release server for a newer version",
                                update::APP_VERSION
                            ))
                            .clicked()
                        {
                            update::start_self_update(&self.state);
                        }
                        if let SelfUpdate::Failed(e) = &self_update {
                            ui.colored_label(egui::Color32::from_rgb(220, 70, 70), icon::WARNING)
                                .on_hover_text(e);
                        }
                    }
                    SelfUpdate::Working(what) => {
                        ui.spinner();
                        ui.label(what);
                    }
                    SelfUpdate::ReadyToRestart(v) => {
                        if ui
                            .button(format!("{}  Restart into v{v}", icon::ARROW_CLOCKWISE))
                            .clicked()
                        {
                            update::restart_into_staged(&self.state);
                        }
                    }
                }
                if let Some(m) = &update_manifest {
                    let outdated = clients.iter().any(|c| update::available_for(m, c).is_some());
                    if outdated
                        && ui
                            .button(format!("{}  Update fleet (v{})", icon::UPLOAD_SIMPLE, m.version))
                            .on_hover_text("Stage the new client binary on every out-of-date client")
                            .clicked()
                    {
                        update::update_fleet(&self.state);
                    }
                    let staged = clients
                        .iter()
                        .filter(|c| matches!(c.update, ClientUpdate::Staged(_)))
                        .count();
                    if staged > 0
                        && ui
                            .button(format!("{}  Apply fleet ({staged} staged)", icon::CHECK_FAT))
                            .on_hover_text("Restart every staged client into the new version (idle clients only)")
                            .clicked()
                    {
                        update::apply_fleet(&self.state);
                    }
                }
            });
        });

        // Edit mode takes over the body; run-mode panels are hidden.
        if self.editor.open {
            self.editor_panel(ctx);
            self.drive_dialogs(ctx);
            return;
        }

        egui::SidePanel::left("cues").min_width(300.0).show(ctx, |ui| {
            ui.heading("Cues");
            ui.separator();
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, (name, kind, _fade_in)) in cues.iter().enumerate() {
                    let is_sel = selected == Some(i);
                    let on_air = playing_idx == Some(i);
                    let marker = if on_air { icon::PLAY } else { " " };
                    let kind_icon = match kind {
                        CueKind::Video => icon::FILM_STRIP,
                        CueKind::Image => icon::IMAGE,
                        CueKind::Color => icon::PALETTE,
                    };
                    let label = format!("{marker} {i:>3}  {kind_icon} {name}");
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
                    let dot_color = if stale {
                        egui::Color32::from_rgb(220, 70, 70)
                    } else {
                        egui::Color32::from_rgb(80, 200, 110)
                    };
                    ui.horizontal(|ui| {
                        ui.colored_label(dot_color, icon::CIRCLE);
                        ui.label(format!("{}  ({})", c.name, &c.client_id[..8.min(c.client_id.len())]));
                    });
                    ui.label(format!("addr: {}   state: {:?}", c.addr, c.state));
                    if let Some(cue) = &c.current_cue {
                        ui.label(format!("cue: {cue}  @ {}", fmt_ms(c.position_ms)));
                    }
                    ui.label(format!(
                        "offset: {}   drift: {}",
                        c.offset_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into()),
                        c.last_drift_ms.map(|v| format!("{v} ms")).unwrap_or_else(|| "—".into()),
                    ));
                    {
                        let version = if c.app_version.is_empty() { "?" } else { &c.app_version };
                        ui.label(format!("version: {version}"))
                            .on_hover_text(if c.target_triple.is_empty() {
                                "platform unknown (pre-update client)".to_string()
                            } else {
                                c.target_triple.clone()
                            });
                        let available = update_manifest
                            .as_ref()
                            .and_then(|m| update::available_for(m, c).map(|_| m.version.clone()));
                        match (&c.update, available) {
                            (ClientUpdate::Pushing, _) => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("sending update…");
                                });
                            }
                            (ClientUpdate::Applying, _) => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("restarting into new version…");
                                });
                            }
                            (ClientUpdate::Staged(v), _) => {
                                if ui
                                    .button(format!("{}  Apply v{v} (restart)", icon::CHECK_FAT))
                                    .on_hover_text("Client refuses unless idle")
                                    .clicked()
                                {
                                    update::send_apply(&self.state, &c.client_id);
                                }
                            }
                            (ClientUpdate::Failed(e), available) => {
                                ui.colored_label(
                                    egui::Color32::from_rgb(220, 70, 70),
                                    format!("{} update failed", icon::WARNING),
                                )
                                .on_hover_text(e);
                                if let Some(v) = available {
                                    if ui.button(format!("{}  Retry update to v{v}", icon::UPLOAD_SIMPLE)).clicked() {
                                        update::push_update_to(&self.state, c.client_id.clone());
                                    }
                                }
                            }
                            (ClientUpdate::None, Some(v)) => {
                                if ui
                                    .button(format!("{}  Update to v{v}", icon::UPLOAD_SIMPLE))
                                    .on_hover_text("Stage the new binary on this client; apply separately")
                                    .clicked()
                                {
                                    update::push_update_to(&self.state, c.client_id.clone());
                                }
                            }
                            (ClientUpdate::None, None) => {}
                        }
                    }
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
                                ui.label(format!("   {} {} ({what})", icon::WARNING, path.display()));
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
                if ui.button(format!("{}  PREV", icon::CARET_UP)).clicked() {
                    self.move_selection(-1);
                }
                if ui.button(format!("{}  NEXT", icon::CARET_DOWN)).clicked() {
                    self.move_selection(1);
                }
                ui.separator();
                if ui.button(format!("{}  PAUSE", icon::PAUSE)).clicked() {
                    broadcast(&self.state, ControllerMsg::Pause);
                }
                if ui.button(format!("{}  RESUME", icon::PLAY)).clicked() {
                    broadcast(&self.state, ControllerMsg::Resume);
                }
                ui.separator();
                if ui.button(format!("{}  BLACKOUT", icon::MOON)).clicked() {
                    self.blackout();
                }
                if ui.button(format!("{}  STOP", icon::STOP)).clicked() {
                    self.stop_all();
                }
            });
            ui.label("space = GO    up / down arrows = select cue");
            ui.separator();
            ui.heading("Log");
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for line in log_tail.iter().rev() {
                    ui.monospace(line);
                }
            });
        });

        self.drive_dialogs(ctx);
    }
}
