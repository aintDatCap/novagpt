//! Version-aware ChromeDriver installation.
//!
//! Verified behavior:
//! - ChromeDriver and Chrome are distinct binaries; only ChromeDriver is
//!   downloaded and patched here.
//! - The requested Chrome *milestone* selects a matching driver release.
//! - Downloads are HTTPS-only from two documented hosts, bounded, redirect
//!   free, and optionally pinned to a caller-supplied SHA-256.
//! - Archives are parsed defensively: no traversal, symlinks, duplicates,
//!   oversized or unexpected entries. Only the exact driver entry is written.
//! - Installation is atomic and reuses an earlier verified/patched build.
//!
//! Chrome for Testing does not publish per-archive SHA-256 checksums (checked
//! 2026-11-04: `*.sha256` sidecars return 404). NovaGPT therefore records the
//! archive and binary digests at first install and re-verifies them on reuse,
//! and supports an explicit pin for reproducible deployments.
use super::{
    error::UpstreamError,
    manifest::{
        self, CFT_MILESTONES_URL, CFT_STABLE_URL, allowed_download_url, allowed_fetch_url,
        archive_binary_path,
    },
    patch::PatchReport,
    patch::{PatchStatus, contains_version, patch_driver},
    platform::{ChromeVersion, Platform},
};
use async_trait::async_trait;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::{
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
};
use std::{
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::task::spawn_blocking;

const STORE_MARKER: &[u8] = b"NovaGPT ChromeDriver store v1\n";
const STORE_MARKER_FILE: &str = ".novagpt-chromedriver-store";
const STORE_LOCK_FILE: &str = ".novagpt-chromedriver.lock";
const METADATA_FILE: &str = "install.json";
const DIGEST_FILE: &str = "chromedriver.sha256";
const MAX_ARCHIVE_BYTES: usize = 64 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(120);
const METADATA_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InstallRequest {
    /// Exact driver version. Takes priority over `milestone`/`chrome_version`.
    pub exact_version: Option<ChromeVersion>,
    /// Explicit `version_main`-style Chrome milestone.
    pub milestone: Option<u32>,
    /// Installed Chrome version; only its milestone is used, matching
    /// ChromeDriver's documented compatibility boundary.
    pub chrome_version: Option<ChromeVersion>,
    /// Optional SHA-256 pin for the downloaded archive.
    pub archive_sha256: Option<[u8; 32]>,
    /// Test/deployment override; never taken from a remote request.
    pub platform: Option<Platform>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledDriver {
    pub path: PathBuf,
    pub version: ChromeVersion,
    pub platform: Platform,
    pub binary_sha256: [u8; 32],
    pub freshly_installed: bool,
}

#[derive(Serialize, Deserialize)]
struct InstallMetadata {
    schema: u32,
    version: String,
    platform: String,
    archive_sha256: String,
    binary_sha256: String,
    patched: bool,
    source: String,
}

#[async_trait]
pub trait DriverFetcher: Send + Sync {
    async fn fetch(&self, url: &str, limit: usize) -> Result<Vec<u8>, UpstreamError>;
}

#[derive(Default)]
pub struct HttpFetcher;

#[async_trait]
impl DriverFetcher for HttpFetcher {
    async fn fetch(&self, url: &str, limit: usize) -> Result<Vec<u8>, UpstreamError> {
        if !allowed_fetch_url(url) {
            return Err(UpstreamError::Manifest);
        }
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| UpstreamError::Download)?;
        let mut response = client
            .get(url)
            .send()
            .await
            .map_err(|_| UpstreamError::Download)?;
        if !response.status().is_success() {
            return Err(UpstreamError::Download);
        }
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(UpstreamError::TooLarge);
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if chunk.len() <= limit - body.len() => {
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                _ => return Err(UpstreamError::TooLarge),
            }
        }
        Ok(body)
    }
}

pub struct ChromeDriverStore {
    root: PathBuf,
    platform: Platform,
    fetcher: Arc<dyn DriverFetcher>,
}

