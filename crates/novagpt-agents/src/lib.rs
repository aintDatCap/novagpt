//! Untrusted, opt-in asynchronous subagents. No implicit ChatGPT chat identity.
mod activity;
mod backend;
mod contract;
mod mutations;

pub use backend::{AgentBackend, BackendFailure, ChatGptBackend};
pub use contract::{
    ActivityEvent, AgentActivity, AgentActivityWait, AgentCompletionAck, AgentCreateReceipt,
    AgentError, AgentInfo, AgentLogStage, AgentOutput, AgentPatchReceipt, AgentResult, AgentRole,
    AgentState, AgentStatus, AgentToolProbe, AgentVmLease, AgentVmState, AgentWait, SpawnAgent,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use novagpt_core::WorkspaceRegistry;
use novagpt_vm::{VmCreate, VmManager};
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
use tokio::sync::{Mutex, watch};
use uuid::Uuid;

const MAX_AGENTS: usize = 6;
const MAX_RECORDS: usize = 64;

/// Completed conversations stay open for follow-up messages, but the Chrome
/// window must not accumulate one tab per agent forever. The oldest completed
/// tab is closed when the cap is reached; a later follow-up then opens a new
/// conversation instead of reusing the evicted tab.
const MAX_LIVE_TABS: usize = 8;
const MAX_ACTIVITY_EVENTS: usize = 32;
const MAX_ACTIVITY_REPORTS: u16 = 1024;
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

#[derive(Clone)]
struct Entry {
    info: AgentInfo,
    pending_prompt: String,
    completion_code: String,
    completion_failures: u8,
    completion_claimed: bool,
    patch_attempts: u8,
    create_attempts: u16,
    activity_reports: u16,
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

    /// Only credential-authenticated worker calls can be attributed to this
    /// agent. An MCP transport session is not a conversation identity.
    fn record_activity(
        &mut self,
        kind: &str,
        description: &'static str,
    ) -> AgentResult<ActivityEvent> {
        let sequence = self
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
            id: self.info.id.clone(),
            epoch: self.epoch,
            sequence,
            timestamp_ms,
            kind: kind.to_owned(),
            description,
        };
        self.info.activity_sequence = sequence;
        self.activity_events.push_back(event.clone());
        if self.activity_events.len() > MAX_ACTIVITY_EVENTS
            && let Some(evicted) = self.activity_events.pop_front()
        {
            self.activity_dropped_before = evicted.sequence;
        }
        self.updates.send_replace(self.info.clone());
        Ok(event)
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
/// Receives (agent id, conversation URL). Must not block.
type ConversationRecorder = Arc<dyn Fn(&str, &str) + Send + Sync>;
/// Opaque agent/repository identifiers, request/reply character counts and failure only.
type AgentUsageReporter = Arc<dyn Fn(&str, &str, usize, Option<usize>, bool) + Send + Sync>;

struct Inner {
    backend: Arc<dyn AgentBackend>,
    workspaces: WorkspaceRegistry,
    vm: OnceLock<VmManager>,
    spawn_gate: Mutex<()>,
    tab_gate: Mutex<()>,
    entries: Mutex<HashMap<String, Entry>>,
    max_agents: usize,
    max_live_tabs: usize,
    /// Monotonic completion order for bounded tab eviction.
    next_completed_seq: AtomicU64,
    reporter: OnceLock<AgentDiagnosticReporter>,
    usage_reporter: OnceLock<AgentUsageReporter>,
    /// Optional owner-facing session index observer. Receives the agent id and
    /// the validated conversation URL; never prompts or replies.
    conversation_recorder: OnceLock<ConversationRecorder>,
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
                vm: OnceLock::new(),
                spawn_gate: Mutex::new(()),
                tab_gate: Mutex::new(()),
                entries: Mutex::new(HashMap::new()),
                max_agents: MAX_AGENTS,
                max_live_tabs: MAX_LIVE_TABS,
                next_completed_seq: AtomicU64::new(1),
                reporter: OnceLock::new(),
                usage_reporter: OnceLock::new(),
                conversation_recorder: OnceLock::new(),
            }),
        }
    }

    /// Enable explicit per-subagent disposable VM forks. Without this opt-in,
    /// a spawn request naming a VM image fails before opening a browser tab.
    pub fn with_vm_manager(self, vm: VmManager) -> Self {
        let _ = self.inner.vm.set(vm);
        self
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

    /// Configure a non-blocking metadata-only usage observer for backend turns.
    /// No prompt or reply bodies are forwarded; counts are estimates, not billing.
    pub fn with_usage_reporter(
        self,
        reporter: impl Fn(&str, &str, usize, Option<usize>, bool) + Send + Sync + 'static,
    ) -> Self {
        let _ = self.inner.usage_reporter.set(Arc::new(reporter));
        self
    }

    /// Configure one optional owner-facing session index observer. It receives
    /// the agent id and the conversation URL so the TUI can list and reopen the
    /// chat later. The callback must not block: the CLI forwards the value to a
    /// blocking writer. It is never called for prompts or assistant text.
    pub fn with_conversation_recorder(
        self,
        recorder: impl Fn(&str, &str) + Send + Sync + 'static,
    ) -> Self {
        let _ = self.inner.conversation_recorder.set(Arc::new(recorder));
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
        self.spawn_internal(request, None, None).await
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
        self.spawn_internal(request, connector_name, None).await
    }

    /// A VM is created only when the parent explicitly chooses a previously
    /// approved base. Its lifetime is bound to this single worker execution.
    pub async fn spawn_with_connector_vm(
        &self,
        request: SpawnAgent,
        connector_name: Option<&str>,
        vm_image: Option<&str>,
    ) -> AgentResult<AgentInfo> {
        self.spawn_internal(request, connector_name, vm_image).await
    }

    async fn spawn_internal(
        &self,
        request: SpawnAgent,
        connector_name: Option<&str>,
        vm_image: Option<&str>,
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
        // Admission is serialized only across spawns. VM conversion/startup is
        // allowed to await without locking any unrelated agent actions.
        let _spawn_guard = self.inner.spawn_gate.lock().await;
        {
            let agents = self.inner.entries.lock().await;
            if agents.len() >= MAX_RECORDS {
                return Err(AgentError::RecordCapacity);
            }
            if agents
                .values()
                .filter(|entry| {
                    matches!(entry.info.state, AgentState::Running | AgentState::Waiting)
                })
                .count()
                >= self.inner.max_agents
            {
                return Err(AgentError::Capacity);
            }
        }
        let vm_lease = if let Some(image) = vm_image {
            let manager = self.inner.vm.get().ok_or(AgentError::VmUnavailable)?;
            let guest = manager
                .create(VmCreate {
                    image: image.to_owned(),
                    cpus: None,
                    memory_mb: None,
                    linux_options: None,
                    installer_iso: None,
                })
                .await
                .map_err(|_| AgentError::VmUnavailable)?;
            Some(AgentVmLease {
                image: image.to_owned(),
                id: guest.instance.id,
                state: AgentVmState::Active,
            })
        } else {
            None
        };
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
            "{prompt}\n\nWhile actively working, call subagent_progress with id={id}, code={code}, and kind chosen from working, reading, editing, testing, reviewing, blocked, after each other NovaGPT MCP tool call. This authenticated progress is how your parent knows that tool usage belongs to you; ordinary tool calls cannot be safely attributed to a specific worker. Do not send free-text details, prompts, URLs or raw tool inputs/results."
        );
        let tool_instructions = connector_name.map(|name| format!(
            "**NovaGPT workspace tools**\nYour coding workspace is managed by the NovaGPT connector named {}.\nYou MUST use this specific connected app/plugin to interact with the workspace whenever its tools are available. Discover the actual tool namespace and exposed capabilities before attempting file operations. Do not derive the namespace from the display name. For existing-file modifications use authenticated subagent_apply_patch; for new files use subagent_create_file; report activity through subagent_progress and completion through subagent_finished when exposed. Use the current private execution credential supplied by NovaGPT. Never print it, include it in reports, or expose it in logs. A successful server receipt is required before claiming a file was created or modified. If required tools are missing, disconnected, disabled, or inaccessible, report that explicitly; do not silently switch plugins or claim modifications.",
            serde_json::to_string(name).unwrap_or_default(),
        )).unwrap_or_else(|| "**NovaGPT workspace tools**\nThe parent cannot supply a ChatGPT connector display name. Discover actual connected NovaGPT tools by their advertised capabilities; do not invent any display name or derive a tool namespace. Report missing tools explicitly and never claim unverified edits.".to_owned());
        // The task is sent as the first browser turn, without a separate preflight.
        let mut prompt = format!("{tool_instructions}\n\n{prompt}");
        if let Some(lease) = &vm_lease {
            prompt.push_str(&format!(
                "\n\nISOLATED TEST GUEST: VM id={}, approved base={}. This guest is disposable, independent of the host workspace. The VM will be destroyed after completion, failure or cancellation. Never claim you ran tests inside it without a successful VM guest-tool receipt; On Linux-hosted managed QEMU forks, use vm_guest_status and vm_exec/vm_upload/vm_download through the QEMU Guest Agent only after the guest has installed and started qemu-guest-agent; guest control is unavailable otherwise. On Windows-hosted Hyper-V forks, these tools require operation-scoped guest credentials provided explicitly by the parent, never from the host workspace or prompt. Never invent guest readiness or credentials; report unavailable access. Do not expose host secrets or credentials to the guest.",
                lease.id, lease.image
            ));
        }
        let info = AgentInfo {
            id: id.clone(),
            workspace_id: workspace.workspace_id,
            role: request.role,
            state: AgentState::Running,
            backend: self.inner.backend.name().into(),
            model: self.inner.backend.model().into(),
            connector_name: connector_name.map(str::to_owned),
            vm: vm_lease,
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
            if agents.len() >= MAX_RECORDS {
                return Err(AgentError::RecordCapacity);
            }
            if agents
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
        self.start(id, prompt, true, 1, info.workspace_id.clone());
        Ok(info)
    }

    fn start(
        &self,
        id: String,
        prompt: String,
        must_create: bool,
        epoch: u64,
        workspace_id: String,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            let outcome = async {
                if must_create {
                    let _tab_guard = manager.inner.tab_gate.lock().await;
                    if !manager.is_active(&id, epoch).await {
                        return Err(BackendFailure::Unavailable);
                    }
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
                    manager.mark_ready(&id, epoch).await;
                } else {
                    manager.mark_ready(&id, epoch).await;
                }
                if !manager.is_active(&id, epoch).await {
                    manager.inner.backend.close(&id).await;
                    return Err(BackendFailure::Unavailable);
                }
                let reply = manager.inner.backend.send(&id, &prompt).await;
                if let Some(reporter) = manager.inner.usage_reporter.get() {
                    reporter(
                        &id,
                        &workspace_id,
                        prompt.chars().count(),
                        reply.as_ref().ok().map(|(text, _)| text.chars().count()),
                        reply.is_err(),
                    );
                }
                let stage = match &reply {
                    Ok(_) => AgentLogStage::BrowserReplyReceived,
                    Err(BackendFailure::Uncertain(_)) => AgentLogStage::BrowserReplyUncertain,
                    Err(BackendFailure::Blocked(_)) => AgentLogStage::BrowserReplyBlocked,
                    Err(BackendFailure::Manual(_)) => AgentLogStage::BrowserReplyManual,
                    Err(BackendFailure::Unavailable) => AgentLogStage::BrowserReplyFailed,
                };
                let still_observed = {
                    let entries = manager.inner.entries.lock().await;
                    entries.get(&id).is_some_and(|entry| {
                        entry.epoch == epoch
                            && entry.info.state == AgentState::Running
                            && !entry.completion_claimed
                    })
                };
                if still_observed {
                    manager.emit(&id, stage);
                }
                reply
            }
            .await;
            // Record the conversation outside the per-agent lock; the observer
            // is required to be non-blocking. A discarded stale reply may still
            // have created a real ChatGPT conversation worth listing.
            if let (Ok((_, Some(url))), Some(recorder)) =
                (&outcome, manager.inner.conversation_recorder.get())
            {
                recorder(&id, url);
            }
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
            if matches!(terminal, Some(AgentState::Completed | AgentState::Failed)) {
                manager.cleanup_vm(&id).await;
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
            && entry.info.state == AgentState::Running
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
        let _admission = self.inner.spawn_gate.lock().await;
        let mut agents = self.inner.entries.lock().await;
        let active = agents
            .values()
            .filter(|entry| matches!(entry.info.state, AgentState::Running | AgentState::Waiting))
            .count();
        let entry = agents.get_mut(id).ok_or(AgentError::NotFound)?;
        if entry.completion_claimed {
            return Err(AgentError::Busy);
        }
        if entry.info.state == AgentState::Cancelled {
            return Err(AgentError::Cancelled);
        }
        if entry.info.vm.is_some() && !resume {
            return Err(AgentError::VmLeaseEnded);
        }
        if entry
            .info
            .vm
            .as_ref()
            .is_some_and(|vm| vm.state != AgentVmState::Active)
        {
            return Err(AgentError::VmLeaseEnded);
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
        if !resume && active >= self.inner.max_agents {
            return Err(AgentError::Capacity);
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
        self.start(
            id.to_owned(),
            prompt,
            created,
            epoch,
            info.workspace_id.clone(),
        );
        Ok(info)
    }

    /// Destroy a VM lease once, independently of the browser task outcome.
    /// A failed cleanup remains visible in the returned agent record. A crash
    /// may leave a disk and instance manifest for explicit owner recovery.
    async fn cleanup_vm(&self, id: &str) {
        let vm_id = {
            let mut entries = self.inner.entries.lock().await;
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            let Some(lease) = entry.info.vm.as_mut() else {
                return;
            };
            if lease.state != AgentVmState::Active {
                return;
            }
            lease.state = AgentVmState::Cleaning;
            let vm_id = lease.id.clone();
            entry.updates.send_replace(entry.info.clone());
            vm_id
        };
        let result = match self.inner.vm.get() {
            Some(vm) => vm.destroy(&vm_id).await.is_ok(),
            None => false,
        };
        let mut entries = self.inner.entries.lock().await;
        if let Some(entry) = entries.get_mut(id)
            && let Some(lease) = entry.info.vm.as_mut()
            && lease.id == vm_id
            && lease.state == AgentVmState::Cleaning
        {
            lease.state = if result {
                AgentVmState::Destroyed
            } else {
                AgentVmState::CleanupFailed
            };
            entry.updates.send_replace(entry.info.clone());
        }
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
        self.cleanup_vm(id).await;
        Ok(self
            .inner
            .entries
            .lock()
            .await
            .get(id)
            .map(|entry| entry.info.clone())
            .unwrap_or(info))
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
        let patch_guard = gate.lock_owned().await;
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
        let manager = self.clone();
        let id = id.to_owned();
        tokio::spawn(async move {
            let _patch_guard = patch_guard;
            manager.finish_claimed(&id).await
        })
        .await
        .map_err(|_| AgentError::Backend)?
    }

    async fn finish_claimed(&self, id: &str) -> AgentResult<AgentCompletionAck> {
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
        self.cleanup_vm(&ack.id).await;
        Ok(ack)
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
mod tests;
