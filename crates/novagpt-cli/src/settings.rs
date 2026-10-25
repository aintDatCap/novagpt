use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const LIMIT: u64 = 128 * 1024;
fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}
fn real_dir(path: &Path) -> io::Result<()> {
    let m = fs::symlink_metadata(path)?;
    if m.file_type().is_symlink() || !m.is_dir() {
        return Err(invalid("redirected or invalid configuration directory"));
    }
    Ok(())
}
fn read_limited(path: &Path) -> io::Result<Vec<u8>> {
    let m = fs::symlink_metadata(path)?;
    if m.file_type().is_symlink() || !m.is_file() || m.len() > LIMIT {
        return Err(invalid("unsafe or oversized configuration input"));
    }
    let mut result = Vec::new();
    fs::File::open(path)?
        .take(LIMIT + 1)
        .read_to_end(&mut result)?;
    if result.len() as u64 > LIMIT {
        return Err(invalid("oversized configuration input"));
    }
    Ok(result)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct LocalSettings {
    pub version: u8,
    pub port: u16,
    pub allowed_roots: Vec<PathBuf>,
    pub vm_enabled: bool,
    pub browser_enabled: bool,
    pub chrome_executable: Option<PathBuf>,
    pub ngrok_enabled: bool,
    pub ngrok_domain: Option<String>,
}
impl Default for LocalSettings {
    fn default() -> Self {
        Self {
            version: 1,
            port: 8787,
            allowed_roots: Vec::new(),
            vm_enabled: false,
            browser_enabled: false,
            chrome_executable: None,
            ngrok_enabled: false,
            ngrok_domain: None,
        }
    }
}
pub fn local_dir(root: &Path) -> PathBuf {
    root.join(".novagpt")
}
pub fn config_path(root: &Path) -> PathBuf {
    local_dir(root).join("config.json")
}
impl LocalSettings {
    pub fn validate(&self) -> io::Result<()> {
        if self.version != 1 || self.port == 0 || self.allowed_roots.len() > 16 {
            return Err(invalid("invalid version, port or allowed root count"));
        }
        for path in &self.allowed_roots {
            if !path.is_absolute() || !path.is_dir() {
                return Err(invalid(
                    "allowed root must be an existing absolute directory",
                ));
            }
        }
        if self
            .ngrok_domain
            .as_deref()
            .is_some_and(|host| !crate::tunnel::valid_domain(host))
        {
            return Err(invalid("invalid ngrok hostname"));
        }
        if self.browser_enabled
            && !self
                .chrome_executable
                .as_ref()
                .is_some_and(|p| p.is_absolute() && p.is_file())
        {
            return Err(invalid(
                "browser requires a valid absolute Chrome executable path",
            ));
        }
        Ok(())
    }
    pub fn load(root: &Path) -> io::Result<Self> {
        let dir = local_dir(root);
        if !dir.exists() {
            return Ok(Self::default());
        }
        real_dir(&dir)?;
        let file = config_path(root);
        if !file.exists() {
            return Ok(Self::default());
        }
        let cfg: Self = serde_json::from_slice(&read_limited(&file)?)?;
        cfg.validate()?;
        Ok(cfg)
    }
    pub fn save(&self, root: &Path) -> io::Result<PathBuf> {
        self.validate()?;
        let dir = local_dir(root);
        if dir.exists() {
            real_dir(&dir)?;
        } else {
            fs::create_dir(&dir)?;
        }
        let file = config_path(root);
        if file.exists() {
            let meta = fs::symlink_metadata(&file)?;
            if meta.file_type().is_symlink() || !meta.is_file() {
                return Err(invalid("unsafe settings target"));
            }
        }
        let bytes = serde_json::to_vec_pretty(self)?;
        if bytes.len() as u64 > LIMIT {
            return Err(invalid("settings too large"));
        }
        let mut f = if file.exists() {
            OpenOptions::new().write(true).truncate(true).open(&file)?
        } else {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&file)?
        };
        f.write_all(&bytes)?;
        f.sync_all()?;
        Ok(file)
    }
}
#[derive(Clone, Debug)]
pub struct CodexProfile {
    pub origin: PathBuf,
    pub root_label: String,
    pub port: Option<u16>,
    pub tunnel: Option<String>,
    pub ngrok_domain: Option<String>,
}
/// Reads only selected public configuration fields from CodexPro workspace profiles.
pub fn discover_codexpro_profiles(home: &Path) -> io::Result<Vec<CodexProfile>> {
    let dir = home.join("profiles");
    if !dir.exists() {
        return Ok(Vec::new());
    }
    real_dir(&dir)?;
    let mut found = Vec::new();
    for entry in fs::read_dir(&dir)?.take(64) {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Ok(bytes) = read_limited(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(root) = v.get("root").and_then(Value::as_str) else {
            continue;
        };
        if root.len() > 1024 || root.chars().any(|ch| ch.is_control()) {
            continue;
        }
        let port = v
            .get("port")
            .and_then(|v| match v {
                Value::String(s) => s.parse::<u16>().ok(),
                Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
                _ => None,
            })
            .filter(|p| *p > 0);
        // Allowlist only the public connection settings; never import
        // token, ngrokConfig, allowedRoots, browser or guest state.
        let tunnel = v
            .get("tunnel")
            .and_then(Value::as_str)
            .filter(|s| {
                matches!(
                    *s,
                    "ngrok" | "none" | "cloudflare" | "cloudflare-named" | "tailscale"
                )
            })
            .map(str::to_owned);
        let ngrok_domain = if tunnel.as_deref() == Some("ngrok") {
            v.get("hostname")
                .and_then(Value::as_str)
                .filter(|host| crate::tunnel::valid_domain(host))
                .map(str::to_owned)
        } else {
            None
        };
        found.push(CodexProfile {
            origin: path,
            root_label: root.to_string(),
            port,
            tunnel,
            ngrok_domain,
        });
    }
    found.sort_by(|a, b| a.root_label.cmp(&b.root_label));
    Ok(found)
}
pub fn codexpro_home() -> PathBuf {
    if let Some(home) = std::env::var_os("CODEXPRO_HOME") {
        return PathBuf::from(home);
    }
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".codexpro")
}
/// Never imports secrets, ngrok configuration file contents, granted roots,
/// private browser profiles, or VM state. Called only after TUI confirmation.
pub fn import_supported(settings: &mut LocalSettings, profile: &CodexProfile) {
    if let Some(port) = profile.port {
        settings.port = port;
    }
    if let Some(ref tunnel) = profile.tunnel {
        if tunnel == "ngrok" {
            // A malformed/absent hostname must not enable a misleading public tunnel.
            if let Some(ref domain) = profile.ngrok_domain {
                settings.ngrok_enabled = true;
                settings.ngrok_domain = Some(domain.clone());
            }
        } else {
            settings.ngrok_enabled = false;
            settings.ngrok_domain = None;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = LocalSettings {
            port: 9988,
            ..LocalSettings::default()
        };
        cfg.save(dir.path()).unwrap();
        assert_eq!(LocalSettings::load(dir.path()).unwrap(), cfg);
        cfg.port = 9989;
        cfg.save(dir.path()).unwrap();
        assert_eq!(LocalSettings::load(dir.path()).unwrap(), cfg);
    }
    #[test]
    fn explicit_import_only_copies_safe_public_connection_fields() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("profiles")).unwrap();
        fs::write(
            home.path().join("profiles").join("one.json"),
            serde_json::json!({"root":"C:/example","port":"9091","allowedRoots":["C:/other"],
                "tunnel":"ngrok","hostname":"project.ngrok-free.dev",
                "ngrokConfig":"C:/private/custom-ngrok.yml",
                "token":"[REDACTED_SECRET]","chatgptBrowserProfile":"not-used"})
            .to_string(),
        )
        .unwrap();
        let profiles = discover_codexpro_profiles(home.path()).unwrap();
        let mut cfg = LocalSettings::default();
        assert_eq!(cfg.port, 8787);
        import_supported(&mut cfg, &profiles[0]);
        assert_eq!(cfg.port, 9091);
        assert!(cfg.ngrok_enabled);
        assert_eq!(cfg.ngrok_domain.as_deref(), Some("project.ngrok-free.dev"));
        assert!(cfg.allowed_roots.is_empty());
        cfg.save(dir.path()).unwrap();
        let saved = fs::read_to_string(config_path(dir.path())).unwrap();
        for excluded in [
            "not-used",
            "REDACTED_SECRET",
            "custom-ngrok.yml",
            "C:/other",
        ] {
            assert!(!saved.contains(excluded), "unexpected imported field");
        }
        assert_eq!(LocalSettings::load(dir.path()).unwrap(), cfg);
    }
    #[test]
    fn invalid_or_unrelated_profile_tunnels_never_enable_ngrok() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("profiles")).unwrap();
        fs::write(
            home.path().join("profiles").join("bad.json"),
            serde_json::json!({"root":"A","port":9020,"tunnel":"ngrok",
                "hostname":"https://unsafe.example/path","token":"[REDACTED_SECRET]"})
            .to_string(),
        )
        .unwrap();
        fs::write(
            home.path().join("profiles").join("other.json"),
            serde_json::json!({"root":"B","tunnel":"cloudflare","hostname":"wrong.example"})
                .to_string(),
        )
        .unwrap();
        let profiles = discover_codexpro_profiles(home.path()).unwrap();
        assert_eq!(profiles.len(), 2);
        let mut settings = LocalSettings {
            ngrok_enabled: true,
            ngrok_domain: Some("prior.ngrok-free.dev".into()),
            ..LocalSettings::default()
        };
        import_supported(&mut settings, &profiles[0]);
        assert_eq!(settings.port, 9020);
        assert_eq!(
            settings.ngrok_domain.as_deref(),
            Some("prior.ngrok-free.dev")
        );
        import_supported(&mut settings, &profiles[1]);
        assert!(!settings.ngrok_enabled);
        assert_eq!(settings.ngrok_domain, None);
        assert!(
            LocalSettings {
                ngrok_domain: Some("https://host.example".into()),
                ..LocalSettings::default()
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn bad_settings_rejected() {
        let cfg = LocalSettings {
            browser_enabled: true,
            ..LocalSettings::default()
        };
        assert!(cfg.validate().is_err());
    }
}
