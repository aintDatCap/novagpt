//! Chrome executable discovery. No browser process is started by this module.
use std::{
    env, io,
    path::{Path, PathBuf},
};

fn executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0);
    }
    #[cfg(not(unix))]
    {
        true
    }
}
fn from_path(name: &Path) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if executable(&candidate) {
            return Some(candidate);
        }
        #[cfg(windows)]
        if name.extension().is_none() {
            let candidate = candidate.with_extension("exe");
            if executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}
#[cfg(windows)]
fn windows_candidates(mut get: impl FnMut(&str) -> Option<std::ffi::OsString>) -> Vec<PathBuf> {
    ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .iter()
        .filter_map(|key| get(key))
        .map(|base| {
            PathBuf::from(base)
                .join("Google")
                .join("Chrome")
                .join("Application")
                .join("chrome.exe")
        })
        .collect()
}

/// Explicit Chrome path, then CodexPro-compatible standard locations.
pub fn discover(explicit: Option<&Path>) -> io::Result<PathBuf> {
    let missing = || {
        io::Error::new(
            io::ErrorKind::NotFound,
            "Chrome executable not found; configure a path or install Chrome",
        )
    };
    if let Some(path) = explicit {
        if path.is_absolute() && executable(path) {
            return Ok(path.to_owned());
        }
        if path.components().count() == 1
            && let Some(found) = from_path(path)
        {
            return Ok(found);
        }
        return Err(missing());
    }
    #[cfg(windows)]
    for candidate in windows_candidates(|key| env::var_os(key)) {
        if executable(&candidate) {
            return Ok(candidate);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let mut paths = vec![PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )];
        if let Some(home) = env::var_os("HOME") {
            paths.push(
                PathBuf::from(home)
                    .join("Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            );
        }
        paths.push(PathBuf::from(
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
        ));
        for path in paths {
            if executable(&path) {
                return Ok(path);
            }
        }
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    for name in [
        "google-chrome",
        "google-chrome-stable",
        "chromium",
        "chromium-browser",
    ] {
        if let Some(found) = from_path(Path::new(name)) {
            return Ok(found);
        }
    }
    Err(missing())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_lookup_matches_codexpro_standard_installations_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let candidates = windows_candidates(|name| match name {
            "ProgramFiles" => Some(temp.path().join("programs").into_os_string()),
            "ProgramFiles(x86)" => Some(temp.path().join("x86").into_os_string()),
            "LOCALAPPDATA" => Some(temp.path().join("user").into_os_string()),
            _ => None,
        });
        assert_eq!(candidates.len(), 3);
        for (base, candidate) in ["programs", "x86", "user"].iter().zip(&candidates) {
            assert_eq!(
                *candidate,
                temp.path()
                    .join(base)
                    .join("Google")
                    .join("Chrome")
                    .join("Application")
                    .join("chrome.exe")
            );
        }
        std::fs::create_dir_all(candidates[2].parent().unwrap()).unwrap();
        std::fs::write(&candidates[2], "fixture").unwrap();
        assert!(candidates.iter().any(|path| executable(path)));
    }
    #[test]
    fn explicit_path_and_rejected_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let chrome = dir.path().join("fixture");
        std::fs::write(&chrome, "fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&chrome, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(discover(Some(&chrome)).unwrap(), chrome);
        assert!(discover(Some(&dir.path().join("missing"))).is_err());
    }
}
