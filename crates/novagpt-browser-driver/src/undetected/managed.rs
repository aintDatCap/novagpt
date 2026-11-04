//! Managed Chrome + ChromeDriver lifecycle, equivalent in responsibility to
//! upstream's `Chrome` class: install/patch ChromeDriver, launch Chrome with a
//! dedicated profile, start the driver service, create a WebDriver session,
//! expose tabs, and recreate the service/session on `reconnect`.
//!
//! Automatic recovery never replays a command. `reconnect` creates a new
//! session and invalidates every old tab handle; a caller must re-find its tab
//! and decide whether an unsent action is safe to repeat.
use super::{
    ChromeDriverService, ChromeOptions, ChromeVersion, DriverRelease, InstallRequest,
    InstalledDriver, Platform, UpstreamError,
    chrome::{OwnedChrome, detect_chrome_version, detect_driver_version},
    install::ChromeDriverStore,
    patch::{binary_version, is_patched, patch_driver},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU16, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Mutex, RwLock};

const DEFAULT_STARTUP_TIMEOUT: Duration = Duration::from_secs(25);
const DEFAULT_SERVICE_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub enum DriverChoice {
    /// Download, verify, patch, and cache a matching ChromeDriver.
    Install(Arc<ChromeDriverStore>),
    /// Use an explicitly supplied ChromeDriver. It is still version-checked
    /// and patched by NovaGPT before execution.
    Existing(PathBuf),
}

#[derive(Clone)]
pub struct ManagedConfig {
    pub chrome_executable: PathBuf,
    pub profile: PathBuf,
    pub options: ChromeOptions,
    pub driver: DriverChoice,
    pub startup_timeout: Duration,
    pub service_timeout: Duration,
    pub command_timeout: Duration,
    pub enable_cdp_events: bool,
}

impl ManagedConfig {
    pub fn new(
        chrome_executable: impl Into<PathBuf>,
        profile: impl Into<PathBuf>,
        driver: DriverChoice,
    ) -> Self {
        Self {
            chrome_executable: chrome_executable.into(),
            profile: profile.into(),
            options: ChromeOptions::new(),
            driver,
            startup_timeout: DEFAULT_STARTUP_TIMEOUT,
            service_timeout: DEFAULT_SERVICE_TIMEOUT,
            command_timeout: DEFAULT_COMMAND_TIMEOUT,
            enable_cdp_events: false,
        }
    }
}

struct Inner {
    config: ManagedConfig,
    chrome: Mutex<Option<OwnedChrome>>,
    service: Mutex<Option<ChromeDriverService>>,
    session: RwLock<Option<super::WebDriverSession>>,
    lifecycle: Mutex<()>,
    epoch: AtomicU64,
    driver: InstalledDriver,
    chrome_version: ChromeVersion,
    debug_port: AtomicU16,
}

#[derive(Clone)]
pub struct UndetectedChrome {
    inner: Arc<Inner>,
}

