//! Per-workspace audit storage outside the repository and CodexPro profiles.
//! Keeps a separate canonical-root digest regardless of the shared HTTP token.
//! Log-only operations never read or create credentials.
use crate::token;
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

pub fn audit_directory(root: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(root)?;
    if !canonical.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "workspace is not a directory",
        ));
    }
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    Ok(token::app_state_dir()?
        .join("logs")
        .join(format!("{digest:x}")))
}
