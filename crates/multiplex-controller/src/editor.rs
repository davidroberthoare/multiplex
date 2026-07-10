//! Show editor: create and edit `*.cuemesh.toml` shows in-app.
//!
//! The editor works on a *draft* made of `String`/primitive fields so egui
//! widgets can bind to them directly, converting to/from [`ShowFile`] on enter
//! and on build. Cues are edited in a table; media files come from a scan of
//! the show's `media_root` (keeping paths relative, as the wire protocol and
//! validation require) with a free-text fallback. Colour cues use a colour
//! picker instead of a file.
//!
//! File open/save is handled by the host app via `egui-file-dialog` (a pure
//! egui picker — no native GTK/Qt dependency), so this module only owns the
//! draft and the current path.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use egui_phosphor::regular as icon;

use cuemesh2_shared::show::{
    format_hex_color, parse_hex_color, Cue, CueKind, DropoutPolicy, EndAction, Poster, Show,
    ShowFile, SyncConfig, SyncCorrection,
};

use crate::util::expand_tilde;

/// What the editor asks the host app to do after a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorAction {
    /// Nothing this frame.
    None,
    /// Push the current draft into the running show (state + SHOW_SYNC).
    Apply,
    /// Write to the current path (host opens a Save-as dialog if unset).
    Save,
    /// Always pick a new path via the host's Save-as dialog.
    SaveAs,
    /// Leave edit mode, discarding unsaved draft changes.
    Close,
}