impl UndetectedChrome {
    pub async fn launch(config: ManagedConfig) -> Result<Self, UpstreamError> {
        config.options.validate_profile(&config.profile)?;
        if !config.profile.is_absolute() {
            return Err(UpstreamError::InvalidConfig);
        }
        let profile = config.profile.clone();
        let lease = tokio::task::spawn_blocking(move || super::profile::acquire(&profile))
            .await
            .map_err(|_| UpstreamError::InvalidProfile)??;

        let options = config.options.clone();
        let profile = config.profile.clone();
        tokio::task::spawn_blocking(move || {
            options.handle_prefs(&profile)?;
            options.fix_exit_type(&profile)?;
            Ok::<(), UpstreamError>(())
        })
        .await
        .map_err(|_| UpstreamError::InvalidProfile)??;

        let executable = config.options.executable(&config.chrome_executable);
        let chrome_version = detect_chrome_version(&executable).await?;
        let driver = match &config.driver {
            DriverChoice::Install(store) => {
                store
                    .install(InstallRequest {
                        chrome_version: Some(chrome_version),
                        ..Default::default()
                    })
                    .await?
            }
            DriverChoice::Existing(path) => {
                explicit_driver(path, Platform::current()?, chrome_version).await?
            }
        };

        let language = super::detected_language();
        // Chrome writes the actual ephemeral port to `DevToolsActivePort`.
        // Requesting port 0 avoids a free-port race and matches the native
        // driver's proven launch path.
        let arguments = config
            .options
            .launch_arguments(&config.profile, 0, &language)?;
        let chrome = OwnedChrome::launch(
            &executable,
            &config.profile,
            arguments,
            config.startup_timeout,
            lease,
        )
        .await?;
        let debug_port = chrome.port();

        let mut service = match ChromeDriverService::start_with_timeout(
            &driver.path,
            config.service_timeout,
            config.command_timeout,
        )
        .await
        {
            Ok(service) => service,
            Err(error) => {
                let mut chrome = chrome;
                chrome.terminate().await;
                return Err(error);
            }
        };
        let session = match create_session(&config, &service, &executable, debug_port).await {
            Ok(session) => session,
            Err(error) => {
                service.stop().await;
                let mut chrome = chrome;
                chrome.terminate().await;
                return Err(error);
            }
        };
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                chrome: Mutex::new(Some(chrome)),
                service: Mutex::new(Some(service)),
                session: RwLock::new(Some(session)),
                lifecycle: Mutex::new(()),
                epoch: AtomicU64::new(1),
                driver,
                chrome_version,
                debug_port: AtomicU16::new(debug_port),
            }),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::Acquire)
    }

    pub fn driver_version(&self) -> ChromeVersion {
        self.inner.driver.version
    }

    pub fn chrome_version(&self) -> ChromeVersion {
        self.inner.chrome_version
    }

    pub fn driver_path(&self) -> &std::path::Path {
        &self.inner.driver.path
    }

    /// Loopback DevTools port of the owned Chrome. Only for local lifecycle
    /// code and tests; never serialize into an MCP response or log.
    pub fn debug_port(&self) -> u16 {
        self.inner.debug_port.load(Ordering::Acquire)
    }

    pub async fn status(&self) -> &'static str {
        let session_open = self
            .inner
            .session
            .read()
            .await
            .as_ref()
            .is_some_and(|session| !session.is_closed());
        let chrome_alive = {
            let mut guard = self.inner.chrome.lock().await;
            match guard.as_mut() {
                Some(chrome) => chrome.is_running().await,
                None => false,
            }
        };
        let service_alive = {
            let mut guard = self.inner.service.lock().await;
            match guard.as_mut() {
                Some(service) => service.is_running().await,
                None => false,
            }
        };
        match (chrome_alive, service_alive, session_open) {
            (true, true, true) => "attached",
            (true, _, _) => "disconnected",
            _ => "stopped",
        }
    }

    pub async fn session(&self) -> Result<super::WebDriverSession, UpstreamError> {
        self.inner
            .session
            .read()
            .await
            .clone()
            .ok_or(UpstreamError::SessionExpired)
    }

    pub async fn first_tab(&self) -> Result<super::WebDriverTab, UpstreamError> {
        self.session().await?.first_tab().await
    }

    /// Reuse the current session or reconnect when ChromeDriver has expired
    /// it. This performs only a session-level probe; it never retries a
    /// navigation, input or submission. The caller must re-find its tab and
    /// decide whether its last action was idempotent.
    pub async fn ensure_session(&self) -> Result<super::WebDriverSession, UpstreamError> {
        if let Ok(session) = self.session().await {
            match session.handles().await {
                Ok(_) => return Ok(session),
                Err(UpstreamError::SessionExpired | UpstreamError::TabClosed) => {}
                Err(error) => return Err(error),
            }
        }
        self.reconnect().await?;
        self.session().await
    }

    /// Open a new isolated tab in the owned browser. Each worker gets its own
    /// WebDriver window handle; commands are serialized per session and always
    /// switch to the caller's handle first.
    pub async fn new_tab(&self, url: &str) -> Result<super::WebDriverTab, UpstreamError> {
        let tab = self.session().await?.new_tab().await?;
        if !url.is_empty()
            && let Err(error) = tab.goto(url).await
        {
            let _ = tab.close().await;
            return Err(error);
        }
        Ok(tab)
    }

    pub async fn performance_log(&self) -> Result<Vec<Value>, UpstreamError> {
        self.session().await?.performance_log().await
    }

    pub async fn drain_cdp_events(&self) -> Result<Vec<Value>, UpstreamError> {
        self.session().await?.drain_cdp_events().await
    }

    /// Recreate the ChromeDriver service and WebDriver session. If the owned
    /// Chrome process is still alive it is reused; if it exited, a new Chrome
    /// is launched with the same dedicated profile. This extends upstream's
    /// service-only `reconnect` so a Chrome crash does not strand every worker.
    ///
    /// The old session and every tab handle are invalidated. No navigation,
    /// input, or prompt is replayed. A caller must re-find its tab and decide
    /// whether its last action was idempotent.
    pub async fn reconnect(&self) -> Result<u64, UpstreamError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        // Advance the epoch before changing any state: a failed reconnect
        // still invalidates the old session/tabs and must be observable.
        let epoch = self.inner.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let old_session = self.inner.session.write().await.take();
        if let Some(old_session) = old_session {
            old_session.invalidate();
        }
        let old_service = self.inner.service.lock().await.take();
        if let Some(mut old_service) = old_service {
            old_service.stop().await;
        }

        let executable = self
            .inner
            .config
            .options
            .executable(&self.inner.config.chrome_executable);
        let (dead_chrome, need_launch) = {
            let mut guard = self.inner.chrome.lock().await;
            let alive = match guard.as_mut() {
                Some(chrome) => chrome.is_running().await,
                None => false,
            };
            if alive {
                (None, false)
            } else {
                (guard.take(), true)
            }
        };
        if need_launch {
            if let Some(mut dead_chrome) = dead_chrome {
                dead_chrome.terminate().await;
                drop(dead_chrome);
            }
            let profile = self.inner.config.profile.clone();
            let lease = tokio::task::spawn_blocking({
                let profile = profile.clone();
                move || super::profile::acquire(&profile)
            })
            .await
            .map_err(|_| UpstreamError::InvalidProfile)??;
            let options = self.inner.config.options.clone();
            let prefs_profile = profile.clone();
            tokio::task::spawn_blocking(move || {
                options.handle_prefs(&prefs_profile)?;
                options.fix_exit_type(&prefs_profile)?;
                Ok::<(), UpstreamError>(())
            })
            .await
            .map_err(|_| UpstreamError::InvalidProfile)??;
            let language = super::detected_language();
            let arguments = self
                .inner
                .config
                .options
                .launch_arguments(&profile, 0, &language)?;
            let chrome = OwnedChrome::launch(
                &executable,
                &profile,
                arguments,
                self.inner.config.startup_timeout,
                lease,
            )
            .await?;
            self.inner
                .debug_port
                .store(chrome.port(), Ordering::Release);
            *self.inner.chrome.lock().await = Some(chrome);
        }
        // Upstream sleeps briefly between stop and start; the new service also
        // uses a fresh loopback port, so this only lets the old child reap.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut service = ChromeDriverService::start_with_timeout(
            &self.inner.driver.path,
            self.inner.config.service_timeout,
            self.inner.config.command_timeout,
        )
        .await?;
        let session = match create_session(
            &self.inner.config,
            &service,
            &executable,
            self.inner.debug_port.load(Ordering::Acquire),
        )
        .await
        {
            Ok(session) => session,
            Err(error) => {
                service.stop().await;
                return Err(error);
            }
        };
        *self.inner.service.lock().await = Some(service);
        *self.inner.session.write().await = Some(session);
        Ok(epoch)
    }

    /// Close the WebDriver session, stop the owned driver child, and terminate
    /// the owned Chrome. Idempotent.
    pub async fn shutdown(&self) {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.inner.epoch.fetch_add(1, Ordering::AcqRel);
        if let Some(session) = self.inner.session.write().await.take() {
            let _ = session.close().await;
        }
        if let Some(mut service) = self.inner.service.lock().await.take() {
            service.stop().await;
        }
        if let Some(mut chrome) = self.inner.chrome.lock().await.take() {
            chrome.terminate().await;
        }
    }

    /// Build an `UndetectedChrome` around an existing WebDriver session for
    /// deterministic adapter tests. It owns no Chrome process or driver
    /// service and must never be used in production.
    #[cfg(feature = "test-support")]
    pub fn from_session_for_test(session: super::WebDriverSession) -> Self {
        let placeholder = std::path::PathBuf::from("/nonexistent/novagpt-test");
        let platform = Platform::current().unwrap_or(Platform::Linux64);
        Self {
            inner: Arc::new(Inner {
                config: ManagedConfig::new(
                    &placeholder,
                    "/nonexistent/novagpt-test-profile",
                    DriverChoice::Existing(placeholder.clone()),
                ),
                chrome: Mutex::new(None),
                service: Mutex::new(None),
                session: RwLock::new(Some(session)),
                lifecycle: Mutex::new(()),
                epoch: AtomicU64::new(1),
                driver: InstalledDriver {
                    path: placeholder,
                    version: ChromeVersion::new(156, 0, 0, 0),
                    platform,
                    binary_sha256: [0; 32],
                    freshly_installed: false,
                },
                chrome_version: ChromeVersion::new(156, 0, 0, 0),
                debug_port: AtomicU16::new(0),
            }),
        }
    }

    #[cfg(feature = "test-support")]
    pub async fn simulate_driver_crash(&self) -> Result<(), UpstreamError> {
        let mut service = self.inner.service.lock().await;
        let service = service.as_mut().ok_or(UpstreamError::Service)?;
        service.stop().await;
        Ok(())
    }

    #[cfg(feature = "test-support")]
    pub async fn simulate_chrome_crash(&self) -> Result<(), UpstreamError> {
        let mut chrome = self.inner.chrome.lock().await;
        let chrome = chrome.as_mut().ok_or(UpstreamError::Chrome)?;
        chrome.terminate().await;
        Ok(())
    }
}

