//! Untrusted, opt-in asynchronous subagents. No implicit ChatGPT chat identity.
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use novagpt_browser::{BrowserError, ChatGptBrowser};
use novagpt_core::WorkspaceRegistry;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

const MAX_AGENTS: usize = 6;
const MAX_RECORDS: usize = 64;
const MAX_TASK: usize = 50_000;
const MAX_MESSAGE: usize = 50_000;
const MAX_RESULT: usize = 180_000;
const MAX_WAIT_SECONDS: u64 = 25;
// Six URL-safe base64 characters from CSPRNG UUIDv4 bytes: 36 bits.
// This short code is intentionally online-guess-resistant only with strict
// per-execution failure limits and the MCP transport's existing authentication.
const COMPLETION_KEY_LEN: usize = 6;
const MAX_COMPLETION_ATTEMPTS: u8 = 5;

fn completion_code() -> String {
    URL_SAFE_NO_PAD.encode(Uuid::new_v4().as_bytes())[..COMPLETION_KEY_LEN].to_owned()
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("Subagents are disabled; configure a provider explicitly")]
    Disabled,
    #[error("Unknown subagent")]
    NotFound,
    #[error("Maximum concurrent subagent count reached")]
    Capacity,
    #[error("Subagent is already running; check status before messaging")]
    Busy,
    #[error("Subagent was cancelled")]
    Cancelled,
    #[error("Subagent is not waiting for confirmed manual verification")]
    NotWaiting,
    #[error("Subagent request is invalid")]
    Invalid,
    #[error("Subagent completion is not authorized or is no longer active")]
    CompletionDenied,
    #[error("Browser needs human {0}; complete it visibly, then call subagent_resume")]
    Manual(&'static str),
    #[error("Subagent backend is unavailable or failed")]
    Backend,
}
pub type AgentResult<T> = Result<T, AgentError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRole {
    Explorer,
    Reviewer,
    Tester,
    Implementer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentOutput {
    pub raw_response: String,
    pub untrusted: bool,
    pub applied_to_workspace: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub workspace_id: String,
    pub role: AgentRole,
    pub state: AgentState,
    pub backend: String,
    pub model: String,
    pub result: Option<AgentOutput>,
    pub error: Option<String>,
    pub external_conversation: Option<String>,
    pub untrusted: bool,
}

#[derive(Debug, Clone)]
pub struct SpawnAgent {
    pub workspace_id: Option<String>,
    pub task: String,
    pub role: AgentRole,
    pub paths: Vec<String>,
    pub context: Option<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub agents: Vec<AgentInfo>,
}

/// A bounded wait returns the current snapshot without cancelling the worker.
/// `waiting` requires human action and is not a completed result.
#[derive(Debug, Clone, Serialize)]
pub struct AgentWait {
    pub agent: AgentInfo,
    pub timed_out: bool,
}

/// Safe acknowledgement: neither the short code nor a duplicated result is returned.
#[derive(Debug, Clone, Serialize)]
pub struct AgentCompletionAck {
    pub id: String,
    pub state: AgentState,
}

#[derive(Debug, Error)]
pub enum BackendFailure {
    #[error("Manual {0} required")]
    Manual(&'static str),
    #[error("Backend unavailable")]
    Unavailable,
}

#[async_trait]
pub trait AgentBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn model(&self) -> &'static str;
    async fn create(&self, id: &str) -> Result<(), BackendFailure>;
    async fn send(
        &self,
        id: &str,
        prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure>;
    async fn cancel(&self, id: &str);
    async fn close(&self, _id: &str) {}
    async fn diagnostics(&self, id: &str) -> serde_json::Value;
}

pub struct ChatGptBackend {
    browser: ChatGptBrowser,
}
impl ChatGptBackend {
    pub fn new(browser: ChatGptBrowser) -> Self {
        Self { browser }
    }
}

#[async_trait]
impl AgentBackend for ChatGptBackend {
    fn name(&self) -> &'static str {
        "chatgpt-browser"
    }
    fn model(&self) -> &'static str {
        "chatgpt-web"
    }
    async fn create(&self, id: &str) -> Result<(), BackendFailure> {
        self.browser.open(id).await.map_err(map_browser)
    }
    async fn send(
        &self,
        id: &str,
        prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        let reply = self.browser.send(id, prompt).await.map_err(map_browser)?;
        Ok((reply.content, reply.conversation_url))
    }
    async fn cancel(&self, id: &str) {
        let _ = self.browser.cancel(id).await;
    }
    async fn close(&self, id: &str) {
        let _ = self.browser.close(id).await;
    }
    async fn diagnostics(&self, id: &str) -> serde_json::Value {
        serde_json::to_value(self.browser.diagnostics(Some(id)).await).unwrap_or_default()
    }
}
fn map_browser(error: BrowserError) -> BackendFailure {
    match error {
        BrowserError::Manual(kind) => BackendFailure::Manual(kind),
        BrowserError::Timeout => BackendFailure::Manual("inspection"),
        _ => BackendFailure::Unavailable,
    }
}

