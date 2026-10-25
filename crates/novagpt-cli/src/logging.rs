//! Per-workspace audit storage outside the repository and CodexPro profiles.
//! Reuses the same canonical root digest and OS-private state base as token storage,
//! but never reads the token or creates a token for log-only operations.
use crate::token;
use std::{
    io,
    path::{Path, PathBuf},
};

pub fn audit_directory(root: &Path) -> io::Result<PathBuf> {
    let token_path = token::path(root)?;
    let digest = token_path.file_stem().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "Invalid workspace identifier")
    })?;
    let tokens = token_path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "Invalid token state location")
    })?;
    let app = tokens.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Missing application state directory",
        )
    })?;
    Ok(app.join("logs").join(digest))
}
