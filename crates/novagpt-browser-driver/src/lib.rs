//! Rust-native browser automation for one dedicated, persistent Chrome profile.
//!
//! Chrome process ownership, ChromeDriver installation/patching, the W3C
//! WebDriver session, tabs, and input all live in `undetected`. The ChatGPT
//! adapter in `novagpt-browser` is the only consumer; this crate contains no
//! provider selectors, credentials, or conversation policy.
pub mod undetected;

pub use undetected::discover_chrome;
