//! Owner-private index of ChatGPT conversations NovaGPT opened.
//!
//! Only opaque conversation identifiers, whole conversation URLs and fixed
//! metadata are stored. Prompts, replies, cookies and tokens are never
//! written. The file is a convenience for reopening a chat, not an audit log:
//! a missing or damaged index degrades to an empty list instead of blocking
//! the TUI or the MCP server.
use crate::settings::global_dir;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    path::PathBuf,
};

const FILE_NAME: &str = "chat-sessions.json";
const LIMIT: u64 = 64 * 1024;
/// Keep the index small enough to rewrite cheaply on every completed reply.
pub const MAX_SESSIONS: usize = 100;
const MAX_ID: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// A conversation started from the TUI chat page.
    Chat,
    /// A subagent worker conversation.
    Subagent,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Opaque local identifier: `tui-chat` or the agent id.
    pub id: String,
    pub kind: SessionKind,
    /// Validated `https://chatgpt.com/c/<id>`.
    pub url: String,
    pub created_ms: u64,
    pub updated_ms: u64,
    pub workspace_id: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct StoredSessions {
    version: u8,
    sessions: Vec<SessionRecord>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Private per-user index next to the settings and the browser profile.
    pub fn open_default() -> io::Result<Self> {
        Ok(Self::new(global_dir()?.join(FILE_NAME)))
    }

    /// Best-effort read. Returns newest first and silently drops records whose
    /// URL no longer validates, so a hand-edited file cannot redirect a tab.
    pub fn load(&self) -> Vec<SessionRecord> {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return Vec::new();
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > LIMIT {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        let Ok(file) = fs::File::open(&self.path) else {
            return Vec::new();
        };
        if file.take(LIMIT + 1).read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        if bytes.len() as u64 > LIMIT {
            return Vec::new();
        }
        let Ok(stored) = serde_json::from_slice::<StoredSessions>(&bytes) else {
            return Vec::new();
        };
        if stored.version != 1 {
            return Vec::new();
        }
        stored
            .sessions
            .into_iter()
            .filter(|record| {
                safe_id(&record.id)
                    && novagpt_browser::project::conversation_url(&record.url)
                        .is_some_and(|url| url == record.url)
            })
            .take(MAX_SESSIONS)
            .collect()
    }

    /// Serialize read-modify-replace across all local server instances. The
    /// index itself remains atomically replaced, so readers need no lock.
    fn lock_index(&self) -> io::Result<fs::File> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("missing session directory"))?;
        fs::create_dir_all(parent)?;
        let lock_path = self.path.with_extension("lock");
        if let Ok(metadata) = fs::symlink_metadata(&lock_path)
            && (metadata.file_type().is_symlink() || !metadata.is_file())
        {
            return Err(invalid("unsafe session lock target"));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(lock_path)?;
        file.lock_exclusive()?;
        Ok(file)
    }

    /// Record or refresh one conversation. Duplicate URLs are moved to the
    /// front and keep their original creation time. Writes atomically.
    pub fn record(
        &self,
        kind: SessionKind,
        id: &str,
        url: &str,
        workspace_id: Option<&str>,
    ) -> io::Result<()> {
        if !safe_id(id) {
            return Err(invalid("invalid session id"));
        }
        let url = novagpt_browser::project::conversation_url(url)
            .ok_or_else(|| invalid("invalid conversation URL"))?;
        let _lock = self.lock_index()?;
        let now = now_ms();
        let mut sessions = self.load();
        if let Some(position) = sessions.iter().position(|record| record.url == url) {
            let mut existing = sessions.remove(position);
            existing.updated_ms = now;
            existing.id = id.to_owned();
            existing.kind = kind;
            existing.workspace_id = workspace_id.map(str::to_owned);
            sessions.insert(0, existing);
        } else {
            sessions.insert(
                0,
                SessionRecord {
                    id: id.to_owned(),
                    kind,
                    url,
                    created_ms: now,
                    updated_ms: now,
                    workspace_id: workspace_id.map(str::to_owned),
                },
            );
        }
        sessions.truncate(MAX_SESSIONS);
        self.write(&sessions)
    }

    /// Remove every stored reference. The conversations remain in ChatGPT.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn clear(&self) -> io::Result<()> {
        let _lock = self.lock_index()?;
        match fs::symlink_metadata(&self.path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                Err(invalid("unsafe session index target"))
            }
            Ok(_) => fs::remove_file(&self.path),
        }
    }
    fn write(&self, sessions: &[SessionRecord]) -> io::Result<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| invalid("missing session directory"))?;
        fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(parent)?.permissions().mode() & 0o077 != 0 {
                return Err(invalid("session directory is not private"));
            }
        }
        let bytes = serde_json::to_vec(&StoredSessions {
            version: 1,
            sessions: sessions.to_vec(),
        })?;
        if bytes.len() as u64 > LIMIT {
            return Err(invalid("session index too large"));
        }
        let temporary = parent.join(format!(".sessions-{}.tmp", uuid::Uuid::new_v4().simple()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let result = (|| -> io::Result<()> {
            let mut file = options.open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            // Fail closed if the destination was replaced during the write.
            match fs::symlink_metadata(&self.path) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                    return Err(invalid("unsafe session index target"));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
                .expect("private session directory");
        }
        let store = SessionStore::new(dir.path().join(FILE_NAME));
        (dir, store)
    }

    #[test]
    fn records_newest_first_and_rejects_invalid_inputs() {
        let (_dir, store) = store();
        assert!(store.load().is_empty());
        store
            .record(
                SessionKind::Chat,
                "tui-chat",
                "https://chatgpt.com/c/aaa",
                None,
            )
            .expect("first");
        store
            .record(
                SessionKind::Subagent,
                "agent-1",
                "https://chatgpt.com/c/bbb",
                Some("default"),
            )
            .expect("second");
        let loaded = store.load();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].url, "https://chatgpt.com/c/bbb");
        assert_eq!(loaded[1].url, "https://chatgpt.com/c/aaa");
        assert_eq!(loaded[1].id, "tui-chat");
        assert_eq!(loaded[0].workspace_id.as_deref(), Some("default"));

        for (id, url) in [
            ("bad/id", "https://chatgpt.com/c/ccc"),
            ("agent-2", "https://evil.test/c/ccc"),
            ("agent-2", "https://chatgpt.com/g/g-p-x/project"),
            ("", "https://chatgpt.com/c/ccc"),
        ] {
            assert!(
                store.record(SessionKind::Subagent, id, url, None).is_err(),
                "accepted {id} {url}"
            );
        }
    }

    #[test]
    fn duplicate_conversations_move_to_the_front_without_duplicating() {
        let (_dir, store) = store();
        store
            .record(
                SessionKind::Chat,
                "tui-chat",
                "https://chatgpt.com/c/aaa",
                None,
            )
            .expect("first");
        store
            .record(
                SessionKind::Subagent,
                "agent-1",
                "https://chatgpt.com/c/bbb",
                None,
            )
            .expect("second");
        store
            .record(
                SessionKind::Subagent,
                "agent-1",
                "https://chatgpt.com/c/aaa",
                None,
            )
            .expect("refresh");
        let loaded = store.load();
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].url, "https://chatgpt.com/c/aaa");
        assert_eq!(loaded[0].kind, SessionKind::Subagent);
        assert_eq!(loaded[0].id, "agent-1");
        assert!(loaded[0].created_ms <= loaded[0].updated_ms);
    }

    #[test]
    fn index_is_bounded_and_clear_removes_references() {
        let (_dir, store) = store();
        for index in 0..(MAX_SESSIONS + 5) {
            store
                .record(
                    SessionKind::Subagent,
                    &format!("agent-{index}"),
                    &format!("https://chatgpt.com/c/c{index}"),
                    None,
                )
                .expect("record");
        }
        assert_eq!(store.load().len(), MAX_SESSIONS);
        store.clear().expect("clear");
        assert!(store.load().is_empty());
        store.clear().expect("clear is idempotent");
    }

    #[test]
    fn tampered_index_is_ignored_instead_of_redirecting_a_tab() {
        let (_dir, store) = store();
        let tampered = StoredSessions {
            version: 1,
            sessions: vec![
                SessionRecord {
                    id: "agent-1".into(),
                    kind: SessionKind::Subagent,
                    url: "https://evil.test/c/aaa".into(),
                    created_ms: 1,
                    updated_ms: 1,
                    workspace_id: None,
                },
                SessionRecord {
                    id: "../escape".into(),
                    kind: SessionKind::Chat,
                    url: "https://chatgpt.com/c/bbb".into(),
                    created_ms: 1,
                    updated_ms: 1,
                    workspace_id: None,
                },
            ],
        };
        fs::create_dir_all(store.path.as_path().parent().unwrap()).unwrap();
        fs::write(store.path.as_path(), serde_json::to_vec(&tampered).unwrap()).unwrap();
        assert!(store.load().is_empty());
    }

    #[test]
    fn concurrent_writers_preserve_distinct_conversations() {
        use std::sync::{Arc, Barrier};
        let (dir, store) = store();
        let path = store.path.clone();
        let barrier = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|worker| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let store = SessionStore::new(path);
                    for index in 0..4 {
                        store
                            .record(
                                SessionKind::Subagent,
                                &format!("agent-{worker}"),
                                &format!("https://chatgpt.com/c/worker{worker}-{index}"),
                                None,
                            )
                            .expect("serialized record");
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("writer thread");
        }
        let loaded = SessionStore::new(dir.path().join(FILE_NAME)).load();
        assert_eq!(loaded.len(), 32, "no simultaneous update may be lost");
        let urls: std::collections::HashSet<_> =
            loaded.iter().map(|session| session.url.as_str()).collect();
        assert_eq!(urls.len(), 32);
    }

    #[test]
    fn oversized_index_is_ignored() {
        let (_dir, store) = store();
        fs::create_dir_all(store.path.as_path().parent().unwrap()).unwrap();
        let oversized = vec![b' '; (LIMIT as usize) + 1];
        fs::write(store.path.as_path(), oversized).unwrap();
        assert!(store.load().is_empty());
    }
}
