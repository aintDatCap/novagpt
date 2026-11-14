//! Linux-only disposable Windows 11 ISO provisioning through user-session libvirt.
//! Unattended answer-file approach adapted from CodexPro (MIT license).
//! Copyright (c) 2026 CodexPro contributors. See LICENSE in that project.
#[cfg(target_os = "linux")]
use crate::resources;
use crate::{VmError, VmManager, VmResult, valid_name};
use serde::Serialize;
#[cfg(target_os = "linux")]
use std::fs;
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use tokio::{
    process::Command,
    time::{Duration, timeout},
};

#[derive(Debug, Clone)]
pub struct WindowsInstallRequest {
    pub name: String,
    pub iso: PathBuf,
    pub username: String,
    pub cpus: u8,
    pub memory_mb: u32,
    pub disk_gib: u32,
}

#[derive(Debug, Serialize)]
pub struct WindowsInstallResult {
    pub domain: String,
    pub cpus: u8,
    pub memory_mb: u32,
    pub disk_gib: u32,
    pub unattended: bool,
}

fn valid_user(username: &str) -> bool {
    let lower = username.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "administrator" | "guest" | "defaultaccount" | "wdagutilityaccount" | "system"
    ) && username.len() <= 20
        && valid_name(username, 20)
}

// The blank local-account password is intentionally limited to a disposable guest;
// never expose this installer to a reachable untrusted network.
pub fn unattended_xml(username: &str) -> VmResult<String> {
    if !valid_user(username) {
        return Err(VmError::InvalidName);
    }
    Ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<unattend xmlns="urn:schemas-microsoft-com:unattend">
  <settings pass="windowsPE">
    <component name="Microsoft-Windows-International-Core-WinPE" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <SetupUILanguage><UILanguage>en-US</UILanguage></SetupUILanguage>
      <InputLocale>en-US</InputLocale><SystemLocale>en-US</SystemLocale><UILanguage>en-US</UILanguage><UserLocale>en-US</UserLocale>
    </component>
    <component name="Microsoft-Windows-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <UserData><AcceptEula>true</AcceptEula><ProductKey><Key /></ProductKey></UserData>
      <DiskConfiguration><WillShowUI>OnError</WillShowUI><DisableEncryptedDiskProvisioning>true</DisableEncryptedDiskProvisioning>
        <Disk wcm:action="add"><DiskID>0</DiskID><WillWipeDisk>true</WillWipeDisk>
          <CreatePartitions>
            <CreatePartition wcm:action="add"><Order>1</Order><Type>EFI</Type><Size>260</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>2</Order><Type>MSR</Type><Size>16</Size></CreatePartition>
            <CreatePartition wcm:action="add"><Order>3</Order><Type>Primary</Type><Extend>true</Extend></CreatePartition>
          </CreatePartitions>
          <ModifyPartitions>
            <ModifyPartition wcm:action="add"><Order>1</Order><PartitionID>1</PartitionID><Label>EFI</Label><Format>FAT32</Format></ModifyPartition>
            <ModifyPartition wcm:action="add"><Order>2</Order><PartitionID>3</PartitionID><Label>Windows</Label><Letter>C</Letter><Format>NTFS</Format></ModifyPartition>
          </ModifyPartitions>
        </Disk>
      </DiskConfiguration>
      <ImageInstall><OSImage><WillShowUI>OnError</WillShowUI>
        <InstallFrom><MetaData wcm:action="add"><Key>/IMAGE/INDEX</Key><Value>1</Value></MetaData></InstallFrom>
        <InstallTo><DiskID>0</DiskID><PartitionID>3</PartitionID></InstallTo>
      </OSImage></ImageInstall>
    </component>
  </settings>
  <settings pass="specialize">
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <ComputerName>ACE-WIN11-TEST</ComputerName>
    </component>
  </settings>
  <settings pass="oobeSystem">
    <component name="Microsoft-Windows-International-Core" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS">
      <InputLocale>en-US</InputLocale><SystemLocale>en-US</SystemLocale><UILanguage>en-US</UILanguage><UserLocale>en-US</UserLocale>
    </component>
    <component name="Microsoft-Windows-Shell-Setup" processorArchitecture="amd64" publicKeyToken="31bf3856ad364e35" language="neutral" versionScope="nonSxS" xmlns:wcm="http://schemas.microsoft.com/WMIConfig/2002/State">
      <TimeZone>W. Europe Standard Time</TimeZone>
      <OOBE>
        <HideEULAPage>true</HideEULAPage><HideOEMRegistrationScreen>true</HideOEMRegistrationScreen>
        <HideOnlineAccountScreens>true</HideOnlineAccountScreens><HideWirelessSetupInOOBE>true</HideWirelessSetupInOOBE>
        <ProtectYourPC>3</ProtectYourPC>
      </OOBE>
      <UserAccounts><LocalAccounts><LocalAccount wcm:action="add">
        <Name>{username}</Name><DisplayName>{username}</DisplayName><Group>Administrators</Group>
        <Password><Value>UABhAHMAcwB3AG8AcgBkAA==</Value><PlainText>false</PlainText></Password>
      </LocalAccount></LocalAccounts></UserAccounts>
    </component>
  </settings>
</unattend>
"#
    ))
}

