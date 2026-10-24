//! Opt-in disposable VM management. Never touches CodexPro VM state.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};
use thiserror::Error;
use tokio::{
    process::{Child, Command},
    sync::Mutex,
    time::{Duration, timeout},
};
use uuid::Uuid;

const IMAGE_NAME_MAX: usize = 80;
const MAX_COMMAND_OUTPUT: usize = 48 * 1024;
const MAX_EXEC_MS: u64 = 300_000;
const MAX_TRANSFER_BYTES: usize = 1024 * 1024;

#[derive(Debug, Error)]
pub enum VmError {
    #[error("VMs are disabled; start NovaGPT with an explicit VM home")]
    Disabled,
    #[error("Invalid VM name or identifier")]
    InvalidName,
    #[error("VM image or instance is not registered in NovaGPT")]
    NotFound,
    #[error("Only image files approved by a human via the local CLI can be imported")]
    HumanApprovalRequired,
    #[error("VM capability is unavailable: {0}")]
    Unsupported(&'static str),
    #[error("VM command failed; inspect the host hypervisor and NovaGPT VM home")]
    CommandFailed,
    #[error("VM operation timed out")]
    Timeout,
    #[error("VM output exceeded its safety bound")]
    OutputLimit,
    #[error("VM state is inconsistent or no longer owned by this process")]
    NotOwned,
    #[error("VM state is busy")]
    Busy,
    #[error("VM data is invalid")]
    InvalidData,
    #[error("VM filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
}
pub type VmResult<T> = Result<T, VmError>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VmBackend {
    Hyperv,
    Qemu,
}

impl VmBackend {
    pub fn host() -> Self {
        if cfg!(windows) {
            Self::Hyperv
        } else {
            Self::Qemu
        }
    }
    fn disk_ext(self) -> &'static str {
        match self {
            Self::Hyperv => "vhdx",
            Self::Qemu => "qcow2",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmImage {
    pub name: String,
    pub backend: VmBackend,
    pub sha256: String,
    pub bytes: u64,
    pub default_cpus: u8,
    pub default_memory_mb: u32,
    pub human_approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInstance {
    pub id: String,
    pub image: String,
    pub backend: VmBackend,
    pub state: VmState,
    pub cpus: u8,
    pub memory_mb: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum VmState {
    Starting,
    Running,
    Stopped,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct VmImages {
    pub action: &'static str,
    pub images: Vec<VmImage>,
    pub image_count: usize,
}

#[derive(Debug, Serialize)]
pub struct VmStatus {
    pub action: &'static str,
    pub instance: VmInstance,
}

#[derive(Debug, Serialize)]
pub struct VmGuestStatus {
    pub action: &'static str,
    pub id: String,
    pub available: bool,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct VmExecResult {
    pub action: &'static str,
    pub id: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct VmCreate {
    pub image: String,
    pub cpus: Option<u8>,
    pub memory_mb: Option<u32>,
}

struct VmInner {
    root: PathBuf,
    backend: VmBackend,
    commands: Mutex<()>,
    children: Mutex<HashMap<String, Child>>,
}
#[derive(Clone)]
pub struct VmManager {
    inner: Arc<VmInner>,
}

impl VmManager {
    pub fn new(root: impl AsRef<Path>) -> VmResult<Self> {
        let root = root.as_ref();
        if root.exists() && root.symlink_metadata()?.file_type().is_symlink() {
            return Err(VmError::NotOwned);
        }
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        if !root.is_dir() {
            return Err(VmError::InvalidData);
        }
        let marker = root.join("novagpt-vm-owner");
        if marker.exists() {
            if marker.symlink_metadata()?.file_type().is_symlink()
                || fs::read(&marker)? != b"NovaGPT isolated VM storage v1\n"
            {
                return Err(VmError::NotOwned);
            }
        } else {
            // Refuse to initialize a nonempty foreign directory (especially
            // CodexPro's own VM storage) without a NovaGPT ownership marker.
            if fs::read_dir(&root)?.next().is_some() {
                return Err(VmError::NotOwned);
            }
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(marker)?;
            file.write_all(b"NovaGPT isolated VM storage v1\n")?;
            file.sync_all()?;
        }
        fs::create_dir_all(root.join("images"))?;
        fs::create_dir_all(root.join("instances"))?;
        Ok(Self {
            inner: Arc::new(VmInner {
                root,
                backend: VmBackend::host(),
                commands: Mutex::new(()),
                children: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn backend(&self) -> VmBackend {
        self.inner.backend
    }
    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn images(&self) -> VmResult<VmImages> {
        let mut images = Vec::new();
        for item in fs::read_dir(self.inner.root.join("images"))? {
            let item = item?;
            if item.file_type()?.is_symlink() || !item.file_type()?.is_dir() {
                continue;
            }
            if !valid_name(&item.file_name().to_string_lossy(), IMAGE_NAME_MAX) {
                continue;
            }
            let manifest = item.path().join("manifest.json");
            if manifest
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
            {
                continue;
            }
            if let Ok(bytes) = fs::read(&manifest)
                && bytes.len() <= 16_384
                && let Ok(image) = serde_json::from_slice::<VmImage>(&bytes)
                && image.name == item.file_name().to_string_lossy()
                && image.backend == self.inner.backend
                && image.human_approved
            {
                images.push(image);
            }
        }
        images.sort_by(|a, b| a.name.cmp(&b.name));
        let image_count = images.len();
        Ok(VmImages {
            action: "images",
            images,
            image_count,
        })
    }

    /// Human-only local CLI operation; never expose source host paths in MCP tools.
    pub fn import_image(
        &self,
        name: &str,
        source: &Path,
        cpus: u8,
        memory_mb: u32,
    ) -> VmResult<VmImage> {
        validate_image_name(name)?;
        resources(cpus, memory_mb)?;
        let source = fs::canonicalize(source)?;
        if !source.is_file()
            || source.extension().and_then(|s| s.to_str()) != Some(self.inner.backend.disk_ext())
        {
            return Err(VmError::InvalidData);
        }
        let dir = self.inner.root.join("images").join(name);
        fs::create_dir(&dir)?; // never replace an approved image silently
        let target = dir.join(format!("base.{}", self.inner.backend.disk_ext()));
        let mut from = fs::File::open(source)?;
        let mut to = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&target)?;
        let mut hasher = Sha256::new();
        let mut bytes = 0u64;
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let read = from.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            to.write_all(&chunk[..read])?;
            hasher.update(&chunk[..read]);
            bytes += read as u64;
        }
        to.sync_all()?;
        if bytes == 0 {
            return Err(VmError::InvalidData);
        }
        let image = VmImage {
            name: name.to_owned(),
            backend: self.inner.backend,
            sha256: hex::encode(hasher.finalize()),
            bytes,
            default_cpus: cpus,
            default_memory_mb: memory_mb,
            human_approved: true,
        };
        write_json_new(&dir.join("manifest.json"), &image)?;
        Ok(image)
    }

    pub async fn create(&self, req: VmCreate) -> VmResult<VmStatus> {
        validate_image_name(&req.image)?;
        let _serial = self.inner.commands.lock().await;
        let image = self
            .images()?
            .images
            .into_iter()
            .find(|image| image.name == req.image)
            .ok_or(VmError::NotFound)?;
        let cpus = req.cpus.unwrap_or(image.default_cpus);
        let memory = req.memory_mb.unwrap_or(image.default_memory_mb);
        resources(cpus, memory)?;
        let id = format!("vm-{}", &Uuid::new_v4().simple().to_string()[..16]);
        let dir = self.inner.root.join("instances").join(&id);
        fs::create_dir(&dir)?;
        let source = self
            .inner
            .root
            .join("images")
            .join(&image.name)
            .join(format!("base.{}", image.backend.disk_ext()));
        if source.symlink_metadata()?.file_type().is_symlink() || !source.is_file() {
            return Err(VmError::InvalidData);
        }
        // An imported disk must not silently change after human approval.
        let source_clone = source.clone();
        let expected = image.sha256.clone();
        let digest = tokio::task::spawn_blocking(move || sha256_file(&source_clone))
            .await
            .map_err(|_| VmError::InvalidData)??;
        if digest != expected {
            return Err(VmError::InvalidData);
        }
        let overlay = dir.join(format!("disk.{}", image.backend.disk_ext()));
        let instance = VmInstance {
            id: id.clone(),
            image: image.name,
            backend: image.backend,
            state: VmState::Starting,
            cpus,
            memory_mb: memory,
        };
        write_json_new(&dir.join("instance.json"), &instance)?;
        let result = if image.backend == VmBackend::Hyperv {
            start_hyperv(&id, &source, &overlay, cpus, memory).await
        } else {
            self.start_qemu(&id, &source, &overlay, cpus, memory).await
        };
        if result.is_err() {
            // Keep record and disks for explicit diagnosis/cleanup.
            save_instance(
                &dir,
                &VmInstance {
                    state: VmState::Failed,
                    ..instance.clone()
                },
            )?;
        }
        result?;
        let running = VmInstance {
            state: VmState::Running,
            ..instance
        };
        save_instance(&dir, &running)?;
        Ok(VmStatus {
            action: "create",
            instance: running,
        })
    }

    pub async fn status(&self, id: &str) -> VmResult<VmStatus> {
        let mut instance = self.load_instance(id)?;
        if instance.backend == VmBackend::Hyperv {
            let name = hyperv_name(id);
            let script = format!(
                "$vm = Get-VM -Name {} -ErrorAction SilentlyContinue; if ($vm -and $vm.State -eq 'Running') {{ 'running' }} elseif ($vm) {{ 'stopped' }} else {{ 'missing' }}",
                quote_ps(&name)
            );
            let answer = powershell(&script, Duration::from_secs(15)).await?;
            instance.state = match answer.trim() {
                "running" => VmState::Running,
                "stopped" => VmState::Stopped,
                _ => VmState::Failed,
            };
        } else {
            let mut children = self.inner.children.lock().await;
            instance.state = match children.get_mut(id) {
                Some(child) => match child.try_wait()? {
                    None => VmState::Running,
                    Some(_) => VmState::Stopped,
                },
                None => VmState::Stopped,
            };
        }
        Ok(VmStatus {
            action: "status",
            instance,
        })
    }

    pub async fn destroy(&self, id: &str) -> VmResult<VmStatus> {
        let _serial = self.inner.commands.lock().await;
        let instance = self.load_instance(id)?;
        if instance.backend == VmBackend::Hyperv {
            let name = quote_ps(&hyperv_name(id));
            let script = format!(
                "$vm = Get-VM -Name {name} -ErrorAction SilentlyContinue; if ($vm) {{ if ($vm.State -ne 'Off') {{ Stop-VM -VM $vm -TurnOff -Force -ErrorAction Stop }}; Remove-VM -VM $vm -Force -ErrorAction Stop }}"
            );
            powershell(&script, Duration::from_secs(60)).await?;
        } else {
            let mut children = self.inner.children.lock().await;
            if let Some(mut child) = children.remove(id) {
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(10), child.wait()).await;
            } else if instance.state == VmState::Running || instance.state == VmState::Starting {
                return Err(VmError::NotOwned);
            }
        }
        let stopped = VmInstance {
            state: VmState::Stopped,
            ..instance
        };
        // Delete only the exact managed instance files, never an arbitrary
        // directory tree or a disk/image supplied by the caller.
        self.remove_owned_instance(id, stopped.backend)?;
        Ok(VmStatus {
            action: "destroy",
            instance: stopped,
        })
    }

    pub fn guest_status(&self, id: &str) -> VmResult<VmGuestStatus> {
        self.load_instance(id)?;
        Ok(VmGuestStatus { action: "guest_status", id: id.to_owned(), available: false,
            reason: "Guest agent / PowerShell Direct control is not enabled in this first Rust VM slice; host operations only".to_owned() })
    }

    pub fn validate_exec(
        argv: Option<&[String]>,
        command: Option<&str>,
        shell: Option<&str>,
        timeout_ms: Option<u64>,
    ) -> VmResult<()> {
        match (argv, command) {
            (Some(argv), None)
                if !argv.is_empty()
                    && argv.len() <= 256
                    && argv
                        .iter()
                        .all(|item| !item.is_empty() && item.len() <= 16_384)
                    && shell.is_none() => {}
            (None, Some(command))
                if !command.is_empty()
                    && command.len() <= 65_536
                    && matches!(shell, Some("powershell" | "cmd" | "sh" | "bash")) => {}
            _ => return Err(VmError::InvalidData),
        }
        if !matches!(timeout_ms.unwrap_or(30_000), 100..=MAX_EXEC_MS) {
            return Err(VmError::InvalidData);
        }
        Ok(())
    }

    pub fn validate_transfer(id: &str, guest_path: &str, byte_count: usize) -> VmResult<()> {
        validate_id(id)?;
        if guest_path.is_empty() || guest_path.len() > 4096 || byte_count > MAX_TRANSFER_BYTES {
            return Err(VmError::InvalidData);
        }
        Ok(())
    }

    fn load_instance(&self, id: &str) -> VmResult<VmInstance> {
        validate_id(id)?;
        let file = self.instance_dir(id).join("instance.json");
        if file
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(VmError::InvalidData);
        }
        let data = fs::read(file).map_err(|_| VmError::NotFound)?;
        if data.len() > 16_384 {
            return Err(VmError::InvalidData);
        }
        let instance: VmInstance =
            serde_json::from_slice(&data).map_err(|_| VmError::InvalidData)?;
        if instance.id != id || instance.backend != self.inner.backend {
            return Err(VmError::NotOwned);
        }
        Ok(instance)
    }
    fn remove_owned_instance(&self, id: &str, backend: VmBackend) -> VmResult<()> {
        validate_id(id)?;
        let parent = self.inner.root.join("instances");
        if fs::canonicalize(&parent)? != parent {
            return Err(VmError::NotOwned);
        }
        let dir = parent.join(id);
        if fs::canonicalize(&dir)? != dir {
            return Err(VmError::NotOwned);
        }
        let disk = format!("disk.{}", backend.disk_ext());
        let allowed = ["instance.json", "instance.next", disk.as_str()];
        let mut paths = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            if entry.file_type()?.is_symlink()
                || !entry.file_type()?.is_file()
                || !allowed.iter().any(|allowed| name == *allowed)
            {
                return Err(VmError::NotOwned);
            }
            paths.push(entry.path());
        }
        for path in paths {
            fs::remove_file(path)?;
        }
        fs::remove_dir(dir)?;
        Ok(())
    }

    fn instance_dir(&self, id: &str) -> PathBuf {
        self.inner.root.join("instances").join(id)
    }

    async fn start_qemu(
        &self,
        id: &str,
        source: &Path,
        overlay: &Path,
        cpus: u8,
        memory: u32,
    ) -> VmResult<()> {
        let output = run_command(
            "qemu-img",
            &[
                "create".into(),
                "-f".into(),
                "qcow2".into(),
                "-F".into(),
                "qcow2".into(),
                "-b".into(),
                source.display().to_string(),
                overlay.display().to_string(),
            ],
            Duration::from_secs(45),
        )
        .await?;
        if !output.status.success() {
            return Err(VmError::CommandFailed);
        }
        let child = Command::new("qemu-system-x86_64")
            .args([
                "-nodefaults",
                "-display",
                "none",
                "-machine",
                "q35",
                "-m",
                &memory.to_string(),
                "-smp",
                &cpus.to_string(),
                "-drive",
            ])
            .arg(format!("file={},format=qcow2,if=virtio", overlay.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(false)
            .spawn()
            .map_err(VmError::Io)?;
        self.inner
            .children
            .lock()
            .await
            .insert(id.to_owned(), child);
        Ok(())
    }
}

fn sha256_file(path: &Path) -> VmResult<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut chunk = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn validate_image_name(name: &str) -> VmResult<()> {
    if valid_name(name, IMAGE_NAME_MAX) {
        Ok(())
    } else {
        Err(VmError::InvalidName)
    }
}
fn validate_id(id: &str) -> VmResult<()> {
    if id.len() == 19 && id.starts_with("vm-") && id[3..].bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(VmError::InvalidName)
    }
}
fn valid_name(s: &str, max: usize) -> bool {
    !s.is_empty()
        && s.len() <= max
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}
fn resources(cpus: u8, memory: u32) -> VmResult<()> {
    if !(1..=64).contains(&cpus) || !(256..=262_144).contains(&memory) {
        Err(VmError::InvalidData)
    } else {
        Ok(())
    }
}
fn write_json_new<T: Serialize>(path: &Path, value: &T) -> VmResult<()> {
    let contents = serde_json::to_vec_pretty(value).map_err(|_| VmError::InvalidData)?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(&contents)?;
    file.sync_all()?;
    Ok(())
}
fn save_instance(dir: &Path, instance: &VmInstance) -> VmResult<()> {
    let bytes = serde_json::to_vec(instance).map_err(|_| VmError::InvalidData)?;
    let path = dir.join("instance.json");
    let pending = dir.join("instance.next");
    fs::write(&pending, bytes)?;
    fs::rename(pending, path)?;
    Ok(())
}
fn quote_ps(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
fn hyperv_name(id: &str) -> String {
    format!("NovaGPT-{id}")
}

async fn start_hyperv(
    id: &str,
    source: &Path,
    overlay: &Path,
    cpus: u8,
    memory: u32,
) -> VmResult<()> {
    let name = quote_ps(&hyperv_name(id));
    let src = quote_ps(&source.display().to_string());
    let dest = quote_ps(&overlay.display().to_string());
    let script = format!(
        "New-VHD -Path {dest} -ParentPath {src} -Differencing -ErrorAction Stop | Out-Null; \
         New-VM -Name {name} -Generation 2 -VHDPath {dest} -MemoryStartupBytes {}MB -ErrorAction Stop | Out-Null; \
         Set-VMProcessor -VMName {name} -Count {cpus} -ErrorAction Stop; \
         Start-VM -Name {name} -ErrorAction Stop",
        memory
    );
    powershell(&script, Duration::from_secs(90))
        .await
        .map(|_| ())
}

async fn powershell(script: &str, timeout_duration: Duration) -> VmResult<String> {
    let output = run_command(
        "powershell.exe",
        &[
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            script.into(),
        ],
        timeout_duration,
    )
    .await?;
    if !output.status.success() {
        return Err(VmError::CommandFailed);
    }
    String::from_utf8(output.stdout).map_err(|_| VmError::InvalidData)
}
async fn run_command(
    program: &str,
    args: &[String],
    limit: Duration,
) -> VmResult<std::process::Output> {
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).kill_on_drop(true);
    let output = timeout(limit, command.output())
        .await
        .map_err(|_| VmError::Timeout)??;
    if output.stdout.len() > MAX_COMMAND_OUTPUT || output.stderr.len() > MAX_COMMAND_OUTPUT {
        return Err(VmError::OutputLimit);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ids_and_resources_are_strict() {
        assert!(validate_id("vm-0123456789abcdef").is_ok());
        assert!(validate_id("vm-../outside").is_err());
        assert!(validate_image_name("../secret").is_err());
        assert!(resources(0, 1024).is_err());
        assert!(resources(2, 1024).is_ok());
        assert!(VmManager::validate_exec(Some(&["echo".into()]), None, None, Some(30000)).is_ok());
        assert!(VmManager::validate_exec(None, Some("echo hi"), None, None).is_err());
        assert!(
            VmManager::validate_transfer("vm-0123456789abcdef", "/tmp/a", MAX_TRANSFER_BYTES + 1)
                .is_err()
        );
    }
    #[test]
    fn foreign_vm_home_is_rejected_and_imports_are_scoped() {
        let foreign = tempfile::tempdir().expect("foreign home");
        fs::write(foreign.path().join("codexpro-marker"), b"foreign data").expect("marker");
        assert!(matches!(
            VmManager::new(foreign.path()),
            Err(VmError::NotOwned)
        ));

        let store = tempfile::tempdir().expect("new home");
        let approved = tempfile::tempdir().expect("approved source");
        let source = approved.path().join(match VmBackend::host() {
            VmBackend::Hyperv => "test.vhdx",
            VmBackend::Qemu => "test.qcow2",
        });
        fs::write(&source, b"test-only-image-content").expect("fixture");
        let manager = VmManager::new(store.path()).expect("VM home");
        let image = manager
            .import_image("test-image", &source, 2, 1024)
            .expect("explicit import");
        assert_eq!(image.bytes, 23);
        assert_eq!(manager.images().expect("images").image_count, 1);
        assert!(matches!(
            manager.import_image("../secret", &source, 2, 1024),
            Err(VmError::InvalidName)
        ));
        assert!(matches!(
            manager.import_image("test-image", &source, 2, 1024),
            Err(VmError::Io(_))
        ));
    }

    #[test]
    fn empty_registry_and_unknown_instance_are_safe() {
        let temp = tempfile::tempdir().expect("fixture");
        let manager = VmManager::new(temp.path()).expect("VM home");
        assert_eq!(manager.images().expect("images").image_count, 0);
        assert!(matches!(
            manager.guest_status("vm-0123456789abcdef"),
            Err(VmError::NotFound)
        ));
    }
}
