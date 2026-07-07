//! Show editor: create and edit `*.cuemesh.toml` shows in-app.
//!
//! The editor works on a *draft* made entirely of `String`/primitive fields so
//! egui widgets can bind to it directly. It converts to/from [`ShowFile`] on
//! enter and on build. Cue media files are picked from a scan of the show's
//! `media_root` (which keeps every path relative to the root, as the wire
//! protocol and validation require) with a free-text fallback.
//!
//! We deliberately avoid a native file-picker dependency (`rfd` links GTK on
//! Linux, which fights the project's "no system GTK/Qt dep" portability goal).
//! The save path is a plain text field; media files come from the root scan.

use std::path::{Path, PathBuf};

use cuemesh2_shared::show::{
    Cue, CueKind, DropoutPolicy, Show, ShowFile, ShowSettings, SyncConfig, SyncCorrection,
};

use crate::util::expand_tilde;

/// What the editor asks the host app to do after a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAction {
    /// Nothing this frame.
    None,
    /// Push the current draft into the running show (state + SHOW_SYNC).
    Apply,
    /// Write the draft to the path in the editor, then apply.
    Save,
    /// Leave edit mode, discarding unsaved draft changes.
    Close,
}

/// One editable cue row (all fields egui can bind to directly).
#[derive(Debug, Clone)]
struct CueDraft {
    id: String,
    name: String,
    kind: CueKind,
    file: String,
    fade_in_ms: u32,
    fade_out_ms: u32,
    crossfade_to_next_ms: u32,
    notes: String,
}

impl Default for CueDraft {
    fn default() -> Self {
        // `CueKind` has no `Default` in the shared crate; video is the norm.
        Self {
            id: String::new(),
            name: String::new(),
            kind: CueKind::Video,
            file: String::new(),
            fade_in_ms: 0,
            fade_out_ms: 0,
            crossfade_to_next_ms: 0,
            notes: String::new(),
        }
    }
}

impl CueDraft {
    fn from_cue(c: &Cue) -> Self {
        Self {
            id: c.id.clone(),
            name: c.name.clone(),
            kind: c.kind,
            file: c.file.to_string_lossy().into_owned(),
            fade_in_ms: c.fade_in_ms,
            fade_out_ms: c.fade_out_ms,
            crossfade_to_next_ms: c.crossfade_to_next_ms,
            notes: c.notes.clone().unwrap_or_default(),
        }
    }

    fn to_cue(&self) -> Cue {
        let notes = self.notes.trim();
        Cue {
            id: self.id.trim().to_string(),
            name: self.name.trim().to_string(),
            kind: self.kind,
            file: PathBuf::from(self.file.trim()),
            fade_in_ms: self.fade_in_ms,
            fade_out_ms: self.fade_out_ms,
            crossfade_to_next_ms: self.crossfade_to_next_ms,
            notes: if notes.is_empty() {
                None
            } else {
                Some(notes.to_string())
            },
        }
    }
}

/// Editable draft of a whole show, owned by the controller app (never behind
/// the shared mutex — egui is single-threaded).
#[derive(Debug, Default)]
pub struct EditorState {
    pub open: bool,
    title: String,
    version: u32,
    media_root: String,
    dropout: DropoutPolicy,
    default_fade_ms: u32,
    max_drift_ms: u32,
    start_lead_ms: u32,
    rate_min: f32,
    rate_max: f32,
    hard_seek_threshold_ms: u32,
    sync_interval_ms: u32,
    cues: Vec<CueDraft>,
    /// Where "Save" writes.
    path: String,
    /// Relative paths of media files found under `media_root`.
    media_files: Vec<String>,
    /// Inline validation / save feedback.
    status: String,
}

/// A deferred structural edit to the cue list, applied after the row loop so
/// we never mutate the vec while iterating it.
enum RowOp {
    MoveUp(usize),
    MoveDown(usize),
    Duplicate(usize),
    Delete(usize),
}

