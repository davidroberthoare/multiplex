//! Signed update bundle: manifest format, artifact verification, version
//! comparison.
//!
//! An update bundle is a directory (or a GitHub release) holding one client
//! and/or controller binary per target triple plus a `manifest.toml` that
//! names them. Every artifact is signed (ed25519, over the raw binary bytes)
//! by the release process; the private key never ships — clients and
//! controllers only carry the public key and verify.
//!
//! ```toml
//! version = "0.2.0"
//!
//! [clients.x86_64-unknown-linux-gnu]
//! file = "cuemesh2-client-x86_64-unknown-linux-gnu"
//! sha256 = "…hex…"
//! signature = "…base64…"
//! min_gstreamer = "1.18"   # optional
//!
//! [controllers.x86_64-pc-windows-msvc]
//! file = "cuemesh2-controller-x86_64-pc-windows-msvc.exe"
//! sha256 = "…hex…"
//! signature = "…base64…"
//! ```
//!
//! `file` values are flat file names (no directories) so the same manifest
//! works as a local bundle directory and as GitHub release assets.

use std::collections::BTreeMap;
use std::path::Path;

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use serde::{Deserialize, Serialize};

/// Default release public key, base64. Overridable at compile time via the
/// `CUEMESH_RELEASE_PUBKEY_B64` env var (for organisations building their own
/// signed distributions) and at runtime via `CUEMESH_UPDATE_PUBKEY` (intended
/// for tests; a runtime attacker who can set env vars already owns the box).
pub const DEFAULT_RELEASE_PUBKEY_B64: &str = "KguEca4EKifaMPP79L4rEqW6Loxrlu5P8UUyCiIbjeY=";

/// Resolve the public key used to verify update artifacts.
pub fn release_pubkey_b64() -> String {
    if let Ok(k) = std::env::var("CUEMESH_UPDATE_PUBKEY") {
        if !k.is_empty() {
            return k;
        }
    }
    option_env!("CUEMESH_RELEASE_PUBKEY_B64")
        .unwrap_or(DEFAULT_RELEASE_PUBKEY_B64)
        .to_string()
}

/// Errors from manifest loading and artifact verification.
#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("manifest parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("bad base64: {0}")]
    Base64(#[from] base64::DecodeError),
    #[error("bad key or signature length")]
    KeyLength,
    #[error("signature verification failed")]
    BadSignature,
    #[error("artifact file name must be flat (no path separators): {0}")]
    UnflatFile(String),
}

/// One signed binary inside an update bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    /// Flat file name of the binary within the bundle / release assets.
    pub file: String,
    /// SHA-256 of the binary, hex.
    pub sha256: String,
    /// ed25519 signature over the binary bytes, base64.
    pub signature: String,
    /// Minimum GStreamer runtime this build needs. A client whose runtime is
    /// older refuses the update and asks for a manual reinstall.
    #[serde(default)]
    pub min_gstreamer: Option<String>,
}

/// The `manifest.toml` at the root of an update bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateManifest {
    /// Semver of every binary in the bundle.
    pub version: String,
    /// Client binaries by target triple.
    #[serde(default)]
    pub clients: BTreeMap<String, Artifact>,
    /// Controller binaries by target triple.
    #[serde(default)]
    pub controllers: BTreeMap<String, Artifact>,
}

impl UpdateManifest {
    /// Parse a manifest from TOML text and reject non-flat artifact paths
    /// (they must be usable as GitHub release asset names, and a path here
    /// could otherwise escape the bundle directory).
    pub fn parse(text: &str) -> Result<Self, UpdateError> {
        let m: Self = toml::from_str(text)?;
        for a in m.clients.values().chain(m.controllers.values()) {
            if a.file.contains('/') || a.file.contains('\\') || a.file.starts_with('.') {
                return Err(UpdateError::UnflatFile(a.file.clone()));
            }
        }
        Ok(m)
    }

    /// Load `manifest.toml` from a bundle directory.
    pub fn load(bundle_dir: &Path) -> Result<Self, UpdateError> {
        let text = std::fs::read_to_string(bundle_dir.join("manifest.toml"))?;
        Self::parse(&text)
    }
}

/// True when `candidate` is a strictly newer semver than `current`.
/// Unparseable versions (including the empty string an old client reports)
/// never count as newer on the candidate side, and always count as older on
/// the current side — so "unknown current version" still offers the update,
/// while a malformed pushed version is refused.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let Ok(cand) = semver::Version::parse(candidate) else {
        return false;
    };
    match semver::Version::parse(current) {
        Ok(cur) => cand > cur,
        Err(_) => true,
    }
}

