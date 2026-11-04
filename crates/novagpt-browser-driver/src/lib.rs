//! Rust-native CDP browser automation for one dedicated, persistent Chrome profile.
//!
//! Owns browser lifecycle and general page operations, not ChatGPT selectors,
//! conversations, credentials or identity-provider policy.
mod config;
mod error;
mod lifecycle;
mod profile;

pub use config::{DriverConfig, discover_chrome};
pub use error::DriverError;
pub use lifecycle::{BrowserDriver, TargetInfo};
