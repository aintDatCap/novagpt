//! NovaGPT command-line entrypoint; never modifies a CodexPro installation.
use clap::{Parser, Subcommand};
use novagpt_agents::{AgentManager, ChatGptBackend};
use novagpt_browser::ChatGptBrowser;
use novagpt_core::WorkspaceRegistry;
use novagpt_mcp::NovaExtras;
use novagpt_vm::VmManager;
use std::sync::Arc;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};

#[derive(Parser)]
#[command(
    name = "novagpt",
    version,
    about = "NovaGPT — independent Rust MCP coding server"
)]
struct Cli {
    /// Default allowed workspace. Omitted workspace_id always resolves here.
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Additional approved workspace root (repeatable).
    #[arg(long = "allow-root")]
    allow_root: Vec<PathBuf>,
    /// Explicit, separate NovaGPT VM home. VM operations are disabled unless set.
    #[arg(long)]
    vm_home: Option<PathBuf>,
    /// Local visible Chrome executable for opt-in ChatGPT Browser subagents.
    #[arg(long)]
    chatgpt_chrome: Option<PathBuf>,
    /// Dedicated persistent Chrome profile for subagents, never a personal or CodexPro profile.
    #[arg(long)]
    chatgpt_profile: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server via standard input/output.
    Stdio,
    /// Start a token-protected Streamable HTTP MCP server on loopback.
    Http {
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
    /// Human-only import of an approved qcow2/VHDX disk into NovaGPT's isolated VM image store.
    VmImport {
        #[arg(long)]
        name: String,
        #[arg(long)]
        source: PathBuf,
        #[arg(long, default_value_t = 2)]
        cpus: u8,
        #[arg(long, default_value_t = 4096)]
        memory_mb: u32,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "novagpt=info".into()),
        )
        .init();
    let workspaces = WorkspaceRegistry::new(cli.root, &cli.allow_root)?;
    let vm = cli.vm_home.map(VmManager::new).transpose()?;
    let agents = match (cli.chatgpt_chrome, cli.chatgpt_profile) {
        (Some(executable), Some(profile)) => {
            let browser = ChatGptBrowser::new(executable, profile)?;
            Some(AgentManager::new(
                workspaces.clone(),
                Arc::new(ChatGptBackend::new(browser)),
            ))
        }
        (None, None) => None,
        _ => return Err("--chatgpt-chrome and --chatgpt-profile must be supplied together".into()),
    };
    let extras = NovaExtras { vm, agents };
    match cli.command.unwrap_or(Command::Stdio) {
        Command::Stdio => novagpt_mcp::run_stdio_with(workspaces, extras).await?,
        Command::Http { port } => {
            let token = std::env::var("NOVAGPT_HTTP_TOKEN")
                .map_err(|_| "NOVAGPT_HTTP_TOKEN is required for HTTP transport")?;
            novagpt_mcp::run_http_with(
                workspaces,
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port),
                token,
                extras,
            )
            .await?;
        }
        Command::VmImport {
            name,
            source,
            cpus,
            memory_mb,
        } => {
            let vm = extras
                .vm
                .ok_or("--vm-home must be provided for vm-import")?;
            let image = vm.import_image(&name, &source, cpus, memory_mb)?;
            println!(
                "Imported approved {} VM image {} ({} bytes)",
                match image.backend {
                    novagpt_vm::VmBackend::Hyperv => "Hyper-V",
                    novagpt_vm::VmBackend::Qemu => "QEMU",
                },
                image.name,
                image.bytes
            );
        }
    }
    Ok(())
}
