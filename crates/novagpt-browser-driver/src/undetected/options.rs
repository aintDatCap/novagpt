//! Chrome launch options and preference handling.
//!
//! Mirrors the upstream option lifecycle that matters for a persistent owned
//! profile: user-data-dir selection, locale, bounded arguments, `prefs`
//! merging into `Default/Preferences`, and the stale `exit_type` repair that
//! suppresses the restore-tabs prompt. Security-reducing switches are refused
//! unless the embedding process explicitly opts in.
use super::error::UpstreamError;
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_ARGUMENT_BYTES: usize = 1024;
const MAX_ARGUMENTS: usize = 128;
const MAX_PREFERENCES_BYTES: u64 = 4 * 1024 * 1024;
const MAX_PREF_KEY_BYTES: usize = 256;

const DENIED_ARGUMENTS: &[&str] = &[
    "--disable-web-security",
    "--ignore-certificate-errors",
    "--allow-running-insecure-content",
    "--disable-site-isolation-trials",
    "--remote-debugging-pipe",
];

#[derive(Clone, Debug, Default)]
pub struct ChromeOptions {
    pub binary_location: Option<PathBuf>,
    pub user_data_dir: Option<PathBuf>,
    pub arguments: Vec<String>,
    pub headless: bool,
    /// Upstream enables `--no-sandbox` by default. NovaGPT does not: disabling
    /// the Chrome sandbox weakens the whole host boundary. The embedding
    /// process may opt in only for a container/root environment it controls.
    pub no_sandbox: bool,
    prefs: BTreeMap<String, Value>,
}

