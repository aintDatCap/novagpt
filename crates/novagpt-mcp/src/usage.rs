//! Private opt-in profiler. Bounded metadata only; unrelated audit trail is unchanged.
use novagpt_core::usage::{
    MAX_USAGE_RECORDS, MeasurementSource, TokenCounts, UsageLedger, UsageOutcome, UsageRecord,
    UsageReport,
};
use std::{
    collections::VecDeque,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender, TrySendError},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const FILE_NAME: &str = "usage.jsonl";
const MAX_FILE_BYTES: u64 = 512 * 1024;
const QUEUE_CAPACITY: usize = 512;
const RETENTION_SECONDS: u64 = 30 * 86400;

#[derive(Clone, Default)]
pub struct UsageProfiler {
    state: Arc<Mutex<UsageLedger>>,
    writer: Option<SyncSender<UsageRecord>>,
    pub dropped_events: Arc<AtomicU64>,
    run_id: String,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |t| t.as_secs())
}
fn checked_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MAX_FILE_BYTES {
                return Err(invalid("unsafe or oversized usage file"));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    return Err(invalid("usage file is not private"));
                }
            }
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
fn read_records(directory: &Path) -> io::Result<VecDeque<UsageRecord>> {
    let path = directory.join(FILE_NAME);
    if !checked_file(&path)? {
        return Ok(VecDeque::new());
    }
    let file = fs::File::open(&path)?;
    let mut records = VecDeque::new();
    let current = now();
    for line in BufReader::new(file).lines() {
        // A truncated final line after an interrupted append is discarded.
        let line = line?;
        if line.len() > 2048 {
            continue;
        }
        if let Ok(record) = serde_json::from_str::<UsageRecord>(&line)
            && record.validate().is_ok()
            && record.ended_unix <= current.saturating_add(300)
            && record.ended_unix >= current.saturating_sub(RETENTION_SECONDS)
        {
            if records.len() == MAX_USAGE_RECORDS {
                records.pop_front();
            }
            records.push_back(record);
        }
    }
    Ok(records)
}
fn report_of(records: impl IntoIterator<Item = UsageRecord>) -> UsageReport {
    let mut ledger = UsageLedger::default();
    for record in records {
        let _ = ledger.ingest(record);
    }
    ledger.report()
}
fn compact_file(directory: &Path, records: &mut VecDeque<UsageRecord>) -> io::Result<()> {
    let current = now();
    records.retain(|r| r.ended_unix >= current.saturating_sub(RETENTION_SECONDS));
    while records.len() > MAX_USAGE_RECORDS {
        records.pop_front();
    }
    let path = directory.join(FILE_NAME);
    checked_file(&path)?;
    // Keep the youngest subset if the serialized payload approaches the quota.
    let mut bytes = Vec::new();
    for record in records.iter() {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        bytes.push(line);
    }
    let mut size: usize = bytes.iter().map(Vec::len).sum();
    while size > (MAX_FILE_BYTES as usize / 2) && !bytes.is_empty() {
        size -= bytes.remove(0).len();
        records.pop_front();
    }
    let temporary = directory.join(format!(".usage-{}.tmp", Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> io::Result<()> {
        let mut file = options.open(&temporary)?;
        for line in &bytes {
            file.write_all(line)?;
        }
        file.sync_all()?;
        checked_file(&path)?;
        fs::rename(&temporary, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
fn writer_loop(directory: PathBuf, receiver: mpsc::Receiver<UsageRecord>, errors: Arc<AtomicU64>) {
    let mut records = match read_records(&directory) {
        Ok(records) => records,
        Err(_) => {
            errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };
    for record in receiver {
        records.push_back(record.clone());
        if records.len() > MAX_USAGE_RECORDS {
            records.pop_front();
        }
        let result: io::Result<()> = (|| {
            let path = directory.join(FILE_NAME);
            let existing = checked_file(&path)?;
            let mut line = serde_json::to_vec(&record)?;
            line.push(b'\n');
            let file_size = if existing {
                fs::metadata(&path)?.len()
            } else {
                0
            };
            if file_size.saturating_add(line.len() as u64) > MAX_FILE_BYTES {
                compact_file(&directory, &mut records)?;
            } else {
                let mut options = OpenOptions::new();
                options.append(true).create(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options.open(&path)?;
                file.write_all(&line)?;
                file.flush()?;
            }
            Ok(())
        })();
        if result.is_err() {
            errors.fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl UsageProfiler {
    /// No writer, thread, collection or disk writes unless explicitly enabled.
    pub fn disabled() -> Self {
        Self::default()
    }
    pub fn persistent(directory: PathBuf) -> io::Result<Self> {
        let parent = directory
            .parent()
            .ok_or_else(|| invalid("missing usage parent"))?;
        let grandparent = parent
            .parent()
            .ok_or_else(|| invalid("missing state root"))?;
        super::audit::private_dir(grandparent)?;
        super::audit::private_dir(parent)?;
        super::audit::private_dir(&directory)?;
        let mut records = read_records(&directory)?;
        // On opt-in startup, enforce time and size retention even if no new calls arrive.
        if checked_file(&directory.join(FILE_NAME))? {
            compact_file(&directory, &mut records)?;
        }
        let mut ledger = UsageLedger::default();
        for record in records {
            let _ = ledger.ingest(record);
        }
        let (tx, rx) = mpsc::sync_channel(QUEUE_CAPACITY);
        let dropped_events = Arc::new(AtomicU64::new(0));
        let errors = Arc::clone(&dropped_events);
        std::thread::Builder::new()
            .name("novagpt-usage".into())
            .spawn(move || writer_loop(directory, rx, errors))?;
        Ok(Self {
            state: Arc::new(Mutex::new(ledger)),
            writer: Some(tx),
            dropped_events,
            run_id: Uuid::new_v4().simple().to_string(),
        })
    }
    pub fn enabled(&self) -> bool {
        self.writer.is_some()
    }
    /// Idempotent provider receipt ingestion. Must be called only on trusted API usage data.
    pub fn record(&self, record: UsageRecord) -> Result<bool, &'static str> {
        if !self.enabled() {
            return Ok(false);
        }
        let mut ledger = self.state.lock().map_err(|_| "usage state unavailable")?;
        let accepted = ledger.ingest(record.clone())?;
        if accepted
            && let Some(tx) = &self.writer
            && let Err(err) = tx.try_send(record)
        {
            match err {
                TrySendError::Full(_) | TrySendError::Disconnected(_) => {
                    self.dropped_events.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Ok(accepted)
    }
    /// Tool response length is only a character-based approximation, never provider billing.
    pub fn observe_tool(
        &self,
        stage: &'static str,
        response_chars: usize,
        failed: bool,
        repository_id: &str,
    ) {
        if !self.enabled() {
            return;
        }
        let timestamp = now();
        let output = (response_chars as u64).saturating_add(3) / 4;
        let record = UsageRecord {
            receipt_id: Uuid::new_v4().simple().to_string(),
            run_id: self.run_id.clone(),
            task_id: None,
            agent_id: None,
            workflow_id: None,
            repository_id: Some(repository_id.to_string()),
            backend_id: "mcp_tool".into(),
            stage: stage.into(),
            started_unix: timestamp,
            ended_unix: timestamp,
            measurement_source: MeasurementSource::LocalEstimate,
            tokens: TokenCounts {
                output: Some(output),
                ..TokenCounts::default()
            },
            cost_micros: None,
            currency: None,
            pricing_revision: None,
            call_count: 1,
            outcome: if failed {
                UsageOutcome::Error
            } else {
                UsageOutcome::Ok
            },
        };
        let _ = self.record(record);
    }
    /// The ChatGPT Web backend does not expose provider billing. Record only
    /// locally estimated character counts for one agent turn and its opaque IDs.
    pub fn observe_model_turn(
        &self,
        agent_id: &str,
        repository_id: &str,
        prompt_chars: usize,
        reply_chars: Option<usize>,
        failed: bool,
    ) {
        if !self.enabled() {
            return;
        }
        let timestamp = now();
        let record = UsageRecord {
            receipt_id: Uuid::new_v4().simple().to_string(),
            run_id: self.run_id.clone(),
            task_id: Some(agent_id.to_string()),
            agent_id: Some(agent_id.to_string()),
            workflow_id: None,
            repository_id: Some(repository_id.to_string()),
            backend_id: "chatgpt_web".into(),
            stage: "model_turn".into(),
            started_unix: timestamp,
            ended_unix: timestamp,
            measurement_source: MeasurementSource::LocalEstimate,
            tokens: TokenCounts {
                input: Some((prompt_chars as u64).saturating_add(3) / 4),
                output: reply_chars.map(|c| (c as u64).saturating_add(3) / 4),
                ..TokenCounts::default()
            },
            cost_micros: None,
            currency: None,
            pricing_revision: None,
            call_count: 1,
            outcome: if failed {
                UsageOutcome::Error
            } else {
                UsageOutcome::Ok
            },
        };
        let _ = self.record(record);
    }
    pub fn report(&self) -> UsageReport {
        self.state
            .lock()
            .map_or_else(|_| UsageReport::default(), |s| s.report())
    }
    pub fn persisted_report(directory: &Path) -> io::Result<UsageReport> {
        // Never create directories or inspect another repository here.
        if !directory.exists() {
            return Ok(UsageReport::default());
        }
        if let Some(parent) = directory.parent() {
            super::audit::private_dir(parent)?;
        }
        super::audit::private_dir(directory)?;
        read_records(directory).map(report_of)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_is_no_op_and_does_not_create_files() {
        let store = UsageProfiler::disabled();
        store.observe_tool("read", 100, false, "repo-1");
        assert_eq!(store.report().totals.calls, 0);
    }
    #[test]
    fn stores_private_metadata_without_response_and_reopens() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let dir = temp.path().join("NovaGPT").join("logs").join("fixture");
        let store = UsageProfiler::persistent(dir.clone()).unwrap();
        store.observe_tool("read", 123, false, "repo-1");
        assert_eq!(store.report().totals.estimated_output, 31);
        drop(store);
        // The producer has been dropped; the worker channel drains before terminating.
        let mut persisted = UsageReport::default();
        for _ in 0..80 {
            persisted = UsageProfiler::persisted_report(&dir).unwrap();
            if persisted.totals.calls == 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(persisted.totals.calls, 1);
        let raw = fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert!(!raw.contains("secret"));
        assert!(!raw.contains("http:"));
        assert!(raw.contains("local_estimate"));
        let _reopened = UsageProfiler::persistent(dir).unwrap();
    }
    #[test]
    fn rejects_symlink_store() {
        #[cfg(unix)]
        {
            let temp = tempfile::tempdir().unwrap();
            use std::os::unix::fs::{PermissionsExt, symlink};
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
            let dir = temp.path().join("NovaGPT").join("logs").join("fixture");
            fs::create_dir_all(&dir).unwrap();
            let target = temp.path().join("unsafe");
            fs::write(&target, "").unwrap();
            symlink(&target, dir.join(FILE_NAME)).unwrap();
            assert!(UsageProfiler::persistent(dir).is_err());
        }
    }
    #[test]
    fn concurrent_estimates_have_distinct_receipts_and_remain_private() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let dir = temp.path().join("NovaGPT").join("logs").join("concurrent");
        let store = UsageProfiler::persistent(dir.clone()).unwrap();
        let mut workers = Vec::new();
        for _ in 0..4 {
            let worker_store = store.clone();
            workers.push(std::thread::spawn(move || {
                for _ in 0..20 {
                    worker_store.observe_tool("read", 123, false, "repo-1");
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        let report = store.report();
        assert_eq!(report.totals.calls, 80);
        assert_eq!(report.totals.estimated_output, 80 * 31);
        assert_eq!(report.by_repository["repo-1"].calls, 80);
        assert_eq!(report.totals.provider_output, 0);
        drop(store);
        let mut persisted = UsageReport::default();
        for _ in 0..100 {
            persisted = UsageProfiler::persisted_report(&dir).unwrap();
            if persisted.totals.calls == 80 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(persisted.totals.calls, 80);
        assert_eq!(persisted.duplicates_ignored, 0);
    }

    #[test]
    fn expires_old_records_and_reads_exact_provider_fixture() {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let dir = temp.path().join("NovaGPT").join("logs").join("fixture");
        super::super::audit::private_dir(&temp.path().join("NovaGPT")).unwrap();
        super::super::audit::private_dir(&temp.path().join("NovaGPT").join("logs")).unwrap();
        super::super::audit::private_dir(&dir).unwrap();
        let fixture = UsageRecord {
            receipt_id: "provider-1".into(),
            run_id: "run-1".into(),
            task_id: Some("task-1".into()),
            agent_id: Some("agent-1".into()),
            workflow_id: Some("workflow-1".into()),
            repository_id: Some("repo-1".into()),
            backend_id: "api".into(),
            stage: "model_turn".into(),
            started_unix: now(),
            ended_unix: now(),
            measurement_source: MeasurementSource::Provider,
            tokens: TokenCounts {
                input: Some(150),
                output: Some(25),
                cached_input: Some(50),
                cache_write: None,
                reasoning_output: Some(5),
            },
            cost_micros: Some(42_000),
            currency: Some("USD".into()),
            pricing_revision: Some("fixture-1".into()),
            call_count: 1,
            outcome: UsageOutcome::Ok,
        };
        let mut stale = fixture.clone();
        stale.receipt_id = "old-provider".into();
        stale.started_unix = now().saturating_sub(RETENTION_SECONDS + 10);
        stale.ended_unix = stale.started_unix;
        let mut bytes = serde_json::to_vec(&stale).unwrap();
        bytes.push(b'\n');
        bytes.extend(serde_json::to_vec(&fixture).unwrap());
        bytes.push(b'\n');
        fs::write(dir.join(FILE_NAME), &bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.join(FILE_NAME), fs::Permissions::from_mode(0o600)).unwrap();
        }
        let snap = UsageProfiler::persisted_report(&dir).unwrap();
        assert_eq!(snap.totals.provider_input, 150);
        assert_eq!(snap.totals.provider_output, 25);
        assert_eq!(snap.totals.provider_cost_micros["USD"], 42_000);
        assert_eq!(snap.by_task["task-1"].calls, 1);
        let store = UsageProfiler::persistent(dir.clone()).unwrap();
        assert_eq!(store.report().totals.records, 1);
        let on_disk = fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert!(
            !on_disk.contains("old-provider"),
            "startup prunes expired records"
        );
    }
}
