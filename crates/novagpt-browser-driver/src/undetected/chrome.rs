//! Owned visible Chrome process for the managed ChromeDriver backend.
//!
//! NovaGPT launches Chrome itself so it can prove process ownership and keep
//! the profile lease. ChromeDriver later attaches through `debuggerAddress`;
//! killing ChromeDriver must never orphan or replace the browser. Only a
//! process spawned here is ever terminated.
use super::{
    error::UpstreamError,
    platform::{ChromeVersion, parse_launcher_output},
    profile::ProfileLease,
};
use serde_json::Value;
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};

const MAX_LAUNCHER_OUTPUT: usize = 16 * 1024;

pub(crate) struct OwnedChrome {
    child: Child,
    port: u16,
    _lease: ProfileLease,
}

impl OwnedChrome {
    pub(crate) async fn launch(
        executable: &Path,
        profile: &Path,
        arguments: Vec<String>,
        startup_timeout: Duration,
        lease: ProfileLease,
    ) -> Result<Self, UpstreamError> {
        if !executable.is_file() || profile.as_os_str().is_empty() || !profile.is_absolute() {
            return Err(UpstreamError::InvalidConfig);
        }
        let _ = tokio::fs::remove_file(profile.join("DevToolsActivePort")).await;
        let mut command = Command::new(executable);
        command
            .args(&arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = super::spawn_with_retry(&mut command)
            .await
            .map_err(|_| UpstreamError::Chrome)?;
        let endpoint = profile.join("DevToolsActivePort");
        let deadline = Instant::now() + startup_timeout;
        let port = loop {
            if let Ok(content) = tokio::fs::read_to_string(&endpoint).await
                && let Some(line) = content.lines().next()
                && let Ok(port) = line.parse::<u16>()
                && port != 0
            {
                break port;
            }
            if child
                .try_wait()
                .map_err(|_| UpstreamError::Chrome)?
                .is_some()
                || Instant::now() >= deadline
            {
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(3), child.wait()).await;
                return Err(UpstreamError::ChromeStartup);
            }
            sleep(Duration::from_millis(200)).await;
        };

        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .map_err(|_| UpstreamError::Protocol)?;
        loop {
            if healthy_browser(&client, port).await {
                break;
            }
            if child
                .try_wait()
                .map_err(|_| UpstreamError::Chrome)?
                .is_some()
                || Instant::now() >= deadline
            {
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(3), child.wait()).await;
                return Err(UpstreamError::ChromeStartup);
            }
            sleep(Duration::from_millis(200)).await;
        }
        Ok(Self {
            child,
            port,
            _lease: lease,
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    pub(crate) async fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Terminate only this owned Chrome. The profile lease is released when
    /// the process object is dropped, after the child has exited or been
    /// killed.
    pub(crate) async fn terminate(&mut self) {
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(5), self.child.wait()).await;
    }
}

async fn healthy_browser(client: &reqwest::Client, port: u16) -> bool {
    const MAX_VERSION_BYTES: usize = 16 * 1024;
    let Ok(mut response) = client
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .send()
        .await
    else {
        return false;
    };
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|length| length > MAX_VERSION_BYTES as u64)
    {
        return false;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() <= MAX_VERSION_BYTES - body.len() => {
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            _ => return false,
        }
    }
    serde_json::from_slice::<serde_json::Value>(&body)
        .is_ok_and(|value| valid_browser_endpoint(port, &value))
}

/// Detect the installed Chrome version from its own launcher. The child is
/// bounded and killed on timeout; its output is parsed in memory and never
/// logged.
/// Chrome installs each release in a versioned sibling directory
/// (`Application/<version>/chrome.exe` on Windows). That layout is
/// authoritative and needs no process launch. It is the only reliable source
/// on Windows, where the GUI launcher does not write `--version` output to a
/// parent console.
fn installed_layout_version(executable: &Path) -> Option<ChromeVersion> {
    let binary = if cfg!(windows) {
        "chrome.exe"
    } else {
        "chrome"
    };
    let parent = executable.parent()?; // A caller may pass the launcher inside the versioned directory.
    if let Some(name) = parent.file_name().and_then(|name| name.to_str())
        && let Ok(version) = name.parse::<ChromeVersion>()
        && executable.is_file()
    {
        return Some(version);
    }
    let mut best: Option<ChromeVersion> = None;
    for entry in std::fs::read_dir(parent).ok()? {
        let Ok(entry) = entry else {
            continue;
        };
        if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(version) = name.parse::<ChromeVersion>() else {
            continue;
        };
        // The launcher at the Application root delegates to this directory.
        // Windows installations keep the main module (`chrome.dll`) here and
        // only a signature for the launcher, so either marker proves the
        // directory is a real browser installation.
        if !(entry.path().join(binary).is_file() || entry.path().join("chrome.dll").is_file()) {
            continue;
        }
        best = Some(match best {
            Some(current) if current >= version => current,
            _ => version,
        });
    }
    best
}

