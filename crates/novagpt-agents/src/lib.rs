//! Untrusted, opt-in asynchronous subagents. No implicit ChatGPT chat identity.
use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use novagpt_browser::{BrowserError, ChatGptBrowser};
use novagpt_core::WorkspaceRegistry;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use thiserror::Error;
use tokio::sync::{Mutex, watch};
use tokio::time::{Duration, timeout};
use uuid::Uuid;

const MAX_AGENTS: usize = 6;
const MAX_RECORDS: usize = 64;

/// Compare workspace-relative paths regardless of the platform separator.
/// Patch targets always use `/`, while file-creation receipts use the OS
/// separator on Windows; a plain equality check would deny patching a file
/// this same run had just created.
fn same_relative_path(left: &str, right: &str) -> bool {
    left.replace('\\', "/") == right.replace('\\', "/")
}
/// Completed conversations stay open for follow-up messages, but the Chrome
/// window must not accumulate one tab per agent forever. The oldest completed
/// tab is closed when the cap is reached; a later follow-up then opens a new
/// conversation instead of reusing the evicted tab.
const MAX_LIVE_TABS: usize = 8;
const MAX_ACTIVITY_EVENTS: usize = 32;
const MAX_ACTIVITY_REPORTS: u8 = 64;
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
    #[error("connector_name, when supplied, must contain a valid connected-app display name")]
    ConnectorMissing,
    #[error("Subagent completion is not authorized or is no longer active")]
    CompletionDenied,
    #[error("Subagent activity callback is not authorized or is no longer active")]
    ActivityDenied,
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

/// Fixed, server-observed subagent lifecycle stages; no worker free text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentLogStage {
    Created,
    BrowserCreated,
    BrowserCreateFailed,
    BrowserReplyReceived,
    BrowserReplyFailed,
    BrowserReplyUncertain,
    BrowserReplyBlocked,
    BrowserReplyManual,
    Waiting,
    Completed,
    Failed,
}

