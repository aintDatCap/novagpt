//! Per-workspace audit storage outside the repository and other applications.
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
        .join(hex::encode(digest)))
}

fn owner_private_dir(path: &Path, create: bool) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unsafe support bundle directory",
        )),
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Support bundle directory is not private",
                    ));
                }
            }
            #[cfg(not(unix))]
            let _ = meta;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && create => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(path)?;
            }
            #[cfg(not(unix))]
            fs::create_dir(path)?;
            owner_private_dir(path, false)
        }
        Err(error) => Err(error),
    }
}

/// Select a new, private, non-project file; the exporter itself uses create_new.
pub fn support_bundle_destination() -> io::Result<PathBuf> {
    let app = token::app_state_dir()?;
    owner_private_dir(&app, false)?;
    let exports = app.join("exports");
    owner_private_dir(&exports, true)?;
    Ok(exports.join(format!(
        "novagpt-support-{}.log.zst",
        uuid::Uuid::new_v4().simple()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn support_export_directory_is_private_and_rejects_symlinks() {
        let tmp = tempfile::tempdir().expect("private state");
        let export = tmp.path().join("exports");
        owner_private_dir(&export, true).expect("private export folder");
        owner_private_dir(&export, false).expect("reusable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            assert_eq!(
                fs::metadata(&export).expect("mode").permissions().mode() & 0o077,
                0
            );
            let unsafe_path = tmp.path().join("link");
            symlink(&export, &unsafe_path).expect("fixture link");
            assert!(owner_private_dir(&unsafe_path, false).is_err());
        }
    }
}
