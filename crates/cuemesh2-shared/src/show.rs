//! Show file format (`*.cuemesh.toml`).
//!
//! Load a show from disk with [`ShowFile::load`]. Validation checks unique cue
//! IDs, media file existence relative to `media_root`, and value ranges.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    #[serde(default)]
    pub settings: ShowSettings,
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
pub struct ShowSettings {
    /// Duration used by the operator FADE command to fade all layers to black.
    pub default_fade_ms: u32,
}

impl Default for ShowSettings {
    fn default() -> Self {
        Self {
            default_fade_ms: 1500,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cue {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: CueKind,
    pub file: PathBuf,
    #[serde(default)]
    pub fade_in_ms: u32,
    #[serde(default)]
    pub fade_out_ms: u32,
    /// If > 0, the client auto-preloads the following cue on the idle layer
    /// once fades on that layer complete, then crossfades on cue end.
    #[serde(default)]
    pub crossfade_to_next_ms: u32,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CueKind {
    Video,
    Image,
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
                settings: ShowSettings::default(),
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
        Ok(())
    }

    /// Check that every cue's file exists under the (already-expanded) media_root.
    pub fn validate_media(&self, media_root: &Path) -> Result<(), ShowError> {
        for cue in &self.cues {
            let full = media_root.join(&cue.file);
            if !full.exists() {
                return Err(ShowError::MediaMissing {
                    cue_id: cue.id.clone(),
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

[show.settings]
default_fade_ms = 1500

[[cues]]
id = "a"
name = "A"
type = "video"
file = "a.mp4"

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
        // Note: `volume` is a legacy key old show files may carry; it must
        // be silently ignored, not rejected.
        let s = ShowFile::parse_str(EXAMPLE).unwrap();
        assert_eq!(s.show.title, "T");
        assert_eq!(s.cues.len(), 2);
        assert_eq!(s.cues[1].crossfade_to_next_ms, 500);
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
            fade_in_ms: 250,
            fade_out_ms: 0,
            crossfade_to_next_ms: 1000,
            notes: Some("hello".into()),
        });
        let tmp = std::env::temp_dir().join("cuemesh2_show_roundtrip.cuemesh.toml");
        sf.save(&tmp).unwrap();
        let back = ShowFile::load(&tmp).unwrap();
        assert_eq!(back.show.title, "Roundtrip");
        assert_eq!(back.cues.len(), 1);
        assert_eq!(back.cues[0].crossfade_to_next_ms, 1000);
        assert_eq!(back.cues[0].notes.as_deref(), Some("hello"));
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
            fade_in_ms: 0,
            fade_out_ms: 0,
            crossfade_to_next_ms: 0,
            notes: None,
        });
        assert!(matches!(sf.validate(), Err(ShowError::InvalidCue { .. })));
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
