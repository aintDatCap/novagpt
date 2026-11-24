//! Bounded private restart metadata. Never persist prompts, results or credentials.
use super::{AgentInfo, AgentRole, AgentState};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    sync::Mutex,
};

const LIMIT: usize = 128;
const MAX_BYTES: u64 = 128 * 1024;
#[derive(Clone, Serialize, Deserialize)]
struct Record {
    id: String,
    workspace_id: String,
    role: AgentRole,
    state: AgentState,
    epoch: u64,
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    version: u8,
    records: Vec<Record>,
}
pub(super) struct History {
    path: PathBuf,
    _lease: fs::File,
    records: Mutex<Vec<Record>>,
    pub(super) recovered: Vec<AgentInfo>,
}
fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "Invalid private agent history")
}
fn private(path: &std::path::Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(invalid());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(invalid());
        }
    }
    Ok(())
}
fn valid_id(id: &str, prefix: &str, len: usize) -> bool {
    id.strip_prefix(prefix).is_some_and(|s| {
        s.len() == len
            && s.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
impl History {
    pub(super) fn open(path: PathBuf) -> io::Result<Self> {
        let parent = path.parent().ok_or_else(invalid)?;
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(invalid());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(invalid());
            }
        }
        let lock = path.with_extension("lock");
        if lock.symlink_metadata().is_ok() {
            private(&lock)?;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lease = options.open(&lock)?;
        lease.try_lock_exclusive()?;
        let records = match private(&path) {
            Ok(()) => {
                let mut data = Vec::new();
                fs::File::open(&path)?
                    .take(MAX_BYTES + 1)
                    .read_to_end(&mut data)?;
                if data.len() as u64 > MAX_BYTES {
                    return Err(invalid());
                }
                let snapshot: Snapshot = serde_json::from_slice(&data).map_err(|_| invalid())?;
                if snapshot.version != 1
                    || snapshot.records.len() > LIMIT
                    || snapshot.records.iter().any(|r| {
                        !valid_id(&r.id, "agent-", 8)
                            || !valid_id(&r.workspace_id, "ws_", 24)
                            || r.epoch == 0
                    })
                {
                    return Err(invalid());
                }
                snapshot.records
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let recovered = records.iter().map(Record::recovered).collect();
        Ok(Self {
            path,
            _lease: lease,
            records: Mutex::new(records),
            recovered,
        })
    }
    pub(super) fn record(&self, info: &AgentInfo) -> io::Result<()> {
        let mut records = self.records.lock().map_err(|_| invalid())?;
        if records.iter().any(|r| {
            r.id == info.id
                && (r.epoch > info.epoch
                    || (r.epoch == info.epoch
                        && !matches!(r.state, AgentState::Running | AgentState::Waiting)
                        && matches!(info.state, AgentState::Running | AgentState::Waiting)))
        }) {
            return Ok(());
        }
        let mut next = records.clone();
        next.retain(|r| r.id != info.id);
        next.insert(
            0,
            Record {
                id: info.id.clone(),
                workspace_id: info.workspace_id.clone(),
                role: info.role,
                state: info.state,
                epoch: info.epoch,
            },
        );
        next.truncate(LIMIT);
        let mut temp = tempfile::NamedTempFile::new_in(self.path.parent().ok_or_else(invalid)?)?;
        serde_json::to_writer(
            &mut temp,
            &Snapshot {
                version: 1,
                records: next.clone(),
            },
        )
        .map_err(io::Error::other)?;
        temp.flush()?;
        temp.as_file().sync_all()?;
        temp.persist(&self.path).map_err(|e| e.error)?;
        #[cfg(unix)]
        fs::File::open(self.path.parent().ok_or_else(invalid)?)?.sync_all()?;
        *records = next;
        Ok(())
    }
}
impl Record {
    fn recovered(&self) -> AgentInfo {
        let interrupted = matches!(self.state, AgentState::Running | AgentState::Waiting);
        AgentInfo {
            id: self.id.clone(), workspace_id: self.workspace_id.clone(), role: self.role,
            state: if interrupted { AgentState::Failed } else { self.state },
            backend: "historical".into(), model: "unavailable".into(), connector_name: None, vm: None,
            capability_status: "historical".into(), result: None,
            error: Some(if interrupted { "Execution interrupted by server restart; verify workspace and start a new worker. No automatic replay." } else { "Historical metadata only; result and credentials were not persisted." }.into()),
            external_conversation: None, untrusted: true, patches_applied: 0, patched_paths: Vec::new(),
            files_created: 0, created_paths: Vec::new(), epoch: self.epoch, activity_sequence: 0,
        }
    }
}

impl Drop for History {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self._lease);
    }
}
