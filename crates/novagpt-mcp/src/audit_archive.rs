//! Compressed audit history. Active events remain plain text for crash recovery;
//! closed segments use zstd, with a hard total storage budget and 90-day expiry.
//! All reads validate private regular files and the canonical event grammar.
use super::{
    AuditEvent, AuditState, MAX_EVENTS, MAX_FILE_BYTES, MAX_FILES, checked_file, file_name,
    invalid, parse_event,
};
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

pub(super) const MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
const ARCHIVE_BUDGET: u64 = MAX_TOTAL_BYTES - MAX_FILE_BYTES * MAX_FILES as u64;
const MAX_ARCHIVE_FILE_BYTES: u64 = MAX_FILE_BYTES + 16 * 1024;
const MAX_ARCHIVES: usize = 4096;
const RETENTION: Duration = Duration::from_secs(90 * 24 * 60 * 60);
const TEMP_NAME: &str = "audit.pending.zst.tmp";

#[derive(Debug)]
struct Segment {
    sequence: u64,
    variant: u8,
    path: PathBuf,
    len: u64,
    modified: SystemTime,
}

fn archive_name(sequence: u64, variant: u8) -> String {
    format!("audit-{sequence:020}-{variant:02}.zst")
}

fn parse_archive_name(name: &str) -> Option<(u64, u8)> {
    let body = name.strip_prefix("audit-")?.strip_suffix(".zst")?;
    let (sequence, variant) = body.split_once('-')?;
    if sequence.len() != 20
        || variant.len() != 2
        || !sequence.bytes().all(|b| b.is_ascii_digit())
        || !variant.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((sequence.parse().ok()?, variant.parse().ok()?))
}

fn checked_archive(path: &Path) -> io::Result<fs::Metadata> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_ARCHIVE_FILE_BYTES {
        return Err(invalid("Unsafe or oversized compressed audit file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(invalid("Compressed audit file must be private"));
        }
    }
    Ok(meta)
}

fn archives(dir: &Path) -> io::Result<Vec<Segment>> {
    let mut result = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("audit-") {
            continue;
        }
        let (sequence, variant) =
            parse_archive_name(name).ok_or_else(|| invalid("Invalid compressed audit filename"))?;
        let meta = checked_archive(&entry.path())?;
        result.push(Segment {
            sequence,
            variant,
            path: entry.path(),
            len: meta.len(),
            modified: meta.modified()?,
        });
        if result.len() > MAX_ARCHIVES + 1 {
            return Err(invalid("Too many compressed audit segments"));
        }
    }
    result.sort_by_key(|seg| (seg.sequence, seg.variant));
    Ok(result)
}

/// Incomplete temporary compression is not trusted and can be discarded:
/// the original audit.log is deleted only after an archive was published.
pub(super) fn recover_temporary(dir: &Path) -> io::Result<()> {
    let temp = dir.join(TEMP_NAME);
    match fs::symlink_metadata(&temp) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => fs::remove_file(temp),
        Ok(_) => Err(invalid("Unsafe incomplete audit archive")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn plain_events(path: &Path) -> io::Result<Vec<AuditEvent>> {
    let Some(len) = checked_file(path)? else {
        return Ok(Vec::new());
    };
    let mut bytes = Vec::with_capacity(len as usize);
    fs::File::open(path)?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid("Audit segment exceeded read bound"));
    }
    Ok(bytes
        .split(|b| *b == b'\n')
        .filter_map(parse_event)
        .collect())
}

fn compressed_events(path: &Path) -> io::Result<Vec<AuditEvent>> {
    checked_archive(path)?;
    let mut decoded = zstd::stream::read::Decoder::new(fs::File::open(path)?)?;
    let mut bytes = Vec::new();
    decoded
        .by_ref()
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(invalid("Decompressed audit segment exceeded read bound"));
    }
    Ok(bytes
        .split(|b| *b == b'\n')
        .filter_map(parse_event)
        .collect())
}

/// Read the newest bounded number of events without inflating the whole archive.
/// File name sequences preserve monotonic counts after restart and compaction.
pub(super) fn recent(dir: &Path, limit: usize) -> io::Result<AuditState> {
    let limit = limit.max(1);
    let segments = archives(dir)?;
    let mut count = segments.iter().map(|s| s.sequence).max().unwrap_or(0);
    let mut newest = Vec::with_capacity(limit.min(1000));
    fn push_recent(
        events: Vec<AuditEvent>,
        newest: &mut Vec<AuditEvent>,
        count: &mut u64,
        limit: usize,
    ) {
        for event in events.into_iter().rev() {
            *count = (*count).max(event.sequence);
            if newest.len() < limit && !newest.iter().any(|e| e.sequence == event.sequence) {
                newest.push(event);
            }
        }
    }
    push_recent(
        plain_events(&dir.join(file_name(0)))?,
        &mut newest,
        &mut count,
        limit,
    );
    for segment in segments.iter().rev() {
        if newest.len() >= limit {
            break;
        }
        push_recent(
            compressed_events(&segment.path)?,
            &mut newest,
            &mut count,
            limit,
        );
    }
    for index in 1..MAX_FILES {
        if newest.len() >= limit {
            break;
        }
        push_recent(
            plain_events(&dir.join(file_name(index)))?,
            &mut newest,
            &mut count,
            limit,
        );
    }
    newest.sort_by_key(|e| e.sequence);
    Ok(AuditState {
        count,
        events: VecDeque::from(newest),
    })
}

