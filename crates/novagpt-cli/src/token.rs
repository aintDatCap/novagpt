//! One HTTP secret per OS user, outside workspaces and config.json.
//! Legacy workspace secrets are migrated once; environment override takes precedence.
use fs2::FileExt;
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
use uuid::Uuid;

const FILE_BYTES: u64 = 256;
const SHARED_NAME: &str = "http.token";
const MIGRATION_LOCK: &str = ".token.lock";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Environment,
    Stored { path: PathBuf, created: bool },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Migration {
    None,
    Preserved,
    Rotated,
    Cleaned,
}

pub struct Resolved {
    pub value: String,
    pub origin: Origin,
    pub migration: Migration,
}
impl Resolved {
    pub fn rotation_warning(&self) -> Option<&'static str> {
        (self.migration == Migration::Rotated).then_some(
            "ATTENZIONE: trovati piu' vecchi token. E' stato creato un token condiviso nuovo: riavvia i server HTTP NovaGPT e aggiorna i connettori ChatGPT.",
        )
    }

    pub fn description(&self) -> String {
        let summary = match &self.origin {
            Origin::Environment => {
                "Token HTTP condiviso: uso NOVAGPT_HTTP_TOKEN dall'ambiente (valore nascosto)."
                    .into()
            }
            Origin::Stored { path, created } => format!(
                "Token HTTP condiviso {}: {} (valore nascosto). Per visualizzarlo: novagpt token show",
                if *created {
                    "generato"
                } else {
                    "gia' esistente"
                },
                path.display()
            ),
        };
        if let Some(warning) = self.rotation_warning() {
            format!("{summary} {warning}")
        } else if self.migration == Migration::Preserved {
            format!("{summary} Il precedente token e' stato conservato per tutti i workspace.")
        } else {
            summary
        }
    }
}
fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            Err(invalid("unsafe token directory"))
        }
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    return Err(invalid("token directory permissions are too broad"));
                }
            }
            #[cfg(not(unix))]
            {
                let _ = meta;
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            let created = {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(path)
            };
            #[cfg(not(unix))]
            let created = fs::create_dir(path);
            match created {
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    private_directory(path)
                }
                result => result,
            }
        }
        Err(error) => Err(error),
    }
}
fn private_file(path: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > FILE_BYTES {
        return Err(invalid("unsafe token file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(invalid(
                "token file permissions are too broad (expected 0600)",
            ));
        }
    }
    Ok(())
}
fn validate(raw: &str) -> io::Result<()> {
    if raw.len() < 24 || raw.len() > 256 || raw.bytes().any(|b| !b.is_ascii_graphic()) {
        return Err(invalid(
            "HTTP token must be 24-256 visible ASCII characters without whitespace",
        ));
    }
    Ok(())
}
fn validate_saved(raw: &str) -> io::Result<()> {
    if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid(
            "stored token format is invalid (64 hex characters expected)",
        ));
    }
    Ok(())
}
fn generate() -> String {
    // Uuid::new_v4 uses the OS CSPRNG. Two random UUIDv4s provide 244 random bits.
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}
fn user_state_dir() -> io::Result<PathBuf> {
    let path = if cfg!(windows) {
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| {
                env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData").join("Local"))
            })
            .ok_or_else(|| invalid("LOCALAPPDATA/USERPROFILE not available"))?
    } else {
        env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| env::var_os("HOME").map(|p| PathBuf::from(p).join(".local").join("state")))
            .ok_or_else(|| invalid("HOME not available"))?
    };
    if !path.is_absolute() {
        return Err(invalid("user state directory must be absolute"));
    }
    Ok(path)
}
pub(crate) fn app_state_dir() -> io::Result<PathBuf> {
    Ok(user_state_dir()?.join("NovaGPT"))
}

