//! Bounded persistent metadata-only audit. No tool arguments, paths, prompts,
//! returned data or credentials enter the event schema or the disk writer.
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_EVENTS: usize = 128;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MAX_FILES: usize = 4;
const QUEUE_CAPACITY: usize = 512;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AuditEvent {
    pub sequence: u64,
    pub unix_seconds: u64,
    pub tool: String,
    pub status: String,
}

#[derive(Debug, Serialize)]
pub struct AuditSnapshot {
    pub mode: &'static str,
    pub total_calls: u64,
    pub returned_events: usize,
    pub dropped_events: u64,
    pub write_errors: u64,
    pub human: Vec<String>,
    pub llm: Vec<String>,
}

#[derive(Default)]
struct AuditState {
    count: u64,
    events: VecDeque<AuditEvent>,
}

enum WriteCommand {
    Event(AuditEvent),
    Flush(mpsc::Sender<()>),
}

#[derive(Clone, Default)]
pub struct AuditTrail {
    state: Arc<Mutex<AuditState>>,
    writer: Option<SyncSender<WriteCommand>>,
    dropped: Arc<AtomicU64>,
    write_errors: Arc<AtomicU64>,
}

fn invalid(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, reason)
}

fn private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_dir() {
                return Err(invalid("Unsafe audit directory"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    return Err(invalid("Audit directory must have private permissions"));
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(path)?;
            }
            #[cfg(not(unix))]
            fs::create_dir(path)?;
            private_dir(path)
        }
        Err(error) => Err(error),
    }
}

fn checked_file(path: &Path) -> io::Result<Option<u64>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                return Err(invalid("Unsafe or oversized audit file"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    return Err(invalid("Audit file must have private permissions"));
                }
            }
            Ok(Some(meta.len()))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn file_name(index: usize) -> &'static str {
    match index {
        0 => "audit.log",
        1 => "audit.1.log",
        2 => "audit.2.log",
        _ => "audit.3.log",
    }
}

fn valid_record(event: &AuditEvent) -> bool {
    event.sequence > 0
        && matches!(event.status.as_str(), "ok" | "error")
        && !event.tool.is_empty()
        && event.tool.len() <= 64
        && event
            .tool
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Plain English-readable audit format, with fixed labels and bounded fields.
fn format_event(event: &AuditEvent) -> String {
    format!(
        "event={} timestamp_unix={} tool={} status={}\n",
        event.sequence, event.unix_seconds, event.tool, event.status
    )
}

fn parse_event(line: &[u8]) -> Option<AuditEvent> {
    // Reject noncanonical lines, extra fields and untrusted content rather than
    // interpreting arbitrary text as a valid tool event.
    let text = std::str::from_utf8(line).ok()?;
    let mut fields = text.split_ascii_whitespace();
    let sequence = fields.next()?.strip_prefix("event=")?.parse().ok()?;
    let unix_seconds = fields
        .next()?
        .strip_prefix("timestamp_unix=")?
        .parse()
        .ok()?;
    let tool = fields.next()?.strip_prefix("tool=")?.to_string();
    let status = fields.next()?.strip_prefix("status=")?.to_string();
    if fields.next().is_some() {
        return None;
    }
    let event = AuditEvent {
        sequence,
        unix_seconds,
        tool,
        status,
    };
    valid_record(&event).then_some(event)
}

/// Loads at most 4 x 64 KiB, without following symlinks or trusting disk records
/// to be displayed without schema validation.
fn history(dir: &Path) -> io::Result<AuditState> {
    let mut state = AuditState::default();
    for index in (0..MAX_FILES).rev() {
        let path = dir.join(file_name(index));
        let Some(len) = checked_file(&path)? else {
            continue;
        };
        let mut bytes = Vec::with_capacity(len as usize);
        fs::File::open(&path)?
            .take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(invalid("Audit file exceeded read bound"));
        }
        for line in bytes.split(|b| *b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Some(event) = parse_event(line) {
                state.count = state.count.max(event.sequence);
                if state.events.len() == MAX_EVENTS {
                    state.events.pop_front();
                }
                state.events.push_back(event);
            }
        }
    }
    Ok(state)
}

fn rotate(dir: &Path) -> io::Result<()> {
    for index in 0..MAX_FILES {
        checked_file(&dir.join(file_name(index)))?;
    }
    let oldest = dir.join(file_name(MAX_FILES - 1));
    if checked_file(&oldest)?.is_some() {
        fs::remove_file(&oldest)?;
    }
    for index in (1..MAX_FILES).rev() {
        let from = dir.join(file_name(index - 1));
        if checked_file(&from)?.is_some() {
            fs::rename(from, dir.join(file_name(index)))?;
        }
    }
    Ok(())
}