pub async fn detect_chrome_version(executable: &Path) -> Result<ChromeVersion, UpstreamError> {
    if !executable.is_file() {
        return Err(UpstreamError::Chrome);
    }
    // Filesystem-only fast path: keep it off Tokio workers. It also avoids
    // launching a GUI browser just to ask for its version.
    let layout_path = executable.to_path_buf();
    if let Ok(Some(version)) =
        tokio::task::spawn_blocking(move || installed_layout_version(&layout_path)).await
    {
        return Ok(version);
    }
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = super::spawn_with_retry(&mut command)
        .await
        .map_err(|_| UpstreamError::Chrome)?;
    let output = timeout(Duration::from_secs(8), child.wait_with_output())
        .await
        .map_err(|_| UpstreamError::Timeout)?
        .map_err(|_| UpstreamError::Chrome)?;
    if output.stdout.len() > MAX_LAUNCHER_OUTPUT || output.stderr.len() > MAX_LAUNCHER_OUTPUT {
        return Err(UpstreamError::TooLarge);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_launcher_output(&stdout)
        .or_else(|| stderr.lines().find_map(parse_launcher_output))
        .ok_or(UpstreamError::Chrome)
}

/// Resolve the version of an explicitly supplied ChromeDriver. The binary
/// record is preferred; a bounded `--version` probe handles builds that no
/// longer embed the historical version record.
pub async fn detect_driver_version(executable: &Path) -> Result<ChromeVersion, UpstreamError> {
    if let Ok(version) = super::binary_version(executable) {
        return Ok(version);
    }
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = super::spawn_with_retry(&mut command)
        .await
        .map_err(|_| UpstreamError::Service)?;
    let output = timeout(Duration::from_secs(8), child.wait_with_output())
        .await
        .map_err(|_| UpstreamError::Timeout)?
        .map_err(|_| UpstreamError::Service)?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_launcher_output(&stdout)
        .or_else(|| stderr.lines().find_map(parse_launcher_output))
        .ok_or(UpstreamError::Service)
}

/// Discover an installed Chrome/Chromium executable, with an explicit
/// override. Does not spawn anything or inspect browser profile contents.
pub fn discover_chrome(explicit: Option<&Path>) -> Result<std::path::PathBuf, UpstreamError> {
    use std::path::PathBuf;
    if let Some(path) = explicit {
        return path
            .is_file()
            .then(|| path.to_path_buf())
            .ok_or(UpstreamError::InvalidConfig);
    }
    let candidates: &[&str] = if cfg!(windows) {
        &[
            "Google/Chrome/Application/chrome.exe",
            "Chromium/Application/chrome.exe",
        ]
    } else if cfg!(target_os = "macos") {
        &[
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ]
    } else {
        &[
            "/usr/bin/google-chrome",
            "/usr/bin/google-chrome-stable",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/opt/google/chrome/chrome",
        ]
    };
    if cfg!(windows) {
        for base in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
            if let Some(dir) = std::env::var_os(base) {
                for name in candidates {
                    let path = PathBuf::from(&dir).join(name);
                    if path.is_file() {
                        return Ok(path);
                    }
                }
            }
        }
    } else {
        for name in candidates {
            let path = PathBuf::from(name);
            if path.is_file() {
                return Ok(path);
            }
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            for name in [
                "google-chrome",
                "google-chrome-stable",
                "chromium",
                "chromium-browser",
            ] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
    }
    Err(UpstreamError::Chrome)
}

// A reused loopback port could point to a different service. Require the
// Chrome DevTools version response and an exact local browser websocket path
// before reusing a CDP endpoint. This supplements OS process ownership.
fn valid_browser_endpoint(port: u16, value: &Value) -> bool {
    let Some(browser) = value["Browser"].as_str() else {
        return false;
    };
    if !["Chrome/", "Chromium/", "HeadlessChrome/"]
        .iter()
        .any(|prefix| browser.starts_with(prefix))
    {
        return false;
    }
    let Some(websocket) = value["webSocketDebuggerUrl"].as_str() else {
        return false;
    };
    let local = format!("ws://127.0.0.1:{port}/devtools/browser/");
    let localhost = format!("ws://localhost:{port}/devtools/browser/");
    let Some(id) = websocket
        .strip_prefix(&local)
        .or_else(|| websocket.strip_prefix(&localhost))
    else {
        return false;
    };
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|ch| ch.is_ascii_hexdigit() || ch == b'-')
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    fn shell_fixture(body: &str) -> (tempfile::TempDir, PathBuf) {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/novagpt-test-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("chrome-")
            .tempdir_in(base)
            .unwrap();
        let path = directory.path().join("chrome-fixture");
        let mut script = std::fs::File::create(&path).unwrap();
        writeln!(script, "#!/bin/sh\n{body}").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (directory, path)
    }

    #[tokio::test]
    async fn detects_version_from_launcher_output() {
        let (_directory, executable) = shell_fixture("echo 'Google Chrome 156.0.8078.47' >&2");
        assert_eq!(
            detect_chrome_version(&executable).await.expect("version"),
            ChromeVersion::new(156, 0, 8078, 47)
        );
    }

    #[tokio::test]
    async fn driver_version_probe_handles_builds_without_the_legacy_marker() {
        let (_directory, executable) = shell_fixture(
            "echo 'ChromeDriver 156.0.8078.80 (fb096c0011f9d50f0a264a9376420cdea9c421e3-refs)'",
        );
        assert_eq!(
            detect_driver_version(&executable).await.expect("version"),
            ChromeVersion::new(156, 0, 8078, 80)
        );
    }

    #[tokio::test]
    async fn rejects_missing_or_non_browser_launchers() {
        assert!(matches!(
            detect_chrome_version(Path::new("/missing/chrome")).await,
            Err(UpstreamError::Chrome)
        ));
        let (_directory, executable) = shell_fixture("echo 'not a browser 1.2.3.4'");
        assert!(matches!(
            detect_chrome_version(&executable).await,
            Err(UpstreamError::Chrome)
        ));
    }
}