async fn explicit_driver(
    path: &std::path::Path,
    platform: Platform,
    chrome_version: ChromeVersion,
) -> Result<InstalledDriver, UpstreamError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| UpstreamError::InvalidConfig)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(UpstreamError::InvalidConfig);
    }
    let version = match binary_version(path) {
        Ok(version) => version,
        Err(_) => detect_driver_version(path).await?,
    };
    if !version.same_milestone(chrome_version) {
        return Err(UpstreamError::VersionMismatch);
    }
    if !is_patched(path) {
        // Explicit user binaries are accepted only when NovaGPT can patch
        // them itself. A foreign (upstream) patch is refused rather than
        // executed with unknown provenance.
        patch_driver(path)?;
    }
    let bytes = std::fs::read(path).map_err(|_| UpstreamError::Io)?;
    let digest: [u8; 32] = Sha256::digest(&bytes).into();
    Ok(InstalledDriver {
        path: path.to_path_buf(),
        version,
        platform,
        binary_sha256: digest,
        freshly_installed: false,
    })
}

async fn create_session(
    config: &ManagedConfig,
    service: &ChromeDriverService,
    executable: &std::path::Path,
    debug_port: u16,
) -> Result<super::WebDriverSession, UpstreamError> {
    let language = super::detected_language();
    let args = config
        .options
        .launch_arguments(&config.profile, debug_port, &language)?;
    let chrome_options = json!({
        "binary": executable,
        "args": args,
        "debuggerAddress": format!("127.0.0.1:{debug_port}"),
    });
    let mut capabilities = json!({
        "browserName": "chrome",
        "goog:chromeOptions": chrome_options,
        // Browser logs power the dedicated browsing console. They stay in
        // memory, are redacted, and are never written to audit records.
        "goog:loggingPrefs": {"browser":"ALL"},
    });
    if config.enable_cdp_events {
        capabilities["goog:loggingPrefs"] = json!({"performance":"ALL","browser":"ALL"});
    }
    let session = super::WebDriverSession::create(service.client(), capabilities).await?;
    session.set_timeouts(30_000, 30_000, 0).await?;
    Ok(session)
}

