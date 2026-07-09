//! Sign one update artifact and print its manifest entry.
//!
//! ```sh
//! CUEMESH_SIGNING_KEY=<base64 private key> \
//!   cargo run -p cuemesh2-shared --example update_sign -- \
//!   clients x86_64-unknown-linux-gnu path/to/cuemesh2-client
//! ```
//!
//! Emits the `[clients.<triple>]` (or `[controllers.<triple>]`) TOML block
//! for `manifest.toml`. The release process runs this once per artifact and
//! concatenates the blocks under a `version = "X.Y.Z"` header.

use cuemesh2_shared::{hashing, update};

fn main() -> anyhow::Result<()> {
    let key = std::env::var("CUEMESH_SIGNING_KEY")
        .map_err(|_| anyhow::anyhow!("set CUEMESH_SIGNING_KEY to the base64 private key"))?;
    let mut args = std::env::args().skip(1);
    let (Some(section), Some(triple), Some(path)) = (args.next(), args.next(), args.next()) else {
        anyhow::bail!("usage: update_sign <clients|controllers> <target-triple> <binary-path>");
    };
    if section != "clients" && section != "controllers" {
        anyhow::bail!("section must be 'clients' or 'controllers'");
    }

    let data = std::fs::read(&path)?;
    let sha256 = hashing::to_hex(&hashing::sha256_bytes(&data));
    let signature = update::sign_detached(&key, &data)?;
    let file = std::path::Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("bad file name"))?;

    println!("[{section}.{triple}]");
    println!("file = \"{file}\"");
    println!("sha256 = \"{sha256}\"");
    println!("signature = \"{signature}\"");
    Ok(())
}
