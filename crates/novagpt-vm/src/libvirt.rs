//! Explicitly registered persistent libvirt guests. These are not disposable forks.
#[cfg(target_os = "linux")]
use crate::{VmBackend, VmInstance, VmState};
use crate::{VmError, VmManager, VmResult, VmStatus, resources};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, path::PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    name: String,
    uuid: Uuid,
    disk: PathBuf,
    cpus: u8,
    memory_mb: u32,
}

#[derive(Debug, Serialize)]
pub struct RegisteredDomain {
    pub id: String,
    pub name: String,
    pub cpus: u8,
    pub memory_mb: u32,
}

impl Entry {
    fn id(&self) -> String {
        format!("libvirt-{}", self.uuid)
    }

    fn validate(&self) -> VmResult<()> {
        if !crate::valid_name(&self.name, 80)
            || self.uuid.is_nil()
            || !self.disk.is_absolute()
            || self.disk.extension().and_then(|s| s.to_str()) != Some("qcow2")
        {
            return Err(VmError::InvalidData);
        }
        resources(self.cpus, self.memory_mb)
    }

    fn summary(&self) -> RegisteredDomain {
        RegisteredDomain {
            id: self.id(),
            name: self.name.clone(),
            cpus: self.cpus,
            memory_mb: self.memory_mb,
        }
    }
}

fn read_registry(path: &std::path::Path) -> VmResult<Vec<Entry>> {
    let meta = match path.symlink_metadata() {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 32_768 {
        return Err(VmError::InvalidData);
    }
    let data = fs::read(path)?;
    if data.len() > 32_768 {
        return Err(VmError::InvalidData);
    }
    let entries: Vec<Entry> = serde_json::from_slice(&data).map_err(|_| VmError::InvalidData)?;
    if entries.len() > 32 {
        return Err(VmError::InvalidData);
    }
    let (mut uuids, mut names, mut disks) = (HashSet::new(), HashSet::new(), HashSet::new());
    for entry in &entries {
        entry.validate()?;
        if !uuids.insert(entry.uuid) || !names.insert(&entry.name) || !disks.insert(&entry.disk) {
            return Err(VmError::InvalidData);
        }
    }
    Ok(entries)
}

impl VmManager {
    pub(crate) fn registered_domains(&self) -> VmResult<Vec<RegisteredDomain>> {
        if !cfg!(target_os = "linux") {
            return Ok(Vec::new());
        }
        Ok(read_registry(&self.root().join("libvirt-domains.json"))?
            .iter()
            .map(Entry::summary)
            .collect())
    }

    #[cfg(target_os = "linux")]
    async fn checked_libvirt_entry(&self, id: &str) -> VmResult<Entry> {
        let entry = read_registry(&self.root().join("libvirt-domains.json"))?
            .into_iter()
            .find(|entry| entry.id() == id)
            .ok_or(VmError::NotFound)?;
        if fs::canonicalize(&entry.disk)? != entry.disk || !entry.disk.symlink_metadata()?.is_file()
        {
            return Err(VmError::NotOwned);
        }
        let uuid = entry.uuid.to_string();
        if virsh(&["domname", &uuid]).await?.trim() != entry.name {
            return Err(VmError::NotOwned);
        }
        // UUID plus the exact attached writable disk must still match approval.
        // A renamed, replaced or multi-disk domain must be re-approved locally.
        let attached = virsh(&["domblklist", "--details", &uuid]).await?;
        verify_disk(&attached, &entry.disk)?;
        Ok(entry)
    }

    pub(crate) async fn libvirt_status(&self, id: &str) -> VmResult<VmStatus> {
        #[cfg(target_os = "linux")]
        {
            let entry = self.checked_libvirt_entry(id).await?;
            status(&entry, "status").await
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = id;
            Err(VmError::Unsupported("Linux user-session libvirt only"))
        }
    }
}

#[cfg(target_os = "linux")]
fn verify_disk(output: &str, disk: &std::path::Path) -> VmResult<()> {
    let disks: Vec<_> = output
        .lines()
        .filter_map(|line| {
            // virsh pads columns, so use split_whitespace for the first three,
            // then retain the complete source path including any spaces.
            let mut rest = line.trim();
            let mut columns = Vec::new();
            for _ in 0..3 {
                let end = rest.find(char::is_whitespace)?;
                columns.push(&rest[..end]);
                rest = rest[end..].trim_start();
            }
            (columns[1] == "disk").then_some((columns[0], rest))
        })
        .collect();
    if disks.len() != 1 || disks[0].0 != "file" || std::path::Path::new(disks[0].1) != disk {
        return Err(VmError::NotOwned);
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn virsh(args: &[&str]) -> VmResult<String> {
    use std::process::Stdio;
    use tokio::{
        process::Command,
        time::{Duration, timeout},
    };
    let output = timeout(
        Duration::from_secs(15),
        Command::new("virsh")
            .env("LC_ALL", "C")
            .args(["-c", "qemu:///session"])
            .args(args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| VmError::Timeout)??;
    if output.stdout.len() > 48 * 1024 || output.stderr.len() > 48 * 1024 {
        return Err(VmError::OutputLimit);
    }
    if !output.status.success() {
        return Err(VmError::CommandFailed);
    }
    String::from_utf8(output.stdout).map_err(|_| VmError::InvalidData)
}

#[cfg(target_os = "linux")]
async fn status(entry: &Entry, action: &'static str) -> VmResult<VmStatus> {
    let state = match virsh(&["domstate", &entry.uuid.to_string()]).await?.trim() {
        "running" => VmState::Running,
        "shut off" => VmState::Stopped,
        _ => VmState::Failed,
    };
    Ok(VmStatus {
        action,
        instance: VmInstance {
            id: entry.id(),
            image: entry.name.clone(),
            backend: VmBackend::Qemu,
            state,
            cpus: entry.cpus,
            memory_mb: entry.memory_mb,
            installer_iso: None,
            qemu_pid: None,
            qemu_start_ticks: None,
        },
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn approval_requires_exact_single_disk() {
        let disk = std::path::Path::new("/tmp/test disk.qcow2");
        let line = " file   disk     vda      /tmp/test disk.qcow2";
        assert!(verify_disk(line, disk).is_ok());
        assert!(verify_disk(" file disk vda /tmp/other.qcow2", disk).is_err());
        assert!(verify_disk(&format!("{line}\n file disk vdb /tmp/other.qcow2"), disk).is_err());
        assert!(verify_disk(" block disk vda /tmp/test disk.qcow2", disk).is_err());
        assert!(verify_disk(" file cdrom sda /tmp/test disk.qcow2", disk).is_err());
    }

    #[test]
    fn registry_rejects_duplicate_and_relative_disks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("libvirt-domains.json");
        assert!(read_registry(&path).unwrap().is_empty());
        let mut entry = Entry {
            name: "test-guest".into(),
            uuid: Uuid::new_v4(),
            disk: "/tmp/test.qcow2".into(),
            cpus: 2,
            memory_mb: 4096,
        };
        fs::write(&path, serde_json::to_vec(&[entry.clone()]).unwrap()).unwrap();
        assert_eq!(read_registry(&path).unwrap().len(), 1);
        fs::write(
            &path,
            serde_json::to_vec(&[entry.clone(), entry.clone()]).unwrap(),
        )
        .unwrap();
        assert!(read_registry(&path).is_err());
        entry.disk = "relative.qcow2".into();
        fs::write(&path, serde_json::to_vec(&[entry]).unwrap()).unwrap();
        assert!(read_registry(&path).is_err());
    }
}