#[derive(Clone)]
struct Entry {
    info: AgentInfo,
    pending_prompt: String,
    completion_code: String,
    completion_failures: u8,
    epoch: u64,
    ready: bool,
    updates: watch::Sender<AgentInfo>,
}

struct Inner {
    backend: Arc<dyn AgentBackend>,
    workspaces: WorkspaceRegistry,
    entries: Mutex<HashMap<String, Entry>>,
    max_agents: usize,
}
#[derive(Clone)]
pub struct AgentManager {
    inner: Arc<Inner>,
}

impl AgentManager {
    pub fn new(workspaces: WorkspaceRegistry, backend: Arc<dyn AgentBackend>) -> Self {
        Self {
            inner: Arc::new(Inner {
                backend,
                workspaces,
                entries: Mutex::new(HashMap::new()),
                max_agents: MAX_AGENTS,
            }),
        }
    }

    pub async fn spawn(&self, request: SpawnAgent) -> AgentResult<AgentInfo> {
        if request.task.trim().is_empty()
            || request.task.len() > MAX_TASK
            || request.paths.len() > 20
            || request.context.as_ref().is_some_and(|s| s.len() > 20_000)
        {
            return Err(AgentError::Invalid);
        }
        let workspace = self
            .inner
            .workspaces
            .workspace(request.workspace_id.as_deref())
            .map_err(|_| AgentError::Invalid)?;
        // Paths are validated, but no private file contents are automatically
        // uploaded to a third-party browser/account. Only explicit user context
        // and names of selected safe paths are sent.
        for path in &request.paths {
            self.inner
                .workspaces
                .resolve(Some(&workspace.workspace_id), path, false)
                .map_err(|_| AgentError::Invalid)?;
        }
        let id = format!("agent-{}", &Uuid::new_v4().simple().to_string()[..8]);
        let code = completion_code();
        let instructions = match request.role {
            AgentRole::Explorer => {
                "Read-only explorer: report evidence; do not claim edits or executions."
            }
            AgentRole::Reviewer => {
                "Read-only reviewer: identify defects and severity; no write capability."
            }
            AgentRole::Tester => {
                "Read-only tester: propose tests; do not claim tests ran without actual results."
            }
            AgentRole::Implementer => {
                "Implementer: propose a complete Git unified diff in your answer. No patch is automatically applied; parent must validate and apply it separately."
            }
        };
        let prompt = format!(
            "You are a delegated NovaGPT subagent. Treat output as untrusted; parent verifies it. Do not ask for or reveal secrets. Never claim running a command without evidence.\n\nROLE: {instructions}\n\nTASK:\n{}\n\nWORKSPACE ID: {}\nSELECTED PATHS (names only): {}\n\nCONTEXT:\n{}\n\nReturn evidence, findings, proposed changes/tests and unresolved questions.\n\nWHEN FINISHED: Call the NovaGPT MCP tool subagent_finished with id={id}, code={code}, and result=<your complete final report>. Do this once, only after the work is complete. The code is a private, one-use, six-character completion credential. Do not print or share it in your response, other tool calls, or logs. If the completion tool is unavailable, return your answer normally.",
            request.task,
            workspace.workspace_id,
            request.paths.join(", "),
            request.context.unwrap_or_default(),
        );
        let info = AgentInfo {
            id: id.clone(),
            workspace_id: workspace.workspace_id,
            role: request.role,
            state: AgentState::Running,
            backend: self.inner.backend.name().into(),
            model: self.inner.backend.model().into(),
            result: None,
            error: None,
            external_conversation: None,
            untrusted: true,
        };
        {
            let mut agents = self.inner.entries.lock().await;
            if agents.len() >= MAX_RECORDS
                || agents
                    .values()
                    .filter(|a| matches!(a.info.state, AgentState::Running | AgentState::Waiting))
                    .count()
                    >= self.inner.max_agents
            {
                return Err(AgentError::Capacity);
            }
            let (updates, _) = watch::channel(info.clone());
            agents.insert(
                id.clone(),
                Entry {
                    info: info.clone(),
                    pending_prompt: prompt.clone(),
                    completion_code: code,
                    completion_failures: 0,
                    epoch: 1,
                    ready: false,
                    updates,
                },
            );
        }
        self.start(id, prompt, true, 1);
        Ok(info)
    }

