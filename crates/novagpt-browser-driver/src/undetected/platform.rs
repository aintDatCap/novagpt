//! Platform names and versions used by Chrome for Testing (CfT).
//!
//! The names are part of the public CfT download layout, not copied
//! implementation text. CfT publishes only Chrome 113+; `legacy_*` helpers
//! preserve upstream's pre-115 Google Storage path.
use super::error::UpstreamError;
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Platform {
    Linux64,
    LinuxArm64,
    MacX64,
    MacArm64,
    Win32,
    Win64,
}

impl Platform {
    /// Host platform used for a ChromeDriver download. Fails rather than
    /// guessing so a mismatched executable can never be selected.
    pub fn current() -> Result<Self, UpstreamError> {
        if cfg!(target_os = "macos") {
            return match std::env::consts::ARCH {
                "aarch64" => Ok(Self::MacArm64),
                "x86_64" => Ok(Self::MacX64),
                _ => Err(UpstreamError::UnsupportedPlatform),
            };
        }
        if cfg!(windows) {
            return match std::env::consts::ARCH {
                "x86_64" => Ok(Self::Win64),
                "x86" => Ok(Self::Win32),
                _ => Err(UpstreamError::UnsupportedPlatform),
            };
        }
        if cfg!(target_os = "linux") {
            return match std::env::consts::ARCH {
                "x86_64" => Ok(Self::Linux64),
                "aarch64" => Ok(Self::LinuxArm64),
                _ => Err(UpstreamError::UnsupportedPlatform),
            };
        }
        Err(UpstreamError::UnsupportedPlatform)
    }

    /// Chrome for Testing platform identifier (Chrome 115+).
    pub fn cft_name(self) -> &'static str {
        match self {
            Self::Linux64 => "linux64",
            Self::LinuxArm64 => "linux-arm64",
            Self::MacX64 => "mac-x64",
            Self::MacArm64 => "mac-arm64",
            Self::Win32 => "win32",
            Self::Win64 => "win64",
        }
    }

    /// Legacy ChromeDriver 114-and-earlier platform identifier.
    pub fn legacy_name(self) -> &'static str {
        match self {
            Self::Linux64 => "linux64",
            Self::LinuxArm64 => "linux64",
            Self::MacX64 | Self::MacArm64 => "mac64",
            Self::Win32 | Self::Win64 => "win32",
        }
    }

    pub fn executable_name(self) -> &'static str {
        if matches!(self, Self::Win32 | Self::Win64) {
            "chromedriver.exe"
        } else {
            "chromedriver"
        }
    }

    /// Path inside the archive, relative to its root.
    pub fn archive_binary_path(self) -> String {
        if matches!(self, Self::Win32 | Self::Win64) {
            format!("chromedriver-{}/chromedriver.exe", self.cft_name())
        } else {
            format!("chromedriver-{}/chromedriver", self.cft_name())
        }
    }

    pub fn legacy_archive_binary_path(self) -> &'static str {
        if matches!(self, Self::Win32 | Self::Win64) {
            "chromedriver.exe"
        } else {
            "chromedriver"
        }
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.cft_name())
    }
}

/// A validated four-component Chrome/ChromeDriver version.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ChromeVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
    pub patch: u32,
}

impl ChromeVersion {
    pub const fn new(major: u32, minor: u32, build: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            build,
            patch,
        }
    }

    pub fn milestone(self) -> u32 {
        self.major
    }

    /// Same major milestone, which is ChromeDriver's documented compatibility
    /// boundary. Patch/minor drift within a milestone is accepted.
    pub fn same_milestone(self, other: Self) -> bool {
        self.major == other.major
    }

    /// CfT supports 115+ in the modern layout; older releases use the legacy
    /// storage layout. A caller may still ask for an exact older version.
    pub fn uses_legacy_layout(self) -> bool {
        self.major <= 114
    }
}

