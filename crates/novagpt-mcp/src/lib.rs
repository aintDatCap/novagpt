//! Official rmcp-based protocol and transport boundary.
mod audit;
mod output_store;
pub use audit::AuditTrail;
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use base64::Engine;
use novagpt_agents::{AgentManager, AgentRole, SpawnAgent};
use novagpt_browser::BrowserBridge;
use novagpt_core::{
    BashMode, BashRequest, EditRequest, FileWriteRequest, NovaError, NovaResult, WorkspaceRegistry,
};
use novagpt_vm::{VmCreate, VmManager};
pub use output_store::OutputStore;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock},
    schemars, tool, tool_handler, tool_router,
    transport::{
        stdio,
        streamable_http_server::{
            StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
        },
    },
};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use subtle::ConstantTimeEq;

#[derive(Clone, Default)]
pub struct NovaExtras {
    pub vm: Option<VmManager>,
    pub agents: Option<AgentManager>,
    pub browser_bridge: Option<BrowserBridge>,
    pub output_store: OutputStore,
    pub bash_mode: BashMode,
    pub audit: AuditTrail,
}

#[derive(Clone)]
pub struct NovaMcp {
    workspaces: WorkspaceRegistry,
    extras: NovaExtras,
}

impl NovaMcp {
    pub fn new(workspaces: WorkspaceRegistry) -> Self {
        Self {
            workspaces,
            extras: NovaExtras::default(),
        }
    }
    pub fn with_extras(workspaces: WorkspaceRegistry, extras: NovaExtras) -> Self {
        Self { workspaces, extras }
    }
    fn observed(&self, tool: &'static str, result: CallToolResult) -> CallToolResult {
        self.extras
            .audit
            .record(tool, result.is_error.unwrap_or(false));
        result
    }