/// Open ordinary Chrome for manual sign-in using the dedicated profile.
/// No remote debugging, CDP, WebDriver, or automation flags are enabled.
pub async fn manual_login(
    executable: &std::path::Path,
    profile: &std::path::Path,
    home: &str,
) -> Result<(), UpstreamError> {
    if !executable.is_file()
        || profile.as_os_str().is_empty()
        || !profile.is_absolute()
        || !(home == "about:blank"
            || (home.len() <= 2048
                && home.starts_with("https://")
                && !home.contains(char::is_control)))
    {
        return Err(UpstreamError::InvalidConfig);
    }
    let profile_owned = profile.to_path_buf();
    let lease = tokio::task::spawn_blocking(move || super::profile::acquire(&profile_owned))
        .await
        .map_err(|_| UpstreamError::InvalidProfile)??;
    let mut command = manual_login_command(executable, profile, home);
    let mut child = super::spawn_with_retry(&mut command)
        .await
        .map_err(|_| UpstreamError::Chrome)?;
    let status = child.wait().await.map_err(|_| UpstreamError::Chrome)?;
    drop(lease);
    if !status.success() {
        return Err(UpstreamError::Chrome);
    }
    Ok(())
}

fn manual_login_command(
    executable: &std::path::Path,
    profile: &std::path::Path,
    home: &str,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(executable);
    command
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--new-window",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    command
}