impl AgentLogStage {
    pub const fn audit_name(self) -> &'static str {
        match self {
            Self::Created => "agent_created",
            Self::BrowserCreated => "agent_browser_created",
            Self::BrowserCreateFailed => "agent_browser_create_failed",
            Self::BrowserReplyReceived => "agent_browser_reply_received",
            Self::BrowserReplyFailed => "agent_browser_reply_failed",
            Self::BrowserReplyUncertain => "agent_browser_reply_uncertain",
            Self::BrowserReplyBlocked => "agent_browser_reply_blocked",
            Self::BrowserReplyManual => "agent_browser_reply_manual",
            Self::Waiting => "agent_waiting",
            Self::Completed => "agent_completed",
            Self::Failed => "agent_failed",
        }
    }

    pub const fn is_failure(self) -> bool {
        matches!(
            self,
            Self::BrowserCreateFailed
                | Self::BrowserReplyFailed
                | Self::BrowserReplyUncertain
                | Self::BrowserReplyBlocked
                | Self::BrowserReplyManual
                | Self::Failed
        )
    }
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
    /// Parent-selected connector display name, never inferred from the MCP namespace.
    pub connector_name: Option<String>,
    /// unchecked (legacy local API), probing, verified, unavailable, not_required.
    pub capability_status: String,
    pub result: Option<AgentOutput>,
    pub error: Option<String>,
    pub external_conversation: Option<String>,
    pub untrusted: bool,
    pub patches_applied: usize,
    pub patched_paths: Vec<String>,
    pub files_created: usize,
    pub created_paths: Vec<String>,
    /// The backend execution epoch, rotated on follow-up/resume/cancel.
    pub epoch: u64,
    /// Last accepted activity cursor, monotonically increasing across runs.
    pub activity_sequence: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivityEvent {
    pub id: String,
    pub epoch: u64,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub kind: String,
    /// Fixed server-owned text, never free-form worker-supplied content.
    pub description: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentActivity {
    pub id: String,
    pub epoch: u64,
    pub state: AgentState,
    pub cursor: u64,
    /// Last discarded sequence (ring eviction or follow-up reset).
    pub dropped_before: u64,
    pub events: Vec<ActivityEvent>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentActivityWait {
    pub activity: AgentActivity,
    pub timed_out: bool,
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

/// Proof that a specific authenticated callback is callable in this worker
/// conversation. No file operation is attempted by a capability probe.
#[derive(Debug, Clone, Serialize)]
pub struct AgentToolProbe {
    pub id: String,
    pub capability: &'static str,
    pub available: bool,
}

#[derive(Debug, Error)]
pub enum BackendFailure {
    #[error("Manual {0} required")]
    Manual(&'static str),
    #[error("{0}")]
    Blocked(&'static str),
    #[error("Backend unavailable")]
    Unavailable,
    /// The page may still be responding; no replay or implicit cancellation.
    #[error("Browser outcome uncertain after a possible submission")]
    Uncertain(&'static str),
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
        BrowserError::NavigationTimeout => {
            BackendFailure::Blocked("ChatGPT navigation timed out before prompt submission")
        }
        BrowserError::ComposerUnavailable => {
            BackendFailure::Blocked("ChatGPT composer unavailable; no prompt submitted")
        }
        BrowserError::InteractionBlocked => BackendFailure::Blocked(
            "ChatGPT tab could not be made visible and focused; no prompt submitted",
        ),
        BrowserError::SendUnavailable => BackendFailure::Blocked(
            "ChatGPT send control stayed unavailable after previous submission; coding task NOT sent",
        ),
        BrowserError::AmbiguousSubmission => BackendFailure::Uncertain(
            "Chrome disconnected after possible submission; no prompt replay. Authenticated callbacks remain available",
        ),
        BrowserError::Timeout => BackendFailure::Uncertain(
            "Browser response timed out; worker may still be running. No prompt replay. Authenticated callbacks remain available",
        ),
        BrowserError::ProfileBusy => BackendFailure::Blocked("Dedicated Chrome profile is in use"),
        BrowserError::NotRunning => BackendFailure::Blocked("Dedicated Chrome is unavailable"),
        BrowserError::ProjectMismatch => BackendFailure::Blocked(
            "ChatGPT left the designated subagent project; no message was sent outside it",
        ),
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
    activity_reports: u8,
    activity_events: VecDeque<ActivityEvent>,
    activity_dropped_before: u64,
    allowed_paths: Vec<PathBuf>,
    patch_gate: Arc<Mutex<()>>,
    epoch: u64,
    ready: bool,
    waiting_manual: bool,
    /// 0 until the execution completes; higher means completed later.
    completed_seq: u64,
    updates: watch::Sender<AgentInfo>,
}

impl Entry {
    /// Shared, per-execution authentication budget for all worker callbacks.
    fn accept_code(&mut self, code: &str) -> bool {
        if self.completion_failures >= MAX_COMPLETION_ATTEMPTS {
            return false;
        }
        let correct = code.len() == COMPLETION_KEY_LEN
            && self
                .completion_code
                .as_bytes()
                .ct_eq(code.as_bytes())
                .unwrap_u8()
                == 1;
        if !correct {
            self.completion_failures = self.completion_failures.saturating_add(1);
        }
        correct
    }

    fn activity_since(&self, after_sequence: u64) -> AgentActivity {
        AgentActivity {
            id: self.info.id.clone(),
            epoch: self.epoch,
            state: self.info.state,
            cursor: self.info.activity_sequence,
            dropped_before: self.activity_dropped_before,
            events: self
                .activity_events
                .iter()
                .filter(|e| e.sequence > after_sequence)
                .cloned()
                .collect(),
        }
    }
}

type AgentDiagnosticReporter = Arc<dyn Fn(&str, AgentLogStage) + Send + Sync>;

struct Inner {
    backend: Arc<dyn AgentBackend>,
    workspaces: WorkspaceRegistry,
    entries: Mutex<HashMap<String, Entry>>,
    max_agents: usize,
    max_live_tabs: usize,
    /// Monotonic completion order for bounded tab eviction.
    next_completed_seq: AtomicU64,
    reporter: OnceLock<AgentDiagnosticReporter>,
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
                max_live_tabs: MAX_LIVE_TABS,
                next_completed_seq: AtomicU64::new(1),
                reporter: OnceLock::new(),
            }),
        }
    }

    /// Configure one optional fixed-schema diagnostic observer before spawning agents.
    /// No tasks, prompts, browser text, paths or private callback codes are emitted.
    pub fn with_diagnostic_reporter(
        self,
        reporter: impl Fn(&str, AgentLogStage) + Send + Sync + 'static,
    ) -> Self {
        let _ = self.inner.reporter.set(Arc::new(reporter));
        self
    }

    fn emit(&self, id: &str, stage: AgentLogStage) {
        if let Some(report) = self.inner.reporter.get() {
            report(id, stage);
        }
    }

    #[cfg(test)]
    fn with_live_tab_cap(mut self, cap: usize) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.max_live_tabs = cap.max(1);
        }
        self
    }

    /// Trusted in-process interface for embedders and test fixtures.
    /// MCP entrypoints pass optional connector metadata separately.
    pub async fn spawn(&self, request: SpawnAgent) -> AgentResult<AgentInfo> {
        self.spawn_internal(request, None).await
    }

    /// The authenticated parent passes the display name shown by its selected
    /// connected app. MCP cannot discover a ChatGPT-side alias or namespace.
    /// All roles may omit this hint. It never grants write authorization.
    pub async fn spawn_with_connector(
        &self,
        request: SpawnAgent,
        connector_name: Option<&str>,
    ) -> AgentResult<AgentInfo> {
        if connector_name.is_some_and(|name| name.trim().is_empty()) {
            return Err(AgentError::ConnectorMissing);
        }
        self.spawn_internal(request, connector_name).await
    }

    async fn spawn_internal(
        &self,
        request: SpawnAgent,
        connector_name: Option<&str>,
    ) -> AgentResult<AgentInfo> {
        let connector_name = connector_name.map(str::trim);
        if connector_name.is_some_and(|name| {
            name.is_empty()
                || name.len() > 96
                || name
                    .chars()
                    .any(|ch| ch.is_control() || ch == '`' || ch == '{' || ch == '}')
        }) {
            return Err(AgentError::Invalid);
        }
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
        // A progress report is explicit and credential-bound; never infer it
        // from an unrelated MCP call or browser tab/session.
        let prompt = format!(
            "{prompt}\n\nWhile actively working, optionally call subagent_progress with id={id}, code={code}, and kind chosen from working, reading, editing, testing, reviewing, blocked. Do not send free-text details, prompts, URLs or raw tool inputs/results."
        );
        let tool_instructions = connector_name.map(|name| format!(
            "**NovaGPT workspace tools**\nYour coding workspace is managed by the NovaGPT connector named {}.\nYou MUST use this specific connected app/plugin to interact with the workspace whenever its tools are available. Discover the actual tool namespace and exposed capabilities before attempting file operations. Do not derive the namespace from the display name. For existing-file modifications use authenticated subagent_apply_patch; for new files use subagent_create_file; report activity through subagent_progress and completion through subagent_finished when exposed. Use the current private execution credential supplied by NovaGPT. Never print it, include it in reports, or expose it in logs. A successful server receipt is required before claiming a file was created or modified. If required tools are missing, disconnected, disabled, or inaccessible, report that explicitly; do not silently switch plugins or claim modifications.",
            serde_json::to_string(name).unwrap_or_default(),
        )).unwrap_or_else(|| "**NovaGPT workspace tools**\nThe parent cannot supply a ChatGPT connector display name. Discover actual connected NovaGPT tools by their advertised capabilities; do not invent any display name or derive a tool namespace. Report missing tools explicitly and never claim unverified edits.".to_owned());
        // The task is sent as the first browser turn, without a separate preflight.
        let prompt = format!("{tool_instructions}\n\n{prompt}");
        let info = AgentInfo {
            id: id.clone(),
            workspace_id: workspace.workspace_id,
            role: request.role,
            state: AgentState::Running,
            backend: self.inner.backend.name().into(),
            model: self.inner.backend.model().into(),
            connector_name: connector_name.map(str::to_owned),
            capability_status: "unchecked".into(),
            result: None,
            error: None,
            external_conversation: None,
            untrusted: true,
            patches_applied: 0,
            patched_paths: Vec::new(),
            files_created: 0,
            created_paths: Vec::new(),
            epoch: 1,
            activity_sequence: 0,
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
                    activity_reports: 0,
                    activity_events: VecDeque::new(),
                    activity_dropped_before: 0,
                    allowed_paths,
                    patch_gate: Arc::new(Mutex::new(())),
                    epoch: 1,
                    ready: false,
                    waiting_manual: false,
                    completed_seq: 0,
                    updates,
                },
            );
        }
        self.emit(&id, AgentLogStage::Created);
        self.start(id, prompt, true, 1);
        Ok(info)
    }

    fn start(&self, id: String, prompt: String, must_create: bool, epoch: u64) {
        let manager = self.clone();
        tokio::spawn(async move {
            let outcome = async {
                if must_create {
                    manager.evict_completed_tabs().await;
                    let created = manager.inner.backend.create(&id).await;
                    manager.emit(
                        &id,
                        if created.is_ok() {
                            AgentLogStage::BrowserCreated
                        } else {
                            AgentLogStage::BrowserCreateFailed
                        },
                    );
                    created?;
                }
                manager.mark_ready(&id, epoch).await;
                if !manager.is_active(&id, epoch).await {
                    manager.inner.backend.close(&id).await;
                    return Err(BackendFailure::Unavailable);
                }
                let reply = manager.inner.backend.send(&id, &prompt).await;
                let stage = match &reply {
                    Ok(_) => AgentLogStage::BrowserReplyReceived,
                    Err(BackendFailure::Uncertain(_)) => AgentLogStage::BrowserReplyUncertain,
                    Err(BackendFailure::Blocked(_)) => AgentLogStage::BrowserReplyBlocked,
                    Err(BackendFailure::Manual(_)) => AgentLogStage::BrowserReplyManual,
                    Err(BackendFailure::Unavailable) => AgentLogStage::BrowserReplyFailed,
                };
                manager.emit(&id, stage);
                reply
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
                            entry.completed_seq = manager
                                .inner
                                .next_completed_seq
                                .fetch_add(1, Ordering::AcqRel);
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
                        entry.waiting_manual = true;
                        entry.info.error =
                            Some(format!("Manual {reason} required in visible browser"));
                    }
                    Err(BackendFailure::Uncertain(reason)) => {
                        entry.info.state = AgentState::Waiting;
                        entry.waiting_manual = false;
                        entry.info.error = Some(reason.to_owned());
                    }
                    Err(BackendFailure::Unavailable) => {
                        entry.info.state = AgentState::Failed;
                        entry.info.error = Some("Subagent backend failed or is unavailable".into());
                    }
                    Err(BackendFailure::Blocked(reason)) => {
                        entry.info.state = AgentState::Failed;
                        entry.info.error = Some(reason.to_owned());
                    }
                }
                entry.updates.send_replace(entry.info.clone());
            }
            let failed_tab = agents
                .get(&id)
                .is_some_and(|entry| entry.ready && entry.info.state == AgentState::Failed);
            if failed_tab && let Some(entry) = agents.get_mut(&id) {
                // A failed execution has no usable continuation; close its tab
                // instead of leaving it open for the life of the process.
                entry.ready = false;
            }
            let terminal = agents.get(&id).map(|entry| entry.info.state);
            drop(agents);
            match terminal {
                Some(AgentState::Completed) => manager.emit(&id, AgentLogStage::Completed),
                Some(AgentState::Failed) => manager.emit(&id, AgentLogStage::Failed),
                Some(AgentState::Waiting) => manager.emit(&id, AgentLogStage::Waiting),
                _ => {}
            }
            if failed_tab {
                manager.inner.backend.close(&id).await;
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

    /// Close the oldest completed worker tab when the live-tab cap is reached.
    /// Running and waiting agents are never evicted.
    async fn evict_completed_tabs(&self) {
        let mut entries = self.inner.entries.lock().await;
        let live = entries.values().filter(|entry| entry.ready).count();
        if live < self.inner.max_live_tabs {
            return;
        }
        let candidate = entries
            .values()
            .filter(|entry| {
                entry.ready && entry.info.state == AgentState::Completed && entry.completed_seq > 0
            })
            .min_by_key(|entry| entry.completed_seq)
            .map(|entry| entry.info.id.clone());
        let Some(id) = candidate else {
            return;
        };
        if let Some(entry) = entries.get_mut(&id) {
            // `ready = false` makes a later follow-up create a new
            // tab/conversation instead of targeting a closed tab.
            entry.ready = false;
        }
        drop(entries);
        self.inner.backend.close(&id).await;
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
        if entry.info.state == AgentState::Cancelled {
            return Err(AgentError::Cancelled);
        }
        if resume && !entry.waiting_manual {
            return Err(AgentError::NotWaiting);
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
            entry.activity_reports = 0;
            entry.activity_dropped_before = entry.info.activity_sequence;
            entry.activity_events.clear();
            let mut next = format!(
                "{}\n\nWHEN FINISHED: Call the NovaGPT MCP tool subagent_finished with id={id}, code={code}, and result=<your complete final report>. Do this once. Keep the six-character code private; do not include it in messages or logs.",
                new_prompt.unwrap_or_default()
            );
            next.push_str("\n\nWhile actively working, use subagent_progress with this id/code and one of: working, reading, editing, testing, reviewing, blocked. Do not include free-text details.");
            if entry.info.role == AgentRole::Implementer {
                next.push_str("\n\nFor existing files call subagent_apply_patch with this id/code, one selected file per patch. For NEW files anywhere within the authorized workspace call subagent_create_file with id/code/path/content; paths need not be selected. Check the receipt before saying APPLIED; otherwise label changes NOT APPLIED.");
            }
            next
        };
        if prompt.is_empty() {
            return Err(AgentError::Invalid);
        }
        entry.epoch += 1;
        entry.info.epoch = entry.epoch;
        let epoch = entry.epoch;
        entry.completed_seq = 0;
        entry.info.state = AgentState::Running;
        entry.waiting_manual = false;
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
            entry.info.epoch = entry.epoch;
            entry.info.state = AgentState::Cancelled;
            entry.ready = false;
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
            if !entry.accept_code(code) {
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
        entry.completed_seq = self.inner.next_completed_seq.fetch_add(1, Ordering::AcqRel);
        entry.updates.send_replace(entry.info.clone());
        let ack = AgentCompletionAck {
            id: entry.info.id.clone(),
            state: AgentState::Completed,
        };
        drop(agents);
        self.emit(&ack.id, AgentLogStage::Completed);
        Ok(ack)
    }

    /// Report one safe, authenticated event for a ready, nonterminal execution.
    /// The worker cannot supply arbitrary descriptions or other metadata.
    pub async fn progress(&self, id: &str, code: &str, kind: &str) -> AgentResult<ActivityEvent> {
        let mut agents = self.inner.entries.lock().await;
        let entry = agents.get_mut(id).ok_or(AgentError::ActivityDenied)?;
        if !entry.ready
            || !matches!(entry.info.state, AgentState::Running | AgentState::Waiting)
            || entry.completion_claimed
            || entry.activity_reports >= MAX_ACTIVITY_REPORTS
            || !entry.accept_code(code)
        {
            return Err(AgentError::ActivityDenied);
        }
        let description = match kind {
            "working" => "Working on the assigned task",
            "reading" => "Reviewing relevant source material",
            "editing" => "Preparing changes",
            "testing" => "Checking behavior",
            "reviewing" => "Reviewing findings",
            "blocked" => "Worker reported a blocker",
            _ => return Err(AgentError::Invalid),
        };
        let sequence = entry
            .info
            .activity_sequence
            .checked_add(1)
            .ok_or(AgentError::ActivityDenied)?;
        let timestamp_ms = u64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        let event = ActivityEvent {
            id: id.to_owned(),
            epoch: entry.epoch,
            sequence,
            timestamp_ms,
            kind: kind.to_owned(),
            description,
        };
        entry.info.activity_sequence = sequence;
        entry.activity_reports += 1;
        entry.activity_events.push_back(event.clone());
        if entry.activity_events.len() > MAX_ACTIVITY_EVENTS
            && let Some(evicted) = entry.activity_events.pop_front()
        {
            entry.activity_dropped_before = evicted.sequence;
        }
        // Lifecycle watchers will ignore running-to-running progress events.
        entry.updates.send_replace(entry.info.clone());
        Ok(event)
    }

    /// Read-only activity snapshot. No credential is disclosed.
    pub async fn activity(&self, id: &str) -> AgentResult<AgentActivity> {
        self.activity_since(id, 0).await
    }

    pub async fn activity_since(
        &self,
        id: &str,
        after_sequence: u64,
    ) -> AgentResult<AgentActivity> {
        let entries = self.inner.entries.lock().await;
        Ok(entries
            .get(id)
            .ok_or(AgentError::NotFound)?
            .activity_since(after_sequence))
    }

    /// Cursor-based bounded wait for new progress, epoch change or terminal state.
    /// This does not alter the existing subagent_wait lifecycle contract.
    pub async fn wait_activity(
        &self,
        id: &str,
        after_sequence: u64,
        known_epoch: Option<u64>,
        max_wait_seconds: u64,
    ) -> AgentResult<AgentActivityWait> {
        if !(1..=MAX_WAIT_SECONDS).contains(&max_wait_seconds) {
            return Err(AgentError::Invalid);
        }
        let mut updates = {
            let entries = self.inner.entries.lock().await;
            entries
                .get(id)
                .ok_or(AgentError::NotFound)?
                .updates
                .subscribe()
        };
        let changed = |info: &AgentInfo| {
            info.activity_sequence > after_sequence
                || known_epoch.is_some_and(|epoch| info.epoch != epoch)
                || matches!(
                    info.state,
                    AgentState::Completed | AgentState::Failed | AgentState::Cancelled
                )
        };
        let initial = updates.borrow_and_update().clone();
        let timed_out = if changed(&initial) {
            false
        } else {
            match timeout(Duration::from_secs(max_wait_seconds), async {
                loop {
                    updates.changed().await.map_err(|_| AgentError::Backend)?;
                    if changed(&updates.borrow_and_update()) {
                        return Ok::<(), AgentError>(());
                    }
                }
            })
            .await
            {
                Ok(Ok(())) => false,
                Ok(Err(error)) => return Err(error),
                Err(_) => true,
            }
        };
        let activity = self.activity_since(id, after_sequence).await?;
        Ok(AgentActivityWait {
            timed_out: timed_out
                && matches!(activity.state, AgentState::Running | AgentState::Waiting)
                && activity.cursor <= after_sequence
                && !known_epoch.is_some_and(|epoch| activity.epoch != epoch),
            activity,
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

    /// Confirm a live callback endpoint without changing repository state.
    /// The optional no-write probe checks the current execution credential.
    /// It never requires a separate browser conversation turn.
    pub async fn probe_tool(
        &self,
        id: &str,
        code: &str,
        capability: &str,
    ) -> AgentResult<AgentToolProbe> {
        let mut entries = self.inner.entries.lock().await;
        let entry = entries.get_mut(id).ok_or(AgentError::PatchDenied)?;
        if !entry.ready
            || entry.info.role != AgentRole::Implementer
            || entry.info.state != AgentState::Running
            || entry.completion_claimed
            || !entry.accept_code(code)
        {
            return Err(AgentError::PatchDenied);
        }
        let capability = match capability {
            "subagent_apply_patch" => "subagent_apply_patch",
            "subagent_create_file" => "subagent_create_file",
            _ => return Err(AgentError::Invalid),
        };
        Ok(AgentToolProbe {
            id: id.to_owned(),
            capability,
            available: true,
        })
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
                if !entry.accept_code(&code) {
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
                    && !entry
                        .info
                        .created_paths
                        .iter()
                        .any(|path| same_relative_path(path, &relative))
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
                if !entry.accept_code(&code) {
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

    #[test]
    fn browser_failures_have_specific_safe_parent_messages() {
        assert_eq!(
            map_browser(BrowserError::NavigationTimeout).to_string(),
            "ChatGPT navigation timed out before prompt submission"
        );
        assert_eq!(
            map_browser(BrowserError::ComposerUnavailable).to_string(),
            "ChatGPT composer unavailable; no prompt submitted"
        );
        assert_eq!(
            map_browser(BrowserError::AmbiguousSubmission).to_string(),
            "Browser outcome uncertain after a possible submission"
        );
        assert_eq!(
            map_browser(BrowserError::Timeout).to_string(),
            "Browser outcome uncertain after a possible submission"
        );
        assert!(matches!(
            map_browser(BrowserError::Manual("verification")),
            BackendFailure::Manual("verification")
        ));
    }

    #[tokio::test]
    async fn invalid_connector_hints_are_denied_before_tab_creation() {
        let dir = tempfile::tempdir().expect("fixture");
        let backend = Arc::new(SingleTurnBackend::default());
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
            backend.clone(),
        );
        let request = SpawnAgent {
            workspace_id: None,
            task: "do not dispatch".into(),
            role: AgentRole::Implementer,
            paths: vec![],
            context: None,
        };
        assert!(matches!(
            manager
                .spawn_with_connector(request.clone(), Some("  "))
                .await,
            Err(AgentError::ConnectorMissing)
        ));
        assert!(matches!(
            manager
                .spawn_with_connector(request, Some("malicious\nconnector"))
                .await,
            Err(AgentError::Invalid)
        ));
        assert!(backend.prompts.lock().await.is_empty());
    }

    #[derive(Default)]
    struct SingleTurnBackend {
        prompts: Mutex<HashMap<String, Vec<String>>>,
        permits: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
    }

    #[async_trait]
    impl AgentBackend for SingleTurnBackend {
        fn name(&self) -> &'static str {
            "single-turn-fixture"
        }
        fn model(&self) -> &'static str {
            "fixture"
        }
        async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
            Ok(())
        }
        async fn send(
            &self,
            id: &str,
            prompt: &str,
        ) -> Result<(String, Option<String>), BackendFailure> {
            let permit = Arc::new(tokio::sync::Notify::new());
            self.permits
                .lock()
                .await
                .insert(id.to_owned(), permit.clone());
            self.prompts
                .lock()
                .await
                .entry(id.to_owned())
                .or_default()
                .push(prompt.to_owned());
            permit.notified().await;
            Ok(("fixture response".into(), None))
        }
        async fn cancel(&self, id: &str) {
            self.complete(id).await;
        }
        async fn complete(&self, id: &str) {
            if let Some(permit) = self.permits.lock().await.get(id) {
                permit.notify_one();
            }
        }
        async fn diagnostics(&self, _id: &str) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    #[tokio::test]
    async fn three_implementers_receive_one_distinct_assignment_per_tab() {
        let dir = tempfile::tempdir().expect("fixture");
        let backend = Arc::new(SingleTurnBackend::default());
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
            backend.clone(),
        );
        let mut workers = Vec::new();
        for index in 0..3 {
            let task = format!("UNIQUE ASSIGNMENT {index}");
            let info = manager
                .spawn_with_connector(
                    SpawnAgent {
                        workspace_id: None,
                        role: AgentRole::Implementer,
                        task: task.clone(),
                        paths: vec![],
                        context: None,
                    },
                    Some("NovaGPT Laptop"),
                )
                .await
                .expect("spawn");
            let code = manager.inner.entries.lock().await[&info.id]
                .completion_code
                .clone();
            workers.push((info.id, code, task));
        }
        timeout(Duration::from_secs(3), async {
            loop {
                if backend.prompts.lock().await.len() == 3 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("all workers received an initial prompt");
        let prompts = backend.prompts.lock().await;
        for (id, _, task) in &workers {
            let own = &prompts[id];
            assert_eq!(own.len(), 1, "one initial send per worker");
            assert!(own[0].contains(task), "worker received its own task");
            assert!(!own[0].contains("CAPABILITY PREFLIGHT"));
            for (_, _, other) in workers.iter().filter(|(_, _, other)| other != task) {
                assert!(!own[0].contains(other), "tasks must not cross tabs");
            }
        }
        drop(prompts);
        for (id, code, _) in &workers {
            let status = manager.status(Some(id)).await.expect("status");
            assert_eq!(status.agents[0].capability_status, "unchecked");
            assert_eq!(status.agents[0].state, AgentState::Running);
            manager
                .finished(id, code, "completed")
                .await
                .expect("finish");
        }
        manager
            .message(&workers[1].0, "FOLLOW-UP FOR WORKER ONE")
            .await
            .expect("follow-up");
        timeout(Duration::from_secs(3), async {
            loop {
                if backend
                    .prompts
                    .lock()
                    .await
                    .get(&workers[1].0)
                    .is_some_and(|p| p.len() == 2)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("follow-up reached the same worker");
        let prompts = backend.prompts.lock().await;
        assert!(prompts[&workers[1].0][1].contains("FOLLOW-UP FOR WORKER ONE"));
        assert_eq!(prompts[&workers[0].0].len(), 1);
        assert_eq!(prompts[&workers[2].0].len(), 1);
    }

    #[tokio::test]
    async fn single_turn_implementer_retains_authenticated_write_guards() {
        let dir = tempfile::tempdir().expect("fixture");
        std::fs::write(dir.path().join("approved.rs"), "old\n").expect("existing");
        std::fs::write(dir.path().join("other.rs"), "untouched\n").expect("existing");
        let backend = Arc::new(SingleTurnBackend::default());
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
            backend.clone(),
        );
        let info = manager
            .spawn_with_connector(
                SpawnAgent {
                    workspace_id: None,
                    role: AgentRole::Implementer,
                    task: "Edit the approved file".into(),
                    paths: vec!["approved.rs".into()],
                    context: None,
                },
                None,
            )
            .await
            .expect("spawn");
        timeout(Duration::from_secs(3), async {
            loop {
                if backend.prompts.lock().await.contains_key(&info.id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("prompt sent");
        let code = manager.inner.entries.lock().await[&info.id]
            .completion_code
            .clone();
        assert!(matches!(
            manager
                .create_file(&info.id, "WRONG!", "bad.txt", "bad")
                .await,
            Err(AgentError::PatchDenied)
        ));
        assert!(matches!(
            manager
                .probe_tool(&info.id, "WRONG!", "subagent_apply_patch")
                .await,
            Err(AgentError::PatchDenied)
        ));
        assert!(
            manager
                .probe_tool(&info.id, &code, "subagent_apply_patch")
                .await
                .expect("optional no-write probe")
                .available
        );
        manager
            .create_file(&info.id, &code, "new/created.txt", "created\n")
            .await
            .expect("create receipt");
        manager
            .apply_patch(
                &info.id,
                &code,
                "--- a/approved.rs\n+++ b/approved.rs\n@@ -1 +1 @@\n-old\n+updated\n",
            )
            .await
            .expect("patch receipt");
        assert!(matches!(
            manager
                .apply_patch(
                    &info.id,
                    &code,
                    "--- a/other.rs\n+++ b/other.rs\n@@ -1 +1 @@\n-untouched\n+changed\n",
                )
                .await,
            Err(AgentError::PatchDenied)
        ));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("approved.rs")).expect("read"),
            "updated\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("other.rs")).expect("read"),
            "untouched\n"
        );
        manager
            .finished(&info.id, &code, "done")
            .await
            .expect("finished");
        assert!(matches!(
            manager
                .create_file(&info.id, &code, "late.txt", "denied")
                .await,
            Err(AgentError::PatchDenied)
        ));
    }

    struct TimeoutAfterSubmission;
    #[async_trait]
    impl AgentBackend for TimeoutAfterSubmission {
        fn name(&self) -> &'static str {
            "timeout-fixture"
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
            Err(BackendFailure::Uncertain(
                "Reply timeout after possible submission",
            ))
        }
        async fn cancel(&self, _id: &str) {}
        async fn diagnostics(&self, _id: &str) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    #[tokio::test]
    async fn ambiguous_browser_timeout_keeps_execution_credential_valid() {
        let dir = tempfile::tempdir().expect("fixture");
        let reported = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = reported.clone();
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
            Arc::new(TimeoutAfterSubmission),
        )
        .with_diagnostic_reporter(move |_, stage| {
            captured.lock().expect("diagnostic lock").push(stage);
        });
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "already sent".into(),
                role: AgentRole::Reviewer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        let timed_out = manager.wait(&info.id, 2).await.expect("wait");
        assert_eq!(timed_out.agent.state, AgentState::Waiting);
        assert!(
            reported
                .lock()
                .expect("diagnostics")
                .contains(&AgentLogStage::BrowserReplyUncertain)
        );
        assert!(
            timed_out
                .agent
                .error
                .as_deref()
                .unwrap_or("")
                .contains("timeout")
        );
        assert!(
            matches!(
                manager.resume(&info.id, true).await,
                Err(AgentError::NotWaiting)
            ),
            "no accidental replay"
        );
        let code = manager.inner.entries.lock().await[&info.id]
            .completion_code
            .clone();
        assert!(manager.progress(&info.id, &code, "working").await.is_ok());
        manager
            .finished(&info.id, &code, "late completion")
            .await
            .expect("callback still active");
        assert_eq!(
            manager.result(&info.id).await.expect("result").state,
            AgentState::Completed
        );
    }

    struct BlockedBackend;
    #[async_trait]
    impl AgentBackend for BlockedBackend {
        fn name(&self) -> &'static str {
            "blocked-fixture"
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
            Err(BackendFailure::Blocked(
                "ChatGPT composer unavailable; no prompt submitted",
            ))
        }
        async fn cancel(&self, _id: &str) {}
        async fn diagnostics(&self, _id: &str) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    #[tokio::test]
    async fn diagnostic_observer_reports_minted_id_and_browser_failure_stage() {
        let dir = tempfile::tempdir().expect("workspace");
        let reported = Arc::new(std::sync::Mutex::new(Vec::<(String, AgentLogStage)>::new()));
        let recorded = reported.clone();
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
            Arc::new(BlockedBackend),
        )
        .with_diagnostic_reporter(move |id, stage| {
            recorded
                .lock()
                .expect("report mutex")
                .push((id.to_owned(), stage));
        });
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                role: AgentRole::Reviewer,
                task: "check source".into(),
                context: Some("private fixture not logged".into()),
                paths: vec![],
            })
            .await
            .expect("spawn");
        let completed = manager.wait(&info.id, 2).await.expect("wait");
        assert_eq!(completed.agent.state, AgentState::Failed);
        let events = reported.lock().expect("diagnostics").clone();
        assert!(events.iter().all(|(id, _)| id == &info.id));
        assert!(
            events
                .iter()
                .any(|(_, stage)| *stage == AgentLogStage::Created)
        );
        assert!(
            events
                .iter()
                .any(|(_, stage)| *stage == AgentLogStage::BrowserCreated)
        );
        assert!(
            events
                .iter()
                .any(|(_, stage)| *stage == AgentLogStage::BrowserReplyBlocked)
        );
        assert!(
            !events
                .iter()
                .any(|(_, stage)| stage.audit_name().contains("private"))
        );
    }

    #[tokio::test]
    async fn blocked_browser_reason_reaches_parent_without_manual_wait() {
        let dir = tempfile::tempdir().expect("fixture");
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            Arc::new(BlockedBackend),
        );
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "review".into(),
                role: AgentRole::Reviewer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        let result = manager.wait(&info.id, 2).await.expect("completed wait");
        assert!(!result.timed_out);
        assert_eq!(result.agent.state, AgentState::Failed);
        assert_eq!(
            result.agent.error.as_deref(),
            Some("ChatGPT composer unavailable; no prompt submitted")
        );
    }

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
    async fn unnamed_read_only_roles_can_spawn_from_legacy_mcp_clients() {
        let dir = tempfile::tempdir().expect("fixture");
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
            Arc::new(FakeBackend),
        );
        for role in [AgentRole::Explorer, AgentRole::Reviewer, AgentRole::Tester] {
            let info = manager
                .spawn_with_connector(
                    SpawnAgent {
                        workspace_id: None,
                        task: "inspect without changing files".into(),
                        role,
                        paths: vec![],
                        context: None,
                    },
                    None,
                )
                .await
                .expect("read-only spawn without client-owned alias");
            assert_eq!(info.connector_name, None);
            assert_eq!(info.capability_status, "unchecked");
            let finished = manager.wait(&info.id, 2).await.expect("worker result");
            assert!(!finished.timed_out);
            assert_eq!(finished.agent.state, AgentState::Completed);
            assert!(!finished.agent.result.expect("result").applied_to_workspace);
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
    async fn waiting_worker_can_report_activity_without_changing_wait_semantics() {
        let dir = tempfile::tempdir().expect("fixture");
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            Arc::new(ManualBackend),
        );
        let worker = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "inspect".into(),
                role: AgentRole::Explorer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        let lifecycle = manager.wait(&worker.id, 2).await.expect("wait");
        assert_eq!(lifecycle.agent.state, AgentState::Waiting);
        let code = manager
            .inner
            .entries
            .lock()
            .await
            .get(&worker.id)
            .expect("entry")
            .completion_code
            .clone();
        let wait_manager = manager.clone();
        let id = worker.id.clone();
        let waiter = tokio::spawn(async move {
            wait_manager
                .wait_activity(&id, 0, Some(1), 5)
                .await
                .expect("activity wait")
        });
        tokio::task::yield_now().await;
        let event = manager
            .progress(&worker.id, &code, "working")
            .await
            .expect("waiting worker progress");
        let returned = tokio::time::timeout(Duration::from_secs(2), waiter)
            .await
            .expect("wake")
            .expect("join");
        assert!(!returned.timed_out);
        assert_eq!(returned.activity.events[0].sequence, event.sequence);
        assert_eq!(returned.activity.state, AgentState::Waiting);
        manager.cancel(&worker.id).await.expect("cancel");
    }

    #[tokio::test]
    async fn progress_is_authenticated_and_shares_the_completion_rate_limit() {
        let dir = tempfile::tempdir().expect("fixture");
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            backend.clone(),
        );
        let worker = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "inspect".into(),
                role: AgentRole::Reviewer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        backend.entered.notified().await;
        let code = manager
            .inner
            .entries
            .lock()
            .await
            .get(&worker.id)
            .expect("entry")
            .completion_code
            .clone();
        let wrong = if code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
        for _ in 0..MAX_COMPLETION_ATTEMPTS {
            assert!(matches!(
                manager.progress(&worker.id, wrong, "working").await,
                Err(AgentError::ActivityDenied)
            ));
        }
        assert!(matches!(
            manager.finished(&worker.id, &code, "too late").await,
            Err(AgentError::CompletionDenied)
        ));
        assert!(matches!(
            manager.progress(&worker.id, &code, "working").await,
            Err(AgentError::ActivityDenied)
        ));
        assert!(
            manager
                .activity(&worker.id)
                .await
                .unwrap()
                .events
                .is_empty()
        );
        manager.cancel(&worker.id).await.expect("cancel");
        backend.release.notify_one();
    }

    #[tokio::test]
    async fn progress_buffer_is_bounded_and_followup_rotates_epoch_and_code() {
        let dir = tempfile::tempdir().expect("fixture");
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            backend.clone(),
        );
        let worker = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "inspect".into(),
                role: AgentRole::Tester,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        backend.entered.notified().await;
        let old = manager
            .inner
            .entries
            .lock()
            .await
            .get(&worker.id)
            .expect("entry")
            .completion_code
            .clone();
        assert!(matches!(
            manager
                .progress(&worker.id, &old, "https://private.invalid")
                .await,
            Err(AgentError::Invalid)
        ));
        for _ in 0..MAX_ACTIVITY_REPORTS {
            manager
                .progress(&worker.id, &old, "working")
                .await
                .expect("progress");
        }
        assert!(matches!(
            manager.progress(&worker.id, &old, "working").await,
            Err(AgentError::ActivityDenied)
        ));
        let feed = manager.activity(&worker.id).await.expect("feed");
        assert_eq!(feed.events.len(), MAX_ACTIVITY_EVENTS);
        assert_eq!(feed.cursor, u64::from(MAX_ACTIVITY_REPORTS));
        assert_eq!(feed.dropped_before, 32);
        assert!(
            feed.events
                .iter()
                .all(|e| e.timestamp_ms > 0 && e.description == "Working on the assigned task")
        );
        manager
            .finished(&worker.id, &old, "complete")
            .await
            .expect("finish");
        assert!(matches!(
            manager.progress(&worker.id, &old, "working").await,
            Err(AgentError::ActivityDenied)
        ));
        manager
            .message(&worker.id, "next task")
            .await
            .expect("followup");
        let rotated = manager
            .wait_activity(&worker.id, feed.cursor, Some(feed.epoch), 1)
            .await
            .expect("epoch change");
        assert!(!rotated.timed_out);
        assert_eq!(rotated.activity.epoch, feed.epoch + 1);
        backend.entered.notified().await;
        let new = manager
            .inner
            .entries
            .lock()
            .await
            .get(&worker.id)
            .expect("entry")
            .completion_code
            .clone();
        assert_ne!(old, new);
        let reset = manager.activity(&worker.id).await.expect("reset");
        assert_eq!(reset.cursor, feed.cursor);
        assert_eq!(reset.epoch, feed.epoch + 1);
        assert!(reset.events.is_empty());
        assert!(matches!(
            manager.progress(&worker.id, &old, "working").await,
            Err(AgentError::ActivityDenied)
        ));
        let event = manager
            .progress(&worker.id, &new, "testing")
            .await
            .expect("new run");
        assert_eq!(event.sequence, feed.cursor + 1);
        manager.cancel(&worker.id).await.expect("cancel");
        assert!(matches!(
            manager.progress(&worker.id, &new, "testing").await,
            Err(AgentError::ActivityDenied)
        ));
        backend.release.notify_one();
    }

    #[tokio::test]
    async fn activity_wait_wakes_one_worker_only_and_then_on_cancellation() {
        let dir = tempfile::tempdir().expect("fixture");
        let backend = Arc::new(HeldSendBackend {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
            backend.clone(),
        );
        let mut ids = Vec::new();
        let mut codes = Vec::new();
        for _ in 0..2 {
            let worker = manager
                .spawn(SpawnAgent {
                    workspace_id: None,
                    task: "inspect".into(),
                    role: AgentRole::Explorer,
                    paths: vec![],
                    context: None,
                })
                .await
                .expect("spawn");
            backend.entered.notified().await;
            codes.push(
                manager
                    .inner
                    .entries
                    .lock()
                    .await
                    .get(&worker.id)
                    .expect("entry")
                    .completion_code
                    .clone(),
            );
            ids.push(worker.id);
        }
        let first_manager = manager.clone();
        let first_id = ids[0].clone();
        let first = tokio::spawn(async move {
            first_manager
                .wait_activity(&first_id, 0, Some(1), 5)
                .await
                .expect("first wait")
        });
        let second_manager = manager.clone();
        let second_id = ids[1].clone();
        let second = tokio::spawn(async move {
            second_manager
                .wait_activity(&second_id, 0, Some(1), 5)
                .await
                .expect("second wait")
        });
        tokio::task::yield_now().await;
        manager
            .progress(&ids[0], &codes[0], "reading")
            .await
            .expect("first report");
        let updated = tokio::time::timeout(Duration::from_secs(2), first)
            .await
            .expect("first wake")
            .expect("join");
        assert!(!updated.timed_out);
        assert_eq!(updated.activity.events.len(), 1);
        assert!(!second.is_finished(), "other worker must not wake");
        manager.cancel(&ids[1]).await.expect("cancel second");
        let cancelled = tokio::time::timeout(Duration::from_secs(2), second)
            .await
            .expect("cancel wake")
            .expect("join");
        assert_eq!(cancelled.activity.state, AgentState::Cancelled);
        assert!(!cancelled.timed_out);
        assert!(matches!(
            manager.wait_activity(&ids[0], 1, Some(1), 0).await,
            Err(AgentError::Invalid)
        ));
        manager.cancel(&ids[0]).await.expect("cancel first");
        backend.release.notify_one();
        backend.release.notify_one();
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
        assert_eq!(created.path.replace('\\', "/"), "src/deep/generated.rs");
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
        assert_eq!(
            status.agents[0]
                .created_paths
                .iter()
                .map(|path| path.replace('\\', "/"))
                .collect::<Vec<_>>(),
            ["src/deep/generated.rs"]
        );
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

    struct TabLifecycle {
        closed: Mutex<Vec<String>>,
        fail_send: bool,
    }

    #[async_trait]
    impl AgentBackend for TabLifecycle {
        fn name(&self) -> &'static str {
            "tab-fixture"
        }
        fn model(&self) -> &'static str {
            "fixture"
        }
        async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
            Ok(())
        }
        async fn send(
            &self,
            id: &str,
            _prompt: &str,
        ) -> Result<(String, Option<String>), BackendFailure> {
            if self.fail_send {
                Err(BackendFailure::Unavailable)
            } else {
                Ok((format!("response for {id}"), None))
            }
        }
        async fn cancel(&self, _id: &str) {}
        async fn close(&self, id: &str) {
            self.closed.lock().await.push(id.to_owned());
        }
        async fn diagnostics(&self, _id: &str) -> serde_json::Value {
            serde_json::json!({})
        }
    }

    fn tab_task(index: usize) -> SpawnAgent {
        SpawnAgent {
            workspace_id: None,
            task: format!("tab lifecycle task {index}"),
            role: AgentRole::Reviewer,
            paths: vec![],
            context: None,
        }
    }

    #[tokio::test]
    async fn completed_tabs_are_evicted_at_the_live_cap_and_failed_tabs_close() {
        let root = tempfile::tempdir().expect("owned test workspace");
        let backend = Arc::new(TabLifecycle {
            closed: Mutex::new(Vec::new()),
            fail_send: false,
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(root.path(), &[]).expect("workspace"),
            backend.clone(),
        )
        .with_live_tab_cap(2);
        let mut ids = Vec::new();
        for index in 0..3 {
            let info = manager.spawn(tab_task(index)).await.expect("spawn");
            let wait = manager.wait(&info.id, 3).await.expect("wait");
            assert_eq!(wait.agent.state, AgentState::Completed);
            ids.push(info.id);
            if index < 2 {
                assert!(
                    backend.closed.lock().await.is_empty(),
                    "tabs stay open below the cap"
                );
            }
        }
        assert_eq!(
            backend.closed.lock().await.as_slice(),
            std::slice::from_ref(&ids[0]),
            "the oldest completed tab is evicted at the cap"
        );
        // A follow-up to an evicted conversation must create a new tab rather
        // than targeting the closed one; the agent still completes.
        let reopened = manager
            .message(&ids[0], "follow up")
            .await
            .expect("message");
        assert_eq!(reopened.state, AgentState::Running);
        let wait = manager.wait(&ids[0], 3).await.expect("follow-up");
        assert_eq!(wait.agent.state, AgentState::Completed);

        let failing = Arc::new(TabLifecycle {
            closed: Mutex::new(Vec::new()),
            fail_send: true,
        });
        let manager = AgentManager::new(
            WorkspaceRegistry::new(root.path(), &[]).expect("workspace"),
            failing.clone(),
        );
        let info = manager.spawn(tab_task(9)).await.expect("spawn");
        let wait = manager.wait(&info.id, 3).await.expect("wait");
        assert_eq!(wait.agent.state, AgentState::Failed);
        assert_eq!(
            failing.closed.lock().await.as_slice(),
            std::slice::from_ref(&info.id),
            "a failed execution closes its tab"
        );
    }
}