impl EditorState {
    /// Enter edit mode, seeding the draft from an existing show (or a blank
    /// one) and the path it was loaded from.
    pub fn enter(&mut self, show: Option<&ShowFile>, path: Option<&Path>) {
        let show = match show {
            Some(s) => s.clone(),
            None => ShowFile::new_empty("Untitled Show"),
        };
        self.title = show.show.title.clone();
        self.version = show.show.version;
        self.media_root = show.show.media_root.to_string_lossy().into_owned();
        self.dropout = show.show.dropout_policy;
        self.default_fade_ms = show.show.settings.default_fade_ms;
        self.max_drift_ms = show.show.sync.max_drift_ms;
        self.start_lead_ms = show.show.sync.start_lead_ms;
        self.rate_min = show.show.sync.correction.rate_min;
        self.rate_max = show.show.sync.correction.rate_max;
        self.hard_seek_threshold_ms = show.show.sync.correction.hard_seek_threshold_ms;
        self.sync_interval_ms = show.show.sync.correction.sync_interval_ms;
        self.cues = show.cues.iter().map(CueDraft::from_cue).collect();
        self.path = path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.status.clear();
        self.refresh_media_files();
        self.open = true;
    }

    /// Assemble a [`ShowFile`] from the current draft (unvalidated).
    pub fn build(&self) -> ShowFile {
        ShowFile {
            show: Show {
                title: self.title.trim().to_string(),
                version: self.version.max(1),
                media_root: PathBuf::from(self.media_root.trim()),
                dropout_policy: self.dropout,
                sync: SyncConfig {
                    max_drift_ms: self.max_drift_ms,
                    start_lead_ms: self.start_lead_ms,
                    correction: SyncCorrection {
                        rate_min: self.rate_min,
                        rate_max: self.rate_max,
                        hard_seek_threshold_ms: self.hard_seek_threshold_ms,
                        sync_interval_ms: self.sync_interval_ms,
                    },
                },
                settings: ShowSettings {
                    default_fade_ms: self.default_fade_ms,
                },
            },
            cues: self.cues.iter().map(CueDraft::to_cue).collect(),
        }
    }

    /// The path "Save" writes to (may be empty).
    pub fn save_path(&self) -> Option<PathBuf> {
        let p = self.path.trim();
        if p.is_empty() {
            None
        } else {
            Some(PathBuf::from(p))
        }
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
    }

    /// Scan `media_root` (recursively, shallow theatre trees) for playable
    /// files and record their paths relative to the root.
    fn refresh_media_files(&mut self) {
        let root = expand_tilde(Path::new(self.media_root.trim()));
        let mut out = Vec::new();
        collect_media(&root, &root, 0, &mut out);
        out.sort();
        self.media_files = out;
    }

    /// Render the editor. Returns the action the host app should take.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> EditorAction {
        let mut action = EditorAction::None;

        ui.horizontal(|ui| {
            ui.heading("Show editor");
            if ui.button("Apply to running show").clicked() {
                action = EditorAction::Apply;
            }
            if ui.button("Save to file").clicked() {
                action = EditorAction::Save;
            }
            if ui.button("Close").clicked() {
                action = EditorAction::Close;
            }
        });
        if !self.status.is_empty() {
            ui.colored_label(egui::Color32::from_rgb(220, 140, 60), &self.status);
        }
        ui.separator();

        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("show_meta")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Title");
                    ui.text_edit_singleline(&mut self.title);
                    ui.end_row();

                    ui.label("Media root");
                    ui.horizontal(|ui| {
                        if ui.text_edit_singleline(&mut self.media_root).lost_focus() {
                            self.refresh_media_files();
                        }
                        if ui.button("Rescan").clicked() {
                            self.refresh_media_files();
                        }
                    });
                    ui.end_row();