impl ChromeDriverStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, UpstreamError> {
        Self::with_fetcher(root, Arc::new(HttpFetcher))
    }

    pub fn with_fetcher(
        root: impl Into<PathBuf>,
        fetcher: Arc<dyn DriverFetcher>,
    ) -> Result<Self, UpstreamError> {
        let root = root.into();
        if root.as_os_str().is_empty() || !root.is_absolute() {
            return Err(UpstreamError::InvalidConfig);
        }
        Ok(Self {
            root,
            platform: Platform::current()?,
            fetcher,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn default_root() -> Result<PathBuf, UpstreamError> {
        if let Some(explicit) = std::env::var_os("NOVAGPT_CHROMEDRIVER_HOME") {
            let path = PathBuf::from(explicit);
            return path
                .is_absolute()
                .then_some(path)
                .ok_or(UpstreamError::InvalidConfig);
        }
        #[cfg(windows)]
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .ok_or(UpstreamError::InvalidConfig)?;
        #[cfg(target_os = "macos")]
        let base = std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support"))
            .ok_or(UpstreamError::InvalidConfig)?;
        #[cfg(all(unix, not(target_os = "macos")))]
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
            .ok_or(UpstreamError::InvalidConfig)?;
        #[cfg(not(any(windows, unix)))]
        return Err(UpstreamError::UnsupportedPlatform);
        #[cfg(any(windows, unix))]
        Ok(base.join("novagpt/chromedriver"))
    }

    /// Install or reuse a driver matching the request. Reuse is verified
    /// against the recorded digest and patch marker; a corrupted or foreign
    /// installation is never executed.
    pub async fn install(&self, request: InstallRequest) -> Result<InstalledDriver, UpstreamError> {
        let platform = request.platform.unwrap_or(self.platform);
        let (exact, milestone) = match (request.exact_version, request.milestone) {
            (Some(exact), Some(milestone)) if exact.milestone() != milestone => {
                return Err(UpstreamError::InvalidConfig);
            }
            (Some(exact), _) => (Some(exact), Some(exact.milestone())),
            (None, Some(0)) => {
                return Err(UpstreamError::InvalidConfig);
            }
            (None, Some(milestone)) => (None, Some(milestone)),
            (None, None) => (None, request.chrome_version.map(ChromeVersion::milestone)),
        };

        let root = self.root.clone();
        spawn_blocking(move || ensure_store(&root))
            .await
            .map_err(|_| UpstreamError::Io)??;
        let lease = self.acquire_lock().await?;

        // Re-check a complete existing installation while holding the lock.
        if let Some(milestone) = milestone {
            let root = self.root.clone();
            let existing =
                spawn_blocking(move || find_installed(&root, platform, exact, milestone))
                    .await
                    .map_err(|_| UpstreamError::Io)??;
            if let Some(driver) = existing {
                drop(lease);
                return Ok(driver);
            }
        }

        let release = match exact {
            Some(version) => manifest::release_for(version, platform)?,
            None => match milestone {
                Some(milestone) => self.resolve_release(milestone, platform).await?,
                // Upstream's no-version default is the current Stable release.
                None => self.latest_stable().await?,
            },
        };
        if milestone.is_none() && exact.is_none() {
            let root = self.root.clone();
            let version = release.version;
            if let Some(driver) = spawn_blocking(move || {
                find_installed(&root, platform, Some(version), version.milestone())
            })
            .await
            .map_err(|_| UpstreamError::Io)??
            {
                drop(lease);
                return Ok(driver);
            }
        }
        if !allowed_download_url(&release.url) {
            return Err(UpstreamError::Manifest);
        }
        let archive = self.fetcher.fetch(&release.url, MAX_ARCHIVE_BYTES).await?;
        if archive.is_empty() || archive.len() > MAX_ARCHIVE_BYTES {
            return Err(UpstreamError::TooLarge);
        }
        let archive_digest: [u8; 32] = Sha256::digest(&archive).into();
        if request
            .archive_sha256
            .is_some_and(|expected| expected != archive_digest)
        {
            return Err(UpstreamError::Integrity);
        }

        let root = self.root.clone();
        let source = release.url.clone();
        let expected_version = release.version;
        let installed = spawn_blocking(move || {
            install_verified(
                &root,
                platform,
                expected_version,
                &archive,
                archive_digest,
                &source,
            )
        })
        .await
        .map_err(|_| UpstreamError::Io)??;
        drop(lease);
        Ok(installed)
    }

    async fn resolve_release(
        &self,
        milestone: u32,
        platform: Platform,
    ) -> Result<manifest::DriverRelease, UpstreamError> {
        if milestone <= 114 {
            let url = manifest::legacy_release_url(milestone);
            let body = self.fetcher.fetch(&url, 256).await?;
            let version = manifest::parse_legacy_release(&body, milestone)?;
            return manifest::release_for(version, platform);
        }
        let body = self
            .fetcher
            .fetch(CFT_MILESTONES_URL, 8 * 1024 * 1024)
            .await?;
        let version = manifest::parse_milestone(&body, milestone)?;
        manifest::release_for(version, platform)
    }

    async fn acquire_lock(&self) -> Result<StoreLease, UpstreamError> {
        self.acquire_lock_for(LOCK_TIMEOUT).await
    }

    async fn acquire_lock_for(&self, lock_timeout: Duration) -> Result<StoreLease, UpstreamError> {
        let root = self.root.clone();
        spawn_blocking(move || {
            let path = root.join(STORE_LOCK_FILE);
            if path
                .symlink_metadata()
                .is_ok_and(|meta| meta.file_type().is_symlink())
            {
                return Err(UpstreamError::StoreNotOwned);
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
                .map_err(|_| UpstreamError::Io)?;
            let deadline = Instant::now() + lock_timeout;
            loop {
                match file.try_lock_exclusive() {
                    Ok(()) => return Ok(StoreLease { _file: file }),
                    Err(_) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => return Err(UpstreamError::Timeout),
                }
            }
        })
        .await
        .map_err(|_| UpstreamError::Io)?
    }

    /// Locate an already installed, verified, patched driver without any
    /// network access. Used by `novagpt driver status` and by callers that
    /// want to decide before initiating a download.
    pub async fn locate(
        &self,
        request: InstallRequest,
    ) -> Result<Option<InstalledDriver>, UpstreamError> {
        let platform = request.platform.unwrap_or(self.platform);
        let (exact, milestone) = match (request.exact_version, request.milestone) {
            (Some(exact), Some(milestone)) if exact.milestone() != milestone => {
                return Err(UpstreamError::InvalidConfig);
            }
            (Some(exact), _) => (Some(exact), exact.milestone()),
            (None, Some(0)) => return Err(UpstreamError::InvalidConfig),
            (None, Some(milestone)) => (None, milestone),
            (None, None) => match request.chrome_version {
                Some(chrome) => (None, chrome.milestone()),
                None => return Err(UpstreamError::InvalidConfig),
            },
        };
        if !self.root.exists() {
            return Ok(None);
        }
        let root = self.root.clone();
        spawn_blocking(move || find_installed(&root, platform, exact, milestone))
            .await
            .map_err(|_| UpstreamError::Io)?
    }

    /// Stable release for a host without a detectable Chrome. Exposed for a
    /// deterministic CLI smoke path; normal launches always match Chrome.
    pub async fn latest_stable(&self) -> Result<manifest::DriverRelease, UpstreamError> {
        let body = self.fetcher.fetch(CFT_STABLE_URL, 8 * 1024 * 1024).await?;
        let version = manifest::parse_stable(&body)?;
        manifest::release_for(version, self.platform)
    }
}

struct StoreLease {
    _file: File,
}

fn ensure_store(root: &Path) -> Result<(), UpstreamError> {
    if !root.is_absolute() {
        return Err(UpstreamError::InvalidConfig);
    }
    if root
        .symlink_metadata()
        .is_ok_and(|meta| meta.file_type().is_symlink())
    {
        return Err(UpstreamError::StoreNotOwned);
    }
    fs::create_dir_all(root).map_err(|_| UpstreamError::Io)?;
    let marker = root.join(STORE_MARKER_FILE);
    if marker.exists() {
        if marker
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
            || fs::read(&marker).map_err(|_| UpstreamError::Io)? != STORE_MARKER
        {
            return Err(UpstreamError::StoreNotOwned);
        }
        return Ok(());
    }
    let mut entries = fs::read_dir(root).map_err(|_| UpstreamError::Io)?;
    if entries.next().is_some() {
        return Err(UpstreamError::StoreNotOwned);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|_| UpstreamError::Io)?;
    file.write_all(STORE_MARKER)
        .map_err(|_| UpstreamError::Io)?;
    file.sync_all().map_err(|_| UpstreamError::Io)?;
    Ok(())
}

fn install_dir(root: &Path, version: ChromeVersion, platform: Platform) -> PathBuf {
    root.join(version.to_string()).join(platform.cft_name())
}

fn find_installed(
    root: &Path,
    platform: Platform,
    exact: Option<ChromeVersion>,
    milestone: u32,
) -> Result<Option<InstalledDriver>, UpstreamError> {
    let mut candidates = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return Ok(None),
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Ok(version) = name.parse::<ChromeVersion>() else {
            continue;
        };
        if entry
            .file_type()
            .map_err(|_| UpstreamError::Io)?
            .is_symlink()
        {
            continue;
        }
        if exact.is_some_and(|exact| exact != version) || version.milestone() != milestone {
            continue;
        }
        let path = install_dir(root, version, platform).join(platform.executable_name());
        if path.is_file() {
            candidates.push((version, path));
        }
    }
    candidates.sort_by_key(|(version, _)| *version);
    for (version, path) in candidates.into_iter().rev() {
        if let Some(driver) = verify_installation(root, platform, version, &path)? {
            return Ok(Some(driver));
        }
    }
    Ok(None)
}

fn verify_installation(
    root: &Path,
    platform: Platform,
    version: ChromeVersion,
    executable: &Path,
) -> Result<Option<InstalledDriver>, UpstreamError> {
    let directory = executable.parent().ok_or(UpstreamError::Io)?;
    let metadata_bytes = match fs::read(directory.join(METADATA_FILE)) {
        Ok(bytes) if bytes.len() <= 16 * 1024 => bytes,
        _ => return Ok(None),
    };
    let metadata: InstallMetadata = match serde_json::from_slice(&metadata_bytes) {
        Ok(metadata) => metadata,
        Err(_) => return Ok(None),
    };
    if metadata.schema != METADATA_SCHEMA
        || metadata.version != version.to_string()
        || metadata.platform != platform.cft_name()
        || !metadata.patched
        || !metadata.source.starts_with("https://")
        || !executable.starts_with(root)
    {
        return Ok(None);
    }
    let binary = fs::read(executable).map_err(|_| UpstreamError::Io)?;
    if binary.len() as u64 > MAX_BINARY_BYTES {
        return Ok(None);
    }
    let digest: [u8; 32] = Sha256::digest(&binary).into();
    let expected = hex::decode(&metadata.binary_sha256).map_err(|_| UpstreamError::Io)?;
    if expected.as_slice() != digest {
        return Ok(None);
    }
    let recorded = fs::read_to_string(directory.join(DIGEST_FILE)).unwrap_or_default();
    if recorded.trim() != hex::encode(digest) {
        return Ok(None);
    }
    if !contains_version(&binary, &version) || !super::patch::is_patched(executable) {
        return Ok(None);
    }
    Ok(Some(InstalledDriver {
        path: executable.to_path_buf(),
        version,
        platform,
        binary_sha256: digest,
        freshly_installed: false,
    }))
}

fn install_verified(
    root: &Path,
    platform: Platform,
    expected: ChromeVersion,
    archive: &[u8],
    archive_digest: [u8; 32],
    source: &str,
) -> Result<InstalledDriver, UpstreamError> {
    let binary_bytes = extract_driver(
        archive,
        &archive_binary_path(expected, platform),
        MAX_BINARY_BYTES,
    )?;
    let temp_version = tempfile::Builder::new()
        .prefix(".extract-")
        .tempdir_in(root)
        .map_err(|_| UpstreamError::Io)?;
    let temp_binary = temp_version.path().join(platform.executable_name());
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_binary)
            .map_err(|_| UpstreamError::Io)?;
        file.write_all(&binary_bytes)
            .map_err(|_| UpstreamError::Io)?;
        file.sync_all().map_err(|_| UpstreamError::Io)?;
    }
    if !contains_version(&binary_bytes, &expected) {
        return Err(UpstreamError::VersionMismatch);
    }
    match patch_driver(&temp_binary)? {
        PatchReport {
            status: PatchStatus::Patched | PatchStatus::AlreadyPatched,
            ..
        } => {}
    }
    if !super::patch::is_patched(&temp_binary) {
        return Err(UpstreamError::Unpatchable);
    }
    let patched = fs::read(&temp_binary).map_err(|_| UpstreamError::Io)?;
    let binary_digest: [u8; 32] = Sha256::digest(&patched).into();
    let metadata = InstallMetadata {
        schema: METADATA_SCHEMA,
        version: expected.to_string(),
        platform: platform.cft_name().to_owned(),
        archive_sha256: hex::encode(archive_digest),
        binary_sha256: hex::encode(binary_digest),
        patched: true,
        source: source.to_owned(),
    };
    let final_dir = install_dir(root, expected, platform);
    if let Some(parent) = final_dir.parent() {
        if parent
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(UpstreamError::StoreNotOwned);
        }
        fs::create_dir_all(parent).map_err(|_| UpstreamError::Io)?;
    }
    if final_dir.symlink_metadata().is_ok() {
        if final_dir
            .symlink_metadata()
            .is_ok_and(|meta| meta.file_type().is_symlink())
        {
            return Err(UpstreamError::StoreNotOwned);
        }
        fs::remove_dir_all(&final_dir).map_err(|_| UpstreamError::Io)?;
    }
    fs::write(
        temp_version.path().join(DIGEST_FILE),
        format!("{}\n", hex::encode(binary_digest)),
    )
    .map_err(|_| UpstreamError::Io)?;
    fs::write(
        temp_version.path().join(METADATA_FILE),
        serde_json::to_vec(&metadata).map_err(|_| UpstreamError::Protocol)?,
    )
    .map_err(|_| UpstreamError::Io)?;
    // Rename the same-filesystem directory that already contains the verified
    // executable, digest, and metadata.
    fs::rename(temp_version.path(), &final_dir).map_err(|_| UpstreamError::Io)?;
    let executable = final_dir.join(platform.executable_name());
    // The temporary directory has been consumed by rename; prevent TempDir
    // from trying to delete the installed directory.
    let _ = temp_version.keep();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))
            .map_err(|_| UpstreamError::Io)?;
    }
    if !contains_version(&patched, &expected) || !super::patch::is_patched(&executable) {
        return Err(UpstreamError::Integrity);
    }
    Ok(InstalledDriver {
        path: executable,
        version: expected,
        platform,
        binary_sha256: binary_digest,
        freshly_installed: true,
    })
}