pub(super) fn initialize(dir: &Path) -> io::Result<AuditState> {
    recover_temporary(dir)?;
    prune(dir)?;
    recent(dir, MAX_EVENTS)
}

fn publish_archive(dir: &Path) -> io::Result<()> {
    let active = dir.join(file_name(0));
    let Some(len) = checked_file(&active)? else {
        return Ok(());
    };
    if len == 0 {
        return Ok(());
    }
    let events = plain_events(&active)?;
    let last = events
        .last()
        .ok_or_else(|| invalid("Audit segment contains no valid events"))?;
    let target = (0..100)
        .map(|variant| (archive_name(last.sequence, variant), variant))
        .find(|(name, _)| !dir.join(name).exists())
        .ok_or_else(|| invalid("Audit archive sequence collision"))?;
    let temp = dir.join(TEMP_NAME);
    recover_temporary(dir)?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temp)?;
    let mut input = fs::File::open(&active)?.take(MAX_FILE_BYTES + 1);
    let result = zstd::stream::copy_encode(&mut input, &mut output, 3);
    if let Err(error) = result {
        let _ = fs::remove_file(temp);
        return Err(error);
    }
    output.flush()?;
    output.sync_data()?;
    if checked_archive(&temp)?.len() > MAX_ARCHIVE_FILE_BYTES {
        return Err(invalid("Compressed audit segment exceeded limit"));
    }
    fs::rename(&temp, dir.join(target.0))?;
    fs::remove_file(&active)?;
    Ok(())
}

pub(super) fn rotate(dir: &Path) -> io::Result<()> {
    publish_archive(dir)?;
    prune(dir)
}

/// Keep compressed segments within the budget, reserving room for active and
/// legacy text files. This enforces 100 MiB total even when audit.log fills.
pub(super) fn prune(dir: &Path) -> io::Result<()> {
    prune_to(dir, ARCHIVE_BUDGET, SystemTime::now())
}

fn prune_to(dir: &Path, budget: u64, now: SystemTime) -> io::Result<()> {
    let segments = archives(dir)?;
    let mut total: u64 = segments.iter().map(|s| s.len).sum();
    for seg in segments {
        let expired = now
            .duration_since(seg.modified)
            .is_ok_and(|age| age > RETENTION);
        if expired || total > budget {
            checked_archive(&seg.path)?;
            fs::remove_file(&seg.path)?;
            total = total.saturating_sub(seg.len);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compressed_segments_roundtrip_and_budget() {
        let temp = tempfile::tempdir().expect("fixture");
        let dir = temp.path();
        let source = dir.join("audit.log");
        let text: String = (1..=500)
            .map(|i| format!("event={i} timestamp_unix=1 tool=read status=ok\n"))
            .collect();
        fs::write(&source, text).expect("active log");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&source, fs::Permissions::from_mode(0o600))
                .expect("private fixture");
        }
        rotate(dir).expect("compress");
        let compressed = archives(dir).expect("archives");
        assert_eq!(compressed.len(), 1);
        assert_eq!(
            fs::read(&compressed[0].path).expect("compressed file")[..4],
            [0x28, 0xb5, 0x2f, 0xfd]
        );
        let recent = recent(dir, 500).expect("all events");
        assert_eq!(recent.count, 500);
        assert_eq!(recent.events.len(), 500);
        prune_to(dir, 0, SystemTime::now()).expect("zero quota");
        assert!(archives(dir).expect("pruned").is_empty());
    }
    #[test]
    fn decompression_is_bounded_and_symlinks_are_denied() {
        let temp = tempfile::tempdir().expect("fixture");
        let path = temp.path().join(archive_name(1, 0));
        let text = vec![b'A'; MAX_FILE_BYTES as usize + 100];
        fs::write(
            &path,
            zstd::stream::encode_all(&text[..], 1).expect("compress"),
        )
        .expect("fixture");
        assert!(compressed_events(&path).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_file(&path).expect("remove");
            symlink(temp.path().join("elsewhere"), &path).expect("link");
            assert!(archives(temp.path()).is_err());
        }
    }
    #[test]
    fn stale_archives_are_expired() {
        let temp = tempfile::tempdir().expect("fixture");
        let dir = temp.path();
        fs::write(
            dir.join("audit.log"),
            b"event=1 timestamp_unix=1 tool=read status=ok\n",
        )
        .expect("fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join("audit.log"), fs::Permissions::from_mode(0o600))
                .expect("private fixture");
        }
        rotate(dir).expect("rotate");
        let future = SystemTime::now() + RETENTION + Duration::from_secs(1);
        prune_to(dir, ARCHIVE_BUDGET, future).expect("prune age");
        assert!(archives(dir).expect("archives").is_empty());
    }
}
