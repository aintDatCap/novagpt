//! Execute commands inside a NovaGPT-owned, already-installed Windows libvirt VM.
//! Requires the trusted QEMU guest agent inside the guest; CLI-only (no MCP exposure).
#[cfg(target_os = "linux")]
use crate::valid_name;
use crate::{VmError, VmManager, VmResult};
#[cfg(target_os = "linux")]
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
#[cfg(target_os = "linux")]
use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use std::{fs, time::Instant};
#[cfg(target_os = "linux")]
use tokio::{
    process::Command,
    time::{Duration, sleep, timeout},
};

#[derive(Debug)]
pub struct WindowsGuestExecRequest {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Serialize)]
pub struct WindowsGuestExecResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[cfg(any(target_os = "linux", test))]
fn valid_guest_program(program: &str) -> bool {
    let bytes = program.as_bytes();
    (4..=512).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'\\'
        && !bytes.iter().any(|b| b.is_ascii_control())
}

#[cfg(any(target_os = "linux", test))]
fn valid_guest_args(args: &[String]) -> bool {
    args.len() <= 32
        && args.iter().all(|arg| {
            arg.len() <= 2048 && !arg.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r')
        })
}

#[cfg(target_os = "linux")]
async fn virsh(domain: &str, args: &[&str]) -> VmResult<Vec<u8>> {
    let out = timeout(
        Duration::from_secs(20),
        Command::new("virsh")
            .args(["-c", "qemu:///session"])
            .args(args)
            .output(),
    )
    .await
    .map_err(|_| VmError::Timeout)??;
    if !out.status.success() || out.stdout.len() > 192 * 1024 {
        return Err(if out.stdout.len() > 192 * 1024 {
            VmError::OutputLimit
        } else {
            VmError::CommandFailed
        });
    }
    // Domain is intentionally not a shell string; each argument is separate.
    let _ = domain;
    Ok(out.stdout)
}

#[cfg(target_os = "linux")]
async fn guest_agent(domain: &str, request: Value) -> VmResult<Value> {
    let payload = serde_json::to_string(&request).map_err(|_| VmError::InvalidData)?;
    let out = virsh(
        domain,
        &["qemu-agent-command", "--timeout", "12", domain, &payload],
    )
    .await?;
    let result: Value = serde_json::from_slice(&out).map_err(|_| VmError::InvalidData)?;
    result.get("return").cloned().ok_or(VmError::CommandFailed)
}

#[cfg(target_os = "linux")]
fn decode_output(value: &Value, key: &str) -> VmResult<String> {
    let base64 = value.get(key).and_then(Value::as_str).unwrap_or("");
    if base64.len() > 90 * 1024 {
        return Err(VmError::OutputLimit);
    }
    let bytes = STANDARD.decode(base64).map_err(|_| VmError::InvalidData)?;
    if bytes.len() > 48 * 1024 {
        return Err(VmError::OutputLimit);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

impl VmManager {
    pub async fn exec_windows_installer_guest(
        &self,
        request: WindowsGuestExecRequest,
    ) -> VmResult<WindowsGuestExecResult> {
        #[cfg(not(target_os = "linux"))]
        {
            let _ = request;
            Err(VmError::Unsupported("Linux user-session libvirt only"))
        }
        #[cfg(target_os = "linux")]
        {
            if !valid_name(&request.name, 48)
                || !valid_guest_program(&request.program)
                || !valid_guest_args(&request.args)
                || !(1..=300).contains(&request.timeout_secs)
            {
                return Err(VmError::InvalidData);
            }

            let home = self.root().join("installers").join(&request.name);
            if fs::canonicalize(&home)? != home {
                return Err(VmError::NotOwned);
            }
            let disk = home.join("installed.qcow2");
            if !disk.is_file() || disk.symlink_metadata()?.file_type().is_symlink() {
                return Err(VmError::NotOwned);
            }
            let domain = format!("novagpt-install-{}", request.name);
            // Never execute guest commands against an unrelated libvirt domain
            // that happens to use the same name.
            let disks = virsh(&domain, &["domblklist", &domain]).await?;
            if !String::from_utf8_lossy(&disks).contains(&disk.display().to_string()) {
                return Err(VmError::NotOwned);
            }
            guest_agent(&domain, json!({"execute":"guest-ping"})).await?;
            let start = Instant::now();
            let launched = guest_agent(
                &domain,
                json!({
                    "execute":"guest-exec",
                    "arguments": {
                        "path": request.program,
                        "arg": request.args,
                        "capture-output": true
                    }
                }),
            )
            .await?;
            let pid = launched
                .get("pid")
                .and_then(Value::as_u64)
                .ok_or(VmError::InvalidData)?;
            while start.elapsed() < Duration::from_secs(request.timeout_secs) {
                let state = guest_agent(
                    &domain,
                    json!({
                        "execute":"guest-exec-status", "arguments":{"pid":pid}
                    }),
                )
                .await?;
                if state.get("exited").and_then(Value::as_bool) == Some(true) {
                    let code = state.get("exitcode").and_then(Value::as_i64).unwrap_or(-1);
                    return Ok(WindowsGuestExecResult {
                        exit_code: i32::try_from(code).map_err(|_| VmError::InvalidData)?,
                        stdout: decode_output(&state, "out-data")?,
                        stderr: decode_output(&state, "err-data")?,
                        truncated: state
                            .get("out-truncated")
                            .and_then(Value::as_bool)
                            .unwrap_or(false)
                            || state
                                .get("err-truncated")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                    });
                }
                sleep(Duration::from_millis(400)).await;
            }
            Err(VmError::Timeout)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn guest_program_requires_absolute_windows_path() {
        assert!(valid_guest_program(r"C:\Windows\System32\cmd.exe"));
        assert!(!valid_guest_program(r"..\cmd.exe"));
        assert!(!valid_guest_program(r"\\server\share\cmd.exe"));
        assert!(!valid_guest_program("C:\\Windows\\cmd.exe\nmalicious"));
    }

    #[test]
    fn guest_arguments_reject_control_bytes_and_oversize() {
        assert!(valid_guest_args(&["/c".to_owned(), "whoami".to_owned()]));
        assert!(!valid_guest_args(&["hello\0world".to_owned()]));
        assert!(!valid_guest_args(&["x".repeat(3000)]));
    }
}
