//! Integration test for the client's stage → re-verify → discard lifecycle.
//! Staging happens next to the *test* executable (the same "next to
//! `current_exe`" rule the real client uses), so everything is cleaned up.
//!
//! One test function on purpose: the staged slot and the pubkey env var are
//! process-global.

use cuemesh2_client::update::{
    discard_staged, stage, staged_bin_path, staged_version, take_staged, StagedMeta,
};
use cuemesh2_shared::{hashing, update as shared_update};

/// Fixed test seed — NOT the release key.
const TEST_PRIV: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn meta_for(data: &[u8], version: &str) -> StagedMeta {
    StagedMeta {
        version: version.into(),
        size: data.len() as u64,
        sha256_hex: hashing::to_hex(&hashing::sha256_bytes(data)),
        signature_b64: shared_update::sign_detached(TEST_PRIV, data).unwrap(),
    }
}

fn write_tmp(name: &str, data: &[u8]) -> std::path::PathBuf {
    // Same directory as the staged slot so `stage`'s rename stays on one fs.
    let tmp = staged_bin_path().unwrap().with_file_name(name);
    std::fs::write(&tmp, data).unwrap();
    tmp
}

#[test]
fn stage_verify_and_discard_lifecycle() {
    std::env::set_var(
        "CUEMESH_UPDATE_PUBKEY",
        shared_update::pubkey_of(TEST_PRIV).unwrap(),
    );
    discard_staged(); // clean slate

    let data = b"pretend this is a new client binary".to_vec();

    // Happy path: stage, report, re-verify, take.
    let tmp = write_tmp(".test-update-good", &data);
    stage(&tmp, &meta_for(&data, "99.0.0")).expect("stage should succeed");
    assert_eq!(staged_version().as_deref(), Some("99.0.0"));
    let staged = take_staged()
        .expect("verification should pass")
        .expect("should exist");
    assert_eq!(std::fs::read(&staged).unwrap(), data);
    discard_staged();
    assert_eq!(staged_version(), None);
    assert!(take_staged().unwrap().is_none());

    // Tampered signature: stage() itself must refuse.
    let tmp = write_tmp(".test-update-badsig", &data);
    let mut bad = meta_for(&data, "99.0.0");
    bad.signature_b64 = shared_update::sign_detached(TEST_PRIV, b"other bytes").unwrap();
    let err = stage(&tmp, &bad).unwrap_err().to_string();
    assert!(err.contains("signature"), "{err}");
    discard_staged();

    // Corruption after staging: take_staged() must catch and self-clean.
    let tmp = write_tmp(".test-update-corrupt", &data);
    stage(&tmp, &meta_for(&data, "99.0.0")).unwrap();
    std::fs::write(staged_bin_path().unwrap(), b"tampered on disk").unwrap();
    assert!(
        take_staged().is_err(),
        "corrupted staged binary must be rejected"
    );
    assert_eq!(staged_version(), None, "rejected staging must be discarded");

    // Downgrade guard at apply time: a stale staged version never applies.
    let tmp = write_tmp(".test-update-stale", &data);
    stage(&tmp, &meta_for(&data, "0.0.1")).unwrap();
    let err = take_staged().unwrap_err().to_string();
    assert!(err.contains("not newer"), "{err}");
    assert_eq!(staged_version(), None);
}
