//! Linux QEMU launch policy for disposable, owned forks.
//! Never touches host interfaces, route tables, namespaces, DNS or firewall.
use crate::{VmBootProfile, VmError, VmResult};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::{
    ffi::OsString,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

const OVMF_CODE: &str = "/usr/share/OVMF/x64/OVMF_CODE.4m.fd";
const OVMF_VARS: &str = "/usr/share/OVMF/x64/OVMF_VARS.4m.fd";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VmNetwork {
    /// QEMU user-mode NAT; no host adapter, tap, DNS or firewall changes.
    Nat,
    /// Point-to-point, private Unix stream link. First VM listens, second connects.
    IsolatedListen {
        segment: String,
    },
    IsolatedConnect {
        segment: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinuxVmOptions {
    #[serde(default = "default_networks")]
    pub networks: Vec<VmNetwork>,
    #[serde(default)]
    pub display: bool,
    #[serde(default = "default_true")]
    pub guest_agent: bool,
}

fn default_true() -> bool {
    true
}
fn default_networks() -> Vec<VmNetwork> {
    vec![VmNetwork::Nat]
}
impl Default for LinuxVmOptions {
    fn default() -> Self {
        Self {
            networks: default_networks(),
            display: false,
            guest_agent: true,
        }
    }
}

impl LinuxVmOptions {
    /// Parse the same conservative topology specification for CLI and MCP.
    /// Absence means a single user-mode NAT NIC; explicit "none" means no NIC.
    pub fn from_network_specs(
        specs: Option<&[String]>,
        display: bool,
        guest_agent: bool,
    ) -> VmResult<Self> {
        let networks = match specs {
            None => default_networks(),
            Some(values) if values.len() == 1 && values[0] == "none" => Vec::new(),
            Some(values) => {
                if values.len() > 4 || values.is_empty() {
                    return Err(VmError::InvalidData);
                }
                let mut networks = Vec::new();
                for item in values {
                    let parse = |prefix: &str| -> VmResult<String> {
                        let segment = item.strip_prefix(prefix).ok_or(VmError::InvalidData)?;
                        if !valid_segment(segment) {
                            return Err(VmError::InvalidName);
                        }
                        Ok(segment.to_owned())
                    };
                    let network = if item == "nat" {
                        VmNetwork::Nat
                    } else if item.starts_with("listen:") {
                        VmNetwork::IsolatedListen {
                            segment: parse("listen:")?,
                        }
                    } else if item.starts_with("connect:") {
                        VmNetwork::IsolatedConnect {
                            segment: parse("connect:")?,
                        }
                    } else {
                        return Err(VmError::InvalidData);
                    };
                    networks.push(network);
                }
                networks
            }
        };
        Ok(Self {
            networks,
            display,
            guest_agent,
        })
    }
}

fn safe_path(path: &Path) -> VmResult<String> {
    let value = path.to_str().ok_or(VmError::InvalidData)?;
    if value.is_empty()
        || value.len() > 512
        || value
            .bytes()
            .any(|c| matches!(c, b',' | b'\n' | b'\r' | b'\0'))
    {
        return Err(VmError::InvalidData);
    }
    Ok(value.to_owned())
}

fn valid_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= 48
        && segment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

pub fn network_socket(instance_dir: &Path, segment: &str) -> VmResult<PathBuf> {
    if !valid_segment(segment) {
        return Err(VmError::InvalidName);
    }
    // root/instances/id -> root/networks/segment.sock
    let root = instance_dir
        .parent()
        .and_then(Path::parent)
        .ok_or(VmError::NotOwned)?;
    let directory = root.join("networks");
    if fs::canonicalize(&directory)? != directory {
        return Err(VmError::NotOwned);
    }
    Ok(directory.join(format!("{segment}.sock")))
}

/// Local UEFI pflash state is fork-local; host firmware is a fixed allowlisted read-only file.
/// Windows UEFI/TPM is intentionally rejected until owned swtpm lifecycle is implemented.
pub fn prepare_firmware(instance_dir: &Path, profile: VmBootProfile) -> VmResult<()> {
    match profile {
        VmBootProfile::Legacy => Ok(()),
        VmBootProfile::WindowsUefi => Err(VmError::Unsupported(
            "Windows UEFI forks require owned swtpm state",
        )),
        VmBootProfile::LinuxUefi => {
            let code = Path::new(OVMF_CODE);
            let vars = Path::new(OVMF_VARS);
            if !code.is_file() || !vars.is_file() {
                return Err(VmError::Unsupported(
                    "Host OVMF x64 firmware templates unavailable",
                ));
            }
            let target = instance_dir.join("uefi-vars.fd");
            let mut from = fs::File::open(vars)?;
            let mut to = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&target)?;
            std::io::copy(&mut from, &mut to)?;
            to.flush()?;
            to.sync_all()?;
            Ok(())
        }
    }
}

fn mac(id: &str, index: usize) -> String {
    let mut hasher = Sha256::new();
    hasher.update(id.as_bytes());
    hasher.update([index as u8]);
    let digest = hasher.finalize();
    format!(
        "52:54:00:{:02x}:{:02x}:{:02x}",
        digest[0], digest[1], digest[2]
    )
}

/// Construct argv without a host shell. The parent validates the owned disk and directory.
pub fn build_command(
    instance_dir: &Path,
    disk: &Path,
    iso: Option<&Path>,
    cpus: u8,
    memory: u32,
    boot: VmBootProfile,
    options: &LinuxVmOptions,
) -> VmResult<Vec<OsString>> {
    if !(1..=64).contains(&cpus) || !(256..=262_144).contains(&memory) || options.networks.len() > 4
    {
        return Err(VmError::InvalidData);
    }
    let path = safe_path(disk)?;
    let dir = safe_path(instance_dir)?;
    let instance_id = instance_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or(VmError::InvalidName)?;
    let mut result = vec![
        "-nodefaults".into(), "-machine".into(), "q35".into(),
        "-accel".into(), if Path::new("/dev/kvm").exists() { "kvm" } else { "tcg" }.into(),
        "-m".into(), memory.to_string().into(),
        "-smp".into(), cpus.to_string().into(),
        "-blockdev".into(),
        serde_json::json!({"driver":"qcow2","node-name":"disk0","file":{"driver":"file","filename":path}}).to_string().into(),
        "-device".into(), "virtio-blk-pci,drive=disk0".into(),
        "-device".into(), "virtio-vga".into(),
        "-device".into(), "qemu-xhci".into(),
        "-device".into(), "usb-kbd".into(),
        "-device".into(), "usb-tablet".into(),
        "-display".into(), "none".into(),
        "-qmp".into(), format!("unix:{dir}/qmp.sock,server=on,wait=off").into(),
    ];
    if let Some(media) = iso {
        result.extend([
            "-cdrom".into(),
            safe_path(media)?.into(),
            "-boot".into(),
            "order=cd".into(),
        ]);
    }
    if options.display {
        // A VNC Unix-domain socket is private to the explicitly owned fork dir.
        result.extend(["-vnc".into(), format!("unix:{dir}/display.sock").into()]);
    }
    if options.guest_agent {
        result.extend([
            "-chardev".into(),
            format!("socket,id=qga0,path={dir}/qga.sock,server=on,wait=off").into(),
            "-device".into(),
            "virtio-serial-pci".into(),
            "-device".into(),
            "virtserialport,chardev=qga0,name=org.qemu.guest_agent.0".into(),
        ]);
    }
    match boot {
        VmBootProfile::Legacy => {}
        VmBootProfile::WindowsUefi => {
            return Err(VmError::Unsupported(
                "Windows UEFI forks require owned swtpm state",
            ));
        }
        VmBootProfile::LinuxUefi => {
            let vars = instance_dir.join("uefi-vars.fd");
            if !vars.is_file() || vars.symlink_metadata()?.file_type().is_symlink() {
                return Err(VmError::NotOwned);
            }
            result.extend([
                "-drive".into(),
                format!("if=pflash,format=raw,readonly=on,file={OVMF_CODE}").into(),
                "-drive".into(),
                format!("if=pflash,format=raw,file={}", safe_path(&vars)?).into(),
            ]);
        }
    }
    for (index, net) in options.networks.iter().enumerate() {
        let id = format!("net{index}");
        let value = match net {
            VmNetwork::Nat => format!("user,id={id},net=10.0.{}.0/24", 20 + index),
            VmNetwork::IsolatedListen { segment } => {
                let path = network_socket(instance_dir, segment)?;
                if path.exists() || path.symlink_metadata().is_ok() {
                    return Err(VmError::Busy);
                }
                format!(
                    "stream,id={id},server=on,addr.type=unix,addr.path={}",
                    safe_path(&path)?
                )
            }
            VmNetwork::IsolatedConnect { segment } => {
                let path = network_socket(instance_dir, segment)?;
                #[cfg(unix)]
                if !path
                    .symlink_metadata()
                    .is_ok_and(|m| m.file_type().is_socket())
                {
                    return Err(VmError::NotFound);
                }
                format!(
                    "stream,id={id},server=off,addr.type=unix,addr.path={}",
                    safe_path(&path)?
                )
            }
        };
        result.extend(["-netdev".into(), value.into()]);
        let model = if boot == VmBootProfile::WindowsUefi {
            "e1000e"
        } else {
            "virtio-net-pci"
        };
        result.extend([
            "-device".into(),
            format!("{model},netdev={id},mac={}", mac(instance_id, index)).into(),
        ]);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_malformed_segments_and_host_path_option_injection() {
        assert!(!valid_segment("../host"));
        assert!(!valid_segment("a,b"));
        assert!(valid_segment("vpn-segment_1"));
        assert!(safe_path(Path::new("/tmp/vm,drive=host")).is_err());
    }

    #[test]
    fn nat_and_guest_agent_are_explicit_without_host_network_modification() {
        let opts = LinuxVmOptions {
            networks: vec![VmNetwork::Nat, VmNetwork::Nat],
            display: true,
            guest_agent: true,
        };
        let argv = build_command(
            Path::new("/tmp/owned/instances/vm-0123456789abcdef"),
            Path::new("/tmp/owned/instances/vm-0123456789abcdef/disk.qcow2"),
            None,
            2,
            1024,
            VmBootProfile::Legacy,
            &opts,
        )
        .unwrap();
        let args = argv
            .iter()
            .map(|x| x.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(args.contains("net=10.0.20.0/24"));
        assert!(args.contains("net=10.0.21.0/24"));
        assert!(args.contains("virtserialport,chardev=qga0"));
        assert!(args.contains("-vnc unix:"));
        assert!(!args.contains("tap,"));
        assert!(!args.contains("hostfwd="));
    }

    #[test]
    fn uefi_windows_without_swtpm_fails_closed() {
        let argv = build_command(
            Path::new("/tmp"),
            Path::new("/tmp/disk.qcow2"),
            None,
            2,
            1024,
            VmBootProfile::WindowsUefi,
            &LinuxVmOptions::default(),
        );
        assert!(matches!(argv, Err(VmError::Unsupported(_))));
    }
}
