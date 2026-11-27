//! Human-approved ISO registry and sparse disposable installation bases.
//! MCP clients refer only to stable approved names, never host ISO paths.
use crate::{
    VmBackend, VmBootProfile, VmError, VmImage, VmManager, VmResult, resources, run_command,
    sha256_file, valid_name, validate_image_name, write_json_new,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    time::Duration,
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmIso {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
    pub human_approved: bool,
}

#[derive(Debug, Serialize)]
pub struct VmIsoList {
    pub action: &'static str,
    pub images: Vec<VmIso>,
    pub image_count: usize,
}

impl VmManager {
    /// Import only a human-selected ISO file into isolated NovaGPT storage.
    /// ISO names, not arbitrary paths, are exposed to agents.
    pub fn import_iso(&self, name: &str, source: &Path) -> VmResult<VmIso> {
        validate_image_name(name)?;
        let source = fs::canonicalize(source)?;
        let meta = source.symlink_metadata()?;
        if !meta.is_file()
            || meta.len() == 0
            || meta.len() > 32 * 1024 * 1024 * 1024
            || source
                .extension()
                .and_then(|ext| ext.to_str())
                .is_none_or(|ext| !ext.eq_ignore_ascii_case("iso"))
        {
            return Err(VmError::InvalidData);
        }
        let dir = self.root().join("isos").join(name);
        fs::create_dir(&dir)?;
        let target = dir.join("installer.iso");
        let import = (|| {
            let mut input = fs::File::open(source)?;
            let mut output = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)?;
            let mut chunk = [0u8; 64 * 1024];
            loop {
                let read = input.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                output.write_all(&chunk[..read])?;
            }
            output.sync_all()?;
            let hash = sha256_file(&target)?;
            let record = VmIso {
                name: name.to_owned(),
                sha256: hash,
                bytes: meta.len(),
                human_approved: true,
            };
            let mut perms = fs::metadata(&target)?.permissions();
            perms.set_readonly(true);
            fs::set_permissions(&target, perms)?;
            write_json_new(&dir.join("manifest.json"), &record)?;
            Ok(record)
        })();
        if import.is_err() && dir.join("manifest.json").symlink_metadata().is_err() {
            let _ = fs::remove_file(&target);
            let _ = fs::remove_dir(&dir);
        }
        import
    }

    pub fn isos(&self) -> VmResult<VmIsoList> {
        let root = self.root().join("isos");
        if fs::canonicalize(&root)? != root {
            return Err(VmError::NotOwned);
        }
        let mut images = Vec::new();
        for item in fs::read_dir(root)? {
            let item = item?;
            if !item.file_type()?.is_dir()
                || item.file_type()?.is_symlink()
                || !valid_name(&item.file_name().to_string_lossy(), 80)
            {
                continue;
            }
            let manifest = item.path().join("manifest.json");
            if manifest
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_symlink())
            {
                continue;
            }
            if let Ok(bytes) = fs::read(manifest)
                && bytes.len() <= 16_384
                && let Ok(iso) = serde_json::from_slice::<VmIso>(&bytes)
                && iso.name == item.file_name().to_string_lossy()
                && iso.human_approved
            {
                images.push(iso);
            }
        }
        images.sort_by(|a, b| a.name.cmp(&b.name));
        let image_count = images.len();
        Ok(VmIsoList {
            action: "isos",
            images,
            image_count,
        })
    }

    pub(crate) async fn verified_iso(&self, name: &str) -> VmResult<std::path::PathBuf> {
        validate_image_name(name)?;
        let record = self
            .isos()?
            .images
            .into_iter()
            .find(|item| item.name == name)
            .ok_or(VmError::NotFound)?;
        let dir = self.root().join("isos").join(name);
        if fs::canonicalize(&dir)? != dir {
            return Err(VmError::NotOwned);
        }
        let iso = dir.join("installer.iso");
        let meta = iso.symlink_metadata()?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() != record.bytes {
            return Err(VmError::NotOwned);
        }
        let copy = iso.clone();
        let digest = tokio::task::spawn_blocking(move || sha256_file(&copy))
            .await
            .map_err(|_| VmError::InvalidData)??;
        if digest != record.sha256 {
            return Err(VmError::InvalidData);
        }
        Ok(iso)
    }

    /// Produce a sparse, shared, approved blank qcow2 for ISO-based installation.
    /// This human-only CLI operation never modifies an existing approved image.
    pub async fn create_blank_image(
        &self,
        name: &str,
        disk_gib: u32,
        cpus: u8,
        memory_mb: u32,
        boot_profile: VmBootProfile,
    ) -> VmResult<VmImage> {
        if self.backend() != VmBackend::Qemu {
            return Err(VmError::Unsupported(
                "Sparse blank base creation currently requires QEMU",
            ));
        }
        validate_image_name(name)?;
        resources(cpus, memory_mb)?;
        if !(8..=2048).contains(&disk_gib) || boot_profile == VmBootProfile::WindowsUefi {
            return Err(VmError::InvalidData);
        }
        let stage = self.root().join("staging");
        if fs::canonicalize(&stage)? != stage {
            return Err(VmError::NotOwned);
        }
        let path = stage.join(format!("{}.qcow2", Uuid::new_v4().simple()));
        let output = run_command(
            "qemu-img",
            &[
                "create".into(),
                "-f".into(),
                "qcow2".into(),
                path.display().to_string(),
                format!("{disk_gib}G"),
            ],
            Duration::from_secs(45),
        )
        .await?;
        if !output.status.success() {
            return Err(VmError::CommandFailed);
        }
        let result = self.import_image_with_profile(name, &path, cpus, memory_mb, boot_profile);
        let _ = fs::remove_file(&path);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_registry_rejects_duplicates_and_invalid_names() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("approved.iso");
        fs::write(&input, b"disposable-test-media").unwrap();
        let manager = VmManager::new(tmp.path().join("owned")).unwrap();
        let iso = manager.import_iso("ubuntu-test", &input).unwrap();
        assert!(iso.human_approved);
        assert_eq!(manager.isos().unwrap().image_count, 1);
        assert!(matches!(
            manager.import_iso("ubuntu-test", &input),
            Err(VmError::Io(_))
        ));
        assert!(matches!(
            manager.import_iso("../bad", &input),
            Err(VmError::InvalidName)
        ));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn disposable_iso_boot_uses_approved_media() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("test.iso");
        // Fixed synthetic boot media: verifies the QEMU device path, not an OS install.
        fs::write(&source, vec![0u8; 2 * 1024 * 1024]).unwrap();
        let manager = VmManager::new(tmp.path().join("owned")).unwrap();
        manager.import_iso("empty-iso", &source).unwrap();
        let base = manager
            .create_blank_image("fresh", 8, 1, 512, VmBootProfile::Legacy)
            .await
            .unwrap();
        assert_eq!(base.boot_profile, VmBootProfile::Legacy);
        let created = manager
            .create(crate::VmCreate {
                image: "fresh".into(),
                cpus: None,
                memory_mb: None,
                linux_options: Some(
                    crate::linux_qemu::LinuxVmOptions::from_network_specs(
                        Some(&["none".into()]),
                        false,
                        true,
                    )
                    .unwrap(),
                ),
                installer_iso: Some("empty-iso".into()),
            })
            .await
            .unwrap();
        let id = created.instance.id;
        assert_eq!(created.instance.installer_iso.as_deref(), Some("empty-iso"));
        let result = manager.status(&id).await.unwrap();
        assert_eq!(result.instance.state, crate::VmState::Running);
        manager.stop(&id).await.unwrap();
        let ejected = manager.eject_iso(&id).await.unwrap();
        assert!(ejected.instance.installer_iso.is_none());
        manager.start(&id).await.unwrap();
        assert!(manager.destroy(&id).await.is_ok());
    }

    #[tokio::test]
    async fn rejects_tampered_approved_iso() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("test.iso");
        fs::write(&source, b"approved-test-media").unwrap();
        let manager = VmManager::new(tmp.path().join("owned")).unwrap();
        manager.import_iso("iso-test", &source).unwrap();
        let target = manager.root().join("isos/iso-test/installer.iso");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(not(unix))]
        {
            // Clear the read-only attribute only on the disposable test ISO.
            let mut permissions = fs::metadata(&target).unwrap().permissions();
            #[expect(
                clippy::permissions_set_readonly_false,
                reason = "Non-Unix test fixture only"
            )]
            permissions.set_readonly(false);
            fs::set_permissions(&target, permissions).unwrap();
        }
        fs::write(&target, b"tampered-test-media").unwrap();
        assert!(matches!(
            manager.verified_iso("iso-test").await,
            Err(VmError::InvalidData)
        ));
    }
}