                    ui.label("Dropout policy");
                    egui::ComboBox::from_id_salt("dropout")
                        .selected_text(format!("{:?}", self.dropout))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.dropout, DropoutPolicy::Continue, "Continue");
                            ui.selectable_value(&mut self.dropout, DropoutPolicy::Freeze, "Freeze");
                            ui.selectable_value(&mut self.dropout, DropoutPolicy::Black, "Black");
                        });
                    ui.end_row();

                    ui.label("Default fade (ms)");
                    ui.add(egui::DragValue::new(&mut self.default_fade_ms).range(0..=30_000));
                    ui.end_row();

                    ui.label("Max drift (ms)");
                    ui.add(egui::DragValue::new(&mut self.max_drift_ms).range(0..=2000));
                    ui.end_row();

                    ui.label("Start lead (ms)");
                    ui.add(egui::DragValue::new(&mut self.start_lead_ms).range(0..=5000));
                    ui.end_row();

                    ui.label("Rate min / max");
                    ui.horizontal(|ui| {
                        ui.add(egui::DragValue::new(&mut self.rate_min).speed(0.001).range(0.5..=1.0));
                        ui.add(egui::DragValue::new(&mut self.rate_max).speed(0.001).range(1.0..=1.5));
                    });
                    ui.end_row();

                    ui.label("Hard-seek threshold (ms)");
                    ui.add(egui::DragValue::new(&mut self.hard_seek_threshold_ms).range(50..=5000));
                    ui.end_row();

                    ui.label("Sync interval (ms)");
                    ui.add(egui::DragValue::new(&mut self.sync_interval_ms).range(200..=10_000));
                    ui.end_row();

                    ui.label("Save path");
                    ui.text_edit_singleline(&mut self.path);
                    ui.end_row();
                });

            ui.separator();
            ui.horizontal(|ui| {
                ui.heading(format!("Cues ({})", self.cues.len()));
                if ui.button("＋ Add cue").clicked() {
                    let n = self.cues.len() + 1;
                    self.cues.push(CueDraft {
                        id: format!("cue-{n:03}"),
                        name: format!("Cue {n}"),
                        kind: CueKind::Video,
                        ..Default::default()
                    });
                }
            });

            let mut op: Option<RowOp> = None;
            let media_files = self.media_files.clone();
            for (i, cue) in self.cues.iter_mut().enumerate() {
                ui.push_id(i, |ui| {
                    ui.group(|ui| {
                        ui.horizontal(|ui| {
                            ui.strong(format!("#{i}"));
                            if ui.small_button("↑").clicked() {
                                op = Some(RowOp::MoveUp(i));
                            }
                            if ui.small_button("↓").clicked() {
                                op = Some(RowOp::MoveDown(i));
                            }
                            if ui.small_button("⧉ dup").clicked() {
                                op = Some(RowOp::Duplicate(i));
                            }
                            if ui.small_button("🗑 del").clicked() {
                                op = Some(RowOp::Delete(i));
                            }
                        });
                        egui::Grid::new("cue_grid")
                            .num_columns(2)
                            .spacing([10.0, 4.0])
                            .show(ui, |ui| {
                                ui.label("ID");
                                ui.text_edit_singleline(&mut cue.id);
                                ui.end_row();
                                ui.label("Name");
                                ui.text_edit_singleline(&mut cue.name);
                                ui.end_row();
                                ui.label("Type");
                                egui::ComboBox::from_id_salt("kind")
                                    .selected_text(format!("{:?}", cue.kind))
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(&mut cue.kind, CueKind::Video, "Video");
                                        ui.selectable_value(&mut cue.kind, CueKind::Image, "Image");
                                    });
                                ui.end_row();
                                ui.label("File");
                                ui.horizontal(|ui| {
                                    ui.text_edit_singleline(&mut cue.file);
                                    egui::ComboBox::from_id_salt("file_pick")
                                        .selected_text("pick…")
                                        .show_ui(ui, |ui| {
                                            for f in &media_files {
                                                if ui.selectable_label(false, f).clicked() {
                                                    cue.file = f.clone();
                                                }
                                            }
                                        });
                                });
                                ui.end_row();
                                ui.label("Fade in / out (ms)");
                                ui.horizontal(|ui| {
                                    ui.add(egui::DragValue::new(&mut cue.fade_in_ms).range(0..=30_000));
                                    ui.add(egui::DragValue::new(&mut cue.fade_out_ms).range(0..=30_000));
                                });
                                ui.end_row();
                                ui.label("Crossfade to next (ms)");
                                ui.add(
                                    egui::DragValue::new(&mut cue.crossfade_to_next_ms)
                                        .range(0..=30_000),
                                );
                                ui.end_row();
                                ui.label("Notes");
                                ui.text_edit_singleline(&mut cue.notes);
                                ui.end_row();
                            });
                    });
                });
            }

            if let Some(op) = op {
                self.apply_row_op(op);
            }
        });

        action
    }

    fn apply_row_op(&mut self, op: RowOp) {
        match op {
            RowOp::MoveUp(i) if i > 0 => self.cues.swap(i, i - 1),
            RowOp::MoveUp(_) => {}
            RowOp::MoveDown(i) if i + 1 < self.cues.len() => self.cues.swap(i, i + 1),
            RowOp::MoveDown(_) => {}
            RowOp::Duplicate(i) => {
                if let Some(src) = self.cues.get(i).cloned() {
                    let mut dup = src;
                    dup.id = format!("{}-copy", dup.id);
                    self.cues.insert(i + 1, dup);
                }
            }
            RowOp::Delete(i) if i < self.cues.len() => {
                self.cues.remove(i);
            }
            RowOp::Delete(_) => {}
        }
    }
}