/// Verify an ed25519 signature (base64) over `data` against a public key
/// (base64 of the 32 raw bytes).
pub fn verify_signature(pubkey_b64: &str, data: &[u8], sig_b64: &str) -> Result<(), UpdateError> {
    let key_bytes = base64::engine::general_purpose::STANDARD.decode(pubkey_b64.trim())?;
    let key_arr: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| UpdateError::KeyLength)?;
    let key = VerifyingKey::from_bytes(&key_arr).map_err(|_| UpdateError::KeyLength)?;
    let sig_bytes = base64::engine::general_purpose::STANDARD.decode(sig_b64.trim())?;
    let sig_arr: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| UpdateError::KeyLength)?;
    let sig = Signature::from_bytes(&sig_arr);
    key.verify(data, &sig)
        .map_err(|_| UpdateError::BadSignature)
}

/// Sign `data` with a private key (base64 of the 32 raw seed bytes),
/// returning the signature as base64. Used by the release signing tool and
/// by tests; verification never needs this.
pub fn sign_detached(privkey_b64: &str, data: &[u8]) -> Result<String, UpdateError> {
    let key_bytes = base64::engine::general_purpose::STANDARD.decode(privkey_b64.trim())?;
    let key_arr: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| UpdateError::KeyLength)?;
    let key = SigningKey::from_bytes(&key_arr);
    let sig = key.sign(data);
    Ok(base64::engine::general_purpose::STANDARD.encode(sig.to_bytes()))
}

/// Derive the base64 public key from a base64 private key. For the key
/// generation tool.
pub fn pubkey_of(privkey_b64: &str) -> Result<String, UpdateError> {
    let key_bytes = base64::engine::general_purpose::STANDARD.decode(privkey_b64.trim())?;
    let key_arr: [u8; 32] = key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| UpdateError::KeyLength)?;
    let key = SigningKey::from_bytes(&key_arr);
    Ok(base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed test seed — NOT the release key.
    const TEST_PRIV: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    #[test]
    fn sign_verify_roundtrip() {
        let data = b"binary bytes";
        let sig = sign_detached(TEST_PRIV, data).unwrap();
        let pubkey = pubkey_of(TEST_PRIV).unwrap();
        verify_signature(&pubkey, data, &sig).unwrap();
    }

    #[test]
    fn tampered_data_fails() {
        let sig = sign_detached(TEST_PRIV, b"binary bytes").unwrap();
        let pubkey = pubkey_of(TEST_PRIV).unwrap();
        assert!(matches!(
            verify_signature(&pubkey, b"tampered bytes", &sig),
            Err(UpdateError::BadSignature)
        ));
    }

    #[test]
    fn wrong_key_fails() {
        let sig = sign_detached(TEST_PRIV, b"binary bytes").unwrap();
        let other_priv = "IB8eHRwbGhkYFxYVFBMSERAPDg0MCwoJCAcGBQQDAgE=";
        let pubkey = pubkey_of(other_priv).unwrap();
        assert!(verify_signature(&pubkey, b"binary bytes", &sig).is_err());
    }

    #[test]
    fn version_comparison() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // Unknown current version (old client) → any valid candidate wins.
        assert!(is_newer("0.2.0", ""));
        // Malformed candidate never wins.
        assert!(!is_newer("not-a-version", "0.1.0"));
        assert!(!is_newer("", ""));
    }

    #[test]
    fn manifest_parses_and_rejects_paths() {
        let good = r#"
            version = "0.2.0"
            [clients.x86_64-unknown-linux-gnu]
            file = "cuemesh2-client-x86_64-unknown-linux-gnu"
            sha256 = "ab"
            signature = "c2ln"
            min_gstreamer = "1.18"
        "#;
        let m = UpdateManifest::parse(good).unwrap();
        assert_eq!(m.version, "0.2.0");
        assert_eq!(m.clients.len(), 1);
        assert!(m.controllers.is_empty());
        assert_eq!(
            m.clients["x86_64-unknown-linux-gnu"]
                .min_gstreamer
                .as_deref(),
            Some("1.18")
        );

        let bad = r#"
            version = "0.2.0"
            [clients.t]
            file = "../escape"
            sha256 = "ab"
            signature = "c2ln"
        "#;
        assert!(matches!(
            UpdateManifest::parse(bad),
            Err(UpdateError::UnflatFile(_))
        ));
    }
}