/// One editable cue row (all fields egui can bind to directly). In/out points
/// are edited in seconds for the operator, stored as ms on the wire.
#[derive(Debug, Clone)]
struct CueDraft {
    /// Hidden, auto-generated; kept stable across an edit session.
    id: String,
    name: String,
    kind: CueKind,
    file: String,
    /// Solid colour for `Color` cues.
    color: [u8; 3],
    fade_in_ms: u32,
    /// In-point in seconds.
    in_s: f32,
    /// Whether an explicit out-point is set (else play to natural end).
    has_out: bool,
    /// Out-point in seconds (only meaningful when `has_out`).
    out_s: f32,
    loops: bool,
    on_end: EndAction,
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
            color: [0, 0, 0],
            fade_in_ms: 0,
            in_s: 0.0,
            has_out: false,
            out_s: 0.0,
            loops: false,
            on_end: EndAction::default(),
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
            color: c.color.as_deref().map(parse_hex_color).unwrap_or([0, 0, 0]),
            fade_in_ms: c.fade_in_ms,
            in_s: c.in_ms as f32 / 1000.0,
            has_out: c.out_ms.is_some(),
            out_s: c.out_ms.map(|o| o as f32 / 1000.0).unwrap_or(0.0),
            loops: c.loops,
            on_end: c.on_end,
            notes: c.notes.clone().unwrap_or_default(),
        }
    }

    fn to_cue(&self) -> Cue {
        let notes = self.notes.trim();
        let is_color = self.kind == CueKind::Color;
        Cue {
            id: self.id.trim().to_string(),
            name: self.name.trim().to_string(),
            kind: self.kind,
            file: if is_color {
                PathBuf::new()
            } else {
                PathBuf::from(self.file.trim())
            },
            color: if is_color {
                Some(format_hex_color(self.color))
            } else {
                None
            },
            fade_in_ms: self.fade_in_ms,
            in_ms: (self.in_s.max(0.0) * 1000.0) as u32,
            out_ms: (self.has_out && self.kind == CueKind::Video)
                .then(|| (self.out_s.max(0.0) * 1000.0) as u32),
            loops: self.loops && self.kind == CueKind::Video,
            on_end: self.on_end,
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
    max_drift_ms: u32,
    start_lead_ms: u32,
    rate_min: f32,
    rate_max: f32,
    hard_seek_threshold_ms: u32,
    sync_interval_ms: u32,
    cues: Vec<CueDraft>,
    /// Idle poster (shown by clients on connect / between cues).
    poster_enabled: bool,
    poster_kind: CueKind,
    poster_file: String,
    /// Path last saved to / loaded from (shown in the header, target of "Save").
    path: String,
    /// Relative paths of media files found under `media_root`.
    media_files: Vec<String>,
    /// Inline validation / save feedback.
    status: String,
    /// Monotonic counter for generating hidden cue ids.
    id_counter: u32,
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
        self.max_drift_ms = show.show.sync.max_drift_ms;
        self.start_lead_ms = show.show.sync.start_lead_ms;
        self.rate_min = show.show.sync.correction.rate_min;
        self.rate_max = show.show.sync.correction.rate_max;
        self.hard_seek_threshold_ms = show.show.sync.correction.hard_seek_threshold_ms;
        self.sync_interval_ms = show.show.sync.correction.sync_interval_ms;
        self.poster_enabled = show.show.poster.is_some();
        self.poster_kind = show.show.poster.as_ref().map(|p| p.kind).unwrap_or(CueKind::Image);
        self.poster_file = show
            .show
            .poster
            .as_ref()
            .map(|p| p.file.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.cues = show.cues.iter().map(CueDraft::from_cue).collect();
        self.path = path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.status.clear();
        self.id_counter = 0;
        // Backfill ids for any rows that arrived without one.
        for i in 0..self.cues.len() {
            if self.cues[i].id.trim().is_empty() {
                self.cues[i].id = self.next_id();
            }
        }
        self.refresh_media_files();
        self.open = true;
    }

    /// Assemble a [`ShowFile`] from the current draft. Ensures every cue has a
    /// unique, non-empty id (ids are hidden, so the operator never sees this).
    pub fn build(&self) -> ShowFile {
        let mut seen = HashSet::new();
        let mut next = 1u32;
        let cues = self
            .cues
            .iter()
            .map(|d| {
                let mut c = d.to_cue();
                if c.id.is_empty() || !seen.insert(c.id.clone()) {
                    loop {
                        let cand = format!("cue-{next:04}");
                        next += 1;
                        if seen.insert(cand.clone()) {
                            c.id = cand;
                            break;
                        }
                    }
                }
                c
            })
            .collect();
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
                poster: (self.poster_enabled && !self.poster_file.trim().is_empty()).then(|| {
                    Poster {
                        kind: self.poster_kind,
                        file: PathBuf::from(self.poster_file.trim()),
                    }
                }),
            },
            cues,
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

    /// Record the path a Save-as dialog chose.
    pub fn set_path(&mut self, path: &Path) {
        self.path = path.to_string_lossy().into_owned();
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
    }

    /// A fresh hidden cue id.
    fn next_id(&mut self) -> String {
        self.id_counter += 1;
        format!("cue-{:04}", self.id_counter)
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
            if ui.button(format!("{}  Apply", icon::ARROW_FAT_LINES_UP)).clicked() {
                action = EditorAction::Apply;
            }
            if ui.button(format!("{}  Save", icon::FLOPPY_DISK)).clicked() {
                action = EditorAction::Save;
            }
            if ui.button(format!("{}  Save as…", icon::FLOPPY_DISK_BACK)).clicked() {
                action = EditorAction::SaveAs;
            }
            if ui.button(format!("{}  Close", icon::X)).clicked() {
                action = EditorAction::Close;
            }
        });
        ui.horizontal(|ui| {
            ui.small(if self.path.is_empty() {
                "(unsaved — use Save as…)".to_string()
            } else {
                format!("file: {}", self.path)
            });
        });
        if !self.status.is_empty() {
            ui.colored_label(egui::Color32::from_rgb(220, 140, 60), &self.status);
        }
        ui.separator();

        egui::ScrollArea::vertical().show(ui, |ui| {
            self.show_meta(ui);
            ui.separator();
            self.show_cue_table(ui);
        });

        action
    }

    /// Top section: global show parameters.
    fn show_meta(&mut self, ui: &mut egui::Ui) {
        let media_files = self.media_files.clone();
        egui::Grid::new("show_meta")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                ui.label("Title");
                ui.text_edit_singleline(&mut self.title);
                ui.end_row();

                ui.label("Idle poster");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.poster_enabled, "show");
                    ui.add_enabled_ui(self.poster_enabled, |ui| {
                        egui::ComboBox::from_id_salt("poster_kind")
                            .selected_text(match self.poster_kind {
                                CueKind::Video => "Video",
                                _ => "Image",
                            })
                            .width(72.0)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut self.poster_kind, CueKind::Image, "Image");
                                ui.selectable_value(&mut self.poster_kind, CueKind::Video, "Video");
                            });
                        ui.add(egui::TextEdit::singleline(&mut self.poster_file).desired_width(180.0));
                        egui::ComboBox::from_id_salt("poster_pick")
                            .selected_text(icon::FOLDER_OPEN.to_string())
                            .width(24.0)
                            .show_ui(ui, |ui| {
                                for f in &media_files {
                                    if ui.selectable_label(false, f).clicked() {
                                        self.poster_file = f.clone();
                                    }
                                }
                            });
                    });
                });
                ui.end_row();

                ui.label("Media root");
                ui.horizontal(|ui| {
                    if ui.text_edit_singleline(&mut self.media_root).lost_focus() {
                        self.refresh_media_files();
                    }
                    if ui.button(format!("{}  Rescan", icon::ARROWS_CLOCKWISE)).clicked() {
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
            });
    }

    /// Bottom section: cues as an editable table with actions on the right.
    fn show_cue_table(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading(format!("Cues ({})", self.cues.len()));
            if ui.button(format!("{}  Add cue", icon::PLUS)).clicked() {
                let n = self.cues.len() + 1;
                let id = self.next_id();
                self.cues.push(CueDraft {
                    id,
                    name: format!("Cue {n}"),
                    kind: CueKind::Video,
                    ..Default::default()
                });
            }
        });

        let mut op: Option<RowOp> = None;
        let media_files = self.media_files.clone();
        let n = self.cues.len();
        egui::ScrollArea::horizontal().show(ui, |ui| {
        egui::Grid::new("cue_table")
            .num_columns(9)
            .striped(true)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                for h in ["Name", "Type", "Source", "In (s)", "Out (s)", "Loop", "On end", "Fade-in (ms)", ""] {
                    ui.strong(h);
                }
                ui.end_row();

                for (i, cue) in self.cues.iter_mut().enumerate() {
                    let is_video = cue.kind == CueKind::Video;
                    ui.add(egui::TextEdit::singleline(&mut cue.name).desired_width(130.0));

                    egui::ComboBox::from_id_salt(("kind", i))
                        .selected_text(match cue.kind {
                            CueKind::Video => "Video",
                            CueKind::Image => "Image",
                            CueKind::Color => "Color",
                        })
                        .width(72.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut cue.kind, CueKind::Video, "Video");
                            ui.selectable_value(&mut cue.kind, CueKind::Image, "Image");
                            ui.selectable_value(&mut cue.kind, CueKind::Color, "Color");
                        });

                    // Source: a colour swatch for colour cues, else file + picker.
                    if cue.kind == CueKind::Color {
                        ui.horizontal(|ui| {
                            ui.color_edit_button_srgb(&mut cue.color);
                            ui.label(format_hex_color(cue.color));
                        });
                    } else {
                        ui.horizontal(|ui| {
                            ui.add(egui::TextEdit::singleline(&mut cue.file).desired_width(150.0));
                            egui::ComboBox::from_id_salt(("file", i))
                                .selected_text(icon::FOLDER_OPEN.to_string())
                                .width(24.0)
                                .show_ui(ui, |ui| {
                                    for f in &media_files {
                                        if ui.selectable_label(false, f).clicked() {
                                            cue.file = f.clone();
                                        }
                                    }
                                });
                        });
                    }

                    // In / Out points and loop only apply to seekable video.
                    ui.add_enabled(
                        is_video,
                        egui::DragValue::new(&mut cue.in_s).speed(0.1).range(0.0..=100_000.0).suffix(" s"),
                    );
                    ui.horizontal(|ui| {
                        ui.add_enabled(is_video, egui::Checkbox::without_text(&mut cue.has_out));
                        ui.add_enabled(
                            is_video && cue.has_out,
                            egui::DragValue::new(&mut cue.out_s).speed(0.1).range(0.0..=100_000.0).suffix(" s"),
                        );
                    });
                    ui.add_enabled(is_video, egui::Checkbox::without_text(&mut cue.loops));

                    egui::ComboBox::from_id_salt(("onend", i))
                        .selected_text(match cue.on_end {
                            EndAction::Cut => "Cut",
                            EndAction::Freeze => "Freeze",
                            EndAction::Fade => "Fade",
                        })
                        .width(76.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut cue.on_end, EndAction::Cut, "Cut");
                            ui.selectable_value(&mut cue.on_end, EndAction::Freeze, "Freeze");
                            ui.selectable_value(&mut cue.on_end, EndAction::Fade, "Fade");
                        });

                    ui.add(egui::DragValue::new(&mut cue.fade_in_ms).range(0..=30_000));

                    ui.horizontal(|ui| {
                        if ui.add_enabled(i > 0, egui::Button::new(icon::ARROW_UP)).clicked() {
                            op = Some(RowOp::MoveUp(i));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new(icon::ARROW_DOWN)).clicked() {
                            op = Some(RowOp::MoveDown(i));
                        }
                        if ui.button(icon::COPY).clicked() {
                            op = Some(RowOp::Duplicate(i));
                        }
                        if ui.button(icon::TRASH).clicked() {
                            op = Some(RowOp::Delete(i));
                        }
                    });
                    ui.end_row();
                }
            });
        });

        if let Some(op) = op {
            self.apply_row_op(op);
        }
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
                    dup.id = self.next_id();
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
            kind: CueKind::Video,
            file: PathBuf::from("clip.mp4"),
            color: None,
            fade_in_ms: 500,
            in_ms: 2500,
            out_ms: Some(15_000),
            loops: true,
            on_end: EndAction::Freeze,
            notes: Some("first".into()),
        });
        sf.cues.push(Cue {
            id: "c2".into(),
            name: "Black".into(),
            kind: CueKind::Color,
            file: PathBuf::new(),
            color: Some("#101820".into()),
            fade_in_ms: 1200,
            in_ms: 0,
            out_ms: None,
            loops: false,
            on_end: EndAction::default(),
            notes: None,
        });

        let mut ed = EditorState::default();
        ed.enter(Some(&sf), Some(Path::new("/tmp/show.cuemesh.toml")));
        let built = ed.build();

        assert_eq!(built.show.title, "Editor Test");
        assert_eq!(built.cues.len(), 2);
        assert_eq!(built.cues[0].in_ms, 2500);
        assert_eq!(built.cues[0].out_ms, Some(15_000));
        assert!(built.cues[0].loops);
        assert_eq!(built.cues[0].on_end, EndAction::Freeze);
        // Colour cue: in/out/loop are stripped since they only apply to video.
        assert_eq!(built.cues[1].out_ms, None);
        assert!(!built.cues[1].loops);
        assert_eq!(built.cues[0].id, "c1");
        assert_eq!(built.cues[0].kind, CueKind::Video);
        assert_eq!(built.cues[0].file, PathBuf::from("clip.mp4"));
        assert_eq!(built.cues[1].kind, CueKind::Color);
        assert_eq!(built.cues[1].color.as_deref(), Some("#101820"));
        assert!(built.cues[1].file.as_os_str().is_empty());
        assert_eq!(ed.save_path(), Some(PathBuf::from("/tmp/show.cuemesh.toml")));
        built.validate().unwrap();
    }

    #[test]
    fn build_fills_missing_ids_uniquely() {
        let mut ed = EditorState::default();
        ed.enter(None, None);
        ed.cues.push(CueDraft {
            id: String::new(),
            file: "a.mp4".into(),
            ..Default::default()
        });
        ed.cues.push(CueDraft {
            id: String::new(),
            file: "b.mp4".into(),
            ..Default::default()
        });
        let built = ed.build();
        assert_eq!(built.cues.len(), 2);
        assert_ne!(built.cues[0].id, built.cues[1].id);
        assert!(!built.cues[0].id.is_empty());
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
        ed.apply_row_op(RowOp::Delete(1));
        assert_eq!(ed.cues.len(), 2);
    }
}