    fn start(&self, id: String, prompt: String, must_create: bool, epoch: u64) {
        let manager = self.clone();
        tokio::spawn(async move {
            let outcome = async {
                if must_create {
                    manager.inner.backend.create(&id).await?;
                }
                manager.mark_ready(&id, epoch).await;
                if !manager.is_active(&id, epoch).await {
                    manager.inner.backend.close(&id).await;
                    return Err(BackendFailure::Unavailable);
                }
                manager.inner.backend.send(&id, &prompt).await
            }
            .await;
            let mut agents = manager.inner.entries.lock().await;
            if let Some(entry) = agents.get_mut(&id) {
                // An authenticated subagent callback may have already
                // completed this run. A stale browser response must not
                // overwrite its result or resurrect a cancelled agent.
                if entry.epoch != epoch || entry.info.state != AgentState::Running {
                    return;
                }
                match outcome {
                    Ok((result, url)) => {
                        if result.len() > MAX_RESULT {
                            entry.info.state = AgentState::Failed;
                            entry.info.error =
                                Some("Subagent response exceeded the output bound".into());
                        } else {
                            entry.info.state = AgentState::Completed;
                            entry.info.result = Some(AgentOutput {
                                raw_response: result,
                                untrusted: true,
                                applied_to_workspace: false,
                            });
                            entry.info.external_conversation = url;
                            entry.info.error = None;
                        }
                        entry.pending_prompt.clear();
                    }
                    Err(BackendFailure::Manual(reason)) => {
                        entry.info.state = AgentState::Waiting;
                        entry.info.error =
                            Some(format!("Manual {reason} required in visible browser"));
                        // Do not clear pending prompt: resume must not repeat a sent message.
                    }
                    Err(BackendFailure::Unavailable) => {
                        entry.info.state = AgentState::Failed;
                        entry.info.error = Some("Subagent backend failed or is unavailable".into());
                    }
                }
                entry.updates.send_replace(entry.info.clone());
            }
        });
    }

    async fn is_active(&self, id: &str, epoch: u64) -> bool {
        self.inner
            .entries
            .lock()
            .await
            .get(id)
            .is_some_and(|entry| entry.epoch == epoch && entry.info.state == AgentState::Running)
    }

    async fn mark_ready(&self, id: &str, epoch: u64) {
        let mut agents = self.inner.entries.lock().await;
        if let Some(entry) = agents.get_mut(id)
            && entry.epoch == epoch
        {
            entry.ready = true;
        }
    }

