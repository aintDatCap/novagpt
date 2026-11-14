//! Credential-scoped Windows PowerShell Direct operations against exactly one
//! NovaGPT-owned running differencing disk. No WinRM/network or host path from
//! callers, and no credential is written to disk or a process command line.
use crate::{
    VmBootProfile, VmError, VmExecResult, VmManager, VmResult, windows_hyperv::hyperv_path,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::{path::Path, process::Stdio};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    time::{Duration, timeout},
};

const MAX_FILE_BYTES: usize = 24 * 1024;
const MAX_GUEST_OUTPUT: usize = 48 * 1024;

#[derive(Clone, Serialize)]
pub struct GuestCredential {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
struct HostTask<'a> {
    name: String,
    disk: String,
    credential: &'a GuestCredential,
    guest: GuestTask<'a>,
}

#[derive(Serialize)]
struct GuestTask<'a> {
    operation: &'static str,
    program: Option<&'a str>,
    args: &'a [String],
    guest_path: Option<&'a str>,
    bytes_base64: Option<&'a str>,
}

#[derive(Deserialize)]
struct GuestReply {
    exit_code: Option<i32>,
    stdout: Option<String>,
    stderr: Option<String>,
    truncated: Option<bool>,
    bytes: Option<usize>,
    bytes_base64: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct GuestFileReceipt {
    pub id: String,
    pub bytes: usize,
    pub data_base64: Option<String>,
}

const POWERSHELL_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$payload = [Console]::In.ReadToEnd() | ConvertFrom-Json -ErrorAction Stop
$vm = Get-VM -Name $payload.name -ErrorAction Stop
if ($vm.State -ne 'Running') { throw 'VM is not running' }
$attached = @(Get-VMHardDiskDrive -VM $vm -ErrorAction Stop)
if ($attached.Count -ne 1 -or [IO.Path]::GetFullPath($attached[0].Path) -ine [IO.Path]::GetFullPath($payload.disk)) {
    throw 'Disk ownership check failed'
}
$secure = ConvertTo-SecureString -String $payload.credential.password -AsPlainText -Force
$credential = [PSCredential]::new([string]$payload.credential.username, $secure)
# The guest receives no credential. Only validated guest values are serialized.
$guestJson = ConvertTo-Json -InputObject $payload.guest -Depth 5 -Compress
$response = Invoke-Command -VMId $vm.VMId -Credential $credential -ErrorAction Stop -ScriptBlock {
    param([string]$taskJson)
    $task = ConvertFrom-Json -InputObject $taskJson
    switch ($task.operation) {
        'exec' {
            $global:LASTEXITCODE = 0
            $argv = @($task.args | ForEach-Object { [string]$_ })
            $output = @(& $task.program @argv 2>&1)
            $exit = if ($null -eq $LASTEXITCODE) { 0 } else { [int]$LASTEXITCODE }
            $out = (@($output | Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] }) | Out-String)
            $err = (@($output | Where-Object { $_ -is [System.Management.Automation.ErrorRecord] }) | Out-String)
            $truncated = ($out.Length -gt 32000 -or $err.Length -gt 12000)
            if ($out.Length -gt 32000) { $out = $out.Substring(0,32000) }
            if ($err.Length -gt 12000) { $err = $err.Substring(0,12000) }
            [pscustomobject]@{exit_code=$exit; stdout=$out; stderr=$err; truncated=$truncated}
        }
        'upload' {
            $bytes = [Convert]::FromBase64String([string]$task.bytes_base64)
            if ($bytes.Length -gt 24576) { throw 'File too large' }
            [IO.File]::WriteAllBytes([string]$task.guest_path, $bytes)
            [pscustomobject]@{bytes=$bytes.Length}
        }
        'download' {
            $p = [string]$task.guest_path
            $file = [IO.FileInfo]::new($p)
            if (-not $file.Exists -or $file.Length -gt 24576) { throw 'Invalid file length' }
            $bytes = [IO.File]::ReadAllBytes($p)
            [pscustomobject]@{bytes=$bytes.Length; bytes_base64=[Convert]::ToBase64String($bytes)}
        }
        default { throw 'Unsupported guest operation' }
    }
} -ArgumentList $guestJson
Write-Output ('NOVAGPT_GUEST_JSON:' + (ConvertTo-Json -InputObject $response -Depth 5 -Compress))
"#;

