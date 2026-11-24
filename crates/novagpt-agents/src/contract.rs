//! Public agent requests, responses, states and errors.
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("Subagents are disabled; configure a provider explicitly")]
    Disabled,
    #[error("Unknown subagent")]
    NotFound,
    #[error("Maximum concurrent subagent count reached")]
    Capacity,
    #[error(
        "Subagent registry is full (64 records); completed records are retained until server restart"
    )]
    RecordCapacity,
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
    #[error("An approved VM image and an enabled VM manager are required")]
    VmUnavailable,
    #[error("The bound disposable VM has finished; start a new subagent for another fork")]
    VmLeaseEnded,
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

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentVmState {
    Active,
    Cleaning,
    Destroyed,
    CleanupFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentVmLease {
    pub image: String,
    pub id: String,
    pub state: AgentVmState,
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
    /// Explicitly requested throwaway guest, never implied by MCP session identity.
    pub vm: Option<AgentVmLease>,
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