    async fn observed_compute<T, F>(&self, tool: &'static str, job: F) -> CallToolResult
    where
        T: Serialize + Send + 'static,
        F: FnOnce() -> NovaResult<T> + Send + 'static,
    {
        self.observed(tool, compute(job).await)
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OpenWorkspaceArgs {
    root: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CodexContextArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    target_path: Option<String>,
    #[serde(default)]
    instruction_fingerprint: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ReconnectArgs {
    workspace_id: String,
    #[serde(default)]
    root: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WorkspaceArgs {
    #[serde(default)]
    workspace_id: Option<String>,
}
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct InspectArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    max_entries: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct FileArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct TreeArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_depth: Option<usize>,
    #[serde(default)]
    max_entries: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct SearchArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    query: String,
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct WriteArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    path: String,
    content: String,
    #[serde(default)]
    expected_sha256: Option<String>,
    #[serde(default)]
    create_dirs: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct EditArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    path: String,
    old_text: String,
    new_text: String,
    #[serde(default)]
    expected_sha256: Option<String>,
    #[serde(default)]
    replace_all: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct HandoffCreateArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    id: String,
    instructions: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ProContextArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    title: String,
    selected_paths: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct HandoffWaitArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    plan_hash: Option<String>,
    #[serde(default)]
    since_iteration: Option<u32>,
    #[serde(default)]
    max_wait_seconds: Option<u64>,
    #[serde(default)]
    poll_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct HandoffReadArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ImportAttachment {
    download_url: String,
    file_id: String,
    #[serde(default)]
    mime_type: Option<String>,
    #[serde(default)]
    file_name: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ImportArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    file: ImportAttachment,
    destination: String,
    #[serde(default)]
    overwrite: Option<bool>,
    #[serde(default)]
    expected_sha256: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct PatchArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    patch: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OutputArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    output_resource_id: String,
    #[serde(default)]
    stream: Option<String>,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BashArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    command: String,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GitArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    action: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    staged: Option<bool>,
    #[serde(default)]
    include_diff: Option<bool>,
    #[serde(default)]
    count: Option<usize>,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    start_point: Option<String>,
    #[serde(default)]
    create: Option<bool>,
    #[serde(default)]
    files: Vec<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    discard_changes: Option<bool>,
    #[serde(default)]
    worktree_id: Option<String>,
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct GitReviewArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    staged: Option<bool>,
    #[serde(default)]
    include_diff: Option<bool>,
}

impl GitArgs {
    fn into_request(self) -> novagpt_core::GitRequest {
        novagpt_core::GitRequest {
            workspace_id: self.workspace_id,
            action: self.action,
            path: self.path,
            staged: self.staged.unwrap_or(false),
            include_diff: self.include_diff.unwrap_or(true),
            count: self.count,
            reference: self.reference,
            name: self.name,
            start_point: self.start_point,
            create: self.create.unwrap_or(false),
            files: self.files,
            message: self.message,
            discard_changes: self.discard_changes.unwrap_or(false),
            worktree_id: self.worktree_id,
            branch: self.branch,
        }
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum VmAction {
    Images,
    Create,
    Status,
    Destroy,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct VmArgs {
    action: VmAction,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    cpus: Option<u8>,
    #[serde(default)]
    memory_mb: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct IdArgs {
    id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct BrowserReadArgs {
    /// Explicit CDP target ID from browser_tabs, not a URL or free-form JavaScript.
    tab_id: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AgentSpawnArgs {
    #[serde(default)]
    workspace_id: Option<String>,
    task: String,
    role: String,
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    context: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AgentMessageArgs {
    id: String,
    message: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AgentResumeArgs {
    id: String,
    manual_check_completed: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AgentStatusArgs {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct AgentCancelArgs {
    id: String,
    #[serde(default)]
    cleanup_worktree: Option<bool>,
}

fn external_reply<T: Serialize, E: std::fmt::Display>(result: Result<T, E>) -> CallToolResult {
    tool_reply(result.map_err(|error| NovaError::InvalidInput(error.to_string())))
}

fn tool_reply<T: Serialize>(result: NovaResult<T>) -> CallToolResult {
    match result {
        Ok(value) => {
            let text = serde_json::to_string_pretty(&value)
                .unwrap_or_else(|_| "{\"error\":\"Unable to serialize response\"}".to_string());
            let mut response = CallToolResult::success(vec![ContentBlock::text(text)]);
            response.structured_content = serde_json::to_value(value).ok();
            response
        }
        Err(err) => {
            let message = err.to_string();
            let mut response = CallToolResult::error(vec![ContentBlock::text(message.clone())]);
            response.structured_content = Some(serde_json::json!({ "error": message }));
            response
        }
    }
}

async fn compute<T, F>(job: F) -> CallToolResult
where
    T: Serialize + Send + 'static,
    F: FnOnce() -> NovaResult<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(job).await {
        Ok(result) => tool_reply(result),
        Err(_) => tool_reply::<()>(Err(NovaError::StatePoisoned)),
    }
}

#[tool_router]
impl NovaMcp {
    #[tool(description = "Return bounded metadata-only HUMAN/LLM tool activity for this process")]
    async fn runtime_status(&self) -> CallToolResult {
        tool_reply(Ok(self.extras.audit.snapshot_flushed(50).await))
    }

    #[tool(description = "Show safe server capability flags without revealing secrets")]
    fn server_config(&self) -> CallToolResult {
        self.observed(
            "server_config",
            tool_reply(Ok(serde_json::json!({
                "bash_mode": match self.extras.bash_mode {
                    BashMode::Off => "off", BashMode::Safe => "safe", BashMode::Full => "full",
                },
                "vm_enabled": self.extras.vm.is_some(),
                "subagents_enabled": self.extras.agents.is_some(),
            "browser_bridge_enabled": self.extras.browser_bridge.is_some(),
                "audit_mode": self.extras.audit.snapshot(1).mode,
                "write_mode": "workspace-guarded",
                "transport_session_is_conversation_identity": false,
            }))),
        )
    }

    #[tool(
        description = "Open the default, configured workspace without changing any other conversation's selection"
    )]
    fn open_current_workspace(&self) -> CallToolResult {
        self.observed(
            "open_current_workspace",
            tool_reply(Ok(self.workspaces.default_workspace())),
        )
    }

    #[tool(
        description = "Open an allowed canonical workspace. Subsequent tools can use its workspace_id"
    )]
    async fn open_workspace(
        &self,
        Parameters(args): Parameters<OpenWorkspaceArgs>,
    ) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("open_workspace", move || state.open_workspace(args.root))
            .await
    }

    #[tool(
        description = "Revalidate a known workspace ID or match it against an explicitly supplied allowed canonical root"
    )]
    async fn reconnect_workspace(
        &self,
        Parameters(args): Parameters<ReconnectArgs>,
    ) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("reconnect_workspace", move || {
            state.reconnect_workspace(&args.workspace_id, args.root.as_deref())
        })
        .await
    }

    #[tool(
        description = "Read bounded root-to-target AGENTS.md instructions; unchanged fingerprints omit instructions"
    )]
    async fn codex_context(
        &self,
        Parameters(args): Parameters<CodexContextArgs>,
    ) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("codex_context", move || {
            state.codex_instructions(
                args.workspace_id.as_deref(),
                args.target_path.as_deref(),
                args.instruction_fingerprint.as_deref(),
            )
        })
        .await
    }

    #[tool(
        description = "List explicitly opened allowed workspace roots (no implicit active conversation root)"
    )]
    async fn list_workspaces(&self) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("list_workspaces", move || state.list_workspaces())
            .await
    }