fn valid_guest_exe(exe: &str) -> bool {
    let bytes = exe.as_bytes();
    (4..=1024).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'\\'
        && !bytes.iter().any(|byte| byte.is_ascii_control())
}

fn valid_credential(cred: &GuestCredential) -> bool {
    !cred.username.is_empty()
        && cred.username.len() <= 256
        && !cred.password.is_empty()
        && cred.password.len() <= 1024
        && !cred.username.chars().any(char::is_control)
        && !cred.password.chars().any(char::is_control)
}

fn valid_guest_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    (4..=4096).contains(&bytes.len())
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'\\'
        && !bytes
            .iter()
            .any(|byte| byte == &0 || byte == &10 || byte == &13)
}

async fn send_to_powershell(input: &str, time_limit: Duration) -> VmResult<GuestReply> {
    let utf16: Vec<u8> = POWERSHELL_SCRIPT
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let encoded = STANDARD.encode(utf16);
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encoded])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut stdin = child.stdin.take().ok_or(VmError::CommandFailed)?;
    stdin.write_all(input.as_bytes()).await?;
    drop(stdin);
    let output = timeout(time_limit, child.wait_with_output())
        .await
        .map_err(|_| VmError::Timeout)??;
    if !output.status.success()
        || output.stdout.len() > MAX_GUEST_OUTPUT
        || output.stderr.len() > MAX_GUEST_OUTPUT
    {
        return Err(VmError::CommandFailed);
    }
    let stdout = String::from_utf8(output.stdout).map_err(|_| VmError::InvalidData)?;
    let marker = "NOVAGPT_GUEST_JSON:";
    let json = stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix(marker))
        .ok_or(VmError::InvalidData)?;
    serde_json::from_str(json).map_err(|_| VmError::InvalidData)
}

impl VmManager {
    async fn direct(
        &self,
        id: &str,
        credential: &GuestCredential,
        guest: GuestTask<'_>,
        max_time: Duration,
    ) -> VmResult<GuestReply> {
        if !valid_credential(credential) {
            return Err(VmError::InvalidData);
        }
        let _serial = self.inner.commands.lock().await;
        let instance = self.load_instance(id)?;
        let image = self
            .images()?
            .images
            .into_iter()
            .find(|image| image.name == instance.image)
            .ok_or(VmError::NotFound)?;
        if image.boot_profile != VmBootProfile::WindowsUefi {
            return Err(VmError::Unsupported(
                "PowerShell Direct requires a Windows UEFI base",
            ));
        }
        let disk = self.instance_dir(id).join("disk.vhdx");
        if !disk.is_file() || disk.symlink_metadata()?.file_type().is_symlink() {
            return Err(VmError::NotOwned);
        }
        let input = serde_json::to_string(&HostTask {
            name: format!("NovaGPT-{id}"),
            disk: hyperv_path(Path::new(&disk))?,
            credential,
            guest,
        })
        .map_err(|_| VmError::InvalidData)?;
        send_to_powershell(&input, max_time).await
    }

