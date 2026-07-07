//! Show file format (`*.cuemesh.toml`).
//!
//! Load a show from disk with [`ShowFile::load`]. Validation checks unique cue
//! IDs, media file existence relative to `media_root`, and value ranges.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Duration (ms) used by the operator BLACKOUT command and the `black` dropout
/// policy. Formerly a per-show setting; now a fixed default — per-cue colour
/// cues cover deliberate fades to black/white.
pub const DEFAULT_FADE_MS: u32 = 1500;

#[derive(Debug, thiserror::Error)]
pub enum ShowError {
    #[error("failed to read show file: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("duplicate cue id: {0}")]
    DuplicateCueId(String),
    #[error("cue {cue_id}: media file not found at {path}")]
    MediaMissing { cue_id: String, path: PathBuf },
    #[error("cue {cue_id}: {problem}")]
    InvalidCue { cue_id: String, problem: String },
    #[error("failed to serialize show: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Top-level parsed show file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShowFile {
    pub show: Show,
    #[serde(default)]
    pub cues: Vec<Cue>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Show {
    pub title: String,
    pub version: u32,
    pub media_root: PathBuf,
    #[serde(default)]
    pub dropout_policy: DropoutPolicy,
    #[serde(default)]
    pub sync: SyncConfig,
    /// Optional idle poster (shown on connect and between cues).
    #[serde(default)]
    pub poster: Option<Poster>,
}

/// What a client should do if it loses its controller mid-cue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DropoutPolicy {
    /// Keep playing to the natural end of the current cue.
    #[default]
    Continue,
    /// Freeze at the current frame.
    Freeze,
    /// Cut to black.
    Black,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncConfig {
    pub max_drift_ms: u32,
    pub start_lead_ms: u32,
    pub correction: SyncCorrection,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            max_drift_ms: 150,
            start_lead_ms: 250,
            correction: SyncCorrection::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncCorrection {
    pub rate_min: f32,
    pub rate_max: f32,
    pub hard_seek_threshold_ms: u32,
    pub sync_interval_ms: u32,
}

impl Default for SyncCorrection {
    fn default() -> Self {
        Self {
            rate_min: 0.95,
            rate_max: 1.05,
            hard_seek_threshold_ms: 300,
            sync_interval_ms: 1000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cue {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: CueKind,
    /// Media file, relative to `media_root`. Empty for `color` cues.
    #[serde(default)]
    pub file: PathBuf,
    /// Solid colour for `color` cues, as `#RRGGBB`. Ignored otherwise.
    #[serde(default)]
    pub color: Option<String>,
    /// Fade time. When nothing is on air this is the fade-from-black duration;
    /// when a cue is already playing it is the crossfade duration into this
    /// cue. `0` means a hard cut.
    #[serde(default)]
    pub fade_in_ms: u32,
    /// In-point: start playback this many ms into the file. `0` = the start.
    #[serde(default)]
    pub in_ms: u32,
    /// Out-point: end the cue at this many ms into the file. `None` = play to
    /// the natural end of the media.
    #[serde(default)]
    pub out_ms: Option<u32>,
    /// Loop the clip (between `in_ms` and `out_ms`/end) until replaced.
    #[serde(default, rename = "loop")]
    pub loops: bool,
    /// What to do when the cue reaches its out-point / natural end.
    #[serde(default)]
    pub on_end: EndAction,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CueKind {
    #[default]
    Video,
    Image,
    /// A solid colour (see [`Cue::color`]) — used for fades to black/white.
    Color,
}

/// What a cue does when it reaches its out-point / the media's natural end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndAction {
    /// Drop the layer immediately (reveals the poster/black). The default.
    #[default]
    Cut,
    /// Hold the final frame until the operator acts.
    Freeze,
    /// Fade the layer out over the cue's `fade_in_ms`, then stop.
    Fade,
}

/// A show-level poster: an image or looping video every client shows on connect
/// and drops back to when no cue is on air. `file` is relative to `media_root`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Poster {
    /// `video` (looped) or `image`.
    #[serde(rename = "type")]
    pub kind: CueKind,
    pub file: PathBuf,
}

/// Parse a `#RRGGBB` string into an `[r, g, b]` triple. Returns black on any
/// malformed input so a bad colour never breaks playback.
pub fn parse_hex_color(s: &str) -> [u8; 3] {
    let h = s.trim().trim_start_matches('#');
    if h.len() == 6 {
        if let Ok(v) = u32::from_str_radix(h, 16) {
            return [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        }
    }
    [0, 0, 0]
}

/// Format an `[r, g, b]` triple as `#RRGGBB`.
pub fn format_hex_color(rgb: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", rgb[0], rgb[1], rgb[2])
}

impl ShowFile {
    /// Load, parse, and validate a show file from disk.
    pub fn load(path: &Path) -> Result<Self, ShowError> {
        let raw = fs::read_to_string(path)?;
        let show: ShowFile = toml::from_str(&raw)?;
        show.validate()?;
        Ok(show)
    }

    /// Parse a show file from an in-memory string, then validate.
    /// (Also available through the standard `str::parse::<ShowFile>()`.)
    pub fn parse_str(raw: &str) -> Result<Self, ShowError> {
        let show: ShowFile = toml::from_str(raw)?;
        show.validate()?;
        Ok(show)
    }

    /// A minimal empty show, the starting point for the editor's "New show".
    pub fn new_empty(title: impl Into<String>) -> Self {
        Self {
            show: Show {
                title: title.into(),
                version: 1,
                media_root: PathBuf::from("~/cuemesh_media"),
                dropout_policy: DropoutPolicy::default(),
                sync: SyncConfig::default(),
                poster: None,
            },
            cues: Vec::new(),
        }
    }

    /// Serialize to TOML and write to disk (used by the show editor).
    pub fn save(&self, path: &Path) -> Result<(), ShowError> {
        self.validate()?;
        let toml = toml::to_string_pretty(self)?;
        fs::write(path, toml)?;
        Ok(())
    }

    /// Structural validation. Does *not* touch the filesystem — call
    /// [`Self::validate_media`] separately for that.
    pub fn validate(&self) -> Result<(), ShowError> {
        let mut seen = std::collections::HashSet::new();
        for cue in &self.cues {
            if cue.id.trim().is_empty() {
                return Err(ShowError::InvalidCue {
                    cue_id: cue.id.clone(),
                    problem: "cue id must not be empty".into(),
                });
            }
            if !seen.insert(cue.id.as_str()) {
                return Err(ShowError::DuplicateCueId(cue.id.clone()));
            }
            // Colour cues carry no media file; every other kind needs a
            // relative path.
            if cue.kind != CueKind::Color {
                if cue.file.as_os_str().is_empty() {
                    return Err(ShowError::InvalidCue {
                        cue_id: cue.id.clone(),
                        problem: "file must not be empty".into(),
                    });
                }
                if cue.file.is_absolute() {
                    return Err(ShowError::InvalidCue {
                        cue_id: cue.id.clone(),
                        problem: format!(
                            "file must be relative to media_root, got {}",
                            cue.file.display()
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Check that every media cue's file (and the poster, if any) exists under
    /// the (already-expanded) media_root. Colour cues have no file and are
    /// skipped.
    pub fn validate_media(&self, media_root: &Path) -> Result<(), ShowError> {
        for cue in &self.cues {
            if cue.kind == CueKind::Color {
                continue;
            }
            let full = media_root.join(&cue.file);
            if !full.exists() {
                return Err(ShowError::MediaMissing {
                    cue_id: cue.id.clone(),
                    path: full,
                });
            }
        }
        if let Some(poster) = &self.show.poster {
            let full = media_root.join(&poster.file);
            if !full.exists() {
                return Err(ShowError::MediaMissing {
                    cue_id: "poster".into(),
                    path: full,
                });
            }
        }
        Ok(())
    }
}

impl std::str::FromStr for ShowFile {
    type Err = ShowError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        Self::parse_str(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[show]
title = "T"
version = 1
media_root = "/tmp/media"

[[cues]]
id = "a"
name = "A"
type = "video"
file = "a.mp4"
fade_in_ms = 500

[[cues]]
id = "b"
name = "B"
type = "image"
file = "b.png"
volume = 80
crossfade_to_next_ms = 500
"#;

    #[test]
    fn parses_example() {
        // Note: `volume` and `crossfade_to_next_ms` are legacy keys old show
        // files may carry; they must be silently ignored, not rejected.
        let s = ShowFile::parse_str(EXAMPLE).unwrap();
        assert_eq!(s.show.title, "T");
        assert_eq!(s.cues.len(), 2);
        assert_eq!(s.cues[0].fade_in_ms, 500);
        assert_eq!(s.show.dropout_policy, DropoutPolicy::Continue);
        assert_eq!(s.show.sync.max_drift_ms, 150);
    }

    #[test]
    fn save_load_roundtrip() {
        let mut sf = ShowFile::new_empty("Roundtrip");
        sf.cues.push(Cue {
            id: "c1".into(),
            name: "First".into(),
            kind: CueKind::Video,
            file: PathBuf::from("a.mp4"),
            color: None,
            fade_in_ms: 250,
            in_ms: 2500,
            out_ms: Some(15_000),
            loops: false,
            on_end: EndAction::Freeze,
            notes: Some("hello".into()),
        });
        sf.cues.push(Cue {
            id: "c2".into(),
            name: "To black".into(),
            kind: CueKind::Color,
            file: PathBuf::new(),
            color: Some("#000000".into()),
            fade_in_ms: 1000,
            in_ms: 0,
            out_ms: None,
            loops: false,
            on_end: EndAction::default(),
            notes: None,
        });
        let tmp = std::env::temp_dir().join("cuemesh2_show_roundtrip.cuemesh.toml");
        sf.save(&tmp).unwrap();
        let back = ShowFile::load(&tmp).unwrap();
        assert_eq!(back.show.title, "Roundtrip");
        assert_eq!(back.cues.len(), 2);
        assert_eq!(back.cues[0].fade_in_ms, 250);
        assert_eq!(back.cues[0].in_ms, 2500);
        assert_eq!(back.cues[0].out_ms, Some(15_000));
        assert_eq!(back.cues[0].on_end, EndAction::Freeze);
        assert_eq!(back.cues[0].notes.as_deref(), Some("hello"));
        assert_eq!(back.cues[1].kind, CueKind::Color);
        assert_eq!(back.cues[1].color.as_deref(), Some("#000000"));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn rejects_absolute_cue_path() {
        let mut sf = ShowFile::new_empty("T");
        sf.cues.push(Cue {
            id: "c1".into(),
            name: "Bad".into(),
            kind: CueKind::Video,
            file: PathBuf::from("/etc/passwd"),
            color: None,
            fade_in_ms: 0,
            in_ms: 0,
            out_ms: None,
            loops: false,
            on_end: EndAction::default(),
            notes: None,
        });
        assert!(matches!(sf.validate(), Err(ShowError::InvalidCue { .. })));
    }

    #[test]
    fn color_cue_needs_no_file() {
        let mut sf = ShowFile::new_empty("T");
        sf.cues.push(Cue {
            id: "black".into(),
            name: "Blackout".into(),
            kind: CueKind::Color,
            file: PathBuf::new(),
            color: Some("#000000".into()),
            fade_in_ms: 1500,
            in_ms: 0,
            out_ms: None,
            loops: false,
            on_end: EndAction::default(),
            notes: None,
        });
        sf.validate().unwrap();
        // Media validation skips colour cues even with a bogus root.
        sf.validate_media(Path::new("/nonexistent")).unwrap();
    }

    #[test]
    fn hex_color_roundtrip() {
        assert_eq!(parse_hex_color("#FF8000"), [255, 128, 0]);
        assert_eq!(parse_hex_color("bad"), [0, 0, 0]);
        assert_eq!(format_hex_color([255, 128, 0]), "#FF8000");
    }

    #[test]
    fn rejects_duplicate_cue_ids() {
        let dup = r#"
[show]
title = "T"
version = 1
media_root = "/tmp"

[[cues]]
id = "a"
name = "A"
type = "video"
file = "a.mp4"

[[cues]]
id = "a"
name = "A2"
type = "video"
file = "b.mp4"
"#;
        let err = ShowFile::parse_str(dup).unwrap_err();
        assert!(matches!(err, ShowError::DuplicateCueId(_)));
    }

    #[test]
    fn rejects_empty_cue_id() {
        let bad = r#"
[show]
title = "T"
version = 1
media_root = "/tmp"

[[cues]]
id = ""
name = "A"
type = "video"
file = "a.mp4"
"#;
        let err = ShowFile::parse_str(bad).unwrap_err();
        assert!(matches!(err, ShowError::InvalidCue { .. }));
    }
}
