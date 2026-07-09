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

use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

/// The v2 home directory (`~/.v2`), created if missing.
pub fn home() -> io::Result<PathBuf> {
    let base = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no HOME/USERPROFILE set"))?;
    let dir = PathBuf::from(base).join(".v2");
    std::fs::create_dir_all(&dir)?;
    chmod_private_dir(&dir)?;
    Ok(dir)
}

/// A subdirectory of the home dir, created if missing.
pub fn subdir(name: &str) -> io::Result<PathBuf> {
    let home = home()?;
    let dir = home.join(name);
    std::fs::create_dir_all(&dir)?;
    chmod_subdirs(&home, name)?;
    Ok(dir)
}

/// A file path inside the home dir (parent dirs ensured).
pub fn file(name: &str) -> io::Result<PathBuf> {
    Ok(home()?.join(name))
}

/// Write a private state file. On Unix this creates/truncates with mode 0600.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::OpenOptions::new().create(true).truncate(true).write(true).open(path)?;
        f.write_all(bytes)
    }
}

/// Append to a private state file. On Unix this creates with mode 0600.
pub fn append_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        f.write_all(bytes)
    }
}

fn chmod_subdirs(home: &Path, name: &str) -> io::Result<()> {
    let mut cur = home.to_path_buf();
    for comp in Path::new(name).components() {
        if let Component::Normal(part) = comp {
            cur.push(part);
            chmod_private_dir(&cur)?;
        }
    }
    Ok(())
}

fn chmod_private_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}
