//! Official rmcp-based protocol and transport boundary.
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use novagpt_agents::{AgentManager, AgentRole, SpawnAgent};
use novagpt_core::{EditRequest, FileWriteRequest, NovaError, NovaResult, WorkspaceRegistry};
use novagpt_vm::{VmCreate, VmManager};
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
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct OpenWorkspaceArgs {
    root: String,
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
    #[tool(
        description = "Open the default, configured workspace without changing any other conversation's selection"
    )]
    fn open_current_workspace(&self) -> CallToolResult {
        tool_reply(Ok(self.workspaces.default_workspace()))
    }

    #[tool(
        description = "Open an allowed canonical workspace. Subsequent tools can use its workspace_id"
    )]
    async fn open_workspace(
        &self,
        Parameters(args): Parameters<OpenWorkspaceArgs>,
    ) -> CallToolResult {
        let state = self.workspaces.clone();
        compute(move || state.open_workspace(args.root)).await
    }

    #[tool(
        description = "List safe files and directories within an explicitly selected or default workspace"
    )]
    async fn tree(&self, Parameters(args): Parameters<TreeArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        compute(move || {
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
        compute(move || {
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
        compute(move || state.read_file(args.workspace_id.as_deref(), &args.path)).await
    }

    #[tool(
        description = "Write a workspace text file with optional expected_sha256 conflict detection"
    )]
    async fn write(&self, Parameters(args): Parameters<WriteArgs>) -> CallToolResult {
        let state = self.workspaces.clone();
        compute(move || {
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
        compute(move || {
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
        description = "Manage explicitly imported disposable NovaGPT VM images/instances: images, create, status or destroy. Disabled unless --vm-home is provided."
    )]
    async fn vm(&self, Parameters(args): Parameters<VmArgs>) -> CallToolResult {
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

    #[tool(
        description = "Check VM guest control availability without reading operation-scoped credentials"
    )]
    fn vm_guest_status(&self, Parameters(args): Parameters<IdArgs>) -> CallToolResult {
        match &self.extras.vm {
            Some(vm) => external_reply(vm.guest_status(&args.id)),
            None => external_reply::<(), _>(Err(novagpt_vm::VmError::Disabled)),
        }
    }

    #[tool(
        description = "Spawn a persistent asynchronous untrusted subagent. Valid roles: explorer, reviewer, tester, implementer. The implementer proposes patches but never auto-applies them."
    )]
    async fn subagent_spawn(&self, Parameters(args): Parameters<AgentSpawnArgs>) -> CallToolResult {
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

    #[tool(description = "Send follow-up message to a completed or failed persistent subagent")]
    async fn subagent_message(
        &self,
        Parameters(args): Parameters<AgentMessageArgs>,
    ) -> CallToolResult {
        match &self.extras.agents {
            Some(manager) => external_reply(manager.message(&args.id, &args.message).await),
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        }
    }

    #[tool(
        description = "Resume a waiting subagent after the user confirms manual browser login/verification. Does not bypass verification."
    )]
    async fn subagent_resume(
        &self,
        Parameters(args): Parameters<AgentResumeArgs>,
    ) -> CallToolResult {
        match &self.extras.agents {
            Some(manager) => {
                external_reply(manager.resume(&args.id, args.manual_check_completed).await)
            }
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        }
    }

    #[tool(
        description = "List shared server subagent states without identifying ChatGPT conversations as transport sessions"
    )]
    async fn subagent_status(
        &self,
        Parameters(args): Parameters<AgentStatusArgs>,
    ) -> CallToolResult {
        match &self.extras.agents {
            Some(manager) => external_reply(manager.status(args.id.as_deref()).await),
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        }
    }

    #[tool(
        description = "Return bounded untrusted subagent output for independent parent verification"
    )]
    async fn subagent_result(&self, Parameters(args): Parameters<IdArgs>) -> CallToolResult {
        match &self.extras.agents {
            Some(manager) => external_reply(manager.result(&args.id).await),
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        }
    }

    #[tool(
        description = "Cancel a subagent without auto-removing worktrees or modifying repository content"
    )]
    async fn subagent_cancel(
        &self,
        Parameters(args): Parameters<AgentCancelArgs>,
    ) -> CallToolResult {
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

    #[tool(
        description = "Read safe bounded diagnostics for the dedicated ChatGPT Browser subagent tab; never reads cookies or challenge tokens"
    )]
    async fn subagent_browser_diagnostics(
        &self,
        Parameters(args): Parameters<IdArgs>,
    ) -> CallToolResult {
        match &self.extras.agents {
            Some(manager) => external_reply(manager.diagnostics(&args.id).await),
            None => external_reply::<(), _>(Err(novagpt_agents::AgentError::Disabled)),
        }
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
    let service = NovaMcp::with_extras(workspaces, extras)
        .serve(stdio())
        .await?;
    service.waiting().await?;
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
    axum::serve(listener, router)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
