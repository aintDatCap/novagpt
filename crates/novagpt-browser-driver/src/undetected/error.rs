//! Stable, redacted failures for the managed Chrome/ChromeDriver lifecycle.
//!
//! No variant carries a URL, DOM value, prompt, cookie, profile path or raw
//! remote message. Callers receive a category and can decide whether an
//! action is safe to retry; prompt replay policy belongs to the caller.
use thiserror::Error;

#[derive(Debug, Error, Clone, Copy, Eq, PartialEq)]
pub enum UpstreamError {
    #[error("ChromeDriver management is unavailable on this platform")]
    UnsupportedPlatform,
    #[error("ChromeDriver configuration is invalid")]
    InvalidConfig,
    #[error("ChromeDriver release metadata is invalid or unavailable")]
    Manifest,
    #[error("ChromeDriver download failed")]
    Download,
    #[error("ChromeDriver download exceeded its size limit")]
    TooLarge,
    #[error("ChromeDriver archive is invalid or unsafe")]
    InvalidArchive,
    #[error("ChromeDriver integrity check failed")]
    Integrity,
    #[error("ChromeDriver version does not match the requested browser milestone")]
    VersionMismatch,
    #[error("ChromeDriver binary cannot be patched safely")]
    Unpatchable,
    #[error("ChromeDriver binary was patched by an unknown implementation")]
    ForeignPatch,
    #[error("ChromeDriver installation directory is not owned by NovaGPT")]
    StoreNotOwned,
    #[error("ChromeDriver service failed to start")]
    Service,
    #[error("ChromeDriver service did not become ready before the deadline")]
    ServiceTimeout,
    #[error("WebDriver request was rejected")]
    WebDriver,
    #[error("WebDriver session is invalid or expired")]
    SessionExpired,
    #[error("WebDriver tab is closed or no longer exists")]
    TabClosed,
    #[error("Chrome browser is unavailable")]
    Chrome,
    #[error("Chrome browser did not expose a healthy loopback DevTools endpoint")]
    ChromeStartup,
    #[error("Dedicated Chrome profile is invalid")]
    InvalidProfile,
    #[error("Dedicated Chrome profile is already in use")]
    ProfileBusy,
    #[error("Browser operation was cancelled")]
    Cancelled,
    #[error("Browser operation timed out")]
    Timeout,
    #[error("Remote response was invalid or exceeded its size limit")]
    Protocol,
    #[error("Managed browser file operation failed")]
    Io,
}