#[cfg(target_os = "linux")]
async fn run(program: &str, args: &[String], max_secs: u64) -> VmResult<()> {
    let output = timeout(
        Duration::from_secs(max_secs),
        Command::new(program).args(args).output(),
    )
    .await
    .map_err(|_| VmError::Timeout)??;
    if !output.status.success() {
        return Err(VmError::CommandFailed);
    }
    Ok(())
}

impl VmManager {
    /// Provision a new disposable Linux libvirt VM from an explicitly approved
    /// local ISO. The installer is persistent across NovaGPT restarts.
    /// This is a host-only CLI workflow, not an MCP arbitrary-path operation.
    pub async fn install_windows_iso(
        &self,
        request: WindowsInstallRequest,
    ) -> VmResult<WindowsInstallResult> {
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = request;
            return Err(VmError::Unsupported(
                "Windows ISO installation requires Linux or Windows",
            ));
        }
        #[cfg(windows)]
        {
            use crate::windows_hyperv::{HostGuestOs, HostIsoRequest};
            let result = self
                .install_host_iso(HostIsoRequest {
                    name: request.name,
                    iso: request.iso,
                    os: HostGuestOs::Windows,
                    username: Some(request.username),
                    cpus: request.cpus,
                    memory_mb: request.memory_mb,
                    disk_gib: request.disk_gib,
                })
                .await?;
            Ok(WindowsInstallResult {
                domain: result.domain,
                cpus: result.cpus,
                memory_mb: result.memory_mb,
                disk_gib: result.disk_gib,
                unattended: result.unattended,
            })
        }
        #[cfg(target_os = "linux")]
        {
            if !valid_name(&request.name, 48) || !(64..=2048).contains(&request.disk_gib) {
                return Err(VmError::InvalidData);
            }
            resources(request.cpus, request.memory_mb)?;
            let xml = unattended_xml(&request.username)?;
            let iso = fs::canonicalize(&request.iso)?;
            if !iso.is_file()
                || iso
                    .extension()
                    .and_then(|s| s.to_str())
                    .is_none_or(|ext| !ext.eq_ignore_ascii_case("iso"))
            {
                return Err(VmError::InvalidData);
            }
            let installers = self.root().join("installers");
            if installers.exists() && installers.symlink_metadata()?.file_type().is_symlink() {
                return Err(VmError::NotOwned);
            }
            fs::create_dir_all(&installers)?;
            if fs::canonicalize(&installers)? != installers {
                return Err(VmError::NotOwned);
            }
            let home = installers.join(&request.name);
            fs::create_dir(&home)?; // Never replace an existing test VM's files.
            let answers = home.join("answers");
            fs::create_dir(&answers)?;
            let xml_path = answers.join("Autounattend.xml");
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&xml_path)?;
            use std::io::Write;
            file.write_all(xml.as_bytes())?;
            file.sync_all()?;
            let answer_iso = home.join("answer.iso");
            run(
                "xorriso",
                &[
                    "-as".into(),
                    "mkisofs".into(),
                    "-quiet".into(),
                    "-R".into(),
                    "-J".into(),
                    "-V".into(),
                    "NOVAGPT_UNATTEND".into(),
                    "-o".into(),
                    answer_iso.display().to_string(),
                    answers.display().to_string(),
                ],
                30,
            )
            .await?;
            let domain = format!("novagpt-install-{}", request.name);
            let disk = home.join("installed.qcow2");
            let args = vec![
                "--connect".into(),
                "qemu:///session".into(),
                "--name".into(),
                domain.clone(),
                "--memory".into(),
                request.memory_mb.to_string(),
                "--vcpus".into(),
                request.cpus.to_string(),
                "--cpu".into(),
                "host-passthrough".into(),
                "--osinfo".into(),
                "win11".into(),
                "--disk".into(),
                format!(
                    "path={},size={},format=qcow2,bus=sata",
                    disk.display(),
                    request.disk_gib
                ),
                "--cdrom".into(),
                iso.display().to_string(),
                "--disk".into(),
                format!(
                    "path={},device=cdrom,bus=sata,readonly=on",
                    answer_iso.display()
                ),
                "--network".into(),
                "user,model=e1000e".into(),
                "--tpm".into(),
                "backend.type=emulator,model=tpm-crb".into(),
                "--channel".into(),
                "unix,target.type=virtio,target.name=org.qemu.guest_agent.0".into(),
                "--graphics".into(),
                "spice,listen=127.0.0.1".into(),
                "--video".into(),
                "vga".into(),
                "--boot".into(),
                "uefi".into(),
                "--noautoconsole".into(),
                "--wait".into(),
                "0".into(),
            ];
            run("virt-install", &args, 120).await?;
            Ok(WindowsInstallResult {
                domain,
                cpus: request.cpus,
                memory_mb: request.memory_mb,
                disk_gib: request.disk_gib,
                unattended: true,
            })
        }
    }
}

