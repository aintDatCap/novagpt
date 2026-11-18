//! Optional loopback-only Nova Chat adapter. No MCP shell capabilities are exposed.
//! The local journal is authoritative for admission and idempotency; ChatGPT
//! remains authoritative for actual conversation contents.
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as RoutePath, State},
    http::{HeaderMap, StatusCode, header},
    routing::{get, post},
};
use fs2::FileExt;
use novagpt_browser::ChatGptBrowser;
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::sessions::{SessionKind, SessionStore};

const MAX_SESSIONS: usize = 32;
const JOURNAL_MAX: u64 = 64 * 1024;
const MAX_PROMPT: usize = 4096;
const MAX_REPLY: usize = 32 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Ready,
    Running,
    Succeeded,
    Ambiguous,
}

#[derive(Clone, Serialize, Deserialize)]
struct ChatSession {
    id: String,
    owner: String,
    phase: Phase,
    operation_id: Option<String>,
    #[serde(default)]
    used_operations: Vec<String>,
    conversation_url: Option<String>,
    updated_ms: u64,
    #[serde(skip)]
    reply: Option<String>,
    #[serde(skip)]
    tab_open: bool,
    #[serde(skip)]
    abort: Option<tokio::task::AbortHandle>,
}

#[derive(Serialize, Deserialize)]
struct Journal {
    version: u8,
    sessions: Vec<ChatSession>,
}

#[derive(Serialize)]
struct SessionView {
    id: String,
    status: Phase,
    operation_id: Option<String>,
    conversation_url: Option<String>,
    reply: Option<String>,
    updated_ms: u64,
}

impl From<&ChatSession> for SessionView {
    fn from(value: &ChatSession) -> Self {
        Self {
            id: value.id.clone(),
            status: value.phase,
            operation_id: value.operation_id.clone(),
            conversation_url: value.conversation_url.clone(),
            reply: value.reply.clone(),
            updated_ms: value.updated_ms,
        }
    }
}

#[derive(Serialize)]
struct SessionsView {
    sessions: Vec<SessionView>,
}

#[derive(Deserialize)]
struct SendRequest {
    operation_id: String,
    prompt: String,
}

#[derive(Clone)]
struct ChatApi {
    browser: ChatGptBrowser,
    owners: Arc<Vec<(String, String)>>,
    sessions: Arc<Mutex<Vec<ChatSession>>>,
    journal: PathBuf,
    // Lifetime-long exclusive owner lock prevents competing API processes
    // from accepting work against stale in-memory journal snapshots.
    _owner_lock: Arc<File>,
}

fn safe_id(value: &str, limit: usize) -> bool {
    !value.is_empty()
        && value.len() <= limit
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64)
}

fn private_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsafe journal target",
        )),
        #[cfg(unix)]
        Ok(meta)
            if {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o077 != 0
            } =>
        {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "chat state file must be private",
            ))
        }
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

fn load_journal(path: &Path) -> io::Result<Vec<ChatSession>> {
    private_file(path)?;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > JOURNAL_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "journal oversized",
        ));
    }
    let mut bytes = Vec::new();
    file.take(JOURNAL_MAX + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > JOURNAL_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "journal oversized",
        ));
    }
    let journal: Journal = serde_json::from_slice(&bytes)?;
    if journal.version != 1 || journal.sessions.len() > MAX_SESSIONS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid journal",
        ));
    }
    let mut ids = std::collections::HashSet::new();
    let mut sessions = journal.sessions;
    for entry in &mut sessions {
        if !safe_id(&entry.id, 80)
            || !safe_id(&entry.owner, 32)
            || !ids.insert(entry.id.clone())
            || entry
                .operation_id
                .as_deref()
                .is_some_and(|op| !safe_id(op, 64))
            || entry.used_operations.len() > 32
            || entry.used_operations.iter().any(|op| !safe_id(op, 64))
            || entry
                .used_operations
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != entry.used_operations.len()
            || entry.conversation_url.as_deref().is_some_and(|url| {
                novagpt_browser::project::conversation_url(url).as_deref() != Some(url)
            })
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid journal entry",
            ));
        }
        // An interrupted operation may have submitted its prompt. Never
        // mistake a restart for an unsent operation or replay it.
        if entry.phase == Phase::Running {
            entry.phase = Phase::Ambiguous;
        }
    }
    Ok(sessions)
}

