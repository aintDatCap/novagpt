//! Offline environment checkpoints for owned disposable QEMU guests.
//!
//! Qcow2 internal snapshots share clusters with the base and the writable overlay.
//! This deliberately does not snapshot RAM, UEFI variable storage or TPM state.
use super::{
    VmBackend, VmBootProfile, VmError, VmManager, VmResult, VmState, run_command, valid_name,
};
use serde::Serialize;
use std::{fs, path::Path, time::Duration};

const SNAPSHOT_NAME_MAX: usize = 64;

#[derive(Debug, Serialize)]
pub struct VmSnapshot {
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct VmSnapshotList {
    pub action: &'static str,
    pub id: String,
    pub snapshots: Vec<VmSnapshot>,
}

impl VmManager {
    /// A quiescent disk-only checkpoint; a named environment, not a test case.
    pub async fn snapshot_create(&self, id: &str, name: &str) -> VmResult<VmSnapshotList> {
        if !valid_name(name, SNAPSHOT_NAME_MAX) {
            return Err(VmError::InvalidName);
        }
        let _guard = self.inner.commands.lock().await;
        let disk = self.snapshot_disk(id).await?;
        if self
            .snapshots_for_disk(&disk)
            .await?
            .contains(&name.to_string())
        {
            return Err(VmError::Busy);
        }
        qemu_snapshot(&disk, "-c", name).await?;
        let names = self.snapshots_for_disk(&disk).await?;
        if !names.contains(&name.to_string()) {
            return Err(VmError::CommandFailed);
        }
        Ok(snapshot_list(id, "snapshot_create", names))
    }

    pub async fn snapshot_list(&self, id: &str) -> VmResult<VmSnapshotList> {
        let _guard = self.inner.commands.lock().await;
        let disk = self.snapshot_disk(id).await?;
        Ok(snapshot_list(
            id,
            "snapshot_list",
            self.snapshots_for_disk(&disk).await?,
        ))
    }

    pub async fn snapshot_restore(&self, id: &str, name: &str) -> VmResult<VmSnapshotList> {
        if !valid_name(name, SNAPSHOT_NAME_MAX) {
            return Err(VmError::InvalidName);
        }
        let _guard = self.inner.commands.lock().await;
        let disk = self.snapshot_disk(id).await?;
        if !self
            .snapshots_for_disk(&disk)
            .await?
            .contains(&name.to_string())
        {
            return Err(VmError::NotFound);
        }
        qemu_snapshot(&disk, "-a", name).await?;
        Ok(snapshot_list(
            id,
            "snapshot_restore",
            self.snapshots_for_disk(&disk).await?,
        ))
    }

    pub async fn snapshot_delete(&self, id: &str, name: &str) -> VmResult<VmSnapshotList> {
        if !valid_name(name, SNAPSHOT_NAME_MAX) {
            return Err(VmError::InvalidName);
        }
        let _guard = self.inner.commands.lock().await;
        let disk = self.snapshot_disk(id).await?;
        if !self
            .snapshots_for_disk(&disk)
            .await?
            .contains(&name.to_string())
        {
            return Err(VmError::NotFound);
        }
        qemu_snapshot(&disk, "-d", name).await?;
        Ok(snapshot_list(
            id,
            "snapshot_delete",
            self.snapshots_for_disk(&disk).await?,
        ))
    }