impl VmManager {
    /// Create a persistent, named libvirt snapshot only after the guest is
    /// fully shut off. This does not expose arbitrary libvirt domains via MCP.
    pub async fn snapshot_windows_installer(&self, name: &str, snapshot: &str) -> VmResult<()> {
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = (name, snapshot);
            return Err(VmError::Unsupported(
                "Linux libvirt or Windows Hyper-V required",
            ));
        }
        #[cfg(windows)]
        {
            self.snapshot_host_iso(name, snapshot).await
        }
        #[cfg(target_os = "linux")]
        {
            if !valid_name(name, 48) || !valid_name(snapshot, 48) {
                return Err(VmError::InvalidName);
            }
            let home = self.root().join("installers").join(name);
            if fs::canonicalize(&home)? != home || !home.join("installed.qcow2").is_file() {
                return Err(VmError::NotOwned);
            }
            let domain = format!("novagpt-install-{name}");
            let status = timeout(
                Duration::from_secs(15),
                Command::new("virsh")
                    .args(["-c", "qemu:///session", "domstate", &domain])
                    .output(),
            )
            .await
            .map_err(|_| VmError::Timeout)??;
            if !status.status.success() {
                return Err(VmError::CommandFailed);
            }
            if String::from_utf8_lossy(&status.stdout).trim() != "shut off" {
                return Err(VmError::Busy);
            }
            run(
                "virsh",
                &[
                    "-c".into(),
                    "qemu:///session".into(),
                    "snapshot-create-as".into(),
                    "--domain".into(),
                    domain,
                    "--name".into(),
                    snapshot.to_owned(),
                    "--atomic".into(),
                ],
                60,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn xml_has_disposable_partition_and_local_oobe() {
        let xml = unattended_xml("AceTester").unwrap();
        assert!(xml.contains("<DiskID>0</DiskID>"));
        assert!(xml.contains("<ProtectYourPC>3</ProtectYourPC>"));
        assert!(xml.contains("<Name>AceTester</Name>"));
        assert!(xml.contains("<Value>1</Value>"));
    }
    #[test]
    fn invalid_names_do_not_become_xml() {
        assert!(unattended_xml("<evil>").is_err());
        assert!(unattended_xml("Administrator").is_err());
        assert!(unattended_xml("../oops").is_err());
    }
}