/// Extract exactly one expected executable entry and reject every unsafe
/// archive shape before writing anything.
fn extract_driver(
    archive: &[u8],
    expected_entry: &str,
    binary_limit: u64,
) -> Result<Vec<u8>, UpstreamError> {
    let mut zip =
        zip::ZipArchive::new(Cursor::new(archive)).map_err(|_| UpstreamError::InvalidArchive)?;
    if zip.is_empty() || zip.len() > 64 {
        return Err(UpstreamError::InvalidArchive);
    }
    let mut total = 0_u64;
    let mut found: Option<Vec<u8>> = None;
    for index in 0..zip.len() {
        let mut entry = zip
            .by_index(index)
            .map_err(|_| UpstreamError::InvalidArchive)?;
        let name = entry
            .name()
            .map_err(|_| UpstreamError::InvalidArchive)?
            .into_owned();
        validate_entry_name(&name)?;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170_000 == 0o120_000)
        {
            return Err(UpstreamError::InvalidArchive);
        }
        if entry.is_dir() {
            continue;
        }
        let size = entry.size();
        if size > binary_limit
            || total
                .checked_add(size)
                .is_none_or(|total| total > 2 * binary_limit)
        {
            return Err(UpstreamError::InvalidArchive);
        }
        total += size;
        if name == expected_entry {
            if found.is_some() {
                return Err(UpstreamError::InvalidArchive);
            }
            let mut bytes = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
            entry
                .by_ref()
                .take(binary_limit + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| UpstreamError::InvalidArchive)?;
            if bytes.len() as u64 > binary_limit || bytes.len() as u64 != size {
                return Err(UpstreamError::InvalidArchive);
            }
            found = Some(bytes);
        }
    }
    found.ok_or(UpstreamError::InvalidArchive)
}

