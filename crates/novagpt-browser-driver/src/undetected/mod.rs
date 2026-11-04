//! Native Rust compatibility layer for the ChromeDriver/Selenium lifecycle
//! used by undetected-chromedriver 3.5.5 (upstream commit `757ed6a`).
//!
//! This module is an independent implementation. It does not contain or
//! translate upstream GPL-3.0 source. See
//! `docs/undetected-chromedriver-parity.md` for the source audit, parity
//! matrix, license analysis, and remaining gaps.
mod chrome;
mod error;
mod install;
mod managed;
mod manifest;
mod options;
mod patch;
mod platform;
mod profile;
mod service;
mod webdriver;

pub use chrome::{detect_chrome_version, detect_driver_version, discover_chrome};
pub use error::UpstreamError;
pub use install::{ChromeDriverStore, DriverFetcher, HttpFetcher, InstallRequest, InstalledDriver};
pub use managed::{DriverChoice, ManagedConfig, UndetectedChrome, manual_login, resolve_stable};

/// Spawn a child with a short retry for `ETXTBSY` ("text file busy"). On Unix,
/// a concurrent `fork` in another thread can briefly inherit an open write
/// descriptor for a freshly installed executable; exec then fails even though
/// the descriptor is close-on-exec. Retrying for a few hundred milliseconds is
/// preferable to surfacing a spurious startup failure. Real Chrome/ChromeDriver
/// binaries are not left open for writing, so this is a narrow race guard.
pub(crate) async fn spawn_with_retry(
    command: &mut tokio::process::Command,
) -> std::io::Result<tokio::process::Child> {
    for attempt in 0..10 {
        match command.spawn() {
            Ok(child) => return Ok(child),
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy && attempt < 9 => {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::from(std::io::ErrorKind::ExecutableFileBusy))
}
pub use manifest::{DriverRelease, allowed_download_url, allowed_fetch_url};
pub use options::{ChromeOptions, detected_language};
pub use patch::{PatchReport, PatchStatus, binary_version, is_patched, patch_driver};
pub use platform::{ChromeVersion, Platform, parse_launcher_output};
pub use service::ChromeDriverService;
pub use webdriver::{WebDriverClient, WebDriverSession, WebDriverTab, WebElement};