    /// Flatten an immutable reusable approved base from one offline QCOW2
    /// environment snapshot. Subagents can then fork the base by image name,
    /// sharing read-only storage without one snapshot per individual test.
    pub async fn snapshot_promote(
        &self,
        id: &str,
        snapshot: &str,
        image_name: &str,
    ) -> VmResult<crate::VmImage> {
        if !valid_name(snapshot, SNAPSHOT_NAME_MAX) {
            return Err(VmError::InvalidName);
        }
        crate::validate_image_name(image_name)?;
        let _guard = self.inner.commands.lock().await;
        let instance = self.load_instance(id)?;
        let disk = self.snapshot_disk(id).await?;
        if !self
            .snapshots_for_disk(&disk)
            .await?
            .iter()
            .any(|item| item == snapshot)
        {
            return Err(VmError::NotFound);
        }
        let images = self.root().join("images");
        if fs::canonicalize(&images)? != images {
            return Err(VmError::NotOwned);
        }
        let dir = images.join(image_name);
        fs::create_dir(&dir)?;
        let dest = dir.join("base.qcow2");
        let result = async {
            let args = vec![
                "convert".into(),
                "-f".into(),
                "qcow2".into(),
                "-l".into(),
                snapshot.to_owned(),
                "-O".into(),
                "qcow2".into(),
                disk.display().to_string(),
                dest.display().to_string(),
            ];
            let answer = run_command("qemu-img", &args, Duration::from_secs(900)).await?;
            if !answer.status.success() {
                return Err(VmError::CommandFailed);
            }
            crate::validate_qcow2_base(&dest)?;
            let metadata = dest.symlink_metadata()?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(VmError::NotOwned);
            }
            let hash_path = dest.clone();
            let hash = tokio::task::spawn_blocking(move || crate::sha256_file(&hash_path))
                .await
                .map_err(|_| VmError::InvalidData)??;
            let image = crate::VmImage {
                name: image_name.to_owned(),
                backend: VmBackend::Qemu,
                sha256: hash,
                bytes: metadata.len(),
                default_cpus: instance.cpus,
                default_memory_mb: instance.memory_mb,
                human_approved: true,
                boot_profile: VmBootProfile::Legacy,
            };
            let mut perms = metadata.permissions();
            perms.set_readonly(true);
            fs::set_permissions(&dest, perms)?;
            crate::write_json_new(&dir.join("manifest.json"), &image)?;
            Ok(image)
        }
        .await;
        if result.is_err() && !dir.join("manifest.json").exists() {
            let _ = fs::remove_file(dest);
            let _ = fs::remove_dir(dir);
        }
        result
    }

    async fn snapshot_disk(&self, id: &str) -> VmResult<std::path::PathBuf> {
        let instance = self.load_instance(id)?;
        if instance.backend != VmBackend::Qemu {
            return Err(VmError::Unsupported(
                "Use Hyper-V checkpoints for Windows-hosted VMs",
            ));
        }
        let profile = self
            .images()?
            .images
            .into_iter()
            .find(|image| image.name == instance.image)
            .ok_or(VmError::NotFound)?
            .boot_profile;
        if profile != VmBootProfile::Legacy {
            return Err(VmError::Unsupported(
                "Offline disk snapshots of UEFI/TPM guests require matching per-snapshot firmware and TPM state",
            ));
        }
        // Do not trust stale process-local status after a server restart.
        // Persisted state must have been explicitly transitioned to Stopped.
        if instance.state != VmState::Stopped {
            return Err(VmError::Busy);
        }
        if self.inner.children.lock().await.contains_key(id) {
            return Err(VmError::Busy);
        }
        let dir = self.instance_dir(id);
        if fs::canonicalize(&dir)? != dir {
            return Err(VmError::NotOwned);
        }
        let disk = dir.join("disk.qcow2");
        let meta = disk.symlink_metadata()?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(VmError::NotOwned);
        }
        self.verify_overlay(&instance).await?;
        Ok(disk)
    }

    async fn snapshots_for_disk(&self, disk: &Path) -> VmResult<Vec<String>> {
        let args = vec![
            "snapshot".to_owned(),
            "-l".into(),
            disk.display().to_string(),
        ];
        let output = run_command("qemu-img", &args, Duration::from_secs(30)).await?;
        if !output.status.success() {
            return Err(VmError::CommandFailed);
        }
        let stdout = String::from_utf8(output.stdout).map_err(|_| VmError::InvalidData)?;
        Ok(parse_snapshot_list(&stdout))
    }
}

fn snapshot_list(id: &str, action: &'static str, names: Vec<String>) -> VmSnapshotList {
    VmSnapshotList {
        action,
        id: id.to_owned(),
        snapshots: names.into_iter().map(|name| VmSnapshot { name }).collect(),
    }
}

