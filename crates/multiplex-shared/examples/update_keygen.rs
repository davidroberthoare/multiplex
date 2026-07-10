//! Generate an ed25519 release keypair for signing update bundles.
//!
//! ```sh
//! cargo run -p cuemesh2-shared --example update_keygen -- keys/release_signing.key
//! ```
//!
//! Writes the private key (base64) to the given path — refusing to overwrite
//! an existing file — and prints ONLY the public key. Keep the private key
//! out of the repository (CI secret / maintainer's password manager); the
//! public key goes into `cuemesh2_shared::update::DEFAULT_RELEASE_PUBKEY_B64`
//! or the `CUEMESH_RELEASE_PUBKEY_B64` build-time env var.

use std::io::Write as _;

use base64::Engine as _;

fn main() -> anyhow::Result<()> {
    let Some(out_path) = std::env::args().nth(1) else {
        anyhow::bail!("usage: update_keygen <private-key-out-path>");
    };
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| anyhow::anyhow!("rng: {e}"))?;
    let b64 = base64::engine::general_purpose::STANDARD;
    let priv_b64 = b64.encode(seed);
    let pub_b64 = cuemesh2_shared::update::pubkey_of(&priv_b64)?;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts.open(&out_path)?;
    f.write_all(priv_b64.as_bytes())?;

    println!("private key written to {out_path} — keep it secret, do not commit");
    println!("public key (bake in): {pub_b64}");
    Ok(())
}