    #[tool(
        description = "Return the stable canonical workspace ID and root without exposing file content"
    )]
    async fn workspace_snapshot(
        &self,
        Parameters(args): Parameters<WorkspaceArgs>,
    ) -> CallToolResult {
        self.observed(
            "workspace_snapshot",
            tool_reply(self.workspaces.workspace(args.workspace_id.as_deref())),
        )
    }

    #[tool(
        description = "Inspect a bounded, safe file inventory (extensions/manifests/tests, no file contents)"
    )]
    async fn inspect_workspace(&self, Parameters(args): Parameters<InspectArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("inspect_workspace", move || {
            state.inspect_workspace(
                args.workspace_id.as_deref(),
                args.max_entries.unwrap_or(300),
            )
        })
        .await
    }

    #[tool(
        description = "List safe files and directories within an explicitly selected or default workspace"
    )]
    async fn tree(&self, Parameters(args): Parameters<TreeArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("tree", move || {
            state.tree(
                args.workspace_id.as_deref(),
                args.path.as_deref().unwrap_or("."),
                args.max_depth.unwrap_or(3),
                args.max_entries.unwrap_or(250),
            )
        })
        .await
    }

    #[tool(description = "Search for literal text in safe workspace files")]
    async fn search(&self, Parameters(args): Parameters<SearchArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("search", move || {
            state.search(
                args.workspace_id.as_deref(),
                &args.query,
                args.max_results.unwrap_or(100),
            )
        })
        .await
    }

    #[tool(
        description = "Read a bounded UTF-8 file and return text and SHA-256 for optimistic edits"
    )]
    async fn read(&self, Parameters(args): Parameters<FileArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("read", move || {
            state.read_file(args.workspace_id.as_deref(), &args.path)
        })
        .await
    }

    #[tool(
        description = "Read a bounded PNG, JPEG, GIF or WebP from an explicitly guarded workspace path"
    )]
    async fn view_image(&self, Parameters(args): Parameters<FileArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        let result = tokio::task::spawn_blocking(move || {
            state.view_image(args.workspace_id.as_deref(), &args.path)
        })
        .await;
        let reply = match result {
            Ok(Ok(image)) => {
                let data = base64::engine::general_purpose::STANDARD.encode(&image.bytes);
                let mut answer = CallToolResult::success(vec![ContentBlock::image(
                    data,
                    image.mime_type.to_string(),
                )]);
                answer.structured_content = Some(serde_json::json!({
                    "path": image.path,
                    "mime_type": image.mime_type,
                    "bytes": image.bytes.len()
                }));
                answer
            }
            Ok(Err(error)) => tool_reply::<()>(Err(error)),
            Err(_) => tool_reply::<()>(Err(NovaError::StatePoisoned)),
        };
        self.observed("view_image", reply)
    }

    #[tool(
        description = "Write a workspace text file with optional expected_sha256 conflict detection"
    )]
    async fn write(&self, Parameters(args): Parameters<WriteArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("write", move || {
            state.write_file(FileWriteRequest {
                workspace_id: args.workspace_id.as_deref(),
                path: &args.path,
                content: &args.content,
                expected_sha256: args.expected_sha256.as_deref(),
                create_dirs: args.create_dirs.unwrap_or(true),
            })
        })
        .await
    }

    #[tool(
        description = "Replace one or all exact text matches with optional expected_sha256 conflict detection"
    )]
    async fn edit(&self, Parameters(args): Parameters<EditArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("edit", move || {
            state.edit_file(EditRequest {
                workspace_id: args.workspace_id.as_deref(),
                path: &args.path,
                old_text: &args.old_text,
                new_text: &args.new_text,
                expected_sha256: args.expected_sha256.as_deref(),
                replace_all: args.replace_all.unwrap_or(false),
            })
        })
        .await
    }

    #[tool(
        description = "Create a new bounded handoff plan in the selected workspace; never executes the handoff"
    )]
    async fn handoff_to_agent(
        &self,
        Parameters(args): Parameters<HandoffCreateArgs>,
    ) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("handoff_to_agent", move || {
            state.create_handoff(args.workspace_id.as_deref(), &args.id, &args.instructions)
        })
        .await
    }

    #[tool(description = "Read a handoff artifact by validated ID from the selected workspace")]
    async fn read_handoff(&self, Parameters(args): Parameters<HandoffReadArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("read_handoff", move || {
            state.read_handoff(args.workspace_id.as_deref(), &args.id)
        })
        .await
    }

    #[tool(
        description = "Read-only bounded long-poll for externally produced handoff-run-state.json; does not execute agents"
    )]
    async fn wait_for_handoff(
        &self,
        Parameters(args): Parameters<HandoffWaitArgs>,
    ) -> CallToolResult {
        let max_secs = args.max_wait_seconds.unwrap_or(20);
        let poll_ms = args.poll_ms.unwrap_or(1000);
        if !(1..=60).contains(&max_secs) || !(250..=5000).contains(&poll_ms) {
            return self.observed(
                "wait_for_handoff",
                tool_reply::<()>(Err(NovaError::InvalidInput(
                    "Invalid polling interval or wait duration".into(),
                ))),
            );
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(max_secs);
        loop {
            let state = self.workspaces.clone();
            let id = args.workspace_id.clone();
            let hash = args.plan_hash.clone();
            let iteration = args.since_iteration;
            let result = tokio::task::spawn_blocking(move || {
                state.handoff_status(id.as_deref(), hash.as_deref(), iteration)
            })
            .await;
            match result {
                Ok(Ok(mut status)) => {
                    if status.awaited_terminal || tokio::time::Instant::now() >= deadline {
                        status.next_poll_after_seconds =
                            (!status.awaited_terminal).then_some(poll_ms.div_ceil(1000));
                        return self.observed("wait_for_handoff", tool_reply(Ok(status)));
                    }
                }
                Ok(Err(error)) => {
                    return self.observed("wait_for_handoff", tool_reply::<()>(Err(error)));
                }
                Err(_) => {
                    return self.observed(
                        "wait_for_handoff",
                        tool_reply::<()>(Err(NovaError::StatePoisoned)),
                    );
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            tokio::time::sleep(remaining.min(std::time::Duration::from_millis(poll_ms))).await;
        }
    }

    #[tool(
        description = "Export a bounded offline context from explicitly selected workspace files; never overwrites a previous bundle"
    )]
    async fn export_pro_context(
        &self,
        Parameters(args): Parameters<ProContextArgs>,
    ) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("export_pro_context", move || {
            state.export_selected_pro_context(
                args.workspace_id.as_deref(),
                &args.title,
                &args.selected_paths,
            )
        })
        .await
    }

    #[tool(description = "Load a guarded workspace SKILL.md file at an explicit path")]
    async fn load_skill(&self, Parameters(args): Parameters<FileArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("load_skill", move || {
            state.load_skill(args.workspace_id.as_deref(), &args.path)
        })
        .await
    }

    #[tool(
        description = "Import a ChatGPT Apps SDK HTTPS attachment into a guarded workspace destination; never download arbitrary hosts"
    )]
    async fn import_file(&self, Parameters(args): Parameters<ImportArgs>) -> CallToolResult {
        let response = tool_reply(
            self.workspaces
                .import_attachment(novagpt_core::ImportRequest {
                    workspace_id: args.workspace_id,
                    file: novagpt_core::AttachmentRef {
                        download_url: args.file.download_url,
                        file_id: args.file.file_id,
                        mime_type: args.file.mime_type,
                        file_name: args.file.file_name,
                    },
                    destination: args.destination,
                    overwrite: args.overwrite.unwrap_or(false),
                    expected_sha256: args.expected_sha256,
                })
                .await,
        );
        self.observed("import_file", response)
    }

    #[tool(
        description = "Apply one guarded unified diff to an existing file; reject stale context and multi-file patches"
    )]
    async fn apply_patch(&self, Parameters(args): Parameters<PatchArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed_compute("apply_patch", move || {
            state.apply_unified_patch(args.workspace_id.as_deref(), &args.patch)
        })
        .await
    }

    #[tool(
        description = "Page over recent bounded command output by opaque resource ID, scoped to a workspace (in memory only)"
    )]
    async fn read_output(&self, Parameters(args): Parameters<OutputArgs>) -> CallToolResult {
        let result = self
            .workspaces
            .workspace(args.workspace_id.as_deref())
            .map_err(|_| NovaError::InvalidInput("Unknown workspace".into()))
            .and_then(|workspace| {
                self.extras
                    .output_store
                    .read(
                        &workspace.workspace_id,
                        &args.output_resource_id,
                        args.stream.as_deref().unwrap_or("stdout"),
                        args.offset.unwrap_or(0),
                        args.max_chars.unwrap_or(8000),
                    )
                    .map_err(|message| NovaError::InvalidInput(message.into()))
            });
        self.observed("read_output", tool_reply(result))
    }

    #[tool(
        description = "Execute a bounded local command under the startup-configured off/safe/full shell policy"
    )]
    async fn bash(&self, Parameters(args): Parameters<BashArgs>) -> CallToolResult {
        let workspace_id = self
            .workspaces
            .workspace(args.workspace_id.as_deref())
            .map(|workspace| workspace.workspace_id);
        let result = self
            .workspaces
            .run_bash(
                self.extras.bash_mode,
                BashRequest {
                    workspace_id: args.workspace_id,
                    command: args.command,
                    shell: args.shell,
                    cwd: args.cwd,
                    timeout_ms: args.timeout_ms,
                },
            )
            .await
            .map(|mut value| {
                if let Ok(workspace_id) = workspace_id
                    && let Ok(id) = self.extras.output_store.save(
                        &workspace_id,
                        &value.stdout,
                        &value.stderr,
                        value.truncated,
                    )
                {
                    value.output_resource_id = Some(id);
                }
                value
            });
        self.observed("bash", tool_reply(result))
    }

    #[tool(
        description = "Run bounded, structured Git operations; unsafe or unimplemented write actions fail closed"
    )]
    async fn git(&self, Parameters(args): Parameters<GitArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        self.observed(
            "git",
            tool_reply(state.git_operation(args.into_request()).await),
        )
    }

    #[tool(description = "Read the workspace Git branch and working tree status")]
    async fn git_status(&self, Parameters(args): Parameters<GitReviewArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        let response = tool_reply(
            state
                .git_operation(novagpt_core::GitRequest {
                    workspace_id: args.workspace_id,
                    action: "git_status".into(),
                    path: args.path,
                    ..Default::default()
                })
                .await,
        );
        self.observed("git_status", response)
    }

    #[tool(
        description = "Show Git diff for one guarded path, or workspace stats with include_diff=false"
    )]
    async fn git_diff(&self, Parameters(args): Parameters<GitReviewArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        let response = tool_reply(
            state
                .git_operation(novagpt_core::GitRequest {
                    workspace_id: args.workspace_id,
                    action: "git_diff".into(),
                    path: args.path,
                    staged: args.staged.unwrap_or(false),
                    include_diff: args.include_diff.unwrap_or(true),
                    ..Default::default()
                })
                .await,
        );
        self.observed("git_diff", response)
    }

    #[tool(
        description = "Inspect guarded Git changes; raw patch requires path, workspace-wide summary requires include_diff=false"
    )]
    async fn show_changes(&self, Parameters(args): Parameters<GitReviewArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        let response = tool_reply(
            state
                .git_operation(novagpt_core::GitRequest {
                    workspace_id: args.workspace_id,
                    action: "show_changes".into(),
                    path: args.path,
                    staged: args.staged.unwrap_or(false),
                    include_diff: args.include_diff.unwrap_or(false),
                    ..Default::default()
                })
                .await,
        );
        self.observed("show_changes", response)
    }

    #[tool(
        description = "List only manually open Chrome tabs at the explicit approved HTTPS origin; bridge must be opt-in at startup"
    )]
    async fn browser_tabs(&self) -> CallToolResult {
        let reply = match &self.extras.browser_bridge {
            Some(bridge) => external_reply(bridge.tabs().await),
            None => tool_reply::<()>(Err(NovaError::InvalidInput(
                "Browser Bridge disabled; enable it explicitly at startup".into(),
            ))),
        };
        self.observed("browser_tabs", reply)
    }

    #[tool(
        description = "Read visible text of a selected approved Chrome tab without navigation or browser input"
    )]
    async fn browser_tab_read(
        &self,
        Parameters(args): Parameters<BrowserReadArgs>,
    ) -> CallToolResult {
        let reply = match &self.extras.browser_bridge {
            Some(bridge) => external_reply(bridge.read_tab(&args.tab_id).await),
            None => tool_reply::<()>(Err(NovaError::InvalidInput(
                "Browser Bridge disabled; enable it explicitly at startup".into(),
            ))),
        };
        self.observed("browser_tab_read", reply)
    }

    #[tool(
        description = "Manage explicitly imported disposable NovaGPT VM images/instances: images, create, status or destroy. Disabled unless --vm-home is provided."
    )]
    async fn vm(&self, Parameters(args): Parameters<VmArgs>) -> CallToolResult {
        let response = async {
            let Some(vm) = self.extras.vm.clone() else {
                return external_reply::<(), _>(Err(novagpt_vm::VmError::Disabled));
            };
            match args.action {
                VmAction::Images => external_reply(vm.images()),
                VmAction::Create => match args.image {
                    Some(image) => external_reply(
                        vm.create(VmCreate {
                            image,
                            cpus: args.cpus,
                            memory_mb: args.memory_mb,
                        })
                        .await,
                    ),
                    None => tool_reply::<()>(Err(NovaError::InvalidInput(
                        "image is required for create".into(),
                    ))),
                },
                VmAction::Status => match args.id {
                    Some(id) => external_reply(vm.status(&id).await),
                    None => tool_reply::<()>(Err(NovaError::InvalidInput(
                        "id is required for status".into(),
                    ))),
                },
                VmAction::Destroy => match args.id {
                    Some(id) => external_reply(vm.destroy(&id).await),
                    None => tool_reply::<()>(Err(NovaError::InvalidInput(
                        "id is required for destroy".into(),
                    ))),
                },
            }
        }
        .await;
        self.observed("vm", response)
    }

    #[tool(
        description = "Check VM guest control availability without reading operation-scoped credentials"
    )]
    fn vm_guest_status(&self, Parameters(args): Parameters<IdArgs>) -> CallToolResult {
        self.observed(
            "vm_guest_status",
            match &self.extras.vm {
                Some(vm) => external_reply(vm.guest_status(&args.id)),
                None => external_reply::<(), _>(Err(novagpt_vm::VmError::Disabled)),
            },
        )
    }

    #[tool(
        description = "Spawn a persistent asynchronous untrusted subagent. Valid roles: explorer, reviewer, tester, implementer. The implementer proposes patches but never auto-applies them."
    )]
    async fn subagent_spawn(&self, Parameters(args): Parameters<AgentSpawnArgs>) -> CallToolResult {
        let response = async {
            let Some(manager) = &self.extras.agents else {
                return external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled));
            };
            let role = match args.role.as_str() {
                "explorer" => AgentRole::Explorer,
                "reviewer" => AgentRole::Reviewer,
                "tester" => AgentRole::Tester,
                "implementer" => AgentRole::Implementer,
                _ => {
                    return tool_reply::<()>(Err(NovaError::InvalidInput(
                        "role must be explorer, reviewer, tester or implementer".into(),
                    )));
                }
            };
            external_reply(
                manager
                    .spawn(SpawnAgent {
                        workspace_id: args.workspace_id,
                        task: args.task,
                        role,
                        paths: args.paths,
                        context: args.context,
                    })
                    .await,
            )
        }
        .await;
        self.observed("subagent_spawn", response)
    }

    #[tool(description = "Send follow-up message to a completed or failed persistent subagent")]
    async fn subagent_message(
        &self,
        Parameters(args): Parameters<AgentMessageArgs>,
    ) -> CallToolResult {
        let response = match &self.extras.agents {
            Some(manager) => external_reply(manager.message(&args.id, &args.message).await),
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        };
        self.observed("subagent_message", response)
    }

    #[tool(
        description = "Resume a waiting subagent after the user confirms manual browser login/verification. Does not bypass verification."
    )]
    async fn subagent_resume(
        &self,
        Parameters(args): Parameters<AgentResumeArgs>,
    ) -> CallToolResult {
        let response = match &self.extras.agents {
            Some(manager) => {
                external_reply(manager.resume(&args.id, args.manual_check_completed).await)
            }
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        };
        self.observed("subagent_resume", response)
    }

    #[tool(
        description = "List shared server subagent states without identifying ChatGPT conversations as transport sessions"
    )]
    async fn subagent_status(
        &self,
        Parameters(args): Parameters<AgentStatusArgs>,
    ) -> CallToolResult {
        let response = match &self.extras.agents {
            Some(manager) => external_reply(manager.status(args.id.as_deref()).await),
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        };
        self.observed("subagent_status", response)
    }

    #[tool(
        description = "Return bounded untrusted subagent output for independent parent verification"
    )]
    async fn subagent_result(&self, Parameters(args): Parameters<IdArgs>) -> CallToolResult {
        let response = match &self.extras.agents {
            Some(manager) => external_reply(manager.result(&args.id).await),
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        };
        self.observed("subagent_result", response)
    }

    #[tool(
        description = "Cancel a subagent without auto-removing worktrees or modifying repository content"
    )]
    async fn subagent_cancel(
        &self,
        Parameters(args): Parameters<AgentCancelArgs>,
    ) -> CallToolResult {
        let response = async {
            if args.cleanup_worktree == Some(true) {
                return tool_reply::<()>(Err(NovaError::InvalidInput(
                    "NovaGPT does not auto-create or remove implementer worktrees yet".into(),
                )));
            }
            match &self.extras.agents {
                Some(manager) => external_reply(manager.cancel(&args.id).await),
                None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
            }
        }
        .await;
        self.observed("subagent_cancel", response)
    }

    #[tool(
        description = "Read safe bounded diagnostics for the dedicated ChatGPT Browser subagent tab; never reads cookies or challenge tokens"
    )]
    async fn subagent_browser_diagnostics(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> CallToolResult {
        let response = match &self.extras.agents {
            Some(manager) => external_reply(manager.diagnostics(&args.id).await),
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        };
        self.observed("subagent_browser_diagnostics", response)
    }
}