fn validate_entry_name(name: &str) -> Result<(), UpstreamError> {
    if name.is_empty()
        || name.len() > 256
        || name.contains('\0')
        || name.contains('\\')
        || name.starts_with('/')
    {
        return Err(UpstreamError::InvalidArchive);
    }
    for component in name.split('/') {
        if component.is_empty() || component == "." || component == ".." || component.len() > 128 {
            return Err(UpstreamError::InvalidArchive);
        }
    }
    Ok(())
}

/// Test-only in-memory fetcher shared by deterministic install tests. It is
/// compiled in normal builds because the public store accepts an injected
/// fetcher for deployments that mirror CfT behind an approved endpoint.
#[cfg(test)]
#[derive(Default)]
pub struct StaticFetcher {
    responses: HashMap<String, Vec<u8>>,
    calls: AtomicUsize,
}

#[cfg(test)]
impl StaticFetcher {
    pub fn new(responses: HashMap<String, Vec<u8>>) -> Self {
        Self {
            responses,
            calls: AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
#[async_trait]
impl DriverFetcher for StaticFetcher {
    async fn fetch(&self, url: &str, limit: usize) -> Result<Vec<u8>, UpstreamError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let body = self
            .responses
            .get(url)
            .ok_or(UpstreamError::Download)?
            .clone();
        if body.len() > limit {
            return Err(UpstreamError::TooLarge);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn driver_bytes(version: &str, patched: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x7fELF-novagpt-test\nplatform_handle\x00content\x00");
        bytes.extend_from_slice(version.as_bytes());
        bytes.extend_from_slice(b"\x00\n");
        if patched {
            bytes.extend_from_slice(b"{/*novagpt-chromedriver-patch*/}");
        } else {
            bytes.extend_from_slice(
                b"{window.cdc_do_not_use = {a:1,b:2,c:3,d:4,e:5}; console.log('fixture');}"
                    .as_slice(),
            );
        }
        bytes.resize(bytes.len() + 64, b' ');
        bytes
    }

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut output);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, bytes) in entries {
                writer.start_file(*name, options).expect("entry");
                writer.write_all(bytes).expect("write entry");
            }
            writer.finish().expect("finish zip");
        }
        output.into_inner()
    }

