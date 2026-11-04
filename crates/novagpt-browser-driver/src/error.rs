use thiserror::Error;

/// Stable, redacted failure messages: no raw CDP payloads, DOM text, URLs,
/// browser profile contents, credentials or user-supplied input.
#[derive(Debug, Error)]
pub enum DriverError {
    #[error("Chrome is unavailable or disconnected")]
    NotRunning,
    #[error("Browser driver configuration is invalid")]
    InvalidConfig,
    #[error("Dedicated browser profile is invalid")]
    InvalidProfile,
    #[error("Dedicated browser profile is already in use")]
    ProfileBusy,
    #[error("Browser operation timed out")]
    Timeout,
    #[error("Browser operation was cancelled")]
    Cancelled,
    #[error("CDP protocol operation failed")]
    Protocol,
    #[error("Browser page is missing or was invalidated")]
    StalePage,
    #[error("Browser control is invalid, hidden or disabled")]
    InvalidControl,
    #[error("Browser operation exceeded its size limit")]
    TooLarge,
}
