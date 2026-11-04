//! Untrusted, opt-in asynchronous subagents. No implicit ChatGPT chat identity.
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use novagpt_browser::{BrowserError, ChatGptBrowser};
use novagpt_core::WorkspaceRegistry;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
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
const MAX_PATCH_ATTEMPTS: u8 = 16;
const MAX_CREATE_ATTEMPTS: u16 = 256;
const MAX_AGENT_PATCH_BYTES: usize = 200_000;

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
    #[error("Subagent patch is not authorized for this execution or path")]
    PatchDenied,
    #[error("Subagent patch failed validation or does not apply cleanly")]
    PatchRejected,
    #[error("Subagent file creation was rejected; check path, size and conflicts")]
    CreateRejected,
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
    pub patches_applied: usize,
    pub patched_paths: Vec<String>,
    pub files_created: usize,
    pub created_paths: Vec<String>,
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

#[derive(Debug, Clone, Serialize)]
pub struct AgentCreateReceipt {
    pub id: String,
    pub workspace_id: String,
    pub path: String,
    pub sha256: String,
    pub bytes: usize,
    pub files_created: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentPatchReceipt {
    pub id: String,
    pub workspace_id: String,
    pub paths: Vec<String>,
    pub bytes: u64,
    pub patches_applied: usize,
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
    /// Stop awaiting an already acknowledged response without closing its tab.
    async fn complete(&self, _id: &str) {}
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
    async fn complete(&self, id: &str) {
        let _ = self.browser.complete(id).await;
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
    completion_claimed: bool,
    patch_attempts: u8,
    create_attempts: u16,
    allowed_paths: Vec<PathBuf>,
    patch_gate: Arc<Mutex<()>>,
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
        let mut allowed_paths = Vec::with_capacity(request.paths.len());
        for path in &request.paths {
            let resolved = self
                .inner
                .workspaces
                .resolve(Some(&workspace.workspace_id), path, false)
                .map_err(|_| AgentError::Invalid)?;
            if !allowed_paths.contains(&resolved) {
                allowed_paths.push(resolved);
            }
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
                "Implementer: use the NovaGPT MCP subagent_apply_patch tool to change selected existing files and subagent_create_file to CREATE new files at ANY safe path inside the assigned workspace, even if no paths were selected at spawn. Both tools require your private code. New files may have nested parent directories and must never overwrite existing files. Only a successful tool receipt proves an applied change. Never claim files were written without a receipt; if tools are unavailable, return proposed diffs clearly labelled NOT APPLIED. The parent verifies the real workspace."
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
            patches_applied: 0,
            patched_paths: Vec::new(),
            files_created: 0,
            created_paths: Vec::new(),
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
                    completion_claimed: false,
                    patch_attempts: 0,
                    create_attempts: 0,
                    allowed_paths,
                    patch_gate: Arc::new(Mutex::new(())),
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
            // Serialize completion with an in-flight file patch for this agent.
            let Ok(patch_gate) = manager.patch_gate(&id).await else {
                return;
            };
            let _patch_guard = patch_gate.lock().await;
            let mut agents = manager.inner.entries.lock().await;
            if let Some(entry) = agents.get_mut(&id) {
                // An authenticated subagent callback may have already
                // completed this run. A stale browser response must not
                // overwrite its result or resurrect a cancelled agent.
                if entry.epoch != epoch
                    || entry.info.state != AgentState::Running
                    || entry.completion_claimed
                {
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
                                applied_to_workspace: entry.info.patches_applied > 0
                                    || entry.info.files_created > 0,
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

    async fn patch_gate(&self, id: &str) -> AgentResult<Arc<Mutex<()>>> {
        let agents = self.inner.entries.lock().await;
        Ok(agents
            .get(id)
            .ok_or(AgentError::NotFound)?
            .patch_gate
            .clone())
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
        // A completion callback may be awaiting browser cleanup while holding
        // the patch gate. Report Busy immediately instead of blocking its caller.
        {
            let entries = self.inner.entries.lock().await;
            if entries
                .get(id)
                .ok_or(AgentError::NotFound)?
                .completion_claimed
            {
                return Err(AgentError::Busy);
            }
        }
        let gate = self.patch_gate(id).await?;
        let _patch_guard = gate.lock().await;
        let mut agents = self.inner.entries.lock().await;
        let entry = agents.get_mut(id).ok_or(AgentError::NotFound)?;
        if entry.completion_claimed {
            return Err(AgentError::Busy);
        }
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
            entry.completion_claimed = false;
            entry.patch_attempts = 0;
            entry.create_attempts = 0;
            let mut next = format!(
                "{}\n\nWHEN FINISHED: Call the NovaGPT MCP tool subagent_finished with id={id}, code={code}, and result=<your complete final report>. Do this once. Keep the six-character code private; do not include it in messages or logs.",
                new_prompt.unwrap_or_default()
            );
            if entry.info.role == AgentRole::Implementer {
                next.push_str("\n\nFor existing files call subagent_apply_patch with this id/code, one selected file per patch. For NEW files anywhere within the authorized workspace call subagent_create_file with id/code/path/content; paths need not be selected. Check the receipt before saying APPLIED; otherwise label changes NOT APPLIED.");
            }
            next
        };
        if prompt.is_empty() {
            return Err(AgentError::Invalid);
        }
        entry.epoch += 1;
        let epoch = entry.epoch;
        entry.info.state = AgentState::Running;
        entry.info.error = None;
        entry.info.result = None;
        if !resume {
            entry.info.patches_applied = 0;
            entry.info.patched_paths.clear();
            entry.info.files_created = 0;
            entry.info.created_paths.clear();
        }
        entry.pending_prompt = prompt.clone();
        entry.updates.send_replace(entry.info.clone());
        let created = !entry.ready;
        let info = entry.info.clone();
        drop(agents);
        self.start(id.to_owned(), prompt, created, epoch);
        Ok(info)
    }

    pub async fn cancel(&self, id: &str) -> AgentResult<AgentInfo> {
        let gate = self.patch_gate(id).await?;
        let _patch_guard = gate.lock().await;
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
        let gate = self
            .patch_gate(id)
            .await
            .map_err(|_| AgentError::CompletionDenied)?;
        let _patch_guard = gate.lock().await;
        {
            let mut agents = self.inner.entries.lock().await;
            let entry = agents.get_mut(id).ok_or(AgentError::CompletionDenied)?;
            if !entry.ready
                || entry.completion_claimed
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
            // Claim before releasing the registry lock. Do not wake waiters or
            // permit new messages until the previous browser poller has been
            // signalled to stop: otherwise two sends can race on one tab.
            entry.completion_claimed = true;
            entry.info.error = None;
            entry.info.result = Some(AgentOutput {
                raw_response: result.to_owned(),
                untrusted: true,
                applied_to_workspace: entry.info.patches_applied > 0
                    || entry.info.files_created > 0,
            });
            entry.pending_prompt.clear();
            entry.completion_code.clear();
        }
        self.inner.backend.complete(id).await;
        let mut agents = self.inner.entries.lock().await;
        let entry = agents.get_mut(id).ok_or(AgentError::CompletionDenied)?;
        if entry.info.state == AgentState::Cancelled {
            return Err(AgentError::Cancelled);
        }
        entry.completion_claimed = false;
        entry.info.state = AgentState::Completed;
        entry.updates.send_replace(entry.info.clone());
        Ok(AgentCompletionAck {
            id: entry.info.id.clone(),
            state: AgentState::Completed,
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

    /// Apply an explicitly requested, authenticated, single-file update in the
    /// assigned real workspace. The blocking worker owns the per-agent gate
    /// until Git exits, even if its async caller disconnects.
    pub async fn apply_patch(
        &self,
        id: &str,
        code: &str,
        patch: &str,
    ) -> AgentResult<AgentPatchReceipt> {
        if patch.is_empty() || patch.len() > MAX_AGENT_PATCH_BYTES {
            return Err(AgentError::PatchRejected);
        }
        let id = id.to_owned();
        let code = code.to_owned();
        let patch = patch.to_owned();
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            let relative =
                novagpt_core::patch_target(&patch).map_err(|_| AgentError::PatchRejected)?;
            let gate = manager
                .inner
                .entries
                .blocking_lock()
                .get(&id)
                .ok_or(AgentError::PatchDenied)?
                .patch_gate
                .clone();
            let _gate = gate.blocking_lock();
            let workspace_id = {
                let mut entries = manager.inner.entries.blocking_lock();
                let entry = entries.get_mut(&id).ok_or(AgentError::PatchDenied)?;
                if !entry.ready
                    || entry.info.role != AgentRole::Implementer
                    || !matches!(entry.info.state, AgentState::Running | AgentState::Waiting)
                    || entry.completion_claimed
                    || entry.completion_failures >= MAX_COMPLETION_ATTEMPTS
                    || entry.patch_attempts >= MAX_PATCH_ATTEMPTS
                {
                    return Err(AgentError::PatchDenied);
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
                    return Err(AgentError::PatchDenied);
                }
                let workspace_id = entry.info.workspace_id.clone();
                let target = manager
                    .inner
                    .workspaces
                    .resolve(Some(&workspace_id), &relative, true)
                    .map_err(|_| AgentError::PatchDenied)?;
                // A file this run created is also an authorized edit target,
                // even when its path was not known at spawn time.
                if !entry.allowed_paths.contains(&target)
                    && !entry.info.created_paths.contains(&relative)
                {
                    return Err(AgentError::PatchDenied);
                }
                entry.patch_attempts += 1;
                workspace_id
            };
            let receipt = manager
                .inner
                .workspaces
                .apply_unified_patch(Some(&workspace_id), &patch)
                .map_err(|_| AgentError::PatchRejected)?;
            let mut entries = manager.inner.entries.blocking_lock();
            let entry = entries.get_mut(&id).ok_or(AgentError::PatchDenied)?;
            entry.info.patches_applied += 1;
            for path in &receipt.paths {
                if !entry.info.patched_paths.contains(path) {
                    entry.info.patched_paths.push(path.clone());
                }
            }
            entry.updates.send_replace(entry.info.clone());
            Ok(AgentPatchReceipt {
                id,
                workspace_id,
                paths: receipt.paths,
                bytes: receipt.bytes,
                patches_applied: entry.info.patches_applied,
            })
        })
        .await
        .map_err(|_| AgentError::Backend)?
    }

    /// Create a new text file anywhere inside the agent's approved workspace.
    /// Unlike updating existing files, creation needs no preselected path. The
    /// exclusive core create operation never replaces another agent's work.
    pub async fn create_file(
        &self,
        id: &str,
        code: &str,
        path: &str,
        content: &str,
    ) -> AgentResult<AgentCreateReceipt> {
        // Bound incoming allocations before cloning work into spawn_blocking.
        if path.trim().is_empty() || path.len() > 1024 || content.len() > 1_000_000 {
            return Err(AgentError::CreateRejected);
        }
        let manager = self.clone();
        let id = id.to_owned();
        let code = code.to_owned();
        let path = path.to_owned();
        let content = content.to_owned();
        tokio::task::spawn_blocking(move || {
            let gate = manager
                .inner
                .entries
                .blocking_lock()
                .get(&id)
                .ok_or(AgentError::PatchDenied)?
                .patch_gate
                .clone();
            // Holding the per-agent gate until the blocking write finishes
            // prevents cancellation, completion and new runs from racing it.
            let _guard = gate.blocking_lock();
            let workspace_id = {
                let mut entries = manager.inner.entries.blocking_lock();
                let entry = entries.get_mut(&id).ok_or(AgentError::PatchDenied)?;
                if !entry.ready
                    || entry.info.role != AgentRole::Implementer
                    || !matches!(entry.info.state, AgentState::Running | AgentState::Waiting)
                    || entry.completion_claimed
                    || entry.completion_failures >= MAX_COMPLETION_ATTEMPTS
                    || entry.create_attempts >= MAX_CREATE_ATTEMPTS
                {
                    return Err(AgentError::PatchDenied);
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
                    return Err(AgentError::PatchDenied);
                }
                entry.create_attempts += 1;
                entry.info.workspace_id.clone()
            };
            let receipt = manager
                .inner
                .workspaces
                .create_new_file(Some(&workspace_id), &path, &content)
                .map_err(|_| AgentError::CreateRejected)?;
            let mut entries = manager.inner.entries.blocking_lock();
            let entry = entries.get_mut(&id).ok_or(AgentError::PatchDenied)?;
            entry.info.files_created += 1;
            if !entry.info.created_paths.contains(&receipt.path) {
                entry.info.created_paths.push(receipt.path.clone());
            }
            entry.updates.send_replace(entry.info.clone());
            Ok(AgentCreateReceipt {
                id,
                workspace_id,
                path: receipt.path,
                sha256: receipt.sha256,
                bytes: receipt.bytes,
                files_created: entry.info.files_created,
            })
        })
        .await
        .map_err(|_| AgentError::Backend)?
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
    async fn implementer_applies_only_assigned_files_with_verified_receipts() {
        let dir = tempfile::tempdir().expect("fixture");
        std::fs::write(dir.path().join("one.rs"), "before one\n").unwrap();
        std::fs::write(dir.path().join("two.rs"), "before two\n").unwrap();
        std::fs::write(dir.path().join("other.rs"), "private\n").unwrap();
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("workspace"),
            backend.clone(),
        );
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "edit assigned files".into(),
                role: AgentRole::Implementer,
                paths: vec!["one.rs".into(), "two.rs".into()],
                context: None,
            })
            .await
            .expect("spawn");
        backend.entered.notified().await;
        let code = manager.inner.entries.lock().await[&info.id]
            .completion_code
            .clone();
        let first = "--- a/one.rs\n+++ b/one.rs\n@@ -1 +1 @@\n-before one\n+after one\n";
        let second = "diff --git a/two.rs b/two.rs\n--- a/two.rs\n+++ b/two.rs\n@@ -1 +1 @@\n-before two\n+after two\n";
        let forbidden = "--- a/other.rs\n+++ b/other.rs\n@@ -1 +1 @@\n-private\n+corrupted\n";
        let wrong = if code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
        assert!(matches!(
            manager.apply_patch(&info.id, wrong, first).await,
            Err(AgentError::PatchDenied)
        ));
        assert!(matches!(
            manager.apply_patch(&info.id, &code, forbidden).await,
            Err(AgentError::PatchDenied)
        ));
        let first_receipt = manager
            .apply_patch(&info.id, &code, first)
            .await
            .expect("real first edit");
        assert_eq!(first_receipt.paths, ["one.rs"]);
        assert_eq!(first_receipt.patches_applied, 1);
        let second_receipt = manager
            .apply_patch(&info.id, &code, second)
            .await
            .expect("real second edit");
        assert_eq!(second_receipt.paths, ["two.rs"]);
        assert_eq!(second_receipt.patches_applied, 2);
        assert_eq!(
            std::fs::read_to_string(dir.path().join("one.rs")).unwrap(),
            "after one\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("two.rs")).unwrap(),
            "after two\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("other.rs")).unwrap(),
            "private\n"
        );
        assert!(matches!(
            manager.apply_patch(&info.id, &code, first).await,
            Err(AgentError::PatchRejected)
        ));
        let state = manager
            .status(Some(&info.id))
            .await
            .unwrap()
            .agents
            .remove(0);
        assert_eq!(state.patches_applied, 2);
        assert_eq!(state.patched_paths, ["one.rs", "two.rs"]);
        assert_eq!(state.state, AgentState::Running);
        manager
            .finished(&info.id, &code, "Verified receipts for both edits")
            .await
            .unwrap();
        let done = manager.wait(&info.id, 2).await.unwrap();
        assert!(done.agent.result.unwrap().applied_to_workspace);
        assert!(matches!(
            manager.apply_patch(&info.id, &code, second).await,
            Err(AgentError::PatchDenied)
        ));
        backend.release.notify_one();
    }

    #[tokio::test]
    async fn patch_rejects_read_only_roles_old_codes_and_cancelled_runs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source.rs"), "old\n").unwrap();
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
            backend.clone(),
        );
        let patch = "--- a/source.rs\n+++ b/source.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let reviewer = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "inspect".into(),
                role: AgentRole::Reviewer,
                paths: vec!["source.rs".into()],
                context: None,
            })
            .await
            .unwrap();
        backend.entered.notified().await;
        let review_code = manager.inner.entries.lock().await[&reviewer.id]
            .completion_code
            .clone();
        assert!(matches!(
            manager.apply_patch(&reviewer.id, &review_code, patch).await,
            Err(AgentError::PatchDenied)
        ));
        manager.cancel(&reviewer.id).await.unwrap();
        assert!(matches!(
            manager.apply_patch(&reviewer.id, &review_code, patch).await,
            Err(AgentError::PatchDenied)
        ));
        let worker = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "update".into(),
                role: AgentRole::Implementer,
                paths: vec!["source.rs".into()],
                context: None,
            })
            .await
            .unwrap();
        backend.entered.notified().await;
        let first_code = manager.inner.entries.lock().await[&worker.id]
            .completion_code
            .clone();
        assert!(matches!(
            manager.apply_patch(&worker.id, &review_code, patch).await,
            Err(AgentError::PatchDenied)
        ));
        manager
            .finished(&worker.id, &first_code, "not applied")
            .await
            .unwrap();
        manager.message(&worker.id, "next execution").await.unwrap();
        backend.entered.notified().await;
        let second_code = manager.inner.entries.lock().await[&worker.id]
            .completion_code
            .clone();
        assert_ne!(first_code, second_code);
        assert!(matches!(
            manager.apply_patch(&worker.id, &first_code, patch).await,
            Err(AgentError::PatchDenied)
        ));
        assert_eq!(
            manager
                .apply_patch(&worker.id, &second_code, patch)
                .await
                .unwrap()
                .patches_applied,
            1
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("source.rs")).unwrap(),
            "new\n"
        );
        manager.cancel(&worker.id).await.unwrap();
        assert!(matches!(
            manager.apply_patch(&worker.id, &second_code, patch).await,
            Err(AgentError::PatchDenied)
        ));
        backend.release.notify_waiters();
    }

    #[tokio::test]
    async fn implementer_creates_and_then_edits_any_safe_new_file_without_preselection() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("existing.rs"), "keep\n").unwrap();
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
            backend.clone(),
        );
        let worker = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "create new source files".into(),
                role: AgentRole::Implementer,
                paths: vec![],
                context: None,
            })
            .await
            .unwrap();
        backend.entered.notified().await;
        let code = manager.inner.entries.lock().await[&worker.id]
            .completion_code
            .clone();
        let wrong = if code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
        assert!(matches!(
            manager
                .create_file(&worker.id, wrong, "src/new.rs", "created\n")
                .await,
            Err(AgentError::PatchDenied)
        ));
        let created = manager
            .create_file(&worker.id, &code, "src/deep/generated.rs", "created\n")
            .await
            .unwrap();
        assert_eq!(created.path, "src/deep/generated.rs");
        assert_eq!(created.files_created, 1);
        assert_eq!(created.bytes, "created\n".len());
        assert_eq!(created.sha256.len(), 64);
        assert!(matches!(
            manager
                .create_file(&worker.id, &code, "src/deep/generated.rs", "overwrite\n")
                .await,
            Err(AgentError::CreateRejected)
        ));
        assert!(matches!(
            manager
                .create_file(&worker.id, &code, ".env", "secret")
                .await,
            Err(AgentError::CreateRejected)
        ));
        assert!(matches!(
            manager
                .create_file(&worker.id, &code, "../outside.txt", "escape")
                .await,
            Err(AgentError::CreateRejected)
        ));
        let change = "--- a/src/deep/generated.rs\n+++ b/src/deep/generated.rs\n@@ -1 +1 @@\n-created\n+updated\n";
        assert_eq!(
            manager
                .apply_patch(&worker.id, &code, change)
                .await
                .unwrap()
                .patches_applied,
            1
        );
        let other = "--- a/existing.rs\n+++ b/existing.rs\n@@ -1 +1 @@\n-keep\n+destroyed\n";
        assert!(matches!(
            manager.apply_patch(&worker.id, &code, other).await,
            Err(AgentError::PatchDenied)
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("src/deep/generated.rs")).unwrap(),
            "updated\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("existing.rs")).unwrap(),
            "keep\n"
        );
        let status = manager.status(Some(&worker.id)).await.unwrap();
        assert_eq!(status.agents[0].files_created, 1);
        assert_eq!(status.agents[0].created_paths, ["src/deep/generated.rs"]);
        manager
            .finished(&worker.id, &code, "Created and edited")
            .await
            .unwrap();
        assert!(
            manager
                .result(&worker.id)
                .await
                .unwrap()
                .result
                .unwrap()
                .applied_to_workspace
        );
        backend.release.notify_one();
    }

    #[tokio::test]
    async fn file_creation_is_denied_for_other_roles_cancelled_and_old_runs() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
            backend.clone(),
        );
        let reviewer = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "review".into(),
                role: AgentRole::Reviewer,
                paths: vec![],
                context: None,
            })
            .await
            .unwrap();
        backend.entered.notified().await;
        let reviewer_code = manager.inner.entries.lock().await[&reviewer.id]
            .completion_code
            .clone();
        assert!(matches!(
            manager
                .create_file(&reviewer.id, &reviewer_code, "reviewer.txt", "no")
                .await,
            Err(AgentError::PatchDenied)
        ));
        manager.cancel(&reviewer.id).await.unwrap();
        let worker = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "create".into(),
                role: AgentRole::Implementer,
                paths: vec![],
                context: None,
            })
            .await
            .unwrap();
        backend.entered.notified().await;
        let first_code = manager.inner.entries.lock().await[&worker.id]
            .completion_code
            .clone();
        assert!(matches!(
            manager
                .create_file(&worker.id, &reviewer_code, "cross-agent.txt", "no")
                .await,
            Err(AgentError::PatchDenied)
        ));
        manager
            .finished(&worker.id, &first_code, "nothing created")
            .await
            .unwrap();
        manager
            .message(&worker.id, "create new file")
            .await
            .unwrap();
        backend.entered.notified().await;
        let second_code = manager.inner.entries.lock().await[&worker.id]
            .completion_code
            .clone();
        assert_ne!(first_code, second_code);
        assert!(matches!(
            manager
                .create_file(&worker.id, &first_code, "stale.txt", "no")
                .await,
            Err(AgentError::PatchDenied)
        ));
        assert_eq!(
            manager
                .create_file(&worker.id, &second_code, "new.txt", "yes")
                .await
                .unwrap()
                .files_created,
            1
        );
        manager.cancel(&worker.id).await.unwrap();
        assert!(matches!(
            manager
                .create_file(&worker.id, &second_code, "after-cancel.txt", "no")
                .await,
            Err(AgentError::PatchDenied)
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
            "yes"
        );
        for name in [
            "reviewer.txt",
            "cross-agent.txt",
            "stale.txt",
            "after-cancel.txt",
        ] {
            assert!(!dir.path().join(name).exists(), "unexpected file: {name}");
        }
        backend.release.notify_waiters();
    }

    #[tokio::test]
    async fn patch_attempts_are_bounded_even_when_git_rejects_every_diff() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("source.rs"), "before\n").unwrap();
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
            backend.clone(),
        );
        let worker = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "bounded edits".into(),
                role: AgentRole::Implementer,
                paths: vec!["source.rs".into()],
                context: None,
            })
            .await
            .unwrap();
        backend.entered.notified().await;
        let code = manager.inner.entries.lock().await[&worker.id]
            .completion_code
            .clone();
        let conflict = "--- a/source.rs\n+++ b/source.rs\n@@ -1 +1 @@\n-nonexistent\n+after\n";
        let valid = "--- a/source.rs\n+++ b/source.rs\n@@ -1 +1 @@\n-before\n+after\n";
        for _ in 0..MAX_PATCH_ATTEMPTS {
            assert!(matches!(
                manager.apply_patch(&worker.id, &code, conflict).await,
                Err(AgentError::PatchRejected)
            ));
        }
        assert!(matches!(
            manager.apply_patch(&worker.id, &code, valid).await,
            Err(AgentError::PatchDenied)
        ));
        assert_eq!(
            manager.status(Some(&worker.id)).await.unwrap().agents[0].patches_applied,
            0
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("source.rs")).unwrap(),
            "before\n"
        );
        manager.cancel(&worker.id).await.unwrap();
        backend.release.notify_one();
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

    struct DelayedCompleteBackend {
        sending: tokio::sync::Notify,
        completing: tokio::sync::Notify,
        release_complete: tokio::sync::Notify,
    }

    #[async_trait]
    impl AgentBackend for DelayedCompleteBackend {
        fn name(&self) -> &'static str {
            "delayed-complete"
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
            self.sending.notify_one();
            std::future::pending().await
        }
        async fn cancel(&self, _id: &str) {}
        async fn complete(&self, _id: &str) {
            self.completing.notify_one();
            self.release_complete.notified().await;
        }
        async fn diagnostics(&self, _id: &str) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    #[tokio::test]
    async fn completion_callback_blocks_new_messages_until_backend_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let backend = Arc::new(DelayedCompleteBackend {
            sending: tokio::sync::Notify::new(),
            completing: tokio::sync::Notify::new(),
            release_complete: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
            backend.clone(),
        );
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "review implementation".into(),
                role: AgentRole::Reviewer,
                paths: vec![],
                context: None,
            })
            .await
            .unwrap();
        backend.sending.notified().await;
        let code = manager
            .inner
            .entries
            .lock()
            .await
            .get(&info.id)
            .unwrap()
            .completion_code
            .clone();
        let worker = manager.clone();
        let id = info.id.clone();
        let callback =
            tokio::spawn(async move { worker.finished(&id, &code, "callback result").await });
        backend.completing.notified().await;
        assert!(matches!(
            manager.message(&info.id, "next task").await,
            Err(AgentError::Busy)
        ));
        assert!(matches!(
            manager.result(&info.id).await.unwrap().state,
            AgentState::Running
        ));
        backend.release_complete.notify_one();
        assert_eq!(
            callback.await.unwrap().unwrap().state,
            AgentState::Completed
        );
        assert_eq!(
            manager
                .result(&info.id)
                .await
                .unwrap()
                .result
                .unwrap()
                .raw_response,
            "callback result"
        );
        assert_eq!(
            manager.message(&info.id, "next task").await.unwrap().state,
            AgentState::Running
        );
        backend.sending.notified().await;
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
    /// Each delegated worker independently parks on its own response gate.
    /// Releasing them out of spawn order must never swap their page/result ID.
    struct OrderedWorkers {
        entered: tokio::sync::mpsc::UnboundedSender<(String, tokio::sync::oneshot::Sender<()>)>,
        cancelled: Mutex<Vec<String>>,
        closed: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AgentBackend for OrderedWorkers {
        fn name(&self) -> &'static str {
            "ordered-workers"
        }
        fn model(&self) -> &'static str {
            "chatgpt-web-fixture"
        }
        async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
            Ok(())
        }
        async fn send(
            &self,
            id: &str,
            _prompt: &str,
        ) -> Result<(String, Option<String>), BackendFailure> {
            let (release, waiting) = tokio::sync::oneshot::channel();
            self.entered
                .send((id.to_owned(), release))
                .map_err(|_| BackendFailure::Unavailable)?;
            waiting.await.map_err(|_| BackendFailure::Unavailable)?;
            Ok((
                format!("response for {id}"),
                Some(format!("https://chatgpt.com/c/{id}")),
            ))
        }
        async fn cancel(&self, id: &str) {
            self.cancelled.lock().await.push(id.to_owned());
        }
        async fn close(&self, id: &str) {
            self.closed.lock().await.push(id.to_owned());
        }
        async fn diagnostics(&self, id: &str) -> serde_json::Value {
            serde_json::json!({"agent_id":id})
        }
    }

    #[tokio::test]
    async fn four_concurrent_workers_out_of_order_with_isolated_cancellation() {
        use std::collections::{HashMap, HashSet};
        use tokio::sync::mpsc;
        let root = tempfile::tempdir().expect("owned test workspace");
        let sentinel = root.path().join("source.rs");
        std::fs::write(&sentinel, "user-owned source is unchanged").expect("fixture source");
        let (entered, mut received) = mpsc::unbounded_channel();
        let backend = Arc::new(OrderedWorkers {
            entered,
            cancelled: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(root.path(), &[]).expect("workspace"),
            backend.clone(),
        );
        let mut ids = Vec::new();
        for role in [
            AgentRole::Explorer,
            AgentRole::Reviewer,
            AgentRole::Tester,
            AgentRole::Implementer,
        ] {
            let info = manager
                .spawn(SpawnAgent {
                    workspace_id: None,
                    task: format!("independent task for {role:?}"),
                    role,
                    paths: vec![],
                    context: None,
                })
                .await
                .expect("spawn");
            assert_eq!(info.state, AgentState::Running);
            ids.push(info.id);
        }
        let mut gates = HashMap::new();
        for _ in 0..4 {
            let (id, release) = tokio::time::timeout(Duration::from_secs(3), received.recv())
                .await
                .expect("all sessions started")
                .expect("channel open");
            assert!(gates.insert(id, release).is_none());
        }
        assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 4);
        let cancelled_id = ids[1].clone();
        manager
            .cancel(&cancelled_id)
            .await
            .expect("cancel one agent");
        assert_eq!(
            manager
                .result(&cancelled_id)
                .await
                .expect("cancel snapshot")
                .state,
            AgentState::Cancelled
        );
        // Complete the other three in reverse registration order.
        for id in ids.iter().rev().filter(|id| *id != &cancelled_id) {
            gates
                .remove(id)
                .expect("owned response gate")
                .send(())
                .expect("worker still waiting");
            let output = manager.wait(id, 3).await.expect("worker completed");
            assert!(!output.timed_out);
            assert_eq!(output.agent.state, AgentState::Completed);
            assert_eq!(
                output.agent.external_conversation.as_deref(),
                Some(format!("https://chatgpt.com/c/{id}").as_str())
            );
            let result = output.agent.result.expect("untrusted response");
            assert_eq!(result.raw_response, format!("response for {id}"));
            assert!(result.untrusted);
            assert!(!result.applied_to_workspace);
        }
        // A cancelled worker returning late must never overwrite its state.
        gates
            .remove(&cancelled_id)
            .expect("cancelled response gate")
            .send(())
            .expect("cancelled worker remains test parked");
        tokio::task::yield_now().await;
        let cancelled = manager.wait(&cancelled_id, 2).await.expect("cancelled");
        assert_eq!(cancelled.agent.state, AgentState::Cancelled);
        assert!(cancelled.agent.result.is_none());
        assert_eq!(
            backend.cancelled.lock().await.as_slice(),
            std::slice::from_ref(&cancelled_id)
        );
        assert_eq!(backend.closed.lock().await.as_slice(), &[cancelled_id]);
        assert_eq!(
            std::fs::read_to_string(&sentinel).expect("source"),
            "user-owned source is unchanged"
        );
    }
}