    fn response_map(urls: &[(&str, Vec<u8>)]) -> HashMap<String, Vec<u8>> {
        urls.iter()
            .map(|(url, bytes)| ((*url).to_owned(), bytes.clone()))
            .collect()
    }

    fn platform() -> Platform {
        Platform::current().expect("host platform")
    }

    fn entry_name(version: &str) -> String {
        archive_binary_path(version.parse().expect("version"), platform())
    }

    #[tokio::test]
    async fn installs_verified_patched_driver_and_reuses_it_without_network() {
        let platform = platform();
        let version = "156.0.8078.80";
        let entry = entry_name(version);
        let zip = zip_with(&[(&entry, &driver_bytes(version, false))]);
        let url = manifest::cft_download_url(version.parse().unwrap(), platform);
        let root = tempfile::tempdir().expect("store root");
        let request = InstallRequest {
            milestone: Some(156),
            ..Default::default()
        };
        let metadata_body = br#"{"milestones":{"156":{"version":"156.0.8078.80"}}}"#.to_vec();
        let fetcher = Arc::new(StaticFetcher::new(response_map(&[
            (CFT_MILESTONES_URL, metadata_body),
            (&url, zip),
        ])));
        let store =
            ChromeDriverStore::with_fetcher(root.path().join("owned"), fetcher.clone()).unwrap();
        let first = store.install(request.clone()).await.expect("first install");
        assert!(first.freshly_installed);
        assert!(first.path.is_file());
        assert!(super::super::patch::is_patched(&first.path));
        assert!(contains_version(
            &fs::read(&first.path).unwrap(),
            &version.parse().unwrap()
        ));
        assert_eq!(first.platform, platform);
        let calls = fetcher.calls();
        let second = store.install(request).await.expect("reuse");
        assert!(!second.freshly_installed);
        assert_eq!(second.path, first.path);
        assert_eq!(fetcher.calls(), calls, "reuse must not download again");
    }

