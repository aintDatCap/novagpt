//! Exclusive ownership of the dedicated NovaGPT browser profile.
//! A lock file stays open for the lifetime of the Chrome process. The lock
//! is released by the OS after a crash (unlike a persistent sentinel lock).
use super::error::UpstreamError;
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

fn redirected(path: &Path) -> Result<bool, UpstreamError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| UpstreamError::InvalidProfile)?;
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

pub(crate) fn acquire(profile: &Path) -> Result<ProfileLease, UpstreamError> {
    if profile.as_os_str().is_empty() || !profile.is_absolute() {
        return Err(UpstreamError::InvalidProfile);
    }
    if profile.exists() && redirected(profile)? {
        return Err(UpstreamError::InvalidProfile);
    }
    fs::create_dir_all(profile).map_err(|_| UpstreamError::InvalidProfile)?;
    if redirected(profile)? {
        return Err(UpstreamError::InvalidProfile);
    }
    let marker = profile.join("novagpt-browser-owner");
    if marker.exists() {
        if redirected(&marker)?
            || fs::read(&marker).map_err(|_| UpstreamError::InvalidProfile)? != OWNER
        {
            return Err(UpstreamError::InvalidProfile);
        }
    } else {
        let mut entries = fs::read_dir(profile).map_err(|_| UpstreamError::InvalidProfile)?;
        if entries.next().is_some() {
            // Never take over an arbitrary existing profile.
            return Err(UpstreamError::InvalidProfile);
        }
        let mut owner = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .map_err(|_| UpstreamError::InvalidProfile)?;
        use std::io::Write;
        owner
            .write_all(OWNER)
            .map_err(|_| UpstreamError::InvalidProfile)?;
        owner
            .sync_all()
            .map_err(|_| UpstreamError::InvalidProfile)?;
    }
    let lock_path = profile.join(LOCK_FILE);
    if lock_path.exists() && redirected(&lock_path)? {
        return Err(UpstreamError::InvalidProfile);
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|_| UpstreamError::InvalidProfile)?;
    // A concurrent `fork` in another thread can briefly inherit this lock's
    // file description before exec closes it (close-on-exec). Retry briefly so
    // a transient inherited descriptor is not reported as another owner.
    let mut attempts = 0;
    loop {
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(_) if attempts < 10 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(_) => return Err(UpstreamError::ProfileBusy),
        }
    }
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
        assert!(matches!(acquire(&profile), Err(UpstreamError::ProfileBusy)));
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
            Err(UpstreamError::InvalidProfile)
        ));
        let owned = root.path().join("owned");
        fs::create_dir_all(&owned).unwrap();
        fs::write(owned.join("novagpt-browser-owner"), "another owner").unwrap();
        assert!(matches!(
            acquire(&owned),
            Err(UpstreamError::InvalidProfile)
        ));
    }
}