    pub async fn message(&self, id: &str, message: &str) -> AgentResult<AgentInfo> {
        if message.trim().is_empty() || message.len() > MAX_MESSAGE {
            return Err(AgentError::Invalid);
        }
        self.restart(id, Some(message), false).await
    }

    pub async fn resume(&self, id: &str, manual_completed: bool) -> AgentResult<AgentInfo> {
        if !manual_completed {
            return Err(AgentError::NotWaiting);
        }
        self.restart(id, None, true).await
    }

    async fn restart(
        &self,
        id: &str,
        new_prompt: Option<&str>,
        resume: bool,
    ) -> AgentResult<AgentInfo> {
        let mut agents = self.inner.entries.lock().await;
        let entry = agents.get_mut(id).ok_or(AgentError::NotFound)?;
        match entry.info.state {
            AgentState::Cancelled => return Err(AgentError::Cancelled),
            AgentState::Running => return Err(AgentError::Busy),
            AgentState::Waiting if !resume => return Err(AgentError::NotWaiting),
            state if state != AgentState::Waiting && resume => return Err(AgentError::NotWaiting),
            _ => {}
        }
        let prompt = if resume {
            // Manual verification resumes the same task: do not rotate the
            // credential, since the original prompt may already be visible.
            entry.pending_prompt.clone()
        } else {
            let mut code = completion_code();
            // Even an extremely unlikely random collision must not preserve
            // a stale run's completion credential.
            while code == entry.completion_code {
                code = completion_code();
            }
            entry.completion_code = code.clone();
            entry.completion_failures = 0;
            format!(
                "{}\n\nWHEN FINISHED: Call the NovaGPT MCP tool subagent_finished with id={id}, code={code}, and result=<your complete final report>. Do this once. Keep the six-character code private; do not include it in messages or logs.",
                new_prompt.unwrap_or_default()
            )
        };
        if prompt.is_empty() {
            return Err(AgentError::Invalid);
        }
        entry.epoch += 1;
        let epoch = entry.epoch;
        entry.info.state = AgentState::Running;
        entry.info.error = None;
        entry.info.result = None;
        entry.pending_prompt = prompt.clone();
        entry.updates.send_replace(entry.info.clone());
        let created = !entry.ready;
        let info = entry.info.clone();
        drop(agents);
        self.start(id.to_owned(), prompt, created, epoch);
        Ok(info)
    }

    pub async fn cancel(&self, id: &str) -> AgentResult<AgentInfo> {
        let info = {
            let mut agents = self.inner.entries.lock().await;
            let entry = agents.get_mut(id).ok_or(AgentError::NotFound)?;
            entry.epoch += 1; // invalidates any pending backend completion
            entry.info.state = AgentState::Cancelled;
            entry.pending_prompt.clear();
            entry.updates.send_replace(entry.info.clone());
            entry.info.clone()
        };
        self.inner.backend.cancel(id).await;
        self.inner.backend.close(id).await;
        Ok(info)
    }