/// Common media extensions we surface in the picker.
const MEDIA_EXTS: &[&str] = &[
    "mp4", "webm", "mov", "mkv", "m4v", "avi", "png", "jpg", "jpeg", "bmp", "gif",
];

/// Recursively collect media files under `root`, storing paths relative to it.
/// Depth-limited so a huge media root can't stall the UI thread.
fn collect_media(root: &Path, dir: &Path, depth: usize, out: &mut Vec<String>) {
    if depth > 4 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_media(root, &path, depth + 1, out);
        } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            if MEDIA_EXTS.contains(&ext.to_ascii_lowercase().as_str()) {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draft_roundtrips_through_showfile() {
        let mut sf = ShowFile::new_empty("Editor Test");
        sf.show.media_root = PathBuf::from("/tmp/does-not-matter");
        sf.cues.push(Cue {
            id: "c1".into(),
            name: "Opening".into(),
            kind: CueKind::Image,
            file: PathBuf::from("pic.jpg"),
            fade_in_ms: 500,
            fade_out_ms: 250,
            crossfade_to_next_ms: 1000,
            notes: Some("first".into()),
        });

        let mut ed = EditorState::default();
        ed.enter(Some(&sf), Some(Path::new("/tmp/show.cuemesh.toml")));
        let built = ed.build();

        assert_eq!(built.show.title, "Editor Test");
        assert_eq!(built.cues.len(), 1);
        assert_eq!(built.cues[0].id, "c1");
        assert_eq!(built.cues[0].kind, CueKind::Image);
        assert_eq!(built.cues[0].file, PathBuf::from("pic.jpg"));
        assert_eq!(built.cues[0].crossfade_to_next_ms, 1000);
        assert_eq!(built.cues[0].notes.as_deref(), Some("first"));
        assert_eq!(ed.save_path(), Some(PathBuf::from("/tmp/show.cuemesh.toml")));
        built.validate().unwrap();
    }

    #[test]
    fn add_and_reorder_cues() {
        let mut ed = EditorState::default();
        ed.enter(None, None);
        ed.cues.push(CueDraft {
            id: "a".into(),
            file: "a.mp4".into(),
            ..Default::default()
        });
        ed.cues.push(CueDraft {
            id: "b".into(),
            file: "b.mp4".into(),
            ..Default::default()
        });
        ed.apply_row_op(RowOp::MoveDown(0));
        assert_eq!(ed.cues[0].id, "b");
        assert_eq!(ed.cues[1].id, "a");
        ed.apply_row_op(RowOp::Duplicate(0));
        assert_eq!(ed.cues.len(), 3);
        assert_eq!(ed.cues[1].id, "b-copy");
        ed.apply_row_op(RowOp::Delete(1));
        assert_eq!(ed.cues.len(), 2);
    }
}