/// Resolve the latest Stable driver without a browser. Used by the CLI
/// management command and parity tests; normal agent launches match Chrome.
pub async fn resolve_stable(store: &ChromeDriverStore) -> Result<DriverRelease, UpstreamError> {
    store.latest_stable().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn shell_fixture(body: &str) -> (tempfile::TempDir, PathBuf) {
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/novagpt-test-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("managed-")
            .tempdir_in(base)
            .unwrap();
        let path = directory.path().join("fixture");
        let mut script = std::fs::File::create(&path).unwrap();
        writeln!(script, "#!/bin/sh\n{body}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (directory, path)
    }

    fn driver_bytes(version: &str, patched: bool) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x7fELF-novagpt\nplatform_handle\x00content\x00");
        bytes.extend_from_slice(version.as_bytes());
        bytes.extend_from_slice(b"\x00\n");
        bytes.extend_from_slice(if patched {
            b"{/*novagpt-chromedriver-patch*/}".as_slice()
        } else {
            b"{window.cdc_fixture = true;}                ".as_slice()
        });
        bytes
    }

    #[test]
    fn manual_login_uses_no_automation_or_remote_debugging_flags() {
        let command = manual_login_command(
            std::path::Path::new("chrome"),
            std::path::Path::new("/tmp/novagpt-dedicated"),
            "https://chatgpt.com/",
        );
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg.starts_with("--user-data-dir=")));
        assert!(args.iter().any(|arg| arg == "https://chatgpt.com/"));
        for denied in [
            "--remote-debugging-port",
            "--remote-debugging-address",
            "--headless",
            "--enable-automation",
            "--disable-web-security",
            "--ignore-certificate-errors",
        ] {
            assert!(!args.iter().any(|arg| arg.starts_with(denied)));
        }
    }

    #[tokio::test]
    async fn mismatched_explicit_driver_is_rejected_before_browser_launch() {
        let (_chrome_dir, chrome) = shell_fixture("echo 'Google Chrome 156.0.8078.47'");
        let (_driver_dir, driver) = shell_fixture("");
        std::fs::write(&driver, driver_bytes("155.0.1.1", true)).unwrap();
        let profile = tempfile::tempdir().unwrap().path().join("profile");
        let config = ManagedConfig::new(chrome, profile, DriverChoice::Existing(driver));
        assert!(matches!(
            UndetectedChrome::launch(config).await,
            Err(UpstreamError::VersionMismatch)
        ));
    }

    #[tokio::test]
    async fn duplicate_profile_launch_is_rejected_without_touching_the_owner() {
        let (_chrome_dir, chrome) = shell_fixture("echo 'Google Chrome 156.0.8078.47'");
        let (_driver_dir, driver) = shell_fixture("");
        std::fs::write(&driver, driver_bytes("156.0.8078.47", true)).unwrap();
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        let lease = crate::undetected::profile::acquire(&profile).expect("first lease");
        let config = ManagedConfig::new(chrome, profile, DriverChoice::Existing(driver));
        assert!(matches!(
            UndetectedChrome::launch(config).await,
            Err(UpstreamError::ProfileBusy)
        ));
        drop(lease);
    }
}
