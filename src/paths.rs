//! Filesystem layout for the daemon. Everything v2 writes lives under `~/.v2`.
//!
//! ```text
//! ~/.v2/
//!   key                 node identity (ed25519 seed, 0600)
//!   policy.toml         serving policy (optional)
//!   usage/day-<n>.jsonl append-only metering log
//!   mesh/
//!     org.json          trusted org root pubkey + our membership cert
//!     revoked.json      revocation list
//!     receipts/         signed usage receipts
//! ```

use std::io;
use std::path::PathBuf;

/// The v2 home directory (`~/.v2`), created if missing.
pub fn home() -> io::Result<PathBuf> {
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no HOME/USERPROFILE set"))?;
    let dir = PathBuf::from(base).join(".v2");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A subdirectory of the home dir, created if missing.
pub fn subdir(name: &str) -> io::Result<PathBuf> {
    let dir = home()?.join(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// A file path inside the home dir (parent dirs ensured).
pub fn file(name: &str) -> io::Result<PathBuf> {
    Ok(home()?.join(name))
}