#[tool_handler(
    name = "novagpt",
    version = "0.1.0",
    instructions = "Use workspace_id explicitly for non-default roots; these are guarded local coding tools."
)]
impl ServerHandler for NovaMcp {}

pub async fn run_stdio(workspaces: WorkspaceRegistry) -> Result<(), Box<dyn std::error::Error>> {
    run_stdio_with(workspaces, NovaExtras::default()).await
}

pub async fn run_stdio_with(
    workspaces: WorkspaceRegistry,
    extras: NovaExtras,
) -> Result<(), Box<dyn std::error::Error>> {
    let audit = extras.audit.clone();
    let service = NovaMcp::with_extras(workspaces, extras)
        .serve(stdio())
        .await?;
    let outcome = service.waiting().await;
    audit.flush().await;
    outcome?;
    Ok(())
}

async fn authorize(State(token): State<Arc<String>>, request: Request, next: Next) -> Response {
    let supplied = request
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let expected = format!("Bearer {}", token);
    if supplied.as_bytes().ct_eq(expected.as_bytes()).unwrap_u8() == 1 {
        next.run(request).await
    } else {
        (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
    }
}

pub async fn run_http(
    workspaces: WorkspaceRegistry,
    address: SocketAddr,
    token: String,
) -> Result<(), Box<dyn std::error::Error>> {
    run_http_with(workspaces, address, token, NovaExtras::default()).await
}

pub async fn run_http_with(
    workspaces: WorkspaceRegistry,
    address: SocketAddr,
    token: String,
    extras: NovaExtras,
) -> Result<(), Box<dyn std::error::Error>> {
    if token.len() < 24 {
        return Err("NOVAGPT_HTTP_TOKEN must contain at least 24 characters".into());
    }
    if !address.ip().is_loopback() {
        return Err(
            "HTTP must bind to a loopback address until external security parity is verified"
                .into(),
        );
    }
    let audit = extras.audit.clone();
    let service = StreamableHttpService::new(
        move || Ok(NovaMcp::with_extras(workspaces.clone(), extras.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let router = Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn_with_state(Arc::new(token), authorize));
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(%address, "NovaGPT MCP HTTP listening");
    let result = axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await;
    audit.flush().await;
    result?;
    Ok(())
}