    #[tokio::test]
    async fn rejects_archive_digest_mismatch_before_installation() {
        let version = "156.0.8078.80";
        let entry = entry_name(version);
        let zip = zip_with(&[(&entry, &driver_bytes(version, false))]);
        let url = manifest::cft_download_url(version.parse().unwrap(), platform());
        let fetcher = Arc::new(StaticFetcher::new(response_map(&[(&url, zip)])));
        let root = tempfile::tempdir().expect("root");
        let store = ChromeDriverStore::with_fetcher(root.path().join("owned"), fetcher).unwrap();
        let request = InstallRequest {
            exact_version: Some(version.parse().unwrap()),
            archive_sha256: Some([0_u8; 32]),
            ..Default::default()
        };
        assert_eq!(store.install(request).await, Err(UpstreamError::Integrity));
        assert!(!root.path().join("owned/156.0.8078.80").exists());
    }

    #[tokio::test]
    async fn rejects_traversal_symlink_duplicate_and_wrong_version_archives() {
        let version = "156.0.8078.80";
        let entry = entry_name(version);
        for archive in [
            zip_with(&[("../escape", b"x"), (&entry, &driver_bytes(version, false))]),
            zip_with(&[("/absolute", b"x"), (&entry, &driver_bytes(version, false))]),
            zip_with(&[(&entry, &driver_bytes("155.0.1.1", false))]),
            zip_with(&[("other/chromedriver", &driver_bytes(version, false))]),
        ] {
            let url = manifest::cft_download_url(version.parse().unwrap(), platform());
            let fetcher = Arc::new(StaticFetcher::new(response_map(&[(&url, archive)])));
            let root = tempfile::tempdir().expect("root");
            let store =
                ChromeDriverStore::with_fetcher(root.path().join("owned"), fetcher).unwrap();
            let result = store
                .install(InstallRequest {
                    exact_version: Some(version.parse().unwrap()),
                    ..Default::default()
                })
                .await;
            assert!(
                matches!(
                    result,
                    Err(UpstreamError::InvalidArchive | UpstreamError::VersionMismatch)
                ),
                "unexpected result: {result:?}"
            );
            assert!(!root.path().join("owned/156.0.8078.80").exists());
        }
    }

