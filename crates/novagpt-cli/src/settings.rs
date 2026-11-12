use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
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
#[serde(default)]
pub struct LocalSettings {
    pub version: u8,
    pub language: crate::i18n::LanguageChoice,
    pub port: u16,
    pub allowed_roots: Vec<PathBuf>,
    pub vm_enabled: bool,
    pub browser_enabled: bool,
    pub chrome_executable: Option<PathBuf>,
    pub auto_start_mcp: bool,
    pub ngrok_enabled: bool,
    pub ngrok_domain: Option<String>,
    /// Optional ChatGPT project URL override. When absent, the browser finds
    /// or creates the NovaGPT - subagents project before opening a new worker.
    pub chatgpt_project_url: Option<String>,
    /// Preserve options introduced by newer releases when an older NovaGPT
    /// loads and saves the same per-user configuration file.
    #[serde(flatten)]
    pub(crate) unknown_fields: BTreeMap<String, serde_json::Value>,
}
impl Default for LocalSettings {
    fn default() -> Self {
        Self {
            version: 1,
            language: crate::i18n::LanguageChoice::default(),
            port: 8787,
            allowed_roots: Vec::new(),
            vm_enabled: false,
            browser_enabled: false,
            chrome_executable: None,
            auto_start_mcp: false,
            ngrok_enabled: false,
            ngrok_domain: None,
            chatgpt_project_url: None,
            unknown_fields: BTreeMap::new(),
        }
    }
}
pub fn local_dir(root: &Path) -> PathBuf {
    root.join(".novagpt")
}
/// One configuration for the signed-in OS user, independent of --root.
pub fn global_dir() -> io::Result<PathBuf> {
    crate::token::app_state_dir()
}

pub fn config_path() -> io::Result<PathBuf> {
    Ok(global_dir()?.join("config.json"))
}

/// Isolated state-root helpers keep tests away from the machine's real profile.
fn load_from(dir: &Path) -> io::Result<LocalSettings> {
    if !dir.exists() {
        return Ok(LocalSettings::default());
    }
    real_dir(dir)?;
    let file = dir.join("config.json");
    match fs::symlink_metadata(&file) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(LocalSettings::default());
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let cfg: LocalSettings = serde_json::from_slice(&read_limited(&file)?)?;
    cfg.validate()?;
    Ok(cfg)
}

#[cfg(test)]
pub(crate) fn load_from_for_test(dir: &Path) -> io::Result<LocalSettings> {
    load_from(dir)
}
#[cfg(test)]
pub(crate) fn save_to_for_test(settings: &LocalSettings, base: &Path) -> io::Result<PathBuf> {
    save_to(settings, base)
}