#[cfg(test)]
mod layout_tests {
    use super::*;

    fn binary_name() -> &'static str {
        if cfg!(windows) {
            "chrome.exe"
        } else {
            "chrome"
        }
    }

    /// Windows keeps `chrome.dll` in the version directory and only the
    /// signature of the launcher.
    fn layout_marker() -> &'static str {
        if cfg!(windows) {
            "chrome.dll"
        } else {
            "chrome"
        }
    }

    #[tokio::test]
    async fn version_is_read_from_the_versioned_install_layout_without_launching() {
        let root = tempfile::tempdir().expect("root");
        let application = root.path().join("Application");
        let version_directory = application.join("156.0.8078.80");
        std::fs::create_dir_all(&version_directory).expect("version directory");
        let launcher = application.join(binary_name());
        std::fs::write(&launcher, b"launcher").expect("launcher");
        std::fs::write(version_directory.join(layout_marker()), b"browser").expect("browser");
        assert_eq!(
            detect_chrome_version(&launcher)
                .await
                .expect("layout version"),
            ChromeVersion::new(156, 0, 8078, 80)
        );
        #[cfg(not(windows))]
        assert_eq!(
            detect_chrome_version(&version_directory.join(binary_name()))
                .await
                .expect("nested layout version"),
            ChromeVersion::new(156, 0, 8078, 80)
        );
    }

    #[tokio::test]
    async fn highest_installed_version_wins_and_unrelated_directories_are_ignored() {
        let root = tempfile::tempdir().expect("root");
        let application = root.path().join("Application");
        for version in ["140.0.1.2", "156.0.8078.80", "SetupMetrics"] {
            let directory = application.join(version);
            std::fs::create_dir_all(&directory).expect("directory");
            std::fs::write(directory.join(layout_marker()), b"browser").expect("browser");
        }
        // A versioned name without the browser inside is not an installation.
        std::fs::create_dir_all(application.join("200.0.0.1")).expect("incomplete version");
        let launcher = application.join(binary_name());
        std::fs::write(&launcher, b"launcher").expect("launcher");
        assert_eq!(
            detect_chrome_version(&launcher)
                .await
                .expect("highest version"),
            ChromeVersion::new(156, 0, 8078, 80)
        );
    }
}