fn persist(path: &Path, sessions: &[ChatSession]) -> io::Result<()> {
    let bytes = serde_json::to_vec(&Journal {
        version: 1,
        sessions: sessions.to_vec(),
    })?;
    if bytes.len() as u64 > JOURNAL_MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "journal oversized",
        ));
    }
    private_file(path)?;
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other("missing journal directory"))?;
    let temporary = dir.join(format!(".nova-chat-{}.tmp", Uuid::new_v4().simple()));
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
        private_file(path)?;
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

impl ChatApi {
    fn new(
        browser: ChatGptBrowser,
        path: PathBuf,
        tokens: HashMap<String, String>,
        shell_token: &str,
    ) -> io::Result<Self> {
        if tokens.is_empty() || tokens.len() > 16 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid chat token count",
            ));
        }
        let mut owners: Vec<_> = tokens.into_iter().collect();
        owners.sort_by(|a, b| a.0.cmp(&b.0));
        for (index, (owner, token)) in owners.iter().enumerate() {
            if !safe_id(owner, 32)
                || !(32..=256).contains(&token.len())
                || !token.bytes().all(|b| b.is_ascii_graphic())
                || token == shell_token
                || owners[..index].iter().any(|(_, other)| other == token)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid chat API credentials",
                ));
            }
        }
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("missing journal directory"))?;
        fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if fs::metadata(dir)?.permissions().mode() & 0o077 != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "chat state directory must be private",
                ));
            }
        }
        let lock_path = path.with_extension("lock");
        private_file(&lock_path)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let owner_lock = options.open(lock_path)?;
        owner_lock.try_lock_exclusive()?;
        let sessions = load_journal(&path)?;
        // Persist ambiguous restart transitions before accepting new work.
        persist(&path, &sessions)?;
        Ok(Self {
            browser,
            owners: Arc::new(owners),
            sessions: Arc::new(Mutex::new(sessions)),
            journal: path,
            _owner_lock: Arc::new(owner_lock),
        })
    }

    fn principal(&self, headers: &HeaderMap) -> Result<String, StatusCode> {
        let header = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(StatusCode::UNAUTHORIZED)?;
        for (owner, secret) in self.owners.iter() {
            if header.len() == secret.len()
                && bool::from(header.as_bytes().ct_eq(secret.as_bytes()))
            {
                return Ok(owner.clone());
            }
        }
        Err(StatusCode::UNAUTHORIZED)
    }

    async fn mutate<T: Send + 'static>(
        &self,
        callback: impl FnOnce(&mut Vec<ChatSession>) -> Result<T, StatusCode> + Send + 'static,
    ) -> Result<T, StatusCode> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut sessions = this
                .sessions
                .lock()
                .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
            let previous = sessions.clone();
            let result = callback(&mut sessions)?;
            if persist(&this.journal, &sessions).is_err() {
                *sessions = previous;
                return Err(StatusCode::SERVICE_UNAVAILABLE);
            }
            Ok(result)
        })
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
    }
}

fn routes(state: ChatApi) -> Router {
    Router::new()
        .route("/v1/chat/sessions", post(create).get(list))
        .route("/v1/chat/sessions/{id}", get(status))
        .route("/v1/chat/sessions/{id}/send", post(send))
        .route("/v1/chat/sessions/{id}/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(8192))
        .with_state(state)
}

/// Only called when explicitly enabling the optional loopback API. It never
/// shares the powerful MCP bearer secret or exposes raw MCP dispatch.
pub fn router(browser: ChatGptBrowser, shell_token: &str) -> io::Result<Router> {
    let raw = std::env::var("NOVAGPT_CHAT_API_TOKENS_JSON").map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidInput, "chat API credentials required")
    })?;
    if raw.len() > 8192 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chat API credentials oversized",
        ));
    }
    let tokens: HashMap<String, String> = serde_json::from_str(&raw)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid chat API credentials"))?;
    let path = crate::settings::global_dir()?.join("chat-api-sessions.json");
    Ok(routes(ChatApi::new(browser, path, tokens, shell_token)?))
}

