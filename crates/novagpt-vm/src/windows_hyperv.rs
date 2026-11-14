//! Disposable ISO-based Hyper-V guests on Windows.
//! Owns only VMs whose names and configuration paths match this NovaGPT VM home.
use crate::{VmError, VmManager, VmResult, powershell, resources, valid_name};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tokio::time::Duration;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum HostGuestOs {
    Windows,
    Linux,
}

#[derive(Debug, Clone)]
pub struct HostIsoRequest {
    pub name: String,
    pub iso: PathBuf,
    pub os: HostGuestOs,
    pub username: Option<String>,
    pub cpus: u8,
    pub memory_mb: u32,
    pub disk_gib: u32,
}

#[derive(Debug, Serialize)]
pub struct HostIsoResult {
    pub domain: String,
    pub cpus: u8,
    pub memory_mb: u32,
    pub disk_gib: u32,
    pub unattended: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct OwnedInstaller {
    name: String,
    domain: String,
    os: HostGuestOs,
    cpus: u8,
    memory_mb: u32,
    disk_gib: u32,
}

pub(crate) fn hyperv_path(path: &Path) -> VmResult<String> {
    let raw = path.to_str().ok_or(VmError::InvalidData)?;
    let plain = raw.strip_prefix(r"\\?\").unwrap_or(raw);
    let bytes = plain.as_bytes();
    if bytes.len() > 240
        || bytes.len() < 3
        || !bytes[0].is_ascii_alphabetic()
        || bytes[1] != b':'
        || bytes[2] != b'\\'
    {
        return Err(VmError::InvalidData);
    }
    Ok(plain.to_owned())
}

fn vm_name(name: &str) -> String {
    format!("NovaGPT-install-{name}")
}

fn ps_string(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn valid_request(req: &HostIsoRequest) -> VmResult<()> {
    if !valid_name(&req.name, 48) || !(16..=2048).contains(&req.disk_gib) {
        return Err(VmError::InvalidData);
    }
    resources(req.cpus, req.memory_mb)
}

fn verify_installer_dir(root: &Path, name: &str) -> VmResult<PathBuf> {
    if !valid_name(name, 48) {
        return Err(VmError::InvalidName);
    }
    let parent = root.join("installers");
    if fs::canonicalize(&parent)? != parent {
        return Err(VmError::NotOwned);
    }
    let home = parent.join(name);
    if fs::canonicalize(&home)? != home {
        return Err(VmError::NotOwned);
    }
    Ok(home)
}

impl VmManager {
    /// CLI-only: the caller must explicitly select a trusted local installer ISO.
    pub async fn install_host_iso(&self, req: HostIsoRequest) -> VmResult<HostIsoResult> {
        valid_request(&req)?;
        let iso = fs::canonicalize(&req.iso)?;
        if !iso.is_file()
            || !iso
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|x| x.eq_ignore_ascii_case("iso"))
        {
            return Err(VmError::InvalidData);
        }
        let _serial = self.inner.commands.lock().await;
        let parent = self.root().join("installers");
        if parent.exists() && fs::canonicalize(&parent)? != parent {
            return Err(VmError::NotOwned);
        }
        fs::create_dir_all(&parent)?;
        let home = parent.join(&req.name);
        fs::create_dir(&home)?; // Do not reuse an existing installer or another VM.
        let owned = OwnedInstaller {
            name: req.name.clone(),
            domain: vm_name(&req.name),
            os: req.os,
            cpus: req.cpus,
            memory_mb: req.memory_mb,
            disk_gib: req.disk_gib,
        };
        let mut manifest = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(home.join("manifest.json"))?;
        manifest
            .write_all(&serde_json::to_vec_pretty(&owned).map_err(|_| VmError::InvalidData)?)?;
        manifest.sync_all()?;
        let answer = if req.os == HostGuestOs::Windows {
            let xml = crate::windows_unattend::unattended_xml(
                req.username.as_deref().unwrap_or("NovaTester"),
            )?;
            write_answer_iso(&home.join("answer.iso"), xml.as_bytes())?;
            Some(home.join("answer.iso"))
        } else {
            None
        };

        let name = ps_string(&owned.domain);
        let home_arg = ps_string(&hyperv_path(&home)?);
        let disk = ps_string(&hyperv_path(&home.join("installed.vhdx"))?);
        let install_iso = ps_string(&hyperv_path(&iso)?);
        // No external virtual switch: guests have no network connectivity by default.
        // The selected ISO is read-only and is never rewritten or copied.
        let mut script = format!(
            "$ErrorActionPreference = 'Stop'; \
             if (Get-VM -Name {name} -ErrorAction SilentlyContinue) {{ throw 'VM already exists' }}; \
             New-VHD -Path {disk} -Dynamic -SizeBytes {}GB -ErrorAction Stop | Out-Null; \
             New-VM -Name {name} -Path {home_arg} -Generation 2 -NoVHD -MemoryStartupBytes {}MB -ErrorAction Stop | Out-Null; \
             Add-VMHardDiskDrive -VMName {name} -Path {disk} -ErrorAction Stop; \
             Set-VMProcessor -VMName {name} -Count {} -ErrorAction Stop; \
             Set-VMMemory -VMName {name} -DynamicMemoryEnabled $false -StartupBytes {}MB -ErrorAction Stop; \
             Set-VM -Name {name} -AutomaticStartAction Nothing -AutomaticCheckpointsEnabled $false -ErrorAction Stop; \
             Add-VMDvdDrive -VMName {name} -Path {install_iso} -ErrorAction Stop; ",
            req.disk_gib, req.memory_mb, req.cpus, req.memory_mb
        );
        if let Some(answer_iso) = answer {
            script.push_str(&format!(
                "Add-VMDvdDrive -VMName {name} -Path {} -ErrorAction Stop; \
                 Set-VMFirmware -VMName {name} -EnableSecureBoot On -SecureBootTemplate 'MicrosoftWindows' -ErrorAction Stop; \
                 Set-VMKeyProtector -VMName {name} -NewLocalKeyProtector -ErrorAction Stop; \
                 Enable-VMTPM -VMName {name} -ErrorAction Stop; ",
                ps_string(&hyperv_path(&answer_iso)?)
            ));
        } else {
            script.push_str(&format!(
                "Set-VMFirmware -VMName {name} -EnableSecureBoot Off -ErrorAction Stop; "
            ));
        }
        script.push_str(&format!(
            "$bootDvd = Get-VMDvdDrive -VMName {name} | Where-Object {{ $_.Path -eq {install_iso} }} | Select-Object -First 1; \
             if (-not $bootDvd) {{ throw 'Installer DVD missing' }}; \
             Set-VMFirmware -VMName {name} -FirstBootDevice $bootDvd -ErrorAction Stop; \
             Start-VM -Name {name} -ErrorAction Stop; \
             $vm = Get-VM -Name {name} -ErrorAction Stop; \
             if ($vm.State -ne 'Running') {{ throw 'VM failed to start' }}"
        ));
        if owned.os == HostGuestOs::Windows {
            // Windows EFI media commonly require an explicit key to boot from
            // the DVD. Hold Space only during the first boot of a new blank disk.
            // Send the key to this new VM through Hyper-V WMI, not to the host UI.
            script.push_str(&format!(
                "; Start-Sleep -Milliseconds 750; \
                 $vmcs = Get-WmiObject -Namespace root\\virtualization\\v2 -Class Msvm_ComputerSystem -Filter \"ElementName={name}\" -ErrorAction Stop; \
                 if (-not $vmcs) {{ throw 'New VM not found in Hyper-V WMI' }}; \
                 $keyboard = $vmcs.GetRelated('Msvm_Keyboard') | Select-Object -First 1; \
                 if (-not $keyboard) {{ throw 'Hyper-V VM keyboard unavailable' }}; \
                 $pressed = $keyboard.PressKey([uint32]32); \
                 if ($pressed.ReturnValue -ne 0) {{ throw 'Unable to press guest boot key' }}; \
                 Start-Sleep -Seconds 6; \
                 $released = $keyboard.ReleaseKey([uint32]32); \
                 if ($released.ReturnValue -ne 0) {{ throw 'Unable to release guest boot key' }}"
            ));
        }
        // If a Hyper-V command fails, keep the owned directory for inspection;
        // never guess which partial resources may be safe to remove.
        powershell(&script, Duration::from_secs(150)).await?;
        Ok(HostIsoResult {
            domain: owned.domain,
            cpus: owned.cpus,
            memory_mb: owned.memory_mb,
            disk_gib: owned.disk_gib,
            unattended: owned.os == HostGuestOs::Windows,
        })
    }

    /// Inspect a specifically owned ISO guest without assuming that a name is sufficient ownership.
    pub async fn host_iso_status(&self, name: &str) -> VmResult<String> {
        let (owned, home) = self.load_host_iso(name)?;
        let script = checked_vm_script(&owned.domain, &home, "Write-Output $vm.State")?;
        Ok(powershell(&script, Duration::from_secs(20))
            .await?
            .trim()
            .to_owned())
    }

    pub async fn snapshot_host_iso(&self, name: &str, snapshot: &str) -> VmResult<()> {
        if !valid_name(snapshot, 48) {
            return Err(VmError::InvalidName);
        }
        let _serial = self.inner.commands.lock().await;
        let (owned, home) = self.load_host_iso(name)?;
        let body = format!(
            "if ($vm.State -ne 'Off') {{ throw 'Shut down guest before checkpointing' }}; \
             Checkpoint-VM -VM $vm -SnapshotName {} -ErrorAction Stop | Out-Null",
            ps_string(snapshot)
        );
        powershell(
            &checked_vm_script(&owned.domain, &home, &body)?,
            Duration::from_secs(120),
        )
        .await?;
        Ok(())
    }

    fn load_host_iso(&self, name: &str) -> VmResult<(OwnedInstaller, PathBuf)> {
        let home = verify_installer_dir(self.root(), name)?;
        let path = home.join("manifest.json");
        if path.symlink_metadata()?.file_type().is_symlink() {
            return Err(VmError::NotOwned);
        }
        let bytes = fs::read(path)?;
        if bytes.len() > 4096 {
            return Err(VmError::InvalidData);
        }
        let owned: OwnedInstaller =
            serde_json::from_slice(&bytes).map_err(|_| VmError::InvalidData)?;
        if owned.name != name
            || owned.domain != vm_name(name)
            || !home.join("installed.vhdx").is_file()
        {
            return Err(VmError::NotOwned);
        }
        Ok((owned, home))
    }
}

// The domain and its configuration location must both match our owned record.
// This prevents operations on similarly named VMs belonging to other software.
fn checked_vm_script(domain: &str, home: &Path, action: &str) -> VmResult<String> {
    let name = ps_string(domain);
    let path = ps_string(&hyperv_path(home)?);
    let disk = ps_string(&hyperv_path(&home.join("installed.vhdx"))?);
    Ok(format!(
        "$ErrorActionPreference = 'Stop'; \
         $vm = Get-VM -Name {name} -ErrorAction Stop; \
         if ([IO.Path]::GetFullPath($vm.Path).TrimEnd('\') -ine [IO.Path]::GetFullPath((Join-Path {path} {name})).TrimEnd('\')) {{ \
           throw 'VM configuration is outside the NovaGPT installer home' \
         }}; \
         $disks = @(Get-VMHardDiskDrive -VM $vm -ErrorAction Stop); \
         if ($disks.Count -ne 1 -or [IO.Path]::GetFullPath($disks[0].Path) -ine [IO.Path]::GetFullPath({disk})) {{ \
           throw 'VM disk is outside the NovaGPT installer home' \
         }}; {action}"
    ))
}

// Minimal single-file ISO9660 Level 2 image. Windows Setup searches removable
// optical media for Autounattend.xml; no privileged or third-party ISO tool needed.
fn dual16(dst: &mut [u8], n: u16) {
    dst[0..2].copy_from_slice(&n.to_le_bytes());
    dst[2..4].copy_from_slice(&n.to_be_bytes());
}
fn dual32(dst: &mut [u8], n: u32) {
    dst[0..4].copy_from_slice(&n.to_le_bytes());
    dst[4..8].copy_from_slice(&n.to_be_bytes());
}
fn dir_record(out: &mut [u8], lba: u32, length: u32, id: &[u8], directory: bool) -> usize {
    let size = 33 + id.len() + usize::from(id.len().is_multiple_of(2));
    out[..size].fill(0);
    out[0] = size as u8;
    dual32(&mut out[2..10], lba);
    dual32(&mut out[10..18], length);
    out[18..25].copy_from_slice(&[126, 1, 1, 0, 0, 0, 0]);
    out[25] = if directory { 2 } else { 0 };
    dual16(&mut out[28..32], 1);
    out[32] = id.len() as u8;
    out[33..33 + id.len()].copy_from_slice(id);
    size
}
fn write_answer_iso(destination: &Path, xml: &[u8]) -> VmResult<()> {
    const BLOCK: usize = 2048;
    const ROOT_LBA: usize = 20;
    const XML_LBA: usize = 21;
    if xml.is_empty() || xml.len() > 1024 * 1024 {
        return Err(VmError::InvalidData);
    }
    let blocks = XML_LBA + xml.len().div_ceil(BLOCK);
    let mut bytes = vec![0_u8; blocks * BLOCK];
    {
        let pvd = &mut bytes[16 * BLOCK..17 * BLOCK];
        pvd[0] = 1;
        pvd[1..6].copy_from_slice(b"CD001");
        pvd[6] = 1;
        pvd[8..40].fill(b' ');
        pvd[40..72].fill(b' ');
        pvd[40..56].copy_from_slice(b"NOVAGPT_UNATTEND");
        dual32(&mut pvd[80..88], blocks as u32);
        dual16(&mut pvd[120..124], 1);
        dual16(&mut pvd[124..128], 1);
        dual16(&mut pvd[128..132], BLOCK as u16);
        dual32(&mut pvd[132..140], 10);
        pvd[140..144].copy_from_slice(&18_u32.to_le_bytes());
        pvd[148..152].copy_from_slice(&19_u32.to_be_bytes());
        dir_record(
            &mut pvd[156..190],
            ROOT_LBA as u32,
            BLOCK as u32,
            &[0],
            true,
        );
        pvd[881] = 1;
    }
    let term = &mut bytes[17 * BLOCK..18 * BLOCK];
    term[0] = 255;
    term[1..6].copy_from_slice(b"CD001");
    term[6] = 1;
    for (lba, be) in [(18, false), (19, true)] {
        let table = &mut bytes[lba * BLOCK..(lba + 1) * BLOCK];
        table[0] = 1;
        if be {
            table[2..6].copy_from_slice(&(ROOT_LBA as u32).to_be_bytes());
            table[6..8].copy_from_slice(&1_u16.to_be_bytes());
        } else {
            table[2..6].copy_from_slice(&(ROOT_LBA as u32).to_le_bytes());
            table[6..8].copy_from_slice(&1_u16.to_le_bytes());
        }
    }
    let root = &mut bytes[ROOT_LBA * BLOCK..(ROOT_LBA + 1) * BLOCK];
    let mut pos = dir_record(root, ROOT_LBA as u32, BLOCK as u32, &[0], true);
    pos += dir_record(&mut root[pos..], ROOT_LBA as u32, BLOCK as u32, &[1], true);
    dir_record(
        &mut root[pos..],
        XML_LBA as u32,
        xml.len() as u32,
        b"AUTOUNATTEND.XML;1",
        false,
    );
    bytes[XML_LBA * BLOCK..XML_LBA * BLOCK + xml.len()].copy_from_slice(xml);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_has_valid_volume_and_unattend_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("answer.iso");
        write_answer_iso(&dest, b"<unattend/>").unwrap();
        let bytes = fs::read(dest).unwrap();
        assert_eq!(&bytes[16 * 2048 + 1..16 * 2048 + 6], b"CD001");
        assert_eq!(
            &bytes[20 * 2048 + 101..20 * 2048 + 119],
            b"AUTOUNATTEND.XML;1"
        );
        assert_eq!(&bytes[21 * 2048..21 * 2048 + 11], b"<unattend/>");
    }

    #[test]
    fn hyperv_paths_use_dos_names_without_unc_or_device_prefix() {
        assert_eq!(
            hyperv_path(Path::new(r"\\?\E:\VMs\win11.vhdx")).unwrap(),
            r"E:\VMs\win11.vhdx"
        );
        assert_eq!(
            hyperv_path(Path::new(r"C:\OS\win11.iso")).unwrap(),
            r"C:\OS\win11.iso"
        );
        assert!(hyperv_path(Path::new(r"\\server\share\disk.vhdx")).is_err());
        assert!(hyperv_path(Path::new(r"\relative\disk.vhdx")).is_err());
    }

    #[test]
    fn name_and_resource_rejections() {
        let req = HostIsoRequest {
            name: "../escape".into(),
            iso: PathBuf::new(),
            os: HostGuestOs::Linux,
            username: None,
            cpus: 4,
            memory_mb: 8192,
            disk_gib: 80,
        };
        assert!(valid_request(&req).is_err());
        assert!(
            valid_request(&HostIsoRequest {
                name: "ok".into(),
                cpus: 0,
                ..req.clone()
            })
            .is_err()
        );
        assert!(
            valid_request(&HostIsoRequest {
                name: "ok".into(),
                disk_gib: 4,
                ..req
            })
            .is_err()
        );
    }
}