impl ChromeOptions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_argument(&mut self, argument: impl Into<String>) -> Result<(), UpstreamError> {
        let argument = argument.into();
        if argument.is_empty()
            || argument.len() > MAX_ARGUMENT_BYTES
            || argument.contains('\0')
            || argument.contains('\n')
            || argument.contains('\r')
            || self.arguments.len() >= MAX_ARGUMENTS
        {
            return Err(UpstreamError::InvalidConfig);
        }
        if DENIED_ARGUMENTS
            .iter()
            .any(|denied| argument == *denied || argument.starts_with(&format!("{denied}=")))
        {
            return Err(UpstreamError::InvalidConfig);
        }
        if argument.starts_with("--user-data-dir") || argument.starts_with("--remote-debugging") {
            // These are owned by the lifecycle. Accepting them would let a
            // caller silently reuse another profile or debug endpoint.
            return Err(UpstreamError::InvalidConfig);
        }
        self.arguments.push(argument);
        Ok(())
    }

    /// Add one dotted preference key (for example
    /// `profile.default_content_setting_values.notifications`) merged into the
    /// profile's `Default/Preferences` before launch.
    pub fn add_pref(&mut self, key: impl Into<String>, value: Value) -> Result<(), UpstreamError> {
        let key = key.into();
        if key.is_empty()
            || key.len() > MAX_PREF_KEY_BYTES
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || key.split('.').any(str::is_empty)
        {
            return Err(UpstreamError::InvalidConfig);
        }
        self.prefs.insert(key, value);
        Ok(())
    }

    pub fn preference(&self, key: &str) -> Option<&Value> {
        self.prefs.get(key)
    }

    /// Merge prefs into `<profile>/Default/Preferences`. Existing valid JSON
    /// is preserved; a malformed file aborts instead of being overwritten.
    pub fn handle_prefs(&self, user_data_dir: &Path) -> Result<(), UpstreamError> {
        if self.prefs.is_empty() {
            return Ok(());
        }
        if !user_data_dir.is_absolute() {
            return Err(UpstreamError::InvalidConfig);
        }
        let default = user_data_dir.join("Default");
        fs::create_dir_all(&default).map_err(|_| UpstreamError::Io)?;
        let preferences_path = default.join("Preferences");
        let mut preferences = read_preferences(&preferences_path)?;
        for (key, value) in &self.prefs {
            merge_dotted(&mut preferences, key, value.clone())?;
        }
        write_preferences(&preferences_path, &preferences)
    }

    /// Repair the stale `profile.exit_type` value that causes Chrome's
    /// "restore pages?" prompt. Absence of the file is not an error.
    pub fn fix_exit_type(&self, user_data_dir: &Path) -> Result<bool, UpstreamError> {
        let preferences_path = user_data_dir.join("Default/Preferences");
        if !preferences_path.exists() {
            return Ok(false);
        }
        let bytes = read_bounded(&preferences_path, MAX_PREFERENCES_BYTES)?;
        // A damaged Preferences file must not block browser startup. Do not
        // modify it: a later Chromium run may repair it, and NovaGPT never
        // overwrites profile data it cannot parse.
        let Ok(mut preferences) = serde_json::from_slice::<Map<String, Value>>(&bytes) else {
            return Ok(false);
        };
        let Some(profile) = preferences
            .get_mut("profile")
            .and_then(Value::as_object_mut)
        else {
            return Ok(false);
        };
        if !profile.contains_key("exit_type") || profile["exit_type"].is_null() {
            return Ok(false);
        }
        profile.insert("exit_type".to_owned(), Value::Null);
        write_preferences(&preferences_path, &preferences)?;
        Ok(true)
    }

    /// Build the exact browser command line. Debugging is always loopback,
    /// always uses the caller's dedicated profile, and never reuses the
    /// default Chrome data directory.
    pub fn launch_arguments(
        &self,
        profile: &Path,
        debug_port: u16,
        language: &str,
    ) -> Result<Vec<String>, UpstreamError> {
        if !profile.is_absolute() {
            return Err(UpstreamError::InvalidConfig);
        }
        if !valid_language(language) {
            return Err(UpstreamError::InvalidConfig);
        }
        let mut arguments = vec![
            "--remote-debugging-address=127.0.0.1".to_owned(),
            format!("--remote-debugging-port={debug_port}"),
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".to_owned(),
            "--no-default-browser-check".to_owned(),
            "--new-window".to_owned(),
            format!("--lang={language}"),
            // `--lang` only selects the browser UI language on Linux; the
            // web-facing locale (navigator.languages and Accept-Language)
            // comes from the accept-language list. Keep both consistent.
            format!("--accept-lang={language}"),
            // Chrome sets Blink's AutomationControlled feature whenever the
            // browser exposes a remote-debugging port, which makes every page
            // report navigator.webdriver === true. NovaGPT launches Chrome
            // itself with that port so ChromeDriver can attach, so the feature
            // must be disabled explicitly. This is a launch-time Blink feature
            // switch: no JavaScript getter is patched and no challenge is
            // solved.
            "--disable-blink-features=AutomationControlled".to_owned(),
            "--window-size=1920,1080".to_owned(),
            "--log-level=0".to_owned(),
        ];
        if self.headless {
            arguments.push("--headless=new".to_owned());
        }
        if self.no_sandbox && cfg!(target_os = "linux") {
            arguments.push("--no-sandbox".to_owned());
            arguments.push("--test-type".to_owned());
        }
        arguments.extend(self.arguments.iter().cloned());
        Ok(arguments)
    }

    /// Reject a caller-supplied `user_data_dir` that disagrees with the
    /// dedicated profile NovaGPT owns.
    pub fn validate_profile(&self, profile: &Path) -> Result<(), UpstreamError> {
        if self
            .user_data_dir
            .as_ref()
            .is_some_and(|configured| configured != profile)
        {
            return Err(UpstreamError::InvalidConfig);
        }
        Ok(())
    }

    /// Browser executable preference with the same semantics as
    /// `binary_location`, falling back to the discovered executable.
    pub fn executable(&self, discovered: &Path) -> PathBuf {
        self.binary_location
            .clone()
            .unwrap_or_else(|| discovered.to_path_buf())
    }
}