    pub async fn exec_windows_fork(
        &self,
        id: &str,
        credential: &GuestCredential,
        program: &str,
        args: &[String],
        timeout_ms: u64,
    ) -> VmResult<VmExecResult> {
        if !valid_guest_exe(program)
            || args.len() > 32
            || args
                .iter()
                .any(|arg| arg.len() > 2048 || arg.chars().any(char::is_control))
            || !(100..=300_000).contains(&timeout_ms)
        {
            return Err(VmError::InvalidData);
        }
        let reply = self
            .direct(
                id,
                credential,
                GuestTask {
                    operation: "exec",
                    program: Some(program),
                    args,
                    guest_path: None,
                    bytes_base64: None,
                },
                Duration::from_millis(timeout_ms + 10_000),
            )
            .await?;
        Ok(VmExecResult {
            action: "exec",
            id: id.to_owned(),
            exit_code: reply.exit_code.unwrap_or(-1),
            stdout: reply.stdout.unwrap_or_default(),
            stderr: reply.stderr.unwrap_or_default(),
            truncated: reply.truncated.unwrap_or(false),
        })
    }

    pub async fn upload_windows_fork(
        &self,
        id: &str,
        credential: &GuestCredential,
        guest_path: &str,
        data_base64: &str,
    ) -> VmResult<GuestFileReceipt> {
        if !valid_guest_path(guest_path) || data_base64.len() > (MAX_FILE_BYTES * 4 / 3 + 8) {
            return Err(VmError::InvalidData);
        }
        let data = STANDARD
            .decode(data_base64)
            .map_err(|_| VmError::InvalidData)?;
        if data.len() > MAX_FILE_BYTES || STANDARD.encode(&data) != data_base64 {
            return Err(VmError::InvalidData);
        }
        let reply = self
            .direct(
                id,
                credential,
                GuestTask {
                    operation: "upload",
                    program: None,
                    args: &[],
                    guest_path: Some(guest_path),
                    bytes_base64: Some(data_base64),
                },
                Duration::from_secs(45),
            )
            .await?;
        Ok(GuestFileReceipt {
            id: id.to_owned(),
            bytes: reply.bytes.ok_or(VmError::InvalidData)?,
            data_base64: None,
        })
    }

    pub async fn download_windows_fork(
        &self,
        id: &str,
        credential: &GuestCredential,
        guest_path: &str,
    ) -> VmResult<GuestFileReceipt> {
        if !valid_guest_path(guest_path) {
            return Err(VmError::InvalidData);
        }
        let reply = self
            .direct(
                id,
                credential,
                GuestTask {
                    operation: "download",
                    program: None,
                    args: &[],
                    guest_path: Some(guest_path),
                    bytes_base64: None,
                },
                Duration::from_secs(45),
            )
            .await?;
        let encoded = reply.bytes_base64.ok_or(VmError::InvalidData)?;
        let data = STANDARD
            .decode(&encoded)
            .map_err(|_| VmError::InvalidData)?;
        if data.len() > MAX_FILE_BYTES || reply.bytes != Some(data.len()) {
            return Err(VmError::OutputLimit);
        }
        Ok(GuestFileReceipt {
            id: id.to_owned(),
            bytes: data.len(),
            data_base64: Some(encoded),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_guest_control_input_before_powershell_start() {
        assert!(!valid_guest_exe("cmd.exe"));
        assert!(!valid_guest_exe("C:\\Windows\\System32\\cmd.exe\n"));
        assert!(valid_guest_exe("C:\\Windows\\System32\\cmd.exe"));
        assert!(!valid_guest_path("..\\host"));
        assert!(valid_guest_path("C:\\Temp\\file.txt"));
        assert!(!valid_credential(&GuestCredential {
            username: "guest".into(),
            password: "".into()
        }));
    }

    #[tokio::test]
    async fn unknown_fork_is_denied_without_host_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let vm = VmManager::new(tmp.path()).unwrap();
        let cred = GuestCredential {
            username: "test".into(),
            password: "irrelevant".into(),
        };
        assert!(matches!(
            vm.exec_windows_fork(
                "vm-0123456789abcdef",
                &cred,
                r"C:\Windows\System32\whoami.exe",
                &[],
                30_000
            )
            .await,
            Err(VmError::NotFound)
        ));
    }
}
