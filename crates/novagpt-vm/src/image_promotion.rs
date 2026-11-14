//! Explicit, local-only promotion of a powered-off installer disk into an
//! immutable reusable base. The installer VM and its checkpoints are preserved.
use crate::{
    VmBootProfile, VmError, VmImage, VmManager, VmResult, sha256_file, validate_image_name,
    write_json_new,
};
use std::{fs, path::Path};

#[derive(Debug, Clone)]
pub struct VmPromoteRequest {
    pub installer: String,
    pub image: String,
}

impl VmManager {
    /// Human-approved local CLI operation, never exposed via MCP.
    ///
    /// Creates an independent flattened image, without moving, deleting or
    /// committing an ISO installer disk or any Hyper-V checkpoint.
    pub async fn promote_iso(&self, request: VmPromoteRequest) -> VmResult<VmImage> {
        validate_image_name(&request.image)?;
        if !crate::valid_name(&request.installer, 48) {
            return Err(VmError::InvalidName);
        }
        let _serial = self.inner.commands.lock().await;
        let images = self.root().join("images");
        if fs::canonicalize(&images)? != images {
            return Err(VmError::NotOwned);
        }
        let dir = images.join(&request.image);
        fs::create_dir(&dir)?; // Exclusive: do not replace any existing base.
        let dest = dir.join(format!("base.{}", self.backend().disk_ext()));
        let result = self.convert_installer(&request.installer, &dest).await;
        let (profile, cpus, memory_mb) = result?;
        let disk_meta = dest.symlink_metadata()?;
        if !disk_meta.is_file() || disk_meta.file_type().is_symlink() || disk_meta.len() == 0 {
            return Err(VmError::InvalidData);
        }
        let source = dest.clone();
        let hash = tokio::task::spawn_blocking(move || sha256_file(&source))
            .await
            .map_err(|_| VmError::InvalidData)??;
        let image = VmImage {
            name: request.image,
            backend: self.backend(),
            sha256: hash,
            bytes: disk_meta.len(),
            default_cpus: cpus,
            default_memory_mb: memory_mb,
            human_approved: true,
            boot_profile: profile,
        };
        // Keep the converted base read-only. The backing-file chain in each
        // instance is independent; no guest may write directly to this image.
        let mut permissions = fs::metadata(&dest)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&dest, permissions)?;
        write_json_new(&dir.join("manifest.json"), &image)?;
        Ok(image)
    }

    async fn convert_installer(
        &self,
        name: &str,
        dest: &Path,
    ) -> VmResult<(VmBootProfile, u8, u32)> {
        #[cfg(windows)]
        {
            self.convert_hyperv_installer(name, dest).await
        }
        #[cfg(target_os = "linux")]
        {
            self.convert_linux_installer(name, dest).await
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = (name, dest);
            Err(VmError::Unsupported(
                "ISO image promotion is supported on Windows and Linux",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_invalid_and_duplicate_names_without_touching_other_images() {
        let temp = tempfile::tempdir().unwrap();
        let vm = VmManager::new(temp.path()).unwrap();
        assert!(matches!(
            vm.promote_iso(VmPromoteRequest {
                installer: "../bad".into(),
                image: "safe".into()
            })
            .await,
            Err(VmError::InvalidName)
        ));
        assert!(matches!(
            vm.promote_iso(VmPromoteRequest {
                installer: "valid".into(),
                image: "../bad".into()
            })
            .await,
            Err(VmError::InvalidName)
        ));
        fs::create_dir(vm.root().join("images").join("existing")).unwrap();
        assert!(matches!(
            vm.promote_iso(VmPromoteRequest {
                installer: "valid".into(),
                image: "existing".into()
            })
            .await,
            Err(VmError::Io(_))
        ));
    }
}