fn append(dir: &Path, event: &AuditEvent) -> io::Result<()> {
    let line = format_event(event).into_bytes();
    let current = dir.join(file_name(0));
    if checked_file(&current)?
        .unwrap_or(0)
        .saturating_add(line.len() as u64)
        > MAX_FILE_BYTES
    {
        rotate(dir)?;
    }
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&current)?;
    file.write_all(&line)?;
    file.sync_data()?;
    Ok(())
}

fn writer_loop(dir: PathBuf, rx: mpsc::Receiver<WriteCommand>, errors: Arc<AtomicU64>) {
    for command in rx {
        match command {
            WriteCommand::Event(event) => {
                if append(&dir, &event).is_err() {
                    errors.fetch_add(1, Ordering::Relaxed);
                }
            }
            WriteCommand::Flush(reply) => {
                let _ = reply.send(());
            }
        }
    }
}

impl AuditTrail {
    /// The caller must provide an application-owned location outside any project.
    /// Initialization fails closed on symlinks, oversized files and unsafe Unix modes.
    /// A single NovaGPT server process owns a given audit directory.
    pub fn persistent(directory: PathBuf) -> io::Result<Self> {
        let parent = directory
            .parent()
            .ok_or_else(|| invalid("Missing audit parent"))?;
        let grandparent = parent
            .parent()
            .ok_or_else(|| invalid("Missing application state"))?;
        private_dir(grandparent)?;
        private_dir(parent)?;
        private_dir(&directory)?;
        let initial = history(&directory)?;
        let (writer, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let errors = Arc::new(AtomicU64::new(0));
        let worker_errors = errors.clone();
        std::thread::Builder::new()
            .name("novagpt-audit".into())
            .spawn(move || writer_loop(directory, receiver, worker_errors))?;
        Ok(Self {
            state: Arc::new(Mutex::new(initial)),
            writer: Some(writer),
            dropped: Arc::new(AtomicU64::new(0)),
            write_errors: errors,
        })
    }

    pub fn record(&self, tool: &'static str, failed: bool) {
        // Only internal static tool identifiers are accepted, never user-controlled strings.
        if tool.is_empty()
            || tool.len() > 64
            || !tool
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            state.count = state.count.saturating_add(1);
            let event = AuditEvent {
                sequence: state.count,
                unix_seconds: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |value| value.as_secs()),
                tool: tool.to_string(),
                status: if failed { "error" } else { "ok" }.to_string(),
            };
            if state.events.len() == MAX_EVENTS {
                state.events.pop_front();
            }
            state.events.push_back(event.clone());
            if let Some(sender) = self.writer.as_ref()
                && let Err(error) = sender.try_send(WriteCommand::Event(event))
            {
                match error {
                    TrySendError::Full(_) => {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                    TrySendError::Disconnected(_) => {
                        self.write_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
    }

    pub async fn flush(&self) {
        if let Some(sender) = self.writer.clone() {
            let writer = sender;
            let outcome = tokio::task::spawn_blocking(move || {
                let (tx, rx) = mpsc::channel();
                writer.send(WriteCommand::Flush(tx)).is_ok()
                    && rx.recv_timeout(Duration::from_secs(5)).is_ok()
            })
            .await;
            if !matches!(outcome, Ok(true)) {
                self.write_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub async fn snapshot_flushed(&self, limit: usize) -> AuditSnapshot {
        self.flush().await;
        self.snapshot(limit)
    }

    pub fn snapshot(&self, limit: usize) -> AuditSnapshot {
        let Ok(state) = self.state.lock() else {
            return AuditSnapshot {
                mode: "audit-unavailable",
                total_calls: 0,
                returned_events: 0,
                dropped_events: self.dropped.load(Ordering::Relaxed),
                write_errors: self.write_errors.load(Ordering::Relaxed) + 1,
                human: vec!["Audit metadata unavailable".into()],
                llm: Vec::new(),
            };
        };
        let entries: Vec<_> = state
            .events
            .iter()
            .rev()
            .take(limit.clamp(1, MAX_EVENTS))
            .rev()
            .cloned()
            .collect();
        AuditSnapshot {
            mode: if self.writer.is_some() {
                "metadata-only-persistent"
            } else {
                "metadata-only-memory"
            },
            total_calls: state.count,
            returned_events: entries.len(),
            dropped_events: self.dropped.load(Ordering::Relaxed),
            write_errors: self.write_errors.load(Ordering::Relaxed),
            human: entries
                .iter()
                .map(|item| {
                    format!(
                        "Event #{} at UNIX timestamp {}: tool {} completed with status {}.",
                        item.sequence, item.unix_seconds, item.tool, item.status
                    )
                })
                .collect(),
            llm: entries
                .iter()
                .map(|entry| {
                    format!(
                        "event={} timestamp_unix={} tool={} status={}",
                        entry.sequence, entry.unix_seconds, entry.tool, entry.status
                    )
                })
                .collect(),
        }
    }

    /// Local inspection, no writer or service startup. Rejects symlinked log directories.
    pub fn persisted_snapshot(directory: &Path, limit: usize) -> io::Result<AuditSnapshot> {
        let parent = directory
            .parent()
            .ok_or_else(|| invalid("Missing audit parent"))?;
        let app = parent
            .parent()
            .ok_or_else(|| invalid("Missing application state"))?;
        private_dir(app)?;
        private_dir(parent)?;
        private_dir(directory)?;
        let state = history(directory)?;
        let audit = Self {
            state: Arc::new(Mutex::new(state)),
            writer: None,
            dropped: Arc::new(AtomicU64::new(0)),
            write_errors: Arc::new(AtomicU64::new(0)),
        };
        let mut result = audit.snapshot(limit);
        result.mode = "metadata-only-persistent";
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_only_metadata_and_bounds_ring() {
        let audit = AuditTrail::default();
        for _ in 0..180 {
            audit.record("read", false);
        }
        audit.record("write", true);
        let snapshot = audit.snapshot(999);
        assert_eq!(snapshot.total_calls, 181);
        assert_eq!(snapshot.llm.len(), 128);
        assert!(
            snapshot
                .llm
                .last()
                .expect("latest")
                .contains("status=error")
        );
        let text = snapshot.human.join("\n");
        assert!(text.contains("Event #"));
        assert!(text.contains("tool read"));
        assert!(!text.contains("token"));
        assert!(!text.contains("root"));
    }

    #[tokio::test]
    async fn persists_across_reopen_and_limits_rotation() {
        let home = tempfile::tempdir().expect("private fixture");
        let dir = home.path().join("NovaGPT").join("logs").join("fixture");
        let audit = AuditTrail::persistent(dir.clone()).expect("create audit");
        for _ in 0..1100 {
            audit.record("read", false);
        }
        audit.record("write", true);
        let snap = audit.snapshot_flushed(128).await;
        assert_eq!(snap.total_calls, 1101);
        assert_eq!(snap.write_errors, 0);
        // Burst saturation is possible: report rather than silently pretending persisted.
        assert!(snap.dropped_events <= 1101);
        drop(audit);
        let reopened = AuditTrail::persistent(dir.clone()).expect("reopen");
        let persisted = reopened.snapshot(128);
        assert_eq!(persisted.mode, "metadata-only-persistent");
        assert!(persisted.total_calls > 0);
        assert!(fs::metadata(dir.join("audit.log")).expect("log").len() <= MAX_FILE_BYTES);
        let limit_files = fs::read_dir(&dir).expect("dir").count();
        assert!(limit_files <= MAX_FILES);
    }

    #[tokio::test]
    async fn audit_input_is_never_written_and_corrupt_data_is_not_echoed() {
        let tmp = tempfile::tempdir().expect("home");
        let dir = tmp.path().join("NovaGPT").join("logs").join("privacy");
        let audit = AuditTrail::persistent(dir.clone()).expect("audit");
        audit.record("write", true);
        audit.snapshot_flushed(10).await;
        let data = fs::read_to_string(dir.join("audit.log")).expect("plain text audit");
        assert!(data.contains("tool=write status=error"));
        assert!(!data.trim_start().starts_with('{'));
        assert!(parse_event(b"event=1 timestamp_unix=0 tool=read status=ok").is_some());
        assert!(
            parse_event(b"event=1 timestamp_unix=0 tool=read status=ok token=secret").is_none()
        );
        assert!(!data.contains("authorization"));
        assert!(!data.contains("secret-value"));
        let old = AuditTrail::persisted_snapshot(&dir, 10).expect("disk");
        assert_eq!(old.llm.len(), 1);
        drop(audit);
        fs::write(
            dir.join("audit.log"),
            b"event=2 timestamp_unix=0 tool=read status=ok\n{\"secret\":\"hidden\"}\nnot-an-event\n",
        )
        .expect("malformed test fixture");
        let summary = AuditTrail::persisted_snapshot(&dir, 10).expect("ignore corrupt lines");
        assert_eq!(summary.llm.len(), 1);
        assert_eq!(summary.total_calls, 2);
        assert!(!summary.human.join("\n").contains("secret"));
    }

    #[test]
    fn denies_symlinked_audit_file() {
        let tmp = tempfile::tempdir().expect("fixture");
        let dir = tmp.path().join("NovaGPT").join("logs").join("corrupt");
        let _ = AuditTrail::persistent(dir.clone()).expect("create dirs");
        fs::write(dir.join("audit.log"), b"").expect("fixture");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            fs::remove_file(dir.join("audit.log")).expect("remove");
            symlink(tmp.path().join("outside"), dir.join("audit.log")).expect("link");
            assert!(AuditTrail::persistent(dir).is_err());
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::symlink_file;
            fs::remove_file(dir.join("audit.log")).expect("remove");
            if symlink_file(tmp.path().join("outside"), dir.join("audit.log")).is_ok() {
                assert!(AuditTrail::persistent(dir).is_err());
            }
        }
    }
}