async fn create(
    State(state): State<ChatApi>,
    headers: HeaderMap,
) -> Result<(StatusCode, Json<SessionView>), StatusCode> {
    let owner = state.principal(&headers)?;
    let created = state
        .mutate(move |sessions| {
            if sessions.len() >= MAX_SESSIONS {
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
            let entry = ChatSession {
                id: format!("nova-chat-{}", Uuid::new_v4().simple()),
                owner,
                phase: Phase::Ready,
                operation_id: None,
                used_operations: Vec::new(),
                conversation_url: None,
                updated_ms: now_ms(),
                reply: None,
                tab_open: false,
                abort: None,
            };
            let view = SessionView::from(&entry);
            sessions.push(entry);
            Ok(view)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn list(
    State(state): State<ChatApi>,
    headers: HeaderMap,
) -> Result<Json<SessionsView>, StatusCode> {
    let owner = state.principal(&headers)?;
    let this = state.clone();
    let sessions = tokio::task::spawn_blocking(move || {
        let guard = this
            .sessions
            .lock()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        Ok::<_, StatusCode>(
            guard
                .iter()
                .filter(|entry| entry.owner == owner)
                .map(SessionView::from)
                .collect(),
        )
    })
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)??;
    Ok(Json(SessionsView { sessions }))
}

async fn status(
    State(state): State<ChatApi>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<SessionView>, StatusCode> {
    let owner = state.principal(&headers)?;
    let this = state.clone();
    tokio::task::spawn_blocking(move || {
        let guard = this
            .sessions
            .lock()
            .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
        guard
            .iter()
            .find(|entry| entry.id == id && entry.owner == owner)
            .map(SessionView::from)
            .map(Json)
            .ok_or(StatusCode::NOT_FOUND)
    })
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?
}

async fn send(
    State(state): State<ChatApi>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
    Json(request): Json<SendRequest>,
) -> Result<(StatusCode, Json<SessionView>), StatusCode> {
    let owner = state.principal(&headers)?;
    if !safe_id(&request.operation_id, 64)
        || request.prompt.trim().is_empty()
        || request.prompt.len() > MAX_PROMPT
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let operation_id = request.operation_id.clone();
    let entry_id = id.clone();
    let starting = state
        .mutate(move |sessions| {
            let entry = sessions
                .iter_mut()
                .find(|entry| entry.id == entry_id && entry.owner == owner)
                .ok_or(StatusCode::NOT_FOUND)?;
            if entry.operation_id.as_deref() == Some(&operation_id) {
                return Ok((
                    false,
                    SessionView::from(&*entry),
                    entry.conversation_url.clone(),
                    entry.tab_open,
                ));
            }
            if entry
                .used_operations
                .iter()
                .any(|used| used == &operation_id)
            {
                return Err(StatusCode::CONFLICT);
            }
            if entry.used_operations.len() >= 32 {
                return Err(StatusCode::TOO_MANY_REQUESTS);
            }
            if entry.phase == Phase::Running || entry.phase == Phase::Ambiguous {
                return Err(StatusCode::CONFLICT);
            }
            // A completed first turn with no verified conversation URL cannot
            // be reopened after a restart without silently creating a new chat.
            if entry.phase == Phase::Succeeded
                && entry.conversation_url.is_none()
                && !entry.tab_open
            {
                return Err(StatusCode::CONFLICT);
            }
            entry.phase = Phase::Running;
            entry.used_operations.push(operation_id.clone());
            entry.operation_id = Some(operation_id);
            entry.reply = None;
            entry.abort = None;
            entry.updated_ms = now_ms();
            Ok((
                true,
                SessionView::from(&*entry),
                entry.conversation_url.clone(),
                entry.tab_open,
            ))
        })
        .await?;
    if !starting.0 {
        return Ok((StatusCode::OK, Json(starting.1)));
    }
    let browser = state.browser.clone();
    let backend = state.clone();
    let request_id = request.operation_id;
    let session_id = id.clone();
    let tab_id = id;
    let prompt = request.prompt;
    let resume_url = starting.2;
    let was_open = starting.3;
    let handle = tokio::spawn(async move {
        let opened = if was_open {
            Ok(())
        } else if let Some(url) = resume_url.as_deref() {
            browser.open_at(&tab_id, url).await
        } else {
            browser.open_home(&tab_id).await
        };
        // Any failure after the durable Running marker is conservatively
        // ambiguous. The client must never infer that a retry is safe.
        let opened_ok = opened.is_ok();
        let answer = match opened {
            Ok(()) => browser.send(&tab_id, &prompt).await.ok(),
            Err(_) => None,
        };
        let url = answer.as_ref().and_then(|reply| {
            reply
                .conversation_url
                .as_deref()
                .and_then(novagpt_browser::project::conversation_url)
        });
        let reply = answer.map(|reply| {
            let mut text = reply.content;
            if text.len() > MAX_REPLY {
                let mut end = MAX_REPLY;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
            }
            text
        });
        let succeeded = reply.is_some();
        let saved_id = session_id.clone();
        let saved_url = url.clone();
        let saved_op = request_id.clone();
        let accepted = backend
            .mutate(move |sessions| {
                let Some(entry) = sessions.iter_mut().find(|entry| entry.id == saved_id) else {
                    return Ok(false);
                };
                if entry.phase != Phase::Running || entry.operation_id.as_deref() != Some(&saved_op)
                {
                    return Ok(false);
                }
                entry.phase = if succeeded {
                    Phase::Succeeded
                } else {
                    Phase::Ambiguous
                };
                entry.conversation_url =
                    saved_url.clone().or_else(|| entry.conversation_url.clone());
                entry.reply = reply;
                entry.tab_open = succeeded || opened_ok;
                entry.abort = None;
                entry.updated_ms = now_ms();
                Ok(succeeded)
            })
            .await
            .unwrap_or(false);
        if accepted && let Some(url) = url {
            let _ = tokio::task::spawn_blocking(move || {
                if let Ok(store) = SessionStore::open_default() {
                    let _ = store.record(SessionKind::Chat, &session_id, &url, None);
                }
            })
            .await;
        }
    });
    let abort = handle.abort_handle();
    let pending_id = starting.1.id.clone();
    state
        .mutate(move |sessions| {
            if let Some(entry) = sessions
                .iter_mut()
                .find(|entry| entry.id == pending_id && entry.phase == Phase::Running)
            {
                entry.abort = Some(abort);
            }
            Ok(())
        })
        .await?;
    Ok((StatusCode::ACCEPTED, Json(starting.1)))
}

async fn cancel(
    State(state): State<ChatApi>,
    RoutePath(id): RoutePath<String>,
    headers: HeaderMap,
) -> Result<Json<SessionView>, StatusCode> {
    let owner = state.principal(&headers)?;
    let (view, handle) = state
        .mutate(move |sessions| {
            let entry = sessions
                .iter_mut()
                .find(|entry| entry.id == id && entry.owner == owner)
                .ok_or(StatusCode::NOT_FOUND)?;
            if entry.phase != Phase::Running {
                return Err(StatusCode::CONFLICT);
            }
            entry.phase = Phase::Ambiguous;
            entry.updated_ms = now_ms();
            // Aborting observation cannot prove that no prompt was submitted.
            Ok((SessionView::from(&*entry), entry.abort.take()))
        })
        .await?;
    if let Some(abort) = handle {
        abort.abort();
    }
    Ok(Json(view))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture() -> (tempfile::TempDir, ChatApi) {
        let dir = tempfile::tempdir().expect("temp");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
                .expect("private fixture directory");
        }
        let chrome = dir.path().join("chrome");
        fs::write(&chrome, "").expect("fake executable");
        let browser =
            ChatGptBrowser::new(chrome, dir.path().join("browser")).expect("browser handle");
        let tokens = HashMap::from([
            ("alice".to_owned(), "a".repeat(40)),
            ("bob".to_owned(), "b".repeat(40)),
        ]);
        let api = ChatApi::new(
            browser,
            dir.path().join("journal.json"),
            tokens,
            "mcp-shell-token",
        )
        .expect("api");
        (dir, api)
    }

    async fn server(router: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        (format!("http://{address}"), handle)
    }

    #[tokio::test]
    async fn rejects_missing_invalid_and_other_owner_sessions() {
        let (_dir, api) = fixture();
        let (base, task) = server(routes(api)).await;
        let client = reqwest::Client::new();
        let url = format!("{base}/v1/chat/sessions");
        assert_eq!(
            client
                .get(&url)
                .send()
                .await
                .expect("missing auth")
                .status(),
            401
        );
        assert_eq!(
            client
                .get(&url)
                .bearer_auth("wrong")
                .send()
                .await
                .expect("bad auth")
                .status(),
            401
        );
        let created: serde_json::Value = client
            .post(&url)
            .bearer_auth("a".repeat(40))
            .send()
            .await
            .expect("create")
            .json()
            .await
            .expect("json");
        let id = created["id"].as_str().expect("id");
        let other = format!("{url}/{id}");
        assert_eq!(
            client
                .get(other)
                .bearer_auth("b".repeat(40))
                .send()
                .await
                .expect("cross owner")
                .status(),
            404
        );
        let listing: serde_json::Value = client
            .get(&url)
            .bearer_auth("b".repeat(40))
            .send()
            .await
            .expect("list")
            .json()
            .await
            .expect("json");
        assert_eq!(listing["sessions"].as_array().expect("array").len(), 0);
        task.abort();
    }

    #[tokio::test]
    async fn interrupted_send_is_ambiguous_after_restart_and_never_replays() {
        let (dir, api) = fixture();
        let (base, task) = server(routes(api.clone())).await;
        let client = reqwest::Client::new();
        let url = format!("{base}/v1/chat/sessions");
        let created: serde_json::Value = client
            .post(&url)
            .bearer_auth("a".repeat(40))
            .send()
            .await
            .expect("create")
            .json()
            .await
            .expect("json");
        let id = created["id"].as_str().expect("id").to_owned();
        // Model the durable marker just before the browser receives a prompt.
        api.mutate({
            let id = id.clone();
            move |sessions| {
                let entry = sessions.iter_mut().find(|s| s.id == id).expect("entry");
                entry.phase = Phase::Running;
                entry.operation_id = Some("op-1".to_owned());
                Ok(())
            }
        })
        .await
        .expect("marker persisted");
        task.abort();
        drop(api);
        tokio::time::sleep(Duration::from_millis(10)).await;
        let chrome = dir.path().join("chrome");
        let browser = ChatGptBrowser::new(chrome, dir.path().join("browser")).expect("browser");
        let tokens = HashMap::from([
            ("alice".to_owned(), "a".repeat(40)),
            ("bob".to_owned(), "b".repeat(40)),
        ]);
        let recovered = ChatApi::new(
            browser,
            dir.path().join("journal.json"),
            tokens,
            "mcp-shell-token",
        )
        .expect("recover");
        let (base, task) = server(routes(recovered)).await;
        let target = format!("{base}/v1/chat/sessions/{id}");
        let status: serde_json::Value = client
            .get(&target)
            .bearer_auth("a".repeat(40))
            .send()
            .await
            .expect("status")
            .json()
            .await
            .expect("json");
        assert_eq!(status["status"], "ambiguous");
        let replay = client
            .post(format!("{target}/send"))
            .bearer_auth("a".repeat(40))
            .json(&serde_json::json!({"operation_id":"op-2", "prompt":"do not send"}))
            .send()
            .await
            .expect("retry");
        assert_eq!(replay.status(), 409);
        task.abort();
    }
}
