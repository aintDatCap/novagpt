//! Chrome for Testing and legacy ChromeDriver release metadata.
//!
//! Endpoint layout and JSON field names are documented public interfaces:
//! <https://googlechromelabs.github.io/chrome-for-testing/> and
//! <https://chromedriver.chromium.org/downloads>. Only bounded fields are
//! parsed; no response text is surfaced in errors.
use super::{
    error::UpstreamError,
    platform::{ChromeVersion, Platform},
};
use serde_json::Value;

pub const CFT_STABLE_URL: &str = "https://googlechromelabs.github.io/chrome-for-testing/last-known-good-versions-with-downloads.json";
pub const CFT_MILESTONES_URL: &str = "https://googlechromelabs.github.io/chrome-for-testing/latest-versions-per-milestone-with-downloads.json";
pub const CFT_DOWNLOAD_BASE: &str = "https://storage.googleapis.com/chrome-for-testing-public";
pub const LEGACY_RELEASE_BASE: &str = "https://chromedriver.storage.googleapis.com";

const MAX_MANIFEST_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverRelease {
    pub version: ChromeVersion,
    pub url: String,
    pub legacy: bool,
}

/// Resolve the current Stable Chrome version. Upstream uses this when no
/// browser milestone is known; NovaGPT normally matches the installed Chrome
/// milestone instead.
pub fn parse_stable(manifest: &[u8]) -> Result<ChromeVersion, UpstreamError> {
    if manifest.is_empty() || manifest.len() > MAX_MANIFEST_BYTES {
        return Err(UpstreamError::Manifest);
    }
    let value: Value = serde_json::from_slice(manifest).map_err(|_| UpstreamError::Manifest)?;
    value["channels"]["Stable"]["version"]
        .as_str()
        .ok_or(UpstreamError::Manifest)?
        .parse()
}

pub fn parse_milestone(manifest: &[u8], milestone: u32) -> Result<ChromeVersion, UpstreamError> {
    if manifest.is_empty() || manifest.len() > MAX_MANIFEST_BYTES || milestone == 0 {
        return Err(UpstreamError::Manifest);
    }
    let value: Value = serde_json::from_slice(manifest).map_err(|_| UpstreamError::Manifest)?;
    let entry = value["milestones"]
        .get(milestone.to_string())
        .ok_or(UpstreamError::Manifest)?;
    let version: ChromeVersion = entry["version"]
        .as_str()
        .ok_or(UpstreamError::Manifest)?
        .parse()?;
    if version.milestone() != milestone {
        return Err(UpstreamError::VersionMismatch);
    }
    Ok(version)
}

/// Parse the plain-text response body from `LATEST_RELEASE_<milestone>`.
pub fn parse_legacy_release(body: &[u8], milestone: u32) -> Result<ChromeVersion, UpstreamError> {
    if body.is_empty() || body.len() > 256 || milestone == 0 {
        return Err(UpstreamError::Manifest);
    }
    let version: ChromeVersion = std::str::from_utf8(body)
        .map_err(|_| UpstreamError::Manifest)?
        .trim()
        .parse()?;
    if version.milestone() != milestone || !version.uses_legacy_layout() {
        return Err(UpstreamError::VersionMismatch);
    }
    Ok(version)
}

pub fn cft_download_url(version: ChromeVersion, platform: Platform) -> String {
    format!(
        "{CFT_DOWNLOAD_BASE}/{version}/{platform}/chromedriver-{platform}.zip",
        platform = platform.cft_name()
    )
}

pub fn legacy_release_url(milestone: u32) -> String {
    format!("{LEGACY_RELEASE_BASE}/LATEST_RELEASE_{milestone}")
}

pub fn legacy_download_url(version: ChromeVersion, platform: Platform) -> String {
    format!(
        "{LEGACY_RELEASE_BASE}/{version}/chromedriver_{}.zip",
        platform.legacy_name()
    )
}

/// The archive entry that must contain the executable, and the expected
/// archive layout for the requested release.
pub fn archive_binary_path(version: ChromeVersion, platform: Platform) -> String {
    if version.uses_legacy_layout() {
        platform.legacy_archive_binary_path().to_owned()
    } else {
        platform.archive_binary_path()
    }
}

pub fn release_for(
    version: ChromeVersion,
    platform: Platform,
) -> Result<DriverRelease, UpstreamError> {
    if version.major == 0 {
        return Err(UpstreamError::Manifest);
    }
    Ok(DriverRelease {
        version,
        url: if version.uses_legacy_layout() {
            legacy_download_url(version, platform)
        } else {
            cft_download_url(version, platform)
        },
        legacy: version.uses_legacy_layout(),
    })
}