    pub async fn status(&self, id: Option<&str>) -> AgentResult<AgentStatus> {
        let agents = self.inner.entries.lock().await;
        let statuses = if let Some(id) = id {
            vec![agents.get(id).ok_or(AgentError::NotFound)?.info.clone()]
        } else {
            agents.values().map(|e| e.info.clone()).collect()
        };
        Ok(AgentStatus {
            agents: statuses
                .into_iter()
                .map(|mut info| {
                    info.result = None;
                    info
                })
                .collect(),
        })
    }
    /// Claim completion for exactly one live subagent execution.
    /// The six-character code is provided only inside that worker's prompt;
    /// five failed attempts lock the callback for this execution. A terminal
    /// or stale worker cannot be revived by another caller.
    pub async fn finished(
        &self,
        id: &str,
        code: &str,
        result: &str,
    ) -> AgentResult<AgentCompletionAck> {
        if result.trim().is_empty() || result.len() > MAX_RESULT {
            return Err(AgentError::Invalid);
        }
        let mut agents = self.inner.entries.lock().await;
        let entry = agents.get_mut(id).ok_or(AgentError::CompletionDenied)?;
        if !entry.ready
            || !matches!(entry.info.state, AgentState::Running | AgentState::Waiting)
            || entry.completion_failures >= MAX_COMPLETION_ATTEMPTS
        {
            return Err(AgentError::CompletionDenied);
        }
        let correct = code.len() == COMPLETION_KEY_LEN
            && entry
                .completion_code
                .as_bytes()
                .ct_eq(code.as_bytes())
                .unwrap_u8()
                == 1;
        if !correct {
            entry.completion_failures = entry.completion_failures.saturating_add(1);
            return Err(AgentError::CompletionDenied);
        }
        entry.info.state = AgentState::Completed;
        entry.info.error = None;
        entry.info.result = Some(AgentOutput {
            raw_response: result.to_owned(),
            untrusted: true,
            applied_to_workspace: false,
        });
        entry.pending_prompt.clear();
        entry.completion_code.clear();
        entry.updates.send_replace(entry.info.clone());
        Ok(AgentCompletionAck {
            id: entry.info.id.clone(),
            state: entry.info.state,
        })
    }

