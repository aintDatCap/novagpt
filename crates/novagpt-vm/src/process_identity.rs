//! Conservative Linux process identity verification for a surviving QEMU fork.
//! Never signal a saved PID. Adoption requires matching /proc start ticks,
//! executable, effective owner, exact owned blockdev path and private QMP socket.
use crate::{VmError, VmResult};
use std::{fs, os::unix::fs::MetadataExt, path::Path};

const MAX_PROC_BYTES: usize = 256 * 1024;

pub(crate) fn start_ticks(pid: u32) -> VmResult<u64> {
    if pid == 0 {
        return Err(VmError::NotOwned);
    }
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    if stat.len() > 8192 {
        return Err(VmError::NotOwned);
    }
    let end = stat.rfind(") ").ok_or(VmError::NotOwned)?;
    // A zombie is no longer running QEMU and must not retain a guest lease.
    let mut fields = stat[end + 2..].split_ascii_whitespace();
    if matches!(fields.next(), Some("Z" | "X" | "x") | None) {
        return Err(VmError::NotOwned);
    }
    // Linux proc(5): field 3 is state (token zero), field 22 is
    // starttime (token 19). The iterator has consumed token zero.
    fields
        .nth(18)
        .ok_or(VmError::NotOwned)?
        .parse()
        .map_err(|_| VmError::NotOwned)
}

pub(crate) fn verify(instance_dir: &Path, pid: u32, expected_ticks: u64) -> bool {
    if pid == 0 || expected_ticks == 0 || start_ticks(pid).ok() != Some(expected_ticks) {
        return false;
    }
    let owned = match fs::canonicalize(instance_dir) {
        Ok(path) if path == instance_dir => path,
        _ => return false,
    };
    let disk = owned.join("disk.qcow2");
    if !disk
        .symlink_metadata()
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
    {
        return false;
    }
    // Never adopt a QEMU process running under another OS account.
    let uid = match fs::metadata("/proc/self") {
        Ok(meta) => meta.uid(),
        Err(_) => return false,
    };
    if fs::metadata(format!("/proc/{pid}")).map_or(true, |meta| meta.uid() != uid) {
        return false;
    }
    if fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .and_then(|path| path.file_name().map(|part| part.to_os_string()))
        .as_deref()
        != Some(std::ffi::OsStr::new("qemu-system-x86_64"))
    {
        return false;
    }
    let argv = match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) if bytes.len() <= MAX_PROC_BYTES => bytes,
        _ => return false,
    };
    let tokens: Vec<&[u8]> = argv
        .split(|byte| *byte == 0)
        .filter(|item| !item.is_empty())
        .collect();
    let qmp = format!("unix:{}/qmp.sock,server=on,wait=off", owned.display());
    let correct_qmp = tokens.contains(&qmp.as_bytes());
    let correct_disk = tokens.iter().any(|arg| {
        let Ok(block) = serde_json::from_slice::<serde_json::Value>(arg) else {
            return false;
        };
        block["driver"] == "qcow2"
            && block["node-name"] == "disk0"
            && block["file"]["driver"] == "file"
            && block["file"]["filename"].as_str() == disk.to_str()
    });
    correct_qmp && correct_disk
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Real QEMU restart-recovery regression using a blank, disposable guest.
    /// Simulates server termination by dropping the first manager (QEMU is
    /// intentionally not kill-on-drop) and reattaching under a new home lease.
    #[tokio::test]
    async fn real_disposable_qemu_survives_manager_reopen_and_is_destroyable() {
        use crate::{VmCreate, VmManager, VmState};
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("blank.qcow2");
        let generated = tokio::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(&source)
            .arg("32M")
            .output()
            .await
            .unwrap();
        assert!(generated.status.success());
        let home = temp.path().join("owned");
        let manager = VmManager::new(&home).unwrap();
        manager.import_image("blank", &source, 1, 512).unwrap();
        let created = manager
            .create(VmCreate {
                image: "blank".into(),
                cpus: None,
                memory_mb: None,
                linux_options: Some(crate::linux_qemu::LinuxVmOptions::default()),
                installer_iso: None,
            })
            .await
            .expect("start disposable QEMU");
        let id = created.instance.id.clone();
        let dir = home.join("instances").join(&id);
        let pid = created.instance.qemu_pid.unwrap();
        let ticks = created.instance.qemu_start_ticks.unwrap();
        assert!(
            verify(&dir, pid, ticks),
            "QEMU must match its exact disk and QMP identity"
        );
        assert!(
            !verify(&dir, pid, ticks.wrapping_add(1)),
            "PID reuse must be denied"
        );
        drop(manager);
        let recovered =
            VmManager::new(&home).expect("exclusive lease released after simulated restart");
        let behavior = async {
            let status = recovered.status(&id).await?;
            assert_eq!(status.instance.state, VmState::Running);
            let shot = recovered.desktop_screenshot(&id).await?;
            assert_eq!(shot.mime_type, "image/png");
            assert!(shot.width > 300);
            Ok::<(), VmError>(())
        }
        .await;
        let cleanup = recovered.destroy(&id).await;
        if cleanup.is_err() {
            // Best-effort emergency shutdown via this test's private monitor.
            let _ = crate::qmp::request_quit(&dir.join("qmp.sock")).await;
        }
        assert!(
            behavior.is_ok(),
            "recovered VM must support MCP-backed desktop operations: {behavior:?}"
        );
        assert!(
            cleanup.is_ok(),
            "verified QEMU orphan must be safely destroyable: {cleanup:?}"
        );
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn recovered_disposable_guest_can_stop_and_restart_without_pid_signals() {
        use crate::{VmCreate, VmManager, VmState};
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("blank.qcow2");
        let create = tokio::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(&source)
            .arg("32M")
            .output()
            .await
            .unwrap();
        assert!(create.status.success());
        let home = temp.path().join("owned");
        let first = VmManager::new(&home).unwrap();
        first.import_image("blank", &source, 1, 512).unwrap();
        let original = first
            .create(VmCreate {
                image: "blank".into(),
                cpus: None,
                memory_mb: None,
                linux_options: None,
                installer_iso: None,
            })
            .await
            .unwrap();
        let id = original.instance.id;
        drop(first);
        let second = VmManager::new(&home).unwrap();
        let behavior = async {
            assert_eq!(second.status(&id).await?.instance.state, VmState::Running);
            let stopped = second.stop(&id).await?;
            assert_eq!(stopped.instance.state, VmState::Stopped);
            assert!(stopped.instance.qemu_pid.is_none());
            let started = second.start(&id).await?;
            assert_eq!(started.instance.state, VmState::Running);
            assert!(started.instance.qemu_pid.is_some());
            Ok::<(), VmError>(())
        }
        .await;
        let cleaned = second.destroy(&id).await;
        assert!(
            behavior.is_ok(),
            "recover/stop/start sequence: {behavior:?}"
        );
        assert!(
            cleaned.is_ok(),
            "recovered guest final cleanup: {cleaned:?}"
        );
    }

    #[test]
    fn rejects_missing_process_or_wrong_identity() {
        let temp = tempfile::tempdir().unwrap();
        assert!(!verify(temp.path(), u32::MAX, 123));
        assert!(!verify(temp.path(), std::process::id(), 123));
        assert!(start_ticks(u32::MAX).is_err());
    }

    #[test]
    fn reads_current_proc_start_clock_without_trusting_it_as_qemu() {
        let ticks = start_ticks(std::process::id()).unwrap();
        assert!(ticks > 0);
        let temp = tempfile::tempdir().unwrap();
        assert!(!verify(temp.path(), std::process::id(), ticks));
    }
}