/// Store only in the private user profile, atomically replacing the file.
fn save_to(settings: &LocalSettings, dir: &Path) -> io::Result<PathBuf> {
    settings.validate()?;
    let parent = dir
        .parent()
        .ok_or_else(|| invalid("missing user state directory"))?;
    fs::create_dir_all(parent)?;
    if dir.exists() {
        real_dir(dir)?;
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            match fs::DirBuilder::new().mode(0o700).create(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => real_dir(dir)?,
                Err(e) => return Err(e),
            }
        }
        #[cfg(not(unix))]
        match fs::create_dir(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => real_dir(dir)?,
            Err(e) => return Err(e),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if fs::metadata(dir)?.permissions().mode() & 0o077 != 0 {
            return Err(invalid("global settings directory is not private"));
        }
    }
    let file = dir.join("config.json");
    match fs::symlink_metadata(&file) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
            return Err(invalid("unsafe settings target"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let bytes = serde_json::to_vec_pretty(settings)?;
    if bytes.len() as u64 > LIMIT {
        return Err(invalid("settings too large"));
    }
    let temporary = dir.join(format!(".config-{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> io::Result<()> {
        let mut f = options.open(&temporary)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        drop(f);
        // Fail closed if the destination was redirected during the write.
        match fs::symlink_metadata(&file) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                return Err(invalid("unsafe settings target"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::rename(&temporary, &file)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result?;
    Ok(file)
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
        if self
            .chatgpt_project_url
            .as_deref()
            .is_some_and(|url| novagpt_browser::project::project_url(url).is_none())
        {
            return Err(invalid("invalid ChatGPT project URL"));
        }
        if self.browser_enabled
            && crate::chrome::discover(self.chrome_executable.as_deref()).is_err()
        {
            return Err(invalid(
                "browser requires an installed Chrome executable or valid override",
            ));
        }
        Ok(())
    }
    pub fn load() -> io::Result<Self> {
        load_from(&global_dir()?)
    }

    pub fn save(&self) -> io::Result<PathBuf> {
        save_to(self, &global_dir()?)
    }
}
#[derive(Clone, Debug)]
pub struct ImportProfile {
    pub source: &'static str,
    pub origin: PathBuf,
    pub root_label: String,
    pub port: Option<u16>,
    pub tunnel: Option<String>,
    pub ngrok_domain: Option<String>,
}
/// Read public settings from another NovaGPT workspace without loading its
/// token, permissions, browser or VM state. The source must be a real directory.
pub fn discover_novagpt_workspace(source: &Path) -> io::Result<ImportProfile> {
    if !source.is_absolute() {
        return Err(invalid("source workspace must be an absolute path"));
    }
    real_dir(source)?;
    let source = fs::canonicalize(source)?;
    real_dir(&source)?;
    let config_dir = local_dir(&source);
    real_dir(&config_dir)?;
    let origin = local_dir(&source).join("config.json");
    let settings: LocalSettings = serde_json::from_slice(&read_limited(&origin)?)?;
    // The imported connection values are independently validated; other fields
    // are parsed solely for schema compatibility and are never copied.
    if settings.version != 1
        || settings.port == 0
        || settings
            .ngrok_domain
            .as_deref()
            .is_some_and(|domain| !crate::tunnel::valid_domain(domain))
    {
        return Err(invalid("unsupported NovaGPT source connection settings"));
    }
    Ok(ImportProfile {
        source: "NovaGPT",
        origin,
        root_label: source.display().to_string(),
        port: Some(settings.port),
        tunnel: Some(
            if settings.ngrok_enabled {
                "ngrok"
            } else {
                "none"
            }
            .into(),
        ),
        ngrok_domain: if settings.ngrok_enabled {
            settings.ngrok_domain
        } else {
            None
        },
    })
}

/// Never imports secrets, ngrok configuration file contents, granted roots,
/// private browser profiles, or VM state. Called only after TUI confirmation.
pub fn import_supported(settings: &mut LocalSettings, profile: &ImportProfile) {
    if let Some(port) = profile.port {
        settings.port = port;
    }
    if let Some(ref tunnel) = profile.tunnel {
        if tunnel == "ngrok" {
            settings.ngrok_enabled = true;
            settings.ngrok_domain = profile.ngrok_domain.clone();
        } else {
            settings.ngrok_enabled = false;
            settings.ngrok_domain = None;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn save_legacy_fixture(cfg: &LocalSettings, root: &Path) {
        let dir = local_dir(root);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("config.json"), serde_json::to_vec(cfg).unwrap()).unwrap();
    }
    #[test]
    fn legacy_settings_do_not_implicitly_start_an_http_server() {
        let config: LocalSettings =
            serde_json::from_str(r#"{"version":1,"port":8787,"ngrok_enabled":true}"#).unwrap();
        assert!(!config.auto_start_mcp);
        assert_eq!(config.language, crate::i18n::LanguageChoice::Auto);
    }

    #[test]
    fn newer_settings_survive_load_modify_save() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let future = serde_json::json!({
            "version": 1,
            "port": 8787,
            "chatgpt_project_url": "https://chatgpt.com/g/g-p-abc123/project",
            "future_option": {"enabled": true, "limits": [1, 2, 3]},
            "future_flag": false
        });
        fs::write(dir.path().join("config.json"), future.to_string()).unwrap();

        let mut settings = load_from(dir.path()).unwrap();
        assert_eq!(
            settings.chatgpt_project_url.as_deref(),
            Some("https://chatgpt.com/g/g-p-abc123/project")
        );
        assert_eq!(
            settings.unknown_fields["future_option"],
            future["future_option"]
        );
        settings.port = 9000;
        save_to(&settings, dir.path()).unwrap();

        let saved: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("config.json")).unwrap()).unwrap();
        assert_eq!(saved["future_option"], future["future_option"]);
        assert_eq!(saved["future_flag"], future["future_flag"]);
        assert_eq!(saved["chatgpt_project_url"], future["chatgpt_project_url"]);
        assert_eq!(saved["port"], 9000);
        assert_eq!(load_from(dir.path()).unwrap(), settings);
    }

    #[test]
    fn unknown_settings_do_not_hide_invalid_known_fields() {
        assert!(
            serde_json::from_str::<LocalSettings>(r#"{"port":"not a number","future_field":true}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<LocalSettings>(r#"{"version":2,"future_field":true}"#)
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn settings_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut cfg = LocalSettings {
            port: 9988,
            ..LocalSettings::default()
        };
        save_to(&cfg, dir.path()).unwrap();
        assert_eq!(load_from(dir.path()).unwrap(), cfg);
        cfg.port = 9989;
        save_to(&cfg, dir.path()).unwrap();
        assert_eq!(load_from(dir.path()).unwrap(), cfg);
    }
    #[test]
    fn imports_only_connection_settings_from_other_novagpt_directory() {
        let source = tempfile::tempdir().expect("source");
        let destination = tempfile::tempdir().expect("destination");
        let source_cfg = LocalSettings {
            port: 9431,
            ngrok_enabled: true,
            ngrok_domain: Some("imported.ngrok-free.dev".into()),
            vm_enabled: true,
            allowed_roots: vec![source.path().to_path_buf()],
            ..LocalSettings::default()
        };
        save_legacy_fixture(&source_cfg, source.path());
        let profile = discover_novagpt_workspace(source.path()).expect("discover");
        assert_eq!(profile.source, "NovaGPT");
        assert_eq!(profile.port, Some(9431));
        let mut imported = LocalSettings::default();
        import_supported(&mut imported, &profile);
        assert_eq!(imported.port, 9431);
        assert!(imported.ngrok_enabled);
        assert_eq!(
            imported.ngrok_domain.as_deref(),
            Some("imported.ngrok-free.dev")
        );
        assert!(!imported.vm_enabled);
        assert!(imported.allowed_roots.is_empty());
        assert!(
            !destination.path().join(".novagpt/config.json").exists(),
            "import must only modify draft"
        );
        assert!(discover_novagpt_workspace(destination.path()).is_err());
    }
    #[test]
    fn dynamic_ngrok_preference_roundtrips_without_a_static_hostname() {
        let source = tempfile::tempdir().expect("source");

        let source_config = LocalSettings {
            port: 9300,
            ngrok_enabled: true,
            ngrok_domain: None,
            ..LocalSettings::default()
        };
        save_legacy_fixture(&source_config, source.path());
        let profile = discover_novagpt_workspace(source.path()).expect("profile");
        let mut destination = LocalSettings {
            ngrok_enabled: false,
            ngrok_domain: Some("old.ngrok-free.dev".into()),
            ..LocalSettings::default()
        };
        import_supported(&mut destination, &profile);
        assert!(destination.ngrok_enabled);
        assert_eq!(destination.ngrok_domain, None);
        assert_eq!(destination.port, 9300);
    }

    #[test]
    fn bad_settings_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = LocalSettings {
            browser_enabled: true,
            chrome_executable: Some(dir.path().join("missing-chrome")),
            ..LocalSettings::default()
        };
        assert!(cfg.validate().is_err());
    }
}