async fn qemu_snapshot(disk: &Path, operation: &str, name: &str) -> VmResult<()> {
    let args = vec![
        "snapshot".into(),
        operation.into(),
        name.to_owned(),
        disk.display().to_string(),
    ];
    let output = run_command("qemu-img", &args, Duration::from_secs(90)).await?;
    if !output.status.success() {
        return Err(VmError::CommandFailed);
    }
    Ok(())
}

fn parse_snapshot_list(output: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut header_seen = false;
    for line in output.lines() {
        let line = line.trim();
        if line.starts_with("ID") && line.contains("TAG") {
            header_seen = true;
            continue;
        }
        if !header_seen {
            continue;
        }
        let mut parts = line.split_whitespace();
        if let (Some(id), Some(tag)) = (parts.next(), parts.next())
            && id.bytes().all(|byte| byte.is_ascii_digit())
            && valid_name(tag, SNAPSHOT_NAME_MAX)
        {
            names.push(tag.to_owned());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real disposable QEMU + qcow2 smoke; never inspects existing user VMs.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn qemu_fork_offline_environment_snapshots_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("blank.qcow2");
        let create = tokio::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(&source)
            .arg("32M")
            .output()
            .await
            .expect("qemu-img installed for live smoke");
        assert!(create.status.success());
        let manager = VmManager::new(temp.path().join("vm-home")).unwrap();
        manager.import_image("blank", &source, 1, 512).unwrap();
        let instance = manager
            .create(super::super::VmCreate {
                image: "blank".into(),
                cpus: None,
                memory_mb: None,
                linux_options: None,
                installer_iso: None,
            })
            .await
            .expect("start disposable QEMU");
        let id = instance.instance.id;
        let run_result = async {
            manager.stop(&id).await?;
            manager.snapshot_create(&id, "clean").await?;
            manager.snapshot_create(&id, "with-vpn").await?;
            let listed = manager.snapshot_list(&id).await?;
            assert_eq!(listed.snapshots.len(), 2);
            manager.snapshot_restore(&id, "clean").await?;
            let promoted = manager.snapshot_promote(&id, "clean", "clean-env").await?;
            assert_eq!(promoted.name, "clean-env");
            assert_eq!(manager.images()?.image_count, 2);
            manager.snapshot_delete(&id, "with-vpn").await?;
            assert_eq!(manager.snapshot_list(&id).await?.snapshots.len(), 1);
            manager.start(&id).await?;
            manager.stop(&id).await?;
            Ok::<(), VmError>(())
        }
        .await;
        // Cleanup occurs even if one of the round-trip operations fails.
        if manager.inner.children.lock().await.contains_key(&id) {
            let _ = manager.stop(&id).await;
        }
        let destroyed = manager.destroy(&id).await;
        assert!(
            run_result.is_ok(),
            "live QEMU snapshot round trip: {run_result:?}"
        );
        assert!(destroyed.is_ok(), "disposable cleanup: {destroyed:?}");
    }

    #[test]
    fn parses_only_valid_qcow2_snapshot_tags() {
        let output = "Snapshot list:\nID        TAG               VM SIZE                DATE     VM CLOCK\n1         clean-ubuntu         0 B 2026-11-18 01:00\n2         vpn_gateway          0 B 2026-11-18 02:00\nnot a snapshot\n";
        assert_eq!(
            parse_snapshot_list(output),
            vec!["clean-ubuntu", "vpn_gateway"]
        );
        assert!(parse_snapshot_list("Snapshot list:\n").is_empty());
    }

    #[tokio::test]
    async fn refuses_unknown_vm_and_bad_snapshot_names() {
        let temp = tempfile::tempdir().unwrap();
        let manager = VmManager::new(temp.path()).unwrap();
        assert!(matches!(
            manager
                .snapshot_create("vm-0123456789abcdef", "../bad")
                .await,
            Err(VmError::InvalidName)
        ));
        assert!(matches!(
            manager.snapshot_list("vm-0123456789abcdef").await,
            Err(VmError::NotFound)
        ));
    }
}
