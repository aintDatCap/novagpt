//! Per-workspace HTTP secrets in user-private state, never in a project or config.json.
//! A missing secret is generated once; environment override always takes precedence.
use sha2::{Digest, Sha256};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    Environment,
    Stored { path: PathBuf, created: bool },
}
pub struct Resolved {
    pub value: String,
    pub origin: Origin,
}
impl Resolved {
    pub fn description(&self) -> String {
        match &self.origin {
            Origin::Environment => {
                "Token HTTP: uso NOVAGPT_HTTP_TOKEN dall'ambiente (valore nascosto).".into()
            }
            Origin::Stored { path, created } => format!(
                "Token HTTP {}: {} (valore nascosto). Per visualizzarlo: novagpt token show",
                if *created {
                    "generato"
                } else {
                    "gia' esistente"
                },
                path.display()
            ),
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
pub fn path(root: &Path) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(root)?;
    if !canonical.is_dir() {
        return Err(invalid("workspace is not a directory"));
    }
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    Ok(user_state_dir()?
        .join("NovaGPT")
        .join("tokens")
        .join(format!("{:x}.token", digest)))
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
fn ensure_at(file: &Path) -> io::Result<Resolved> {
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
    private_directory(dir)?;
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
                        });
                    }
                    Err(err) if err.kind() == io::ErrorKind::InvalidData => {
                        private_file(file)?; // fail immediately for symlink, permissions or unexpected type
                        let meta = fs::metadata(file)?;
                        if meta.len() != 0 {
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
pub fn resolve(root: &Path) -> io::Result<Resolved> {
    if let Some(value) = env::var_os("NOVAGPT_HTTP_TOKEN") {
        let value = value
            .into_string()
            .map_err(|_| invalid("invalid NOVAGPT_HTTP_TOKEN encoding"))?;
        validate(&value)?;
        return Ok(Resolved {
            value,
            origin: Origin::Environment,
        });
    }
    ensure_at(&path(root)?)
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
