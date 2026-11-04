use crate::DriverError;
use std::{
    env,
    path::{Path, PathBuf},
    time::Duration,
};

/// Settings for a single visible, dedicated Chrome instance.
#[derive(Clone, Debug)]
pub struct DriverConfig {
    pub executable: PathBuf,
    pub profile: PathBuf,
    pub startup_timeout: Duration,
    pub command_timeout: Duration,
    pub creation_interval: Duration,
}

impl DriverConfig {
    pub fn new(
        executable: impl AsRef<Path>,
        profile: impl AsRef<Path>,
    ) -> Result<Self, DriverError> {
        let executable = executable.as_ref().to_path_buf();
        let profile = profile.as_ref().to_path_buf();
        if !executable.is_file() || !profile.is_absolute() || profile.as_os_str().is_empty() {
            return Err(DriverError::InvalidConfig);
        }
        Ok(Self {
            executable,
            profile,
            startup_timeout: Duration::from_secs(25),
            command_timeout: Duration::from_secs(15),
            creation_interval: Duration::from_millis(400),
        })
    }
}

/// Discover an installed Chrome/Chromium executable, with an explicit
/// override. Does not spawn anything or inspect browser profile contents.
pub fn discover_chrome(explicit: Option<&Path>) -> Result<PathBuf, DriverError> {
    if let Some(path) = explicit {
        return path
            .is_file()
            .then(|| path.to_path_buf())
            .ok_or(DriverError::InvalidConfig);
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
            if let Some(dir) = env::var_os(base) {
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
    if let Some(path) = env::var_os("PATH") {
        for dir in env::split_paths(&path) {
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
    Err(DriverError::NotRunning)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_requires_binary_and_absolute_profile() {
        let bin = tempfile::NamedTempFile::new().expect("binary");
        let dir = tempfile::tempdir().expect("profile");
        let config = DriverConfig::new(bin.path(), dir.path().join("owned")).expect("config");
        assert!(config.profile.is_absolute());
        assert_eq!(config.creation_interval, Duration::from_millis(400));
        assert!(matches!(
            DriverConfig::new("missing-chrome", dir.path()),
            Err(DriverError::InvalidConfig)
        ));
        assert!(matches!(
            DriverConfig::new(bin.path(), "relative/profile"),
            Err(DriverError::InvalidConfig)
        ));
    }

    #[test]
    fn explicit_discovery_requires_file() {
        let bin = tempfile::NamedTempFile::new().expect("binary");
        assert_eq!(
            discover_chrome(Some(bin.path())).expect("found"),
            bin.path()
        );
        assert!(matches!(
            discover_chrome(Some(Path::new("missing"))),
            Err(DriverError::InvalidConfig)
        ));
    }
}