/// Validate that a URL is one of the two ChromeDriver distribution hosts over
/// HTTPS. Download code additionally refuses redirects, so a compromised or
/// misconfigured manifest cannot redirect an executable fetch elsewhere.
pub fn allowed_fetch_url(url: &str) -> bool {
    if allowed_download_url(url) {
        return true;
    }
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    parsed.scheme() == "https"
        && parsed.host_str() == Some("googlechromelabs.github.io")
        && parsed.username().is_empty()
        && parsed.password().is_none()
        && parsed.query().is_none()
        && parsed.fragment().is_none()
        && parsed.path().starts_with("/chrome-for-testing/")
        && parsed.path().ends_with(".json")
}

pub fn allowed_download_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return false;
    }
    matches!(
        parsed.host_str(),
        Some("storage.googleapis.com" | "chromedriver.storage.googleapis.com")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STABLE: &[u8] =
        br#"{"channels":{"Stable":{"version":"157.0.8103.38"},"Beta":{"version":"158.0.0.1"}}}"#;
    const MILESTONES: &[u8] =
        br#"{"milestones":{"156":{"version":"156.0.8078.80"},"157":{"version":"157.0.8103.38"}}}"#;

    #[test]
    fn parses_bounded_metadata_and_rejects_mismatches() {
        assert_eq!(
            parse_stable(STABLE).expect("stable"),
            ChromeVersion::new(157, 0, 8103, 38)
        );
        assert_eq!(
            parse_milestone(MILESTONES, 156).expect("milestone"),
            ChromeVersion::new(156, 0, 8078, 80)
        );
        assert_eq!(
            parse_milestone(MILESTONES, 155),
            Err(UpstreamError::Manifest)
        );
        assert_eq!(parse_milestone(MILESTONES, 0), Err(UpstreamError::Manifest));
        assert_eq!(
            parse_milestone(
                b"{\"milestones\":{\"156\":{\"version\":\"157.0.0.0\"}}}",
                156
            ),
            Err(UpstreamError::VersionMismatch)
        );
        assert_eq!(parse_stable(b"{}"), Err(UpstreamError::Manifest));
        assert_eq!(parse_stable(b"not-json"), Err(UpstreamError::Manifest));
    }

    #[test]
    fn legacy_release_text_is_strict() {
        assert_eq!(
            parse_legacy_release(b"114.0.5735.90\n", 114).expect("legacy"),
            ChromeVersion::new(114, 0, 5735, 90)
        );
        assert_eq!(
            parse_legacy_release(b"114.0.5735.90", 115),
            Err(UpstreamError::VersionMismatch)
        );
        assert_eq!(
            parse_legacy_release(b"115.0.0.0", 115),
            Err(UpstreamError::VersionMismatch)
        );
        assert_eq!(parse_legacy_release(b"", 114), Err(UpstreamError::Manifest));
    }

    #[test]
    fn download_urls_are_exact_and_https_only() {
        let modern = cft_download_url(ChromeVersion::new(156, 0, 8078, 80), Platform::Linux64);
        assert_eq!(
            modern,
            "https://storage.googleapis.com/chrome-for-testing-public/156.0.8078.80/linux64/chromedriver-linux64.zip"
        );
        assert!(allowed_download_url(&modern));
        let legacy = legacy_download_url(ChromeVersion::new(114, 0, 5735, 90), Platform::Linux64);
        assert_eq!(
            legacy,
            "https://chromedriver.storage.googleapis.com/114.0.5735.90/chromedriver_linux64.zip"
        );
        assert!(allowed_download_url(&legacy));
        for denied in [
            "http://storage.googleapis.com/chrome-for-testing-public/1/linux64/x.zip",
            "https://evil.example/chromedriver.zip",
            "https://user:pass@storage.googleapis.com/x.zip",
            "https://storage.googleapis.com/x.zip?redirect=evil",
            "https://storage.googleapis.com.evil.example/x.zip",
            "not a url",
        ] {
            assert!(!allowed_download_url(denied), "{denied}");
        }
        assert!(allowed_fetch_url(CFT_MILESTONES_URL));
        assert!(allowed_fetch_url(CFT_STABLE_URL));
        assert!(!allowed_fetch_url(
            "https://googlechromelabs.github.io/other/manifest.json"
        ));
        assert!(!allowed_fetch_url(
            "https://googlechromelabs.github.io/chrome-for-testing/evil.txt"
        ));
        assert_eq!(
            archive_binary_path(ChromeVersion::new(156, 0, 0, 0), Platform::Win64),
            "chromedriver-win64/chromedriver.exe"
        );
        assert_eq!(
            archive_binary_path(ChromeVersion::new(114, 0, 0, 0), Platform::Linux64),
            "chromedriver"
        );
    }
}
