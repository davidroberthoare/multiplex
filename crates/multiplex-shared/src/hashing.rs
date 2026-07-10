//! SHA-256 helpers for the preflight file-verification step.

use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

/// Hash a file synchronously. Suitable for preflight where we already
/// serialize work per client.
pub fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// Hash an in-memory buffer (update artifacts are read whole for signing
/// and signature verification anyway).
pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// Lowercase hex string, useful for logs and JSON payloads.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_hash_of_empty_file() {
        let tmp = std::env::temp_dir().join("cuemesh2_hash_test_empty");
        std::fs::write(&tmp, b"").unwrap();
        let h = sha256_file(&tmp).unwrap();
        assert_eq!(
            to_hex(&h),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn known_hash_of_abc() {
        let tmp = std::env::temp_dir().join("cuemesh2_hash_test_abc");
        std::fs::write(&tmp, b"abc").unwrap();
        let h = sha256_file(&tmp).unwrap();
        assert_eq!(
            to_hex(&h),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let _ = std::fs::remove_file(&tmp);
    }
}