fn read_preferences(path: &Path) -> Result<Map<String, Value>, UpstreamError> {
    if !path.exists() {
        return Ok(Map::new());
    }
    let bytes = read_bounded(path, MAX_PREFERENCES_BYTES)?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(UpstreamError::Protocol),
    }
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, UpstreamError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(UpstreamError::Io);
        }
        Err(_) => return Err(UpstreamError::Io),
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > limit {
        return Err(UpstreamError::Protocol);
    }
    let mut file = fs::File::open(path).map_err(|_| UpstreamError::Io)?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UpstreamError::Io)?;
    if bytes.len() as u64 > limit {
        return Err(UpstreamError::TooLarge);
    }
    Ok(bytes)
}

fn write_preferences(path: &Path, preferences: &Map<String, Value>) -> Result<(), UpstreamError> {
    let parent = path.parent().ok_or(UpstreamError::Io)?;
    let mut output = tempfile::Builder::new()
        .prefix(".Preferences.novagpt-")
        .tempfile_in(parent)
        .map_err(|_| UpstreamError::Io)?;
    output
        .write_all(
            &serde_json::to_vec(&Value::Object(preferences.clone()))
                .map_err(|_| UpstreamError::Protocol)?,
        )
        .map_err(|_| UpstreamError::Io)?;
    output.as_file().sync_all().map_err(|_| UpstreamError::Io)?;
    output.persist(path).map_err(|_| UpstreamError::Io)?;
    Ok(())
}

fn merge_dotted(
    target: &mut Map<String, Value>,
    key: &str,
    value: Value,
) -> Result<(), UpstreamError> {
    let mut parts: Vec<&str> = key.split('.').collect();
    let leaf = parts.pop().ok_or(UpstreamError::InvalidConfig)?;
    let mut object = target;
    for part in parts {
        let entry = object
            .entry(part.to_owned())
            .or_insert_with(|| Value::Object(Map::new()));
        object = entry.as_object_mut().ok_or(UpstreamError::InvalidConfig)?;
    }
    object.insert(leaf.to_owned(), value);
    Ok(())
}

