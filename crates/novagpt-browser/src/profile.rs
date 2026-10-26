//! Exclusive ownership of the dedicated NovaGPT browser profile.
//! A lock file stays open for the lifetime of the Chrome process. The lock
//! is released by the OS after a crash (unlike a persistent sentinel lock).
use crate::BrowserError;
use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
};

const OWNER: &[u8] = b"NovaGPT dedicated ChatGPT Browser profile v1\n";
const LOCK_FILE: &str = ".novagpt-browser.lock";

pub(crate) struct ProfileLease {
    _lock: File,
}

fn redirected(path: &Path) -> Result<bool, BrowserError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| BrowserError::InvalidProfile)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Windows directory junctions are reparse points even if not symlinks.
        if metadata.file_attributes() & 0x400 != 0 {
            return Ok(true);
        }
    }
    Ok(metadata.file_type().is_symlink())
}

pub(crate) fn acquire(profile: &Path) -> Result<ProfileLease, BrowserError> {
    if profile.as_os_str().is_empty() || !profile.is_absolute() {
        return Err(BrowserError::InvalidProfile);
    }
    if profile.exists() && redirected(profile)? {
        return Err(BrowserError::InvalidProfile);
    }
    fs::create_dir_all(profile).map_err(|_| BrowserError::InvalidProfile)?;
    if redirected(profile)? {
        return Err(BrowserError::InvalidProfile);
    }
    let marker = profile.join("novagpt-browser-owner");
    if marker.exists() {
        if redirected(&marker)?
            || fs::read(&marker).map_err(|_| BrowserError::InvalidProfile)? != OWNER
        {
            return Err(BrowserError::InvalidProfile);
        }
    } else {
        let mut entries = fs::read_dir(profile).map_err(|_| BrowserError::InvalidProfile)?;
        if entries.next().is_some() {
            // Never take over an arbitrary existing profile.
            return Err(BrowserError::InvalidProfile);
        }
        let mut owner = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .map_err(|_| BrowserError::InvalidProfile)?;
        use std::io::Write;
        owner
            .write_all(OWNER)
            .map_err(|_| BrowserError::InvalidProfile)?;
        owner.sync_all().map_err(|_| BrowserError::InvalidProfile)?;
    }
    let lock_path = profile.join(LOCK_FILE);
    if lock_path.exists() && redirected(&lock_path)? {
        return Err(BrowserError::InvalidProfile);
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|_| BrowserError::InvalidProfile)?;
    lock.try_lock_exclusive()
        .map_err(|_| BrowserError::ProfileBusy)?;
    Ok(ProfileLease { _lock: lock })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn profile_is_owned_and_exclusive_without_deleting_user_data() {
        let root = tempfile::tempdir().expect("temp profile");
        let profile = root.path().join("dedicated");
        let lease = acquire(&profile).expect("first lease");
        assert!(matches!(acquire(&profile), Err(BrowserError::ProfileBusy)));
        fs::write(profile.join("saved-session-fixture"), "kept").expect("fixture");
        drop(lease);
        let second = acquire(&profile).expect("OS lock released");
        assert_eq!(
            fs::read_to_string(profile.join("saved-session-fixture")).unwrap(),
            "kept"
        );
        drop(second);
    }
    #[test]
    fn never_take_over_existing_or_redirected_profile() {
        let root = tempfile::tempdir().expect("root");
        let personal = root.path().join("personal");
        fs::create_dir_all(&personal).unwrap();
        fs::write(personal.join("Preferences"), "{}").unwrap();
        assert!(matches!(
            acquire(&personal),
            Err(BrowserError::InvalidProfile)
        ));
        let owned = root.path().join("owned");
        fs::create_dir_all(&owned).unwrap();
        fs::write(owned.join("novagpt-browser-owner"), "another owner").unwrap();
        assert!(matches!(acquire(&owned), Err(BrowserError::InvalidProfile)));
    }
}
