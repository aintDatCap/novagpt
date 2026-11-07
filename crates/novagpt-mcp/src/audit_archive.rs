//! Compressed audit history. Active events remain plain text for crash recovery;
//! closed segments use zstd, with a hard total storage budget and 90-day expiry.
//! All reads validate private regular files and the canonical event grammar.
use super::{
    ACTIVE_LOG, AuditEvent, AuditState, MAX_EVENTS, MAX_FILE_BYTES, checked_file, format_event,
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
const ARCHIVE_BUDGET: u64 = MAX_TOTAL_BYTES - MAX_FILE_BYTES - MAX_ARCHIVE_FILE_BYTES;
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
        plain_events(&dir.join(ACTIVE_LOG))?,
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
    let active = dir.join(ACTIVE_LOG);
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

/// Keep compressed segments within the budget, reserving room for the active
/// plain-text file and one temporary compression file. This bounds
/// active and archived disk usage even during rotation.
pub(super) fn prune(dir: &Path) -> io::Result<()> {
    prune_to(dir, ARCHIVE_BUDGET, SystemTime::now())
}

/// Export only validated records as a shareable zstd stream (not raw files).
pub(super) fn export_compressed(dir: &Path, destination: &Path) -> io::Result<usize> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let output = options.open(destination)?;
    let result = (|| -> io::Result<usize> {
        let mut encoder = zstd::stream::write::Encoder::new(output, 3)?;
        let mut last_sequence = 0;
        let mut written = 0usize;
        let mut add = |events: Vec<AuditEvent>| -> io::Result<()> {
            for event in events {
                if event.sequence > last_sequence {
                    encoder.write_all(format_event(&event).as_bytes())?;
                    last_sequence = event.sequence;
                    written = written.saturating_add(1);
                }
            }
            Ok(())
        };
        for segment in archives(dir)? {
            add(compressed_events(&segment.path)?)?;
        }
        add(plain_events(&dir.join(ACTIVE_LOG))?)?;
        encoder.finish()?.sync_all()?;
        Ok(written)
    })();
    if result.is_err() {
        let _ = fs::remove_file(destination);
    }
    result
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
    fn exported_bundle_contains_only_valid_metadata_and_will_not_overwrite() {
        let temp = tempfile::tempdir().expect("fixture");
        let dir = temp.path().join("logs");
        fs::create_dir(&dir).expect("log folder");
        let active = dir.join("audit.log");
        let mut contents: String = (1..=600)
            .map(|sequence| format!("event={sequence} timestamp_unix=1 tool=read status=ok\n"))
            .collect();
        contents.push_str("event=601 timestamp_unix=1 tool=read status=ok token=not-for-export\n");
        fs::write(&active, contents).expect("events");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&active, fs::Permissions::from_mode(0o600)).expect("private");
        }
        rotate(&dir).expect("close segment");
        let destination = temp.path().join("support.log.zst");
        let count = export_compressed(&dir, &destination).expect("export");
        assert_eq!(count, 600);
        let bytes = fs::read(&destination).expect("bundle");
        let decoded = zstd::stream::decode_all(&bytes[..]).expect("decode");
        let text = String::from_utf8(decoded).expect("metadata ascii");
        assert!(text.contains("event=1 timestamp_unix=1 tool=read status=ok"));
        assert!(text.contains("event=600 timestamp_unix=1 tool=read status=ok"));
        assert!(!text.contains("not-for-export"));
        assert!(
            export_compressed(&dir, &destination).is_err(),
            "no overwrite"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&destination)
                .expect("bundle")
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
    }
    #[test]
    fn old_rotated_plain_logs_are_not_loaded_or_exported() {
        let tmp = tempfile::tempdir().expect("fixture");
        let dir = tmp.path();
        fs::write(
            dir.join("audit.1.log"),
            "event=1 timestamp_unix=1 tool=read status=ok\n",
        )
        .expect("old rotated file");
        fs::write(
            dir.join(ACTIVE_LOG),
            "event=2 timestamp_unix=2 tool=write status=error\n",
        )
        .expect("current active file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join(ACTIVE_LOG), fs::Permissions::from_mode(0o600))
                .expect("private active file");
        }
        let result = recent(dir, 50).expect("new format only");
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].sequence, 2);
        let export = tmp.path().join("support.zst");
        assert_eq!(export_compressed(dir, &export).expect("export"), 1);
        let decoded =
            zstd::stream::decode_all(&fs::read(export).expect("bundle")[..]).expect("decode");
        assert!(!String::from_utf8_lossy(&decoded).contains("event=1 "));
        assert!(
            dir.join("audit.1.log").exists(),
            "no automatic deletion of user history"
        );
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