impl std::str::FromStr for ChromeVersion {
    type Err = UpstreamError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.is_empty() || input.len() > 48 || input != input.trim() {
            return Err(UpstreamError::Manifest);
        }
        let mut parts = input.split('.');
        let mut values = [0_u32; 4];
        for value in &mut values {
            let part = parts.next().ok_or(UpstreamError::Manifest)?;
            if part.is_empty() || part.len() > 10 || !part.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(UpstreamError::Manifest);
            }
            *value = part.parse().map_err(|_| UpstreamError::Manifest)?;
        }
        if parts.next().is_some() {
            return Err(UpstreamError::Manifest);
        }
        Ok(Self::new(values[0], values[1], values[2], values[3]))
    }
}

impl fmt::Display for ChromeVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}.{}.{}.{}",
            self.major, self.minor, self.build, self.patch
        )
    }
}

/// Extract the version from `google-chrome --version` / `chromium --version`
/// output such as `Google Chrome 156.0.8078.47` or `Chromium 140.0.1.2`.
pub fn parse_launcher_output(output: &str) -> Option<ChromeVersion> {
    let line = output.lines().next()?.trim();
    if line.len() > 256 {
        return None;
    }
    let lower = line.to_ascii_lowercase();
    if !lower.contains("chrome") && !lower.contains("chromium") {
        return None;
    }
    line.split_whitespace()
        .map(|token| token.trim_end_matches(['.', ',', ';']))
        .find(|token| {
            token.matches('.').count() == 3 && token.starts_with(|c: char| c.is_ascii_digit())
        })
        .and_then(|token| token.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_strict_and_bounded() {
        assert_eq!(
            "156.0.8078.47".parse::<ChromeVersion>().expect("version"),
            ChromeVersion::new(156, 0, 8078, 47)
        );
        for invalid in [
            "",
            "156",
            "156.0.1",
            "156.0.1.2.3",
            "156.0.1.beta",
            "156..1.2",
            " 156.0.1.2 ",
            "156.0.1.2\n156.0.1.3",
        ] {
            assert!(invalid.parse::<ChromeVersion>().is_err(), "{invalid:?}");
        }
        assert_eq!(
            "156.0.8078.47"
                .parse::<ChromeVersion>()
                .unwrap()
                .milestone(),
            156
        );
    }

    #[test]
    fn launcher_output_is_parsed_without_accepting_chatter() {
        assert_eq!(
            parse_launcher_output("Google Chrome 156.0.8078.47 \n"),
            Some(ChromeVersion::new(156, 0, 8078, 47))
        );
        assert_eq!(
            parse_launcher_output("Chromium 140.0.7339.80 (Official Build)"),
            Some(ChromeVersion::new(140, 0, 7339, 80))
        );
        assert_eq!(
            parse_launcher_output("Google Chrome 156.0.8078.47"),
            Some(ChromeVersion::new(156, 0, 8078, 47))
        );
        assert_eq!(parse_launcher_output("192.168.1.1"), None);
        assert_eq!(parse_launcher_output("Firefox 130.0.1.2"), None);
        assert_eq!(parse_launcher_output(""), None);
    }

    #[test]
    fn platform_layouts_are_distinct() {
        assert_eq!(Platform::Linux64.cft_name(), "linux64");
        assert_eq!(Platform::LinuxArm64.cft_name(), "linux-arm64");
        assert_eq!(Platform::MacX64.legacy_name(), "mac64");
        assert_eq!(Platform::Win64.executable_name(), "chromedriver.exe");
        assert_eq!(
            Platform::Linux64.archive_binary_path(),
            "chromedriver-linux64/chromedriver"
        );
        assert_eq!(
            Platform::Win32.archive_binary_path(),
            "chromedriver-win32/chromedriver.exe"
        );
        assert_eq!(
            Platform::Linux64.legacy_archive_binary_path(),
            "chromedriver"
        );
        assert!(ChromeVersion::new(114, 0, 0, 0).uses_legacy_layout());
        assert!(!ChromeVersion::new(115, 0, 0, 0).uses_legacy_layout());
        assert!(ChromeVersion::new(156, 0, 1, 1).same_milestone(ChromeVersion::new(156, 9, 9, 9)));
        assert!(!ChromeVersion::new(155, 0, 1, 1).same_milestone(ChromeVersion::new(156, 0, 0, 0)));
    }
}