fn valid_language(language: &str) -> bool {
    !language.is_empty()
        && language.len() <= 35
        && language
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

/// Resolve a stable UI language tag from the environment without exposing the
/// value in diagnostics.
pub fn detected_language() -> String {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        let Some(value) = std::env::var_os(key) else {
            continue;
        };
        let value = value.to_string_lossy();
        let tag = value
            .split(['.', '@'])
            .next()
            .unwrap_or_default()
            .replace('_', "-");
        if matches!(tag.as_str(), "C" | "POSIX") {
            return "en-US".to_owned();
        }
        if valid_language(&tag) {
            return tag;
        }
    }
    "en-US".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn arguments_reject_control_switches_and_duplicates_of_owned_flags() {
        let mut options = ChromeOptions::new();
        options
            .add_argument("--disable-background-networking")
            .unwrap();
        for denied in [
            "--disable-web-security",
            "--ignore-certificate-errors",
            "--user-data-dir=/home/user/.config/google-chrome",
            "--remote-debugging-port=9222",
            "--remote-debugging-pipe",
            "",
            "line\nbreak",
        ] {
            assert!(options.add_argument(denied).is_err(), "{denied:?}");
        }
        for _ in 0..MAX_ARGUMENTS {
            let _ = options.add_argument("--bounded");
        }
        assert!(options.add_argument("--overflow").is_err());
    }

    #[test]
    fn launch_arguments_are_loopback_dedicated_and_bounded() {
        let mut options = ChromeOptions::new();
        options.headless = true;
        options
            .add_argument("--disable-background-networking")
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("novagpt-dedicated");
        let args = options
            .launch_arguments(&profile, 4567, "en-US")
            .unwrap();
        assert!(args.contains(&"--remote-debugging-address=127.0.0.1".to_owned()));
        assert!(args.contains(&"--remote-debugging-port=4567".to_owned()));
        assert!(args.contains(&format!("--user-data-dir={}", profile.display())));
        assert!(args.contains(&"--headless=new".to_owned()));
        assert!(!args.iter().any(|arg| arg == "--no-sandbox"));
        assert!(!args.iter().any(|arg| arg == "--enable-automation"));
        // Remote debugging alone sets navigator.webdriver on current Chrome;
        // the launch must explicitly clear Blink's automation feature.
        assert!(
            args.contains(&"--disable-blink-features=AutomationControlled".to_owned()),
            "{args:?}"
        );
        assert!(args.contains(&"--lang=en-US".to_owned()), "{args:?}");
        assert!(args.contains(&"--accept-lang=en-US".to_owned()), "{args:?}");
        assert!(!args.iter().any(|arg| arg == "--start-maximized"));
        assert!(!args.iter().any(|arg| arg.contains("always-on-top")));
        assert!(!args.iter().any(|arg| arg.contains("window-position")));
        assert!(
            options
                .launch_arguments(Path::new("relative"), 4567, "en-US")
                .is_err()
        );
        assert!(options.launch_arguments(&profile, 0, "en-US").is_ok());
        assert!(options.launch_arguments(&profile, 4567, "en US").is_err());
    }

    #[test]
    fn dotted_prefs_merge_without_clobbering_existing_values() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        fs::create_dir_all(profile.join("Default")).unwrap();
        fs::write(
            profile.join("Default/Preferences"),
            br#"{"profile":{"exit_type":"Crashed","kept":"yes"},"existing":{"a":1}}"#,
        )
        .unwrap();
        let mut options = ChromeOptions::new();
        options
            .add_pref(
                "profile.default_content_setting_values.notifications",
                json!(2),
            )
            .unwrap();
        options.add_pref("existing.b", json!(2)).unwrap();
        options.handle_prefs(&profile).expect("merge");
        let prefs: Value =
            serde_json::from_slice(&fs::read(profile.join("Default/Preferences")).unwrap())
                .unwrap();
        assert_eq!(prefs["profile"]["kept"], "yes");
        assert_eq!(
            prefs["profile"]["default_content_setting_values"]["notifications"],
            2
        );
        assert_eq!(prefs["existing"]["a"], 1);
        assert_eq!(prefs["existing"]["b"], 2);
        assert_eq!(prefs["profile"]["exit_type"], "Crashed");
        assert!(options.fix_exit_type(&profile).unwrap());
        let prefs: Value =
            serde_json::from_slice(&fs::read(profile.join("Default/Preferences")).unwrap())
                .unwrap();
        assert!(prefs["profile"]["exit_type"].is_null());
        assert!(!options.fix_exit_type(&profile).unwrap());
    }

    #[test]
    fn malformed_preferences_are_not_overwritten() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profile");
        fs::create_dir_all(profile.join("Default")).unwrap();
        fs::write(profile.join("Default/Preferences"), b"{not json").unwrap();
        let mut options = ChromeOptions::new();
        options.add_pref("a.b", json!(1)).unwrap();
        assert_eq!(options.handle_prefs(&profile), Err(UpstreamError::Protocol));
        assert_eq!(
            fs::read(profile.join("Default/Preferences")).unwrap(),
            b"{not json"
        );
    }

    #[test]
    fn profile_mismatch_and_invalid_pref_keys_are_rejected() {
        let mut options = ChromeOptions::new();
        options.user_data_dir = Some(PathBuf::from("/tmp/a"));
        assert!(options.validate_profile(Path::new("/tmp/a")).is_ok());
        assert_eq!(
            options.validate_profile(Path::new("/tmp/b")),
            Err(UpstreamError::InvalidConfig)
        );
        for key in ["", "a..b", ".a", "a.", "a b", "a/b"] {
            assert!(options.add_pref(key, json!(1)).is_err(), "{key:?}");
        }
    }
}