pub fn path() -> io::Result<PathBuf> {
    Ok(app_state_dir()?.join("tokens").join(SHARED_NAME))
}
fn read_token(file: &Path) -> io::Result<String> {
    private_file(file)?;
    let mut bytes = Vec::with_capacity(64);
    fs::File::open(file)?
        .take(FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > FILE_BYTES {
        return Err(invalid("oversized HTTP token file"));
    }
    let text = String::from_utf8(bytes).map_err(|_| invalid("invalid UTF-8 token file"))?;
    validate_saved(&text)?;
    Ok(text)
}
/// Internal target helper allows deterministic tests without writing to the real user profile.
fn ensure_token_directory(file: &Path) -> io::Result<()> {
    let dir = file
        .parent()
        .ok_or_else(|| invalid("missing token parent directory"))?;
    let app_dir = dir
        .parent()
        .ok_or_else(|| invalid("missing application state directory"))?;
    // The base is OS-managed; NovaGPT's directories must remain private.
    let state_base = app_dir
        .parent()
        .ok_or_else(|| invalid("missing user state directory"))?;
    fs::create_dir_all(state_base)?;
    private_directory(app_dir)?;
    private_directory(dir)
}

/// Persist a caller-selected token in private per-user state, never in
/// config.json. A new random temporary file is synced before atomic rename;
/// failed replacements leave the previous token untouched.
fn set_at(file: &Path, value: &str, replace: bool) -> io::Result<()> {
    validate_saved(value)?;
    ensure_token_directory(file)?;

    match fs::symlink_metadata(file) {
        Ok(_) => {
            private_file(file)?;
            if !replace {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "shared token already exists; use --replace to overwrite it",
                ));
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let dir = file
        .parent()
        .ok_or_else(|| invalid("missing token parent directory"))?;
    let temporary = dir.join(format!(".{}.tmp", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let outcome = (|| -> io::Result<()> {
        let mut handle = options.open(&temporary)?;
        handle.write_all(value.as_bytes())?;
        handle.sync_all()?;
        drop(handle);
        // Recheck target type before replacing: never follow a symlink.
        match fs::symlink_metadata(file) {
            Ok(_) => {
                private_file(file)?;
                if !replace {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "shared token created concurrently; use --replace",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::rename(&temporary, file)
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    outcome
}

fn ensure_at(file: &Path) -> io::Result<Resolved> {
    ensure_token_directory(file)?;
    let generated = generate();
    let create = {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(file)
    };
    match create {
        Ok(mut handle) => {
            // The destination is create_new: never replace or rotate a pre-existing token.
            handle.write_all(generated.as_bytes())?;
            handle.sync_all()?;
            Ok(Resolved {
                value: generated,
                origin: Origin::Stored {
                    path: file.into(),
                    created: true,
                },
                migration: Migration::None,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // Another concurrent process may still be writing the newly-created file.
            for _ in 0..20 {
                match read_token(file) {
                    Ok(value) => {
                        return Ok(Resolved {
                            value,
                            origin: Origin::Stored {
                                path: file.into(),
                                created: false,
                            },
                            migration: Migration::None,
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                        private_file(file)?; // fail immediately for symlink, permissions or unexpected type
                        let meta = fs::metadata(file)?;
                        // create_new publishes the empty file before its contents are
                        // written. A short read can therefore be an in-flight write,
                        // not necessarily a corrupted persisted token.
                        // A full-length malformed token must still fail closed.
                        if meta.len() >= 64 {
                            return Err(err);
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(err) => return Err(err),
                }
            }
            Err(invalid("token file creation did not finish"))
        }
        Err(error) => Err(error),
    }
}
/// Locking prevents concurrent NovaGPT processes from choosing different
/// shared tokens during migration or replacement. The file is never unlinked:
/// waiting processes must not accidentally lock separate inodes.
fn with_token_lock<T>(file: &Path, action: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    ensure_token_directory(file)?;
    let lock_path = file
        .parent()
        .ok_or_else(|| invalid("missing token directory"))?
        .join(MIGRATION_LOCK);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let guard = match options.open(&lock_path) {
        Ok(guard) => guard,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            private_file(&lock_path)?;
            OpenOptions::new().read(true).write(true).open(&lock_path)?
        }
        Err(error) => return Err(error),
    };
    private_file(&lock_path)?;
    guard.lock_exclusive()?;
    action()
}

/// Only consider old NovaGPT workspace-token filenames. Other files stay intact.
fn legacy_files(file: &Path) -> io::Result<Vec<PathBuf>> {
    let dir = file
        .parent()
        .ok_or_else(|| invalid("missing token directory"))?;
    let mut legacy = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|name| name.strip_suffix(".token")) else {
            continue;
        };
        if stem.len() == 64 && stem.bytes().all(|b| b.is_ascii_hexdigit()) {
            legacy.push(entry.path());
        }
    }
    legacy.sort();
    Ok(legacy)
}

/// Install the common token before removing legacy files. If interrupted,
/// the existing common token takes precedence and cleanup can safely resume.
fn migrate_at(file: &Path) -> io::Result<Migration> {
    let legacy = legacy_files(file)?;
    // Check every old file before changing anything: reject unsafe paths,
    // malformed credentials and loose permissions rather than deleting them.
    let mut preserved = None;
    for old in &legacy {
        let value = read_token(old)?;
        if legacy.len() == 1 {
            preserved = Some(value);
        }
    }
    let migration = match fs::symlink_metadata(file) {
        Ok(_) => {
            read_token(file)?;
            if legacy.is_empty() {
                Migration::None
            } else {
                Migration::Cleaned
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(value) = preserved {
                set_at(file, &value, false)?;
                Migration::Preserved
            } else if !legacy.is_empty() {
                ensure_at(file)?;
                Migration::Rotated
            } else {
                Migration::None
            }
        }
        Err(error) => return Err(error),
    };
    for old in legacy {
        // Never follow a symlink introduced after the initial scan.
        private_file(&old)?;
        fs::remove_file(old)?;
    }
    Ok(migration)
}

/// Set one token for all workspaces owned by this OS user.
pub fn set(value: &str, replace: bool) -> io::Result<()> {
    validate_saved(value)?;
    let file = path()?;
    with_token_lock(&file, || {
        migrate_at(&file)?;
        set_at(&file, value, replace)
    })
}

/// Read one token from a pipe without echoing it or passing it in argv.
/// Only an optional final CRLF/LF is removed; extra lines are rejected.
fn parse_piped_token(input: &str) -> io::Result<&str> {
    let value = input
        .strip_suffix("\r\n")
        .or_else(|| input.strip_suffix('\n'))
        .unwrap_or(input);
    validate_saved(value)?;
    Ok(value)
}

pub fn read_from_stdin() -> io::Result<String> {
    use std::io::IsTerminal;
    if io::stdin().is_terminal() {
        return Err(invalid("--stdin needs a pipe, not an interactive terminal"));
    }
    let mut data = String::new();
    io::stdin().take(FILE_BYTES + 3).read_to_string(&mut data)?;
    Ok(parse_piped_token(&data)?.to_owned())
}

/// Read a secret using Crossterm raw terminal mode, never echoing the value.
/// Raw mode is restored on return/error/cancellation.
pub fn prompt_secret() -> io::Result<String> {
    use crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        terminal::enable_raw_mode,
    };
    use std::io::IsTerminal;
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(invalid(
            "interactive token entry needs a terminal; use token set --stdin for pipes",
        ));
    }
    eprint!("Inserisci il token (64 caratteri esadecimali, input nascosto): ");
    io::stderr().flush()?;
    enable_raw_mode()?;
    struct RawModeGuard;
    impl Drop for RawModeGuard {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    let guard = RawModeGuard;
    let mut secret = String::new();
    let result = loop {
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Enter => break parse_piped_token(&secret).map(str::to_owned),
                KeyCode::Esc => break Err(invalid("token entry cancelled")),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    break Err(invalid("token entry cancelled"));
                }
                KeyCode::Backspace => {
                    secret.pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                        && secret.len() < FILE_BYTES as usize =>
                {
                    secret.push(ch);
                }
                _ => {}
            },
            _ => {}
        }
    };
    drop(guard);
    eprintln!();
    // Do not print the submitted token even on validation failure.
    result
}

pub fn resolve() -> io::Result<Resolved> {
    if let Some(value) = env::var_os("NOVAGPT_HTTP_TOKEN") {
        let value = value
            .into_string()
            .map_err(|_| invalid("invalid NOVAGPT_HTTP_TOKEN encoding"))?;
        validate(&value)?;
        return Ok(Resolved {
            value,
            origin: Origin::Environment,
            migration: Migration::None,
        });
    }
    let file = path()?;
    with_token_lock(&file, || {
        let migration = migrate_at(&file)?;
        let mut resolved = ensure_at(&file)?;
        resolved.migration = migration;
        Ok(resolved)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_token_is_stable_strong_and_not_in_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp
            .path()
            .join("NovaGPT")
            .join("tokens")
            .join("fixture.token");
        let first = ensure_at(&file).unwrap();
        let second = ensure_at(&file).unwrap();
        assert_eq!(first.value.len(), 64);
        assert!(first.value.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(first.value, second.value);
        assert!(!first.description().contains(&first.value));
        assert!(matches!(first.origin, Origin::Stored { created: true, .. }));
        assert!(matches!(
            second.origin,
            Origin::Stored { created: false, .. }
        ));
        assert!(!temp.path().join(".novagpt").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(file.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }
    #[test]
    fn chosen_token_persists_and_requires_explicit_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp
            .path()
            .join("NovaGPT")
            .join("tokens")
            .join("chosen.token");
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        set_at(&file, &first, false).unwrap();
        assert_eq!(read_token(&file).unwrap(), first);
        assert_eq!(
            set_at(&file, &second, false).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(read_token(&file).unwrap(), first);
        set_at(&file, &second, true).unwrap();
        assert_eq!(read_token(&file).unwrap(), second);
        assert_eq!(
            fs::read_dir(file.parent().unwrap()).unwrap().count(),
            1,
            "no temporary token file should remain"
        );
    }
    #[test]
    fn invalid_custom_token_does_not_create_or_replace_secret() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp
            .path()
            .join("NovaGPT")
            .join("tokens")
            .join("chosen.token");
        for value in ["short", "z", "a\\nb"] {
            assert!(set_at(&file, value, true).is_err());
            assert!(!file.exists());
        }
        let original = "e".repeat(64);
        set_at(&file, &original, false).unwrap();
        assert!(set_at(&file, &"Z".repeat(64), true).is_err());
        assert_eq!(read_token(&file).unwrap(), original);
    }
    #[test]
    fn piped_secret_parsing_accepts_one_line_and_rejects_extra_input() {
        let secret = "c".repeat(64);
        assert_eq!(parse_piped_token(&format!("{secret}\r\n")).unwrap(), secret);
        assert_eq!(parse_piped_token(&format!("{secret}\n")).unwrap(), secret);
        assert_eq!(parse_piped_token(&secret).unwrap(), secret);
        assert!(parse_piped_token(&format!("{secret}\nextra")).is_err());
        assert!(parse_piped_token(&format!("{secret}\n\n")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn custom_token_refuses_symlink_overwrite() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("NovaGPT").join("tokens");
        fs::create_dir(temp.path().join("NovaGPT")).unwrap();
        fs::create_dir(&dir).unwrap();
        let outside = temp.path().join("outside");
        fs::write(&outside, "unchanged").unwrap();
        let file = dir.join("chosen.token");
        symlink(&outside, &file).unwrap();
        assert!(set_at(&file, &"f".repeat(64), true).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "unchanged");
    }
    #[test]
    fn malformed_file_is_not_silently_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp
            .path()
            .join("NovaGPT")
            .join("tokens")
            .join("fixture.token");
        let previous = ensure_at(&file).unwrap().value;
        fs::write(&file, "invalid").unwrap();
        assert!(ensure_at(&file).is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "invalid");
        assert_ne!(previous, "invalid");
    }
    #[test]
    fn parallel_initializers_reuse_one_token() {
        let temp = tempfile::tempdir().unwrap();
        let file = temp
            .path()
            .join("NovaGPT")
            .join("tokens")
            .join("fixture.token");
        let workers = (0..10)
            .map(|_| {
                let file = file.clone();
                thread::spawn(move || ensure_at(&file).unwrap().value)
            })
            .collect::<Vec<_>>();
        let values = workers
            .into_iter()
            .map(|h| h.join().unwrap())
            .collect::<Vec<_>>();
        assert!(values.iter().all(|token| token == &values[0]));
    }
    #[test]
    fn rejects_weak_tokens_and_controls() {
        for raw in [
            "short",
            "",
            "with whitespace and linebreak \n",
            &"x".repeat(260),
        ] {
            assert!(validate(raw).is_err());
        }
        assert!(validate(&"x".repeat(24)).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_secret_is_never_read_or_overwritten() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("NovaGPT");
        let parent = home.join("tokens");
        fs::create_dir(&home).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(&parent).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
        let target = dir.path().join("other");
        fs::write(&target, "sensitive").unwrap();
        let path = parent.join("fixture.token");
        symlink(&target, &path).unwrap();
        assert!(ensure_at(&path).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "sensitive");
    }
}