    /// Wait for the next terminal or human-action state using a watch channel.
    /// No polling loops or mutex guards are held across the wait; callers can
    /// repeat a timed-out request without interrupting the running subagent.
    pub async fn wait(&self, id: &str, max_wait_seconds: u64) -> AgentResult<AgentWait> {
        if !(1..=MAX_WAIT_SECONDS).contains(&max_wait_seconds) {
            return Err(AgentError::Invalid);
        }
        let mut changes = {
            let agents = self.inner.entries.lock().await;
            agents
                .get(id)
                .ok_or(AgentError::NotFound)?
                .updates
                .subscribe()
        };
        let snapshot = changes.borrow_and_update().clone();
        if snapshot.state != AgentState::Running {
            return Ok(AgentWait {
                agent: snapshot,
                timed_out: false,
            });
        }
        let update = timeout(Duration::from_secs(max_wait_seconds), async {
            loop {
                changes.changed().await.map_err(|_| AgentError::Backend)?;
                let snapshot = changes.borrow_and_update().clone();
                if snapshot.state != AgentState::Running {
                    return Ok(snapshot);
                }
            }
        })
        .await;
        match update {
            Ok(Ok(agent)) => Ok(AgentWait {
                agent,
                timed_out: false,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let agent = changes.borrow().clone();
                Ok(AgentWait {
                    timed_out: agent.state == AgentState::Running,
                    agent,
                })
            }
        }
    }

    pub async fn result(&self, id: &str) -> AgentResult<AgentInfo> {
        self.inner
            .entries
            .lock()
            .await
            .get(id)
            .map(|entry| entry.info.clone())
            .ok_or(AgentError::NotFound)
    }
    pub async fn diagnostics(&self, id: &str) -> AgentResult<serde_json::Value> {
        self.result(id).await?;
        Ok(self.inner.backend.diagnostics(id).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct FakeBackend;
    #[async_trait]
    impl AgentBackend for FakeBackend {
        fn name(&self) -> &'static str {
            "test"
        }
        fn model(&self) -> &'static str {
            "fake"
        }
        async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
            Ok(())
        }
        async fn send(
            &self,
            _id: &str,
            prompt: &str,
        ) -> Result<(String, Option<String>), BackendFailure> {
            Ok((format!("echo: {prompt}"), None))
        }
        async fn cancel(&self, _id: &str) {}
        async fn diagnostics(&self, _id: &str) -> serde_json::Value {
            serde_json::json!({"ready":true})
        }
    }
    #[tokio::test]
    async fn spawn_status_message_and_untrusted_result() {
        let dir = tempfile::tempdir().expect("fixture");
        let workspaces = WorkspaceRegistry::new(dir.path(), &[]).expect("registry");
        let manager = AgentManager::new(workspaces, Arc::new(FakeBackend));
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "inspect".into(),
                role: AgentRole::Explorer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        assert!(info.untrusted);
        for _ in 0..200 {
            if manager.result(&info.id).await.expect("status").state == AgentState::Completed {
                break;
            }
            tokio::task::yield_now().await;
        }
        let info = manager.result(&info.id).await.expect("status");
        assert_eq!(info.state, AgentState::Completed);
        assert!(!info.result.expect("result").applied_to_workspace);
        let continued = manager.message(&info.id, "next").await.expect("followup");
        assert_eq!(continued.state, AgentState::Running);
    }
    struct PausedBackend {
        created: tokio::sync::Notify,
        continue_create: tokio::sync::Notify,
        sent: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl AgentBackend for PausedBackend {
        fn name(&self) -> &'static str {
            "test-paused"
        }
        fn model(&self) -> &'static str {
            "fake"
        }
        async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
            self.created.notify_one();
            self.continue_create.notified().await;
            Ok(())
        }
        async fn send(
            &self,
            _id: &str,
            _prompt: &str,
        ) -> Result<(String, Option<String>), BackendFailure> {
            self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(("not expected".into(), None))
        }
        async fn cancel(&self, _id: &str) {}
        async fn diagnostics(&self, _id: &str) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    #[tokio::test]
    async fn cancellation_during_create_prevents_later_send() {
        let dir = tempfile::tempdir().expect("fixture");
        let backend = Arc::new(PausedBackend {
            created: tokio::sync::Notify::new(),
            continue_create: tokio::sync::Notify::new(),
            sent: std::sync::atomic::AtomicUsize::new(0),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            backend.clone(),
        );
        let agent = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "check".into(),
                role: AgentRole::Tester,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        backend.created.notified().await;
        manager.cancel(&agent.id).await.expect("cancel");
        backend.continue_create.notify_one();
        for _ in 0..200 {
            tokio::task::yield_now().await;
        }
        assert_eq!(backend.sent.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            manager.result(&agent.id).await.expect("status").state,
            AgentState::Cancelled
        );
    }

    #[tokio::test]
    async fn wait_returns_completed_output_and_rejects_invalid_requests() {
        let dir = tempfile::tempdir().expect("fixture");
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            Arc::new(FakeBackend),
        );
        assert!(matches!(
            manager.wait("agent-unknown", 1).await,
            Err(AgentError::NotFound)
        ));
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "read-only review".into(),
                role: AgentRole::Reviewer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        assert!(matches!(
            manager.wait(&info.id, 0).await,
            Err(AgentError::Invalid)
        ));
        assert!(matches!(
            manager.wait(&info.id, MAX_WAIT_SECONDS + 1).await,
            Err(AgentError::Invalid)
        ));
        let finished = manager
            .wait(&info.id, 2)
            .await
            .expect("wait for completion");
        assert!(!finished.timed_out);
        assert_eq!(finished.agent.state, AgentState::Completed);
        let result = finished.agent.result.expect("untrusted result");
        assert!(result.raw_response.contains("read-only review"));
        assert!(result.untrusted);
        assert!(!result.applied_to_workspace);
        // Already completed: return immediately without waiting.
        let again = manager.wait(&info.id, 2).await.expect("already terminal");
        assert_eq!(again.agent.state, AgentState::Completed);
        assert!(!again.timed_out);
    }

    #[tokio::test]
    async fn wait_timeout_does_not_cancel_worker_and_cancel_wakes_waiter() {
        let dir = tempfile::tempdir().expect("fixture");
        let backend = Arc::new(PausedBackend {
            created: tokio::sync::Notify::new(),
            continue_create: tokio::sync::Notify::new(),
            sent: std::sync::atomic::AtomicUsize::new(0),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            backend.clone(),
        );
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "pause".into(),
                role: AgentRole::Tester,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        backend.created.notified().await;
        let timed = manager.wait(&info.id, 1).await.expect("bounded wait");
        assert!(timed.timed_out);
        assert_eq!(timed.agent.state, AgentState::Running);
        let waiting_manager = manager.clone();
        let id = info.id.clone();
        let waiting = tokio::spawn(async move {
            waiting_manager
                .wait(&id, 5)
                .await
                .expect("wait for cancellation")
        });
        tokio::task::yield_now().await;
        manager.cancel(&info.id).await.expect("cancel");
        let cancelled = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("woken promptly")
            .expect("task join");
        assert_eq!(cancelled.agent.state, AgentState::Cancelled);
        assert!(!cancelled.timed_out);
        backend.continue_create.notify_one();
    }

    struct ManualBackend;
    #[async_trait]
    impl AgentBackend for ManualBackend {
        fn name(&self) -> &'static str {
            "manual-test"
        }
        fn model(&self) -> &'static str {
            "fixture"
        }
        async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
            Ok(())
        }
        async fn send(
            &self,
            _id: &str,
            _prompt: &str,
        ) -> Result<(String, Option<String>), BackendFailure> {
            Err(BackendFailure::Manual("verification"))
        }
        async fn cancel(&self, _id: &str) {}
        async fn diagnostics(&self, _id: &str) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    #[tokio::test]
    async fn wait_stops_on_human_action_without_claiming_completion() {
        let dir = tempfile::tempdir().expect("fixture");
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            Arc::new(ManualBackend),
        );
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "requires human".into(),
                role: AgentRole::Explorer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        let snapshot = manager.wait(&info.id, 2).await.expect("wait for manual");
        assert!(!snapshot.timed_out);
        assert_eq!(snapshot.agent.state, AgentState::Waiting);
        assert!(snapshot.agent.result.is_none());
        assert!(
            snapshot
                .agent
                .error
                .as_deref()
                .is_some_and(|s| s.contains("verification"))
        );
    }

    struct HeldSendBackend {
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait]
    impl AgentBackend for HeldSendBackend {
        fn name(&self) -> &'static str {
            "held-send"
        }
        fn model(&self) -> &'static str {
            "fixture"
        }
        async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
            Ok(())
        }
        async fn send(
            &self,
            _id: &str,
            _prompt: &str,
        ) -> Result<(String, Option<String>), BackendFailure> {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(("late browser result".into(), None))
        }
        async fn cancel(&self, _id: &str) {}
        async fn diagnostics(&self, _id: &str) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    #[tokio::test]
    async fn finished_uses_six_character_private_code_and_wakes_waiter() {
        let dir = tempfile::tempdir().expect("fixture");
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            backend.clone(),
        );
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "inspect source".into(),
                role: AgentRole::Reviewer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        backend.entered.notified().await;
        let (code, prompt) = {
            let entries = manager.inner.entries.lock().await;
            let entry = entries.get(&info.id).expect("entry");
            (entry.completion_code.clone(), entry.pending_prompt.clone())
        };
        assert_eq!(code.len(), COMPLETION_KEY_LEN);
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
        assert!(prompt.contains(&format!("code={code}")));
        assert!(prompt.contains(&format!("id={}", info.id)));
        assert!(
            !serde_json::to_string(&info)
                .expect("serialize info")
                .contains(&code)
        );
        assert!(
            !serde_json::to_string(&manager.status(Some(&info.id)).await.expect("status"))
                .expect("serialize status")
                .contains(&code)
        );

        let other = if code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
        assert!(matches!(
            manager.finished(&info.id, other, "forged").await,
            Err(AgentError::CompletionDenied)
        ));
        let waiting_manager = manager.clone();
        let id = info.id.clone();
        let waiter = tokio::spawn(async move {
            waiting_manager
                .wait(&id, 5)
                .await
                .expect("wait for callback")
        });
        tokio::task::yield_now().await;
        let ack = manager
            .finished(&info.id, &code, "verified report")
            .await
            .expect("finish");
        assert_eq!(ack.id, info.id);
        assert_eq!(ack.state, AgentState::Completed);
        let done = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("wake")
            .expect("task");
        assert!(!done.timed_out);
        assert_eq!(done.agent.state, AgentState::Completed);
        assert_eq!(
            done.agent.result.expect("output").raw_response,
            "verified report"
        );
        assert!(matches!(
            manager.finished(&info.id, &code, "duplicate").await,
            Err(AgentError::CompletionDenied)
        ));
        backend.release.notify_one();
        tokio::task::yield_now().await;
        let result = manager.result(&info.id).await.expect("result");
        assert_eq!(
            result.result.expect("report").raw_response,
            "verified report"
        );
    }

    #[tokio::test]
    async fn short_code_is_rate_limited_rotated_and_invalidated_after_cancel() {
        let dir = tempfile::tempdir().expect("fixture");
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            backend.clone(),
        );
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "first".into(),
                role: AgentRole::Tester,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        backend.entered.notified().await;
        let old_code = {
            let entries = manager.inner.entries.lock().await;
            entries
                .get(&info.id)
                .expect("entry")
                .completion_code
                .clone()
        };
        let wrong = if old_code == "AAAAAA" {
            "BBBBBB"
        } else {
            "AAAAAA"
        };
        for _ in 0..MAX_COMPLETION_ATTEMPTS {
            assert!(matches!(
                manager.finished(&info.id, wrong, "bad guess").await,
                Err(AgentError::CompletionDenied)
            ));
        }
        assert!(matches!(
            manager.finished(&info.id, &old_code, "late correct").await,
            Err(AgentError::CompletionDenied)
        ));
        // Browser result is still permitted even after the callback is rate-limited.
        backend.release.notify_one();
        assert_eq!(
            manager
                .wait(&info.id, 3)
                .await
                .expect("browser fallback")
                .agent
                .state,
            AgentState::Completed
        );
        manager
            .message(&info.id, "second task")
            .await
            .expect("followup");
        backend.entered.notified().await;
        let new_code = {
            let entries = manager.inner.entries.lock().await;
            entries
                .get(&info.id)
                .expect("entry")
                .completion_code
                .clone()
        };
        assert_ne!(old_code, new_code);
        assert_eq!(new_code.len(), 6);
        assert!(matches!(
            manager.finished(&info.id, &old_code, "old task").await,
            Err(AgentError::CompletionDenied)
        ));
        manager.cancel(&info.id).await.expect("cancel");
        assert!(matches!(
            manager
                .finished(&info.id, &new_code, "cancelled task")
                .await,
            Err(AgentError::CompletionDenied)
        ));
        backend.release.notify_one();
    }

    #[tokio::test]
    async fn waiting_manual_run_may_be_completed_with_its_current_code() {
        let dir = tempfile::tempdir().expect("fixture");
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            Arc::new(ManualBackend),
        );
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "manual".into(),
                role: AgentRole::Explorer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        let waiting = manager.wait(&info.id, 2).await.expect("wait");
        assert_eq!(waiting.agent.state, AgentState::Waiting);
        let code = {
            let entries = manager.inner.entries.lock().await;
            entries
                .get(&info.id)
                .expect("entry")
                .completion_code
                .clone()
        };
        let ack = manager
            .finished(&info.id, &code, "completed via MCP")
            .await
            .expect("finish");
        assert_eq!(ack.state, AgentState::Completed);
        assert_eq!(
            manager
                .wait(&info.id, 1)
                .await
                .expect("finished")
                .agent
                .state,
            AgentState::Completed
        );
        assert!(matches!(
            manager.resume(&info.id, true).await,
            Err(AgentError::NotWaiting)
        ));
    }

    #[tokio::test]
    async fn cancelled_agent_cannot_be_resumed() {
        let dir = tempfile::tempdir().expect("fixture");
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            Arc::new(FakeBackend),
        );
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "inspect".into(),
                role: AgentRole::Reviewer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        manager.cancel(&info.id).await.expect("cancel");
        assert!(matches!(
            manager.resume(&info.id, true).await,
            Err(AgentError::Cancelled)
        ));
    }
}
