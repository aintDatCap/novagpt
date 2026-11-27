//! Opt-in disposable VM management. Never touches other application VM state.
pub mod image_promotion;
pub mod iso;
mod libvirt;
pub mod linux_qemu;
#[cfg(target_os = "linux")]
mod process_identity;
#[cfg(unix)]
pub mod qga;
#[cfg(unix)]
pub mod qmp;
pub mod snapshots;
#[cfg(windows)]
pub mod windows_direct;
pub mod windows_guest;
#[cfg(windows)]
pub mod windows_hyperv;
pub mod windows_unattend;
use fs2::FileExt;
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
const MAX_REGISTERED_FORKS: usize = 32;
const MAX_ACTIVE_FORKS: usize = 4;
const MAX_ACTIVE_CPUS: u32 = 16;
const MAX_ACTIVE_MEMORY_MB: u64 = 32 * 1024;
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VmBootProfile {
    #[default]
    Legacy,
    WindowsUefi,
    LinuxUefi,
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
    #[serde(default)]
    pub boot_profile: VmBootProfile,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmInstance {
    pub id: String,
    pub image: String,
    pub backend: VmBackend,
    pub state: VmState,
    pub cpus: u8,
    pub memory_mb: u32,
    /// Approved ISO registry name for an install fork, never a host path.
    #[serde(default)]
    pub installer_iso: Option<String>,
    #[serde(default)]
    pub qemu_pid: Option<u32>,
    #[serde(default)]
    pub qemu_start_ticks: Option<u64>,
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
    pub registered_domains: Vec<libvirt::RegisteredDomain>,
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
    pub linux_options: Option<linux_qemu::LinuxVmOptions>,
    pub installer_iso: Option<String>,
}

struct VmInner {
    root: PathBuf,
    // OS-managed exclusive lock: another NovaGPT process cannot mutate this home.
    _home_lease: fs::File,
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
            // another application's VM storage) without a NovaGPT ownership marker.
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
        for subdir in ["images", "instances", "networks", "isos", "staging"] {
            let path = root.join(subdir);
            if path
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_symlink())
            {
                return Err(VmError::NotOwned);
            }
            fs::create_dir_all(&path)?;
            if fs::canonicalize(&path)? != path {
                return Err(VmError::NotOwned);
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for subdir in ["", "instances", "networks"] {
                fs::set_permissions(root.join(subdir), fs::Permissions::from_mode(0o700))?;
            }
        }
        let lease_path = root.join("novagpt-vm-lease");
        if lease_path
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(VmError::NotOwned);
        }
        let lease = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lease_path)?;
        lease.try_lock_exclusive().map_err(|error| {
            if error.kind() == std::io::ErrorKind::WouldBlock {
                VmError::Busy
            } else {
                VmError::Io(error)
            }
        })?;
        Ok(Self {
            inner: Arc::new(VmInner {
                root,
                _home_lease: lease,
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
            registered_domains: self.registered_domains()?,
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
        self.import_image_with_profile(name, source, cpus, memory_mb, VmBootProfile::Legacy)
    }

    pub fn import_image_with_profile(
        &self,
        name: &str,
        source: &Path,
        cpus: u8,
        memory_mb: u32,
        boot_profile: VmBootProfile,
    ) -> VmResult<VmImage> {
        validate_image_name(name)?;
        if self.backend() == VmBackend::Qemu && boot_profile == VmBootProfile::WindowsUefi {
            return Err(VmError::Unsupported(
                "Windows UEFI forks require owned swtpm state",
            ));
        }
        resources(cpus, memory_mb)?;
        let source = fs::canonicalize(source)?;
        if !source.is_file()
            || source.extension().and_then(|s| s.to_str()) != Some(self.inner.backend.disk_ext())
        {
            return Err(VmError::InvalidData);
        }
        if self.inner.backend == VmBackend::Qemu {
            validate_qcow2_base(&source)?;
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
        if self.inner.backend == VmBackend::Qemu {
            validate_qcow2_base(&target)?;
            let mut permissions = fs::metadata(&target)?.permissions();
            permissions.set_readonly(true);
            fs::set_permissions(&target, permissions)?;
        }
        let image = VmImage {
            name: name.to_owned(),
            backend: self.inner.backend,
            sha256: hex::encode(hasher.finalize()),
            bytes,
            default_cpus: cpus,
            default_memory_mb: memory_mb,
            human_approved: true,
            boot_profile,
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
        self.check_capacity(cpus, memory, true)?;
        if req.installer_iso.is_some() && image.backend != VmBackend::Qemu {
            return Err(VmError::Unsupported("QEMU approved ISO boot only"));
        }
        if let Some(name) = &req.installer_iso {
            // Verify immutable approved installation media before reserving a fork.
            self.verified_iso(name).await?;
        }
        if image.backend == VmBackend::Qemu && image.boot_profile == VmBootProfile::WindowsUefi {
            return Err(VmError::Unsupported(
                "Windows UEFI forks require owned swtpm lifecycle",
            ));
        }
        let id = format!("vm-{}", &Uuid::new_v4().simple().to_string()[..16]);
        let dir = self.inner.root.join("instances").join(&id);
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
        if image.backend == VmBackend::Qemu {
            let to_validate = source.clone();
            tokio::task::spawn_blocking(move || validate_qcow2_base(&to_validate))
                .await
                .map_err(|_| VmError::InvalidData)??;
        }
        // An interrupted or rejected hash check must not leave an empty
        // instance directory that later looks like an incomplete VM.
        fs::create_dir(&dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        }
        let overlay = dir.join(format!("disk.{}", image.backend.disk_ext()));
        let instance = VmInstance {
            id: id.clone(),
            image: image.name,
            backend: image.backend,
            state: VmState::Starting,
            cpus,
            memory_mb: memory,
            installer_iso: req.installer_iso,
            qemu_pid: None,
            qemu_start_ticks: None,
        };
        let setup = (|| -> VmResult<()> {
            write_json_new(&dir.join("instance.json"), &instance)?;
            if image.backend == VmBackend::Qemu {
                let options = req.linux_options.unwrap_or_default();
                let config = toml::to_string(&options).map_err(|_| VmError::InvalidData)?;
                write_bytes_new(&dir.join("vm.toml"), config.as_bytes())?;
                linux_qemu::prepare_firmware(&dir, image.boot_profile)?;
            } else if req.linux_options.is_some() {
                return Err(VmError::Unsupported(
                    "QEMU Linux network settings are not available on Hyper-V",
                ));
            }
            Ok(())
        })();
        if let Err(error) = setup {
            // There is no VM process yet; delete only known instance-owned files.
            self.remove_owned_instance(&id, image.backend)?;
            return Err(error);
        }
        let result = if image.backend == VmBackend::Hyperv {
            start_hyperv(&id, &source, &overlay, cpus, memory, image.boot_profile).await
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
        let (qemu_pid, qemu_start_ticks) = if image.backend == VmBackend::Qemu {
            self.running_qemu_identity(&id).await?
        } else {
            (None, None)
        };
        let running = VmInstance {
            state: VmState::Running,
            qemu_pid,
            qemu_start_ticks,
            ..instance
        };
        save_instance(&dir, &running)?;
        Ok(VmStatus {
            action: "create",
            instance: running,
        })
    }

    pub async fn status(&self, id: &str) -> VmResult<VmStatus> {
        if id.starts_with("libvirt-") {
            return self.libvirt_status(id).await;
        }
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
            let child_state = {
                let mut children = self.inner.children.lock().await;
                match children.get_mut(id) {
                    Some(child) => Some(match child.try_wait()? {
                        None if instance.state == VmState::Failed => VmState::Failed,
                        None => VmState::Running,
                        Some(_) => VmState::Stopped,
                    }),
                    None => None,
                }
            };
            instance.state = match child_state {
                Some(state) => state,
                None if matches!(instance.state, VmState::Running | VmState::Starting) => {
                    if self.verified_orphan(&instance).await? {
                        VmState::Running
                    } else {
                        VmState::Failed
                    }
                }
                None => instance.state,
            };
        }
        Ok(VmStatus {
            action: "status",
            instance,
        })
    }

    /// Ask the guest to power off through private QMP ACPI. Never force-kills
    /// on timeout; callers must explicitly request the destructive stop path.
    #[cfg(unix)]
    pub async fn graceful_stop(&self, id: &str) -> VmResult<VmStatus> {
        let _serial = self.inner.commands.lock().await;
        let instance = self.load_instance(id)?;
        if instance.backend != VmBackend::Qemu || instance.state != VmState::Running {
            return Err(VmError::Unsupported(
                "Graceful stop requires a running QEMU guest",
            ));
        }
        let socket = self.owned_qemu_socket(id, "qmp.sock").await?;
        qmp::request_powerdown(&socket).await?;
        let began = std::time::Instant::now();
        loop {
            let child_state = {
                let mut children = self.inner.children.lock().await;
                let exited = match children.get_mut(id) {
                    Some(child) => Some(child.try_wait()?.is_some()),
                    None => None,
                };
                if exited == Some(true) {
                    children.remove(id);
                }
                exited
            };
            let exited = match child_state {
                Some(exited) => exited,
                None => {
                    #[cfg(target_os = "linux")]
                    {
                        !self.verified_orphan(&instance).await?
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        return Err(VmError::NotOwned);
                    }
                }
            };
            if exited {
                self.cleanup_network_listener(id)?;
                let stopped = VmInstance {
                    state: VmState::Stopped,
                    qemu_pid: None,
                    qemu_start_ticks: None,
                    ..instance
                };
                save_instance(&self.instance_dir(id), &stopped)?;
                return Ok(VmStatus {
                    action: "stop",
                    instance: stopped,
                });
            }
            if began.elapsed() >= Duration::from_secs(10) {
                return Err(VmError::Timeout);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Stop a QEMU guest owned by the current NovaGPT process. A stale
    /// process ID from an earlier server is never treated as authorization.
    pub async fn stop(&self, id: &str) -> VmResult<VmStatus> {
        let _serial = self.inner.commands.lock().await;
        let instance = self.load_instance(id)?;
        if instance.backend != VmBackend::Qemu {
            return Err(VmError::Unsupported("QEMU stop only"));
        }
        let owned_child = self.inner.children.lock().await.remove(id);
        if let Some(mut child) = owned_child {
            if child.try_wait()?.is_none() {
                if let Err(error) = child.start_kill() {
                    self.inner
                        .children
                        .lock()
                        .await
                        .insert(id.to_owned(), child);
                    return Err(VmError::Io(error));
                }
                match timeout(Duration::from_secs(15), child.wait()).await {
                    Ok(Ok(_)) => {}
                    _ => {
                        self.inner
                            .children
                            .lock()
                            .await
                            .insert(id.to_owned(), child);
                        return Err(VmError::Timeout);
                    }
                }
            }
        } else if matches!(instance.state, VmState::Running | VmState::Failed) {
            #[cfg(target_os = "linux")]
            self.signal_verified_orphan(&instance, false).await?;
            #[cfg(not(target_os = "linux"))]
            return Err(VmError::NotOwned);
        } else if instance.state != VmState::Stopped {
            return Err(VmError::NotOwned);
        }
        self.cleanup_network_listener(id)?;
        let stopped = VmInstance {
            state: VmState::Stopped,
            qemu_pid: None,
            qemu_start_ticks: None,
            ..instance
        };
        save_instance(&self.instance_dir(id), &stopped)?;
        Ok(VmStatus {
            action: "stop",
            instance: stopped,
        })
    }

    /// Resume a stopped QEMU fork from its existing owned writable overlay.
    pub async fn start(&self, id: &str) -> VmResult<VmStatus> {
        let _serial = self.inner.commands.lock().await;
        let instance = self.load_instance(id)?;
        if instance.backend != VmBackend::Qemu {
            return Err(VmError::Unsupported("QEMU start only"));
        }
        if instance.state != VmState::Stopped || self.inner.children.lock().await.contains_key(id) {
            return Err(VmError::Busy);
        }
        self.check_capacity(instance.cpus, instance.memory_mb, false)?;
        let dir = self.instance_dir(id);
        if fs::canonicalize(&dir)? != dir {
            return Err(VmError::NotOwned);
        }
        let disk = dir.join("disk.qcow2");
        let metadata = disk.symlink_metadata()?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(VmError::NotOwned);
        }
        self.verify_overlay(&instance).await?;
        self.launch_qemu(id, &disk, instance.cpus, instance.memory_mb)
            .await?;
        let (qemu_pid, qemu_start_ticks) = self.running_qemu_identity(id).await?;
        let running = VmInstance {
            state: VmState::Running,
            qemu_pid,
            qemu_start_ticks,
            ..instance
        };
        save_instance(&dir, &running)?;
        Ok(VmStatus {
            action: "start",
            instance: running,
        })
    }

    /// Detach approved ISO media only while the owned fork is stopped.
    /// The next start boots from the installed writable disk.
    pub async fn eject_iso(&self, id: &str) -> VmResult<VmStatus> {
        let _serial = self.inner.commands.lock().await;
        let mut instance = self.load_instance(id)?;
        if instance.backend != VmBackend::Qemu {
            return Err(VmError::Unsupported("QEMU ISO eject only"));
        }
        if instance.state != VmState::Stopped || self.inner.children.lock().await.contains_key(id) {
            return Err(VmError::Busy);
        }
        if instance.installer_iso.is_none() {
            return Err(VmError::NotFound);
        }
        instance.installer_iso = None;
        save_instance(&self.instance_dir(id), &instance)?;
        Ok(VmStatus {
            action: "eject_iso",
            instance,
        })
    }

    pub async fn destroy(&self, id: &str) -> VmResult<VmStatus> {
        let _serial = self.inner.commands.lock().await;
        let instance = self.load_instance(id)?;
        if instance.backend == VmBackend::Hyperv {
            let name = quote_ps(&hyperv_name(id));
            #[cfg(windows)]
            let disk = quote_ps(&windows_hyperv::hyperv_path(
                &self.instance_dir(id).join("disk.vhdx"),
            )?);
            #[cfg(not(windows))]
            let disk = quote_ps(
                &self
                    .instance_dir(id)
                    .join("disk.vhdx")
                    .display()
                    .to_string(),
            );
            // Never authorize destruction using a display name alone. The
            // attached VHDX must match the owned instance's exact disk.
            let script = format!(
                "$ErrorActionPreference = 'Stop'; \
                 $vm = Get-VM -Name {name} -ErrorAction Stop; \
                 $attached = @(Get-VMHardDiskDrive -VM $vm -ErrorAction Stop); \
                 if ($attached.Count -ne 1 -or [IO.Path]::GetFullPath($attached[0].Path) -ine [IO.Path]::GetFullPath({disk})) {{ \
                   throw 'The VM no longer uses its NovaGPT-owned disk' \
                 }}; \
                 if ($vm.State -ne 'Off') {{ Stop-VM -VM $vm -TurnOff -Force -ErrorAction Stop }}; \
                 Remove-VM -VM $vm -Force -ErrorAction Stop"
            );
            powershell(&script, Duration::from_secs(90)).await?;
        } else {
            let owned_child = self.inner.children.lock().await.remove(id);
            if let Some(mut child) = owned_child {
                if child.try_wait()?.is_none() {
                    if let Err(error) = child.start_kill() {
                        self.inner
                            .children
                            .lock()
                            .await
                            .insert(id.to_owned(), child);
                        return Err(VmError::Io(error));
                    }
                    match timeout(Duration::from_secs(10), child.wait()).await {
                        Ok(Ok(_)) => {}
                        _ => {
                            self.inner
                                .children
                                .lock()
                                .await
                                .insert(id.to_owned(), child);
                            return Err(VmError::Timeout);
                        }
                    }
                }
            } else if matches!(instance.state, VmState::Running | VmState::Failed)
                && self.verified_orphan(&instance).await?
            {
                #[cfg(target_os = "linux")]
                self.signal_verified_orphan(&instance, false).await?;
                #[cfg(not(target_os = "linux"))]
                return Err(VmError::NotOwned);
            } else if matches!(instance.state, VmState::Running | VmState::Starting) {
                return Err(VmError::NotOwned);
            } else if instance.state == VmState::Failed {
                // The failed state may have live QEMU without a persistently
                // verified handle. Never unlink disks while a monitor remains.
                if self
                    .instance_dir(id)
                    .join("qmp.sock")
                    .symlink_metadata()
                    .is_ok()
                {
                    return Err(VmError::NotOwned);
                }
            }
        }
        let stopped = VmInstance {
            state: VmState::Stopped,
            qemu_pid: None,
            qemu_start_ticks: None,
            ..instance
        };
        if stopped.backend == VmBackend::Qemu {
            self.cleanup_network_listener(id)?;
        }
        // Delete only the exact managed instance files, never an arbitrary
        // directory tree or a disk/image supplied by the caller.
        self.remove_owned_instance(id, stopped.backend)?;
        Ok(VmStatus {
            action: "destroy",
            instance: stopped,
        })
    }

    #[cfg(unix)]
    async fn owned_qemu_socket(&self, id: &str, name: &str) -> VmResult<PathBuf> {
        let instance = self.load_instance(id)?;
        if instance.backend != VmBackend::Qemu || instance.state != VmState::Running {
            return Err(VmError::Unsupported(
                "QEMU guest is not in a managed running state",
            ));
        }
        let locally_owned = {
            let mut children = self.inner.children.lock().await;
            match children.get_mut(id) {
                Some(child) => child.try_wait()?.is_none(),
                None => false,
            }
        };
        if !locally_owned && !self.verified_orphan(&instance).await? {
            return Err(VmError::NotOwned);
        }
        let dir = self.instance_dir(id);
        if fs::canonicalize(&dir)? != dir {
            return Err(VmError::NotOwned);
        }
        if !matches!(name, "qga.sock" | "qmp.sock") {
            return Err(VmError::InvalidData);
        }
        let socket = dir.join(name);
        use std::os::unix::fs::FileTypeExt;
        if !socket
            .symlink_metadata()
            .is_ok_and(|m| m.file_type().is_socket())
        {
            return Err(VmError::Unsupported("QEMU Guest Agent socket unavailable"));
        }
        Ok(socket)
    }

    #[cfg(unix)]
    async fn owned_qga_socket(&self, id: &str) -> VmResult<PathBuf> {
        self.owned_qemu_socket(id, "qga.sock").await
    }

    #[cfg(unix)]
    pub async fn desktop_screenshot(&self, id: &str) -> VmResult<qmp::VmScreenshot> {
        let socket = self.owned_qemu_socket(id, "qmp.sock").await?;
        let frame = self
            .instance_dir(id)
            .join(format!("screen-{}.png", Uuid::new_v4().simple()));
        let result = qmp::screenshot(id, &socket, &frame).await;
        if frame
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        {
            let _ = fs::remove_file(frame);
        }
        result
    }

    #[cfg(unix)]
    pub async fn desktop_keys(&self, id: &str, keys: &[String]) -> VmResult<qmp::VmDesktopAction> {
        let socket = self.owned_qemu_socket(id, "qmp.sock").await?;
        qmp::keys(id, &socket, keys).await
    }

    #[cfg(unix)]
    pub async fn desktop_pointer(
        &self,
        id: &str,
        x: u16,
        y: u16,
        button: Option<&str>,
    ) -> VmResult<qmp::VmDesktopAction> {
        let socket = self.owned_qemu_socket(id, "qmp.sock").await?;
        qmp::pointer(id, &socket, x, y, button).await
    }

    /// Health is proven with guest-ping, not inferred from a guest-agent socket.
    pub async fn guest_status_live(&self, id: &str) -> VmResult<VmGuestStatus> {
        #[cfg(unix)]
        if self.backend() == VmBackend::Qemu {
            let result = match self.owned_qga_socket(id).await {
                Ok(socket) => qga::ping(&socket).await.is_ok(),
                Err(VmError::NotFound | VmError::InvalidName) => return Err(VmError::NotFound),
                Err(_) => false,
            };
            return Ok(VmGuestStatus {
                action: "guest_status",
                id: id.to_owned(),
                available: result,
                reason: if result {
                    "QEMU Guest Agent responded to guest-ping"
                } else {
                    "Guest Agent unavailable; install and start qemu-guest-agent inside the VM"
                }
                .to_owned(),
            });
        }
        self.guest_status(id)
    }

    #[cfg(unix)]
    pub async fn exec_qemu_fork(
        &self,
        id: &str,
        program: &str,
        args: &[String],
        timeout_ms: u64,
    ) -> VmResult<VmExecResult> {
        let valid_windows = program.len() > 3
            && program.as_bytes()[0].is_ascii_alphabetic()
            && program.as_bytes()[1] == b':'
            && program.as_bytes()[2] == b'\\';
        if !program.starts_with('/') && !valid_windows {
            return Err(VmError::InvalidData);
        }
        let mut argv = vec![program.to_owned()];
        argv.extend_from_slice(args);
        Self::validate_exec(Some(&argv), None, None, Some(timeout_ms))?;
        let socket = self.owned_qga_socket(id).await?;
        let result = qga::exec(id, &socket, program, args, timeout_ms).await;
        if matches!(result, Err(VmError::Timeout)) {
            // QGA guest-exec cannot be cancelled. Fail closed until explicitly
            // stopped and inspected; no second command is issued unknowingly.
            let mut record = self.load_instance(id)?;
            record.state = VmState::Failed;
            save_instance(&self.instance_dir(id), &record)?;
        }
        result
    }

    #[cfg(unix)]
    pub async fn upload_qemu_fork(
        &self,
        id: &str,
        guest_path: &str,
        data_base64: &str,
    ) -> VmResult<serde_json::Value> {
        let socket = self.owned_qga_socket(id).await?;
        qga::upload(id, &socket, guest_path, data_base64).await
    }

    #[cfg(unix)]
    pub async fn download_qemu_fork(
        &self,
        id: &str,
        guest_path: &str,
    ) -> VmResult<serde_json::Value> {
        let socket = self.owned_qga_socket(id).await?;
        qga::download(id, &socket, guest_path).await
    }

    pub fn guest_status(&self, id: &str) -> VmResult<VmGuestStatus> {
        let instance = self.load_instance(id)?;
        let profile = self
            .images()?
            .images
            .into_iter()
            .find(|image| image.name == instance.image)
            .map(|image| image.boot_profile);
        let reason = if cfg!(windows) && profile == Some(VmBootProfile::WindowsUefi) {
            "Windows guest control requires explicit operation-scoped PowerShell Direct credentials; use vm_exec, vm_upload or vm_download"
        } else {
            "Guest control is unavailable for this VM backend or image profile"
        };
        Ok(VmGuestStatus {
            action: "guest_status",
            id: id.to_owned(),
            available: false,
            reason: reason.to_owned(),
        })
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

    /// Conservative aggregate admission control, serialized by the manager's
    /// commands lock and protected across processes by the VM-home lease.
    /// Failed/Starting instances may still own live guests and consume quota.
    fn check_capacity(&self, add_cpus: u8, add_memory_mb: u32, creating: bool) -> VmResult<()> {
        let mut registered = 0usize;
        let mut active = 0usize;
        let mut cpus = 0u32;
        let mut memory = 0u64;
        let parent = self.root().join("instances");
        if fs::canonicalize(&parent)? != parent {
            return Err(VmError::NotOwned);
        }
        for entry in fs::read_dir(&parent)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() {
                return Err(VmError::NotOwned);
            }
            let name = entry.file_name();
            let id = name.to_str().ok_or(VmError::NotOwned)?;
            let instance = self.load_instance(id)?;
            registered = registered.checked_add(1).ok_or(VmError::OutputLimit)?;
            if instance.state != VmState::Stopped {
                active = active.checked_add(1).ok_or(VmError::OutputLimit)?;
                cpus = cpus
                    .checked_add(u32::from(instance.cpus))
                    .ok_or(VmError::OutputLimit)?;
                memory = memory
                    .checked_add(u64::from(instance.memory_mb))
                    .ok_or(VmError::OutputLimit)?;
            }
        }
        if (creating && registered >= MAX_REGISTERED_FORKS)
            || active >= MAX_ACTIVE_FORKS
            || cpus.saturating_add(u32::from(add_cpus)) > MAX_ACTIVE_CPUS
            || memory.saturating_add(u64::from(add_memory_mb)) > MAX_ACTIVE_MEMORY_MB
        {
            return Err(VmError::Busy);
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
        let files = [
            "instance.json",
            "instance.next",
            "vm.toml",
            "uefi-vars.fd",
            disk.as_str(),
        ];
        #[cfg(unix)]
        let sockets = ["qga.sock", "qmp.sock", "display.sock"];
        let mut paths = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let screen = name.to_str().is_some_and(|text| {
                text.strip_prefix("screen-")
                    .and_then(|suffix| suffix.strip_suffix(".png"))
                    .is_some_and(|hex| {
                        hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit())
                    })
            });
            #[cfg(unix)]
            let socket = {
                use std::os::unix::fs::FileTypeExt;
                sockets.iter().any(|candidate| name == *candidate) && entry.file_type()?.is_socket()
            };
            #[cfg(not(unix))]
            let socket = false;
            if entry.file_type()?.is_symlink()
                || !(entry.file_type()?.is_file()
                    && (files.iter().any(|candidate| name == *candidate) || screen)
                    || socket)
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
        self.launch_qemu(id, overlay, cpus, memory).await
    }

    fn cleanup_network_listener(&self, id: &str) -> VmResult<()> {
        for network in self.linux_options(id)?.networks {
            if let linux_qemu::VmNetwork::IsolatedListen { segment } = network {
                let socket = linux_qemu::network_socket(&self.instance_dir(id), &segment)?;
                if let Ok(meta) = socket.symlink_metadata() {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::FileTypeExt;
                        if !meta.file_type().is_socket() {
                            return Err(VmError::NotOwned);
                        }
                        fs::remove_file(socket)?;
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = meta;
                        return Err(VmError::Unsupported("QEMU network sockets require Unix"));
                    }
                }
            }
        }
        Ok(())
    }

    async fn running_qemu_identity(&self, id: &str) -> VmResult<(Option<u32>, Option<u64>)> {
        let pid = {
            let children = self.inner.children.lock().await;
            children
                .get(id)
                .and_then(Child::id)
                .ok_or(VmError::NotOwned)?
        };
        #[cfg(target_os = "linux")]
        {
            let ticks = tokio::task::spawn_blocking(move || process_identity::start_ticks(pid))
                .await
                .map_err(|_| VmError::NotOwned)??;
            Ok((Some(pid), Some(ticks)))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = pid;
            Ok((None, None))
        }
    }

    async fn verified_orphan(&self, instance: &VmInstance) -> VmResult<bool> {
        #[cfg(target_os = "linux")]
        {
            let (Some(pid), Some(ticks)) = (instance.qemu_pid, instance.qemu_start_ticks) else {
                return Ok(false);
            };
            let dir = self.instance_dir(&instance.id);
            tokio::task::spawn_blocking(move || process_identity::verify(&dir, pid, ticks))
                .await
                .map_err(|_| VmError::NotOwned)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = instance;
            Ok(false)
        }
    }

    /// Terminate only an OS-verified survivor of this exact QEMU fork.
    /// QMP is the control channel; a stored PID alone never authorizes signals.
    #[cfg(target_os = "linux")]
    async fn signal_verified_orphan(&self, instance: &VmInstance, graceful: bool) -> VmResult<()> {
        if !self.verified_orphan(instance).await? {
            return Err(VmError::NotOwned);
        }
        let socket = self.instance_dir(&instance.id).join("qmp.sock");
        use std::os::unix::fs::FileTypeExt;
        if !socket
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_socket())
        {
            return Err(VmError::NotOwned);
        }
        // For a quit request, QEMU may close the QMP socket before returning
        // an acknowledgement. The only success condition is process exit.
        let command = if graceful {
            qmp::request_powerdown(&socket).await
        } else {
            qmp::request_quit(&socket).await
        };
        let start = std::time::Instant::now();
        loop {
            if !self.verified_orphan(instance).await? {
                return Ok(());
            }
            if start.elapsed() >= Duration::from_secs(10) {
                return Err(command.err().unwrap_or(VmError::Timeout));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    /// Re-attest the owned QCOW2 overlay before offline inspection, restoration
    /// or launch. A stopped disk must not be rebased to host-private files.
    pub(crate) async fn verify_overlay(&self, instance: &VmInstance) -> VmResult<()> {
        if instance.backend != VmBackend::Qemu {
            return Err(VmError::Unsupported("QEMU QCOW2 overlay only"));
        }
        let approved = self
            .images()?
            .images
            .into_iter()
            .find(|image| image.name == instance.image)
            .ok_or(VmError::NotFound)?;
        let dir = self.instance_dir(&instance.id);
        if fs::canonicalize(&dir)? != dir {
            return Err(VmError::NotOwned);
        }
        let disk = dir.join("disk.qcow2");
        let base = self
            .root()
            .join("images")
            .join(&approved.name)
            .join("base.qcow2");
        for file in [&disk, &base] {
            let meta = file.symlink_metadata()?;
            if !meta.is_file() || meta.file_type().is_symlink() {
                return Err(VmError::NotOwned);
            }
        }
        let output = run_command(
            "qemu-img",
            &[
                "info".into(),
                "--output=json".into(),
                disk.display().to_string(),
            ],
            Duration::from_secs(15),
        )
        .await?;
        if !output.status.success() {
            return Err(VmError::InvalidData);
        }
        let info: serde_json::Value =
            serde_json::from_slice(&output.stdout).map_err(|_| VmError::InvalidData)?;
        if info["format"].as_str() != Some("qcow2")
            || info["filename"].as_str() != disk.to_str()
            || info["backing-filename"].as_str() != base.to_str()
            || info["full-backing-filename"].as_str() != base.to_str()
            || info["backing-filename-format"].as_str() != Some("qcow2")
            || info.get("data-file").is_some()
            || info["encrypted"].as_bool() == Some(true)
            || info["format-specific"]["data"]["corrupt"].as_bool() == Some(true)
        {
            return Err(VmError::InvalidData);
        }
        Ok(())
    }

    fn linux_options(&self, id: &str) -> VmResult<linux_qemu::LinuxVmOptions> {
        let file = self.instance_dir(id).join("vm.toml");
        if !file.exists() {
            return Ok(linux_qemu::LinuxVmOptions::default());
        }
        if file.symlink_metadata()?.file_type().is_symlink() {
            return Err(VmError::NotOwned);
        }
        let bytes = fs::read(file)?;
        if bytes.len() > 8_192 {
            return Err(VmError::OutputLimit);
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| VmError::InvalidData)?;
        toml::from_str(text).map_err(|_| VmError::InvalidData)
    }

    async fn launch_qemu(&self, id: &str, overlay: &Path, cpus: u8, memory: u32) -> VmResult<()> {
        let dir = self.instance_dir(id);
        let options = self.linux_options(id)?;
        let instance = self.load_instance(id)?;
        let profile = self
            .images()?
            .images
            .into_iter()
            .find(|image| image.name == instance.image)
            .ok_or(VmError::NotFound)?
            .boot_profile;
        let iso = match instance.installer_iso.as_deref() {
            Some(name) => Some(self.verified_iso(name).await?),
            None => None,
        };
        // Remove only known stale, owned Unix sockets from a previously stopped fork.
        for name in ["qmp.sock", "qga.sock", "display.sock"] {
            let file = dir.join(name);
            if let Ok(meta) = file.symlink_metadata() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::FileTypeExt;
                    if !meta.file_type().is_socket() {
                        return Err(VmError::NotOwned);
                    }
                    fs::remove_file(&file)?;
                }
                #[cfg(not(unix))]
                {
                    let _ = meta;
                    return Err(VmError::Unsupported("QEMU managed sockets require Unix"));
                }
            }
        }
        let args = linux_qemu::build_command(
            &dir,
            overlay,
            iso.as_deref(),
            cpus,
            memory,
            profile,
            &options,
        )?;
        let mut child = Command::new("qemu-system-x86_64")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(false)
            .spawn()?;
        // Do not report a guest as started if QEMU immediately rejects its arguments.
        tokio::time::sleep(Duration::from_millis(250)).await;
        if child.try_wait()?.is_some() {
            return Err(VmError::CommandFailed);
        }
        self.inner
            .children
            .lock()
            .await
            .insert(id.to_owned(), child);
        Ok(())
    }
}

fn validate_qcow2_base(path: &Path) -> VmResult<()> {
    let output = std::process::Command::new("qemu-img")
        .args(["info", "--output=json"])
        .arg(path)
        .stdin(Stdio::null())
        .output()?;
    if !output.status.success()
        || output.stdout.len() > 48 * 1024
        || output.stderr.len() > 48 * 1024
    {
        return Err(VmError::InvalidData);
    }
    let info: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|_| VmError::InvalidData)?;
    if info.get("format").and_then(serde_json::Value::as_str) != Some("qcow2")
        || info.get("backing-filename").is_some()
        || info.get("full-backing-filename").is_some()
        || info.get("data-file").is_some()
        || info.get("encrypted").and_then(serde_json::Value::as_bool) == Some(true)
    {
        return Err(VmError::InvalidData);
    }
    Ok(())
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
    write_bytes_new(path, &contents)
}
fn write_bytes_new(path: &Path, data: &[u8]) -> VmResult<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(data)?;
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
    boot_profile: VmBootProfile,
) -> VmResult<()> {
    let name = quote_ps(&hyperv_name(id));
    #[cfg(windows)]
    let src = quote_ps(&windows_hyperv::hyperv_path(source)?);
    #[cfg(not(windows))]
    let src = quote_ps(&source.display().to_string());
    #[cfg(windows)]
    let dest = quote_ps(&windows_hyperv::hyperv_path(overlay)?);
    #[cfg(not(windows))]
    let dest = quote_ps(&overlay.display().to_string());
    let mut script = format!(
        "$ErrorActionPreference = 'Stop'; \
         if (Get-VM -Name {name} -ErrorAction SilentlyContinue) {{ throw 'VM already exists' }}; \
         New-VHD -Path {dest} -ParentPath {src} -Differencing -ErrorAction Stop | Out-Null; \
         New-VM -Name {name} -Generation 2 -VHDPath {dest} -MemoryStartupBytes {}MB -ErrorAction Stop | Out-Null; \
         Set-VMProcessor -VMName {name} -Count {cpus} -ErrorAction Stop; \
         Set-VMMemory -VMName {name} -DynamicMemoryEnabled $false -StartupBytes {}MB -ErrorAction Stop; \
         Set-VM -Name {name} -AutomaticStartAction Nothing -AutomaticCheckpointsEnabled $false -ErrorAction Stop; ",
        memory, memory
    );
    match boot_profile {
        VmBootProfile::WindowsUefi => script.push_str(&format!(
            "Set-VMFirmware -VMName {name} -EnableSecureBoot On -SecureBootTemplate 'MicrosoftWindows' -ErrorAction Stop; \
             Set-VMKeyProtector -VMName {name} -NewLocalKeyProtector -ErrorAction Stop; \
             Enable-VMTPM -VMName {name} -ErrorAction Stop; "
        )),
        VmBootProfile::LinuxUefi => script.push_str(&format!(
            "Set-VMFirmware -VMName {name} -EnableSecureBoot Off -ErrorAction Stop; "
        )),
        VmBootProfile::Legacy => {}
    }
    script.push_str(&format!("Start-VM -Name {name} -ErrorAction Stop"));
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
    fn make_fixture_disk(path: &Path) {
        if VmBackend::host() == VmBackend::Qemu {
            let out = std::process::Command::new("qemu-img")
                .args(["create", "-f", "qcow2"])
                .arg(path)
                .arg("32M")
                .output()
                .expect("qemu-img");
            assert!(out.status.success());
        } else {
            fs::write(path, b"test-only-image-content").expect("test vhdx fixture");
        }
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
        make_fixture_disk(&source);
        let manager = VmManager::new(store.path()).expect("VM home");
        let image = manager
            .import_image("test-image", &source, 2, 1024)
            .expect("explicit import");
        assert_eq!(image.bytes, fs::metadata(&source).unwrap().len());
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

    #[tokio::test]
    async fn tampered_base_is_rejected_without_empty_instance_directory() {
        let store = tempfile::tempdir().expect("store");
        let input = tempfile::tempdir().expect("image input");
        let manager = VmManager::new(store.path()).expect("owned VM home");
        let source = input
            .path()
            .join(format!("fixture.{}", manager.backend().disk_ext()));
        make_fixture_disk(&source);
        manager
            .import_image("approved", &source, 2, 1024)
            .expect("import");
        let base = manager
            .root()
            .join("images")
            .join("approved")
            .join(format!("base.{}", manager.backend().disk_ext()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&base, fs::Permissions::from_mode(0o600)).unwrap();
        }
        #[cfg(not(unix))]
        {
            let mut permissions = fs::metadata(&base).unwrap().permissions();
            // Clear the read-only attribute on the disposable non-Unix test file.
            // Unix uses owner-only mode 0600 above, not set_readonly(false).
            #[expect(
                clippy::permissions_set_readonly_false,
                reason = "Non-Unix test fixture only"
            )]
            permissions.set_readonly(false);
            fs::set_permissions(&base, permissions).unwrap();
        }
        fs::write(base, b"changed-after-approval").expect("simulate tampering");
        let result = manager
            .create(VmCreate {
                image: "approved".into(),
                cpus: None,
                memory_mb: None,
                linux_options: None,
                installer_iso: None,
            })
            .await;
        assert!(matches!(result, Err(VmError::InvalidData)));
        assert_eq!(
            fs::read_dir(manager.root().join("instances"))
                .expect("instances")
                .count(),
            0,
            "Hash rejection must not reserve a VM instance directory"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_qcow2_with_host_backing_reference() {
        let temp = tempfile::tempdir().unwrap();
        let raw = temp.path().join("external.raw");
        let overlay = temp.path().join("untrusted.qcow2");
        let base = std::process::Command::new("qemu-img")
            .args(["create", "-f", "raw"])
            .arg(&raw)
            .arg("32M")
            .output()
            .unwrap();
        assert!(base.status.success());
        let linked = std::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2", "-F", "raw", "-b"])
            .arg(&raw)
            .arg(&overlay)
            .output()
            .unwrap();
        assert!(linked.status.success());
        let manager = VmManager::new(temp.path().join("managed")).unwrap();
        assert!(matches!(
            manager.import_image("unsafe", &overlay, 1, 512),
            Err(VmError::InvalidData)
        ));
        assert!(!manager.root().join("images/unsafe").exists());
    }

    #[test]
    fn aggregate_vm_admission_limits_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let manager = VmManager::new(temp.path()).unwrap();
        for number in 1..=MAX_ACTIVE_FORKS {
            let id = format!("vm-{number:016x}");
            let dir = manager.root().join("instances").join(&id);
            fs::create_dir(&dir).unwrap();
            write_json_new(
                &dir.join("instance.json"),
                &VmInstance {
                    id,
                    image: "approved".into(),
                    backend: manager.backend(),
                    state: VmState::Starting,
                    cpus: 1,
                    memory_mb: 512,
                    installer_iso: None,
                    qemu_pid: None,
                    qemu_start_ticks: None,
                },
            )
            .unwrap();
        }
        assert!(matches!(
            manager.check_capacity(1, 512, true),
            Err(VmError::Busy)
        ));
        // Malformed or foreign instance directories also block admission.
        fs::create_dir(
            manager
                .root()
                .join("instances")
                .join("../instances/foreign"),
        )
        .unwrap();
        assert!(matches!(
            manager.check_capacity(1, 512, true),
            Err(VmError::InvalidName)
        ));
    }

    #[test]
    fn only_one_vm_manager_can_own_a_home_across_live_handles() {
        let temp = tempfile::tempdir().unwrap();
        let first = VmManager::new(temp.path()).unwrap();
        assert!(matches!(VmManager::new(temp.path()), Err(VmError::Busy)));
        drop(first);
        // Concurrent tests may fork a QEMU child while the lease fd is open.
        // A forked child shares the lock until exec closes its inherited fd.
        // Allow that brief transition, but fail if the lease remains held.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match VmManager::new(temp.path()) {
                Ok(_) => break,
                Err(VmError::Busy) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => {
                    panic!("VM home must be reusable after releasing its lease: {error:?}")
                }
            }
        }
    }

    /// A local writer must not be able to redirect a stopped overlay to a
    /// different backing disk and then make NovaGPT launch or snapshot it.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn rejects_tampered_overlay_backing_on_start_and_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let initial = temp.path().join("approved.qcow2");
        make_fixture_disk(&initial);
        let vm = VmManager::new(temp.path().join("owned")).unwrap();
        vm.import_image("approved", &initial, 1, 512).unwrap();
        let created = vm
            .create(VmCreate {
                image: "approved".into(),
                cpus: None,
                memory_mb: None,
                linux_options: None,
                installer_iso: None,
            })
            .await
            .unwrap();
        let id = created.instance.id;
        vm.stop(&id).await.unwrap();
        let rogue_base = temp.path().join("external.qcow2");
        make_fixture_disk(&rogue_base);
        let disk = vm.instance_dir(&id).join("disk.qcow2");
        let rebase = tokio::process::Command::new("qemu-img")
            .args(["rebase", "-u", "-f", "qcow2", "-F", "qcow2", "-b"])
            .arg(&rogue_base)
            .arg(&disk)
            .output()
            .await
            .unwrap();
        assert!(rebase.status.success());
        assert!(matches!(vm.start(&id).await, Err(VmError::InvalidData)));
        assert!(matches!(
            vm.snapshot_list(&id).await,
            Err(VmError::InvalidData)
        ));
        // No host-owned or unrelated disk is ever deleted during cleanup.
        vm.destroy(&id).await.unwrap();
        assert!(rogue_base.exists());
        assert!(initial.exists());
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
