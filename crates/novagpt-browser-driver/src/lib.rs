//! Rust-native CDP browser automation for one dedicated, persistent Chrome profile.
//!
//! Owns browser lifecycle and general page operations, not ChatGPT selectors,
//! conversations, credentials or identity-provider policy.
mod config;
mod error;

pub use config::{DriverConfig, discover_chrome};
pub use error::DriverError;
