use crate::{
    DriverConfig, DriverError,
    profile::{self, ProfileLease},
};
use serde_json::Value;
use std::{
    path::Path,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    fs,
    process::{Child, Command},
    sync::Mutex,
    time::{Duration, Instant, sleep, timeout},
};

struct Inner {
    config: DriverConfig,
    http: reqwest::Client,
    process: Mutex<Option<Child>>,
    port: Mutex<Option<u16>>,
    lease: Mutex<Option<ProfileLease>>,
    startup: Mutex<()>,
    creation: Mutex<()>,
    last_creation: Mutex<Option<Instant>>,
    epoch: AtomicU64,
}

/// One owned visible Chrome instance per dedicated profile. Clones share
/// process ownership and serialize only launch and new target allocation.
#[derive(Clone)]
pub struct BrowserDriver {
    inner: Arc<Inner>,
}

#[derive(Clone, Debug)]
pub struct TargetInfo {
    pub id: String,
    pub websocket: String,
    pub epoch: u64,
}

impl BrowserDriver {
    pub fn new(config: DriverConfig) -> Result<Self, DriverError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(config.command_timeout)
            .build()
            .map_err(|_| DriverError::Protocol)?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                http,
                process: Mutex::new(None),
                port: Mutex::new(None),
                lease: Mutex::new(None),
                startup: Mutex::new(()),
                creation: Mutex::new(()),
                last_creation: Mutex::new(None),
                epoch: AtomicU64::new(1),
            }),
        })
    }

    pub fn config(&self) -> &DriverConfig {
        &self.inner.config
    }

    pub fn epoch(&self) -> u64 {
        self.inner.epoch.load(Ordering::Acquire)
    }

    /// The requested Chrome instance is either healthy or relaunched with
    /// its existing dedicated profile. No tab or prompt is replayed on restart.
    pub async fn ensure_started(&self) -> Result<u16, DriverError> {
        let _startup = self.inner.startup.lock().await;
        if let Some(port) = *self.inner.port.lock().await {
            let alive = self
                .inner
                .process
                .lock()
                .await
                .as_mut()
                .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
            if alive && self.healthy(port).await {
                return Ok(port);
            }
            self.invalidate().await;
        }

        let profile = self.inner.config.profile.clone();
        let lease = tokio::task::spawn_blocking(move || profile::acquire(&profile))
            .await
            .map_err(|_| DriverError::InvalidProfile)??;
        // Only after acquiring the profile lock can we clear our own stale
        // DevToolsActivePort file; never touch a profile owned by another process.
        let _ = fs::remove_file(self.inner.config.profile.join("DevToolsActivePort")).await;
        let mut child = Command::new(&self.inner.config.executable)
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "--new-window",
            ])
            .arg(format!(
                "--user-data-dir={}",
                self.inner.config.profile.display()
            ))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| DriverError::NotRunning)?;
        let endpoint = self.inner.config.profile.join("DevToolsActivePort");
        let deadline = Instant::now() + self.inner.config.startup_timeout;
        let port = loop {
            if let Ok(content) = fs::read_to_string(&endpoint).await
                && let Some(line) = content.lines().next()
                && let Ok(port) = line.parse::<u16>()
                && port != 0
            {
                break port;
            }
            if child
                .try_wait()
                .map_err(|_| DriverError::NotRunning)?
                .is_some()
                || Instant::now() >= deadline
            {
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(3), child.wait()).await;
                return Err(DriverError::NotRunning);
            }
            sleep(Duration::from_millis(200)).await;
        };
        if !self.healthy(port).await {
            let _ = child.start_kill();
            let _ = timeout(Duration::from_secs(3), child.wait()).await;
            return Err(DriverError::NotRunning);
        }
        *self.inner.lease.lock().await = Some(lease);
        *self.inner.process.lock().await = Some(child);
        *self.inner.port.lock().await = Some(port);
        Ok(port)
    }

    async fn healthy(&self, port: u16) -> bool {
        self.inner
            .http
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }

    async fn invalidate(&self) {
        self.inner.epoch.fetch_add(1, Ordering::AcqRel);
        self.inner.port.lock().await.take();
        if let Some(mut child) = self.inner.process.lock().await.take() {
            let _ = child.start_kill();
            let _ = timeout(Duration::from_secs(3), child.wait()).await;
        }
        self.inner.lease.lock().await.take();
        self.inner.last_creation.lock().await.take();
    }

    /// Shut down only Chrome launched by this driver, not another user process.
    /// Old tab handles remain invalidated after shutdown.
    pub async fn shutdown(&self) {
        let _startup = self.inner.startup.lock().await;
        self.invalidate().await;
    }

    /// Allocate a new blank target. The caller explicitly navigates after
    /// attaching, avoiding races between target creation and CDP activation.
    pub async fn new_target(&self) -> Result<TargetInfo, DriverError> {
        let _creation = self.inner.creation.lock().await;
        let port = self.ensure_started().await?;
        if let Some(last) = *self.inner.last_creation.lock().await {
            let elapsed = last.elapsed();
            if elapsed < self.inner.config.creation_interval {
                sleep(self.inner.config.creation_interval - elapsed).await;
            }
        }
        let response = self
            .inner
            .http
            .put(format!("http://127.0.0.1:{port}/json/new?about:blank"))
            .send()
            .await
            .map_err(|_| DriverError::NotRunning)?;
        if !response.status().is_success() {
            return Err(DriverError::Protocol);
        }
        if response.content_length().is_some_and(|len| len > 64 * 1024) {
            return Err(DriverError::TooLarge);
        }
        let bytes = response.bytes().await.map_err(|_| DriverError::Protocol)?;
        if bytes.len() > 64 * 1024 {
            return Err(DriverError::TooLarge);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| DriverError::Protocol)?;
        let id = value["id"].as_str().ok_or(DriverError::Protocol)?;
        let ws = value["webSocketDebuggerUrl"]
            .as_str()
            .ok_or(DriverError::Protocol)?;
        if !valid_target_id(id)
            || !(ws == format!("ws://127.0.0.1:{port}/devtools/page/{id}")
                || ws == format!("ws://localhost:{port}/devtools/page/{id}"))
        {
            return Err(DriverError::Protocol);
        }
        *self.inner.last_creation.lock().await = Some(Instant::now());
        Ok(TargetInfo {
            id: id.to_owned(),
            websocket: ws.to_owned(),
            epoch: self.epoch(),
        })
    }

    pub async fn close_target(&self, target_id: &str) -> Result<(), DriverError> {
        if !valid_target_id(target_id) {
            return Err(DriverError::InvalidControl);
        }
        let Some(port) = *self.inner.port.lock().await else {
            return Err(DriverError::StalePage);
        };
        let response = self
            .inner
            .http
            .get(format!("http://127.0.0.1:{port}/json/close/{target_id}"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map_err(|_| DriverError::NotRunning)?;
        if !response.status().is_success() {
            return Err(DriverError::Protocol);
        }
        Ok(())
    }

    /// Manual login is explicitly unaided and uses the same NovaGPT-owned
    /// profile without CDP, WebDriver, or automation flags.
    pub async fn manual_login(
        executable: &Path,
        profile: &Path,
        home: &str,
    ) -> Result<(), DriverError> {
        let config = DriverConfig::new(executable, profile)?;
        let owned = config.profile.clone();
        let _lease = tokio::task::spawn_blocking(move || profile::acquire(&owned))
            .await
            .map_err(|_| DriverError::InvalidProfile)??;
        let mut child = Command::new(&config.executable);
        child
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--new-window",
            ])
            .arg(format!("--user-data-dir={}", config.profile.display()))
            .arg(home)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let status = child
            .spawn()
            .map_err(|_| DriverError::NotRunning)?
            .wait()
            .await
            .map_err(|_| DriverError::NotRunning)?;
        if !status.success() {
            return Err(DriverError::NotRunning);
        }
        Ok(())
    }
}

pub(crate) fn valid_target_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_hex_target_ids() {
        assert!(valid_target_id("a012BC"));
        for input in ["", "../../admin", "id with space", "g", &"a".repeat(65)] {
            assert!(!valid_target_id(input));
        }
    }
    #[tokio::test]
    async fn shutdown_does_not_start_unconfigured_chrome() {
        let binary = tempfile::NamedTempFile::new().expect("binary");
        let temp = tempfile::tempdir().expect("profile");
        let driver = BrowserDriver::new(
            DriverConfig::new(binary.path(), temp.path().join("owned")).expect("config"),
        )
        .expect("driver");
        driver.shutdown().await;
        assert_eq!(driver.epoch(), 2);
        assert!(!temp.path().join("owned").exists());
    }
}