    #[test]
    fn entry_validation_rejects_unsafe_names() {
        for name in [
            "",
            "../x",
            "/x",
            "a/../b",
            "a//b",
            "a\\b",
            "a\0b",
            &"a".repeat(257),
        ] {
            assert!(validate_entry_name(name).is_err(), "{name:?}");
        }
        assert!(validate_entry_name("chromedriver-linux64/chromedriver").is_ok());
    }

    #[tokio::test]
    async fn concurrent_installers_serialize_on_the_store_lock() {
        let root = tempfile::tempdir().expect("root");
        let path = root.path().join("owned");
        ensure_store(&path).expect("owned store");
        let store = ChromeDriverStore::new(path.clone()).expect("store");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.join(STORE_LOCK_FILE))
            .expect("lock file");
        lock.lock_exclusive().expect("external lock");
        assert!(matches!(
            store.acquire_lock_for(Duration::from_millis(100)).await,
            Err(UpstreamError::Timeout)
        ));
        lock.unlock().expect("unlock");
        assert!(
            store
                .acquire_lock_for(Duration::from_millis(500))
                .await
                .is_ok()
        );
    }

    #[test]
    fn store_refuses_unowned_or_redirected_directories() {
        let root = tempfile::tempdir().expect("root");
        let personal = root.path().join("personal");
        fs::create_dir_all(&personal).unwrap();
        fs::write(personal.join("Preferences"), "{}").unwrap();
        assert_eq!(ensure_store(&personal), Err(UpstreamError::StoreNotOwned));
        let owned = root.path().join("owned");
        ensure_store(&owned).expect("owned");
        fs::create_dir_all(owned.join("156.0.0.0").join(platform().cft_name())).unwrap();
        ensure_store(&owned).expect("marker persists");
        #[cfg(unix)]
        {
            let link = root.path().join("link");
            std::os::unix::fs::symlink(&owned, &link).unwrap();
            assert_eq!(ensure_store(&link), Err(UpstreamError::StoreNotOwned));
        }
    }

    #[tokio::test]
    async fn legacy_layout_is_selected_for_old_milestones() {
        let version = "114.0.5735.90";
        let zip = zip_with(&[("chromedriver", &driver_bytes(version, false))]);
        let release_url = manifest::legacy_release_url(114);
        let download_url = manifest::legacy_download_url(version.parse().unwrap(), platform());
        let fetcher = Arc::new(StaticFetcher::new(response_map(&[
            (&release_url, b"114.0.5735.90\n".to_vec()),
            (&download_url, zip),
        ])));
        let root = tempfile::tempdir().expect("root");
        let store = ChromeDriverStore::with_fetcher(root.path().join("owned"), fetcher).unwrap();
        let installed = store
            .install(InstallRequest {
                milestone: Some(114),
                ..Default::default()
            })
            .await
            .expect("legacy install");
        assert_eq!(installed.version.to_string(), version);
    }
}
