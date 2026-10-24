//! NovaGPT command-line entrypoint; never modifies a CodexPro installation.
mod settings;
mod tui;

use clap::{Parser, Subcommand};
use novagpt_agents::{AgentManager, ChatGptBackend};
use novagpt_browser::ChatGptBrowser;
use novagpt_core::WorkspaceRegistry;
use novagpt_mcp::NovaExtras;
use novagpt_vm::VmManager;
use settings::LocalSettings;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
};

#[derive(Parser)]
#[command(
    name = "novagpt",
    version,
    about = "NovaGPT — independent Rust MCP coding server"
)]
struct Cli {
    /// Workspace directory (defaults to the current working directory).
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Launch interactive Ratatui setup for the selected workspace.
    #[arg(long)]
    setup: bool,
    /// Read-only directory containing CodexPro profiles for optional import.
    #[arg(long)]
    codexpro_home: Option<PathBuf>,
    /// Extra explicitly approved workspace root; repeatable.
    #[arg(long = "allow-root")]
    allow_root: Vec<PathBuf>,
    /// Explicit NovaGPT VM storage path; overrides local settings.
    #[arg(long)]
    vm_home: Option<PathBuf>,
    /// Dedicated Chrome executable; overrides local settings.
    #[arg(long)]
    chatgpt_chrome: Option<PathBuf>,
    /// Dedicated browser profile path; never reuse a personal or CodexPro profile.
    #[arg(long)]
    chatgpt_profile: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over stdin/stdout (for MCP clients).
    Stdio,
    /// Start a token-protected Streamable HTTP MCP server on loopback.
    Http {
        /// Explicit port overrides the workspace's saved port.
        #[arg(long)]
        port: Option<u16>,
    },
    /// Import an approved qcow2/VHDX disk to NovaGPT's owned VM image store.
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
    let root = std::fs::canonicalize(&cli.root)?;
    if !root.is_dir() {
        return Err("workspace root must be an existing directory".into());
    }
    if cli.setup && cli.command.is_some() {
        return Err("--setup cannot be combined with a server subcommand".into());
    }
    let local = LocalSettings::load(&root)?;
    if cli.setup || cli.command.is_none() {
        let home = cli.codexpro_home.unwrap_or_else(settings::codexpro_home);
        let profiles = settings::discover_codexpro_profiles(&home)?;
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || tui::run(root, local, cli.setup, profiles, handle))
            .await??;
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "novagpt=info".into()),
        )
        .init();

    let mut approved_roots = local.allowed_roots;
    approved_roots.extend(cli.allow_root);
    let workspaces = WorkspaceRegistry::new(&root, &approved_roots)?;
    let vm_home = cli.vm_home.or_else(|| {
        local
            .vm_enabled
            .then(|| settings::local_dir(&root).join("vms"))
    });
    let vm = vm_home.map(VmManager::new).transpose()?;
    let browser_enabled = local.browser_enabled;
    let chrome = cli.chatgpt_chrome.or(if browser_enabled {
        local.chrome_executable
    } else {
        None
    });
    let profile = cli
        .chatgpt_profile
        .or_else(|| browser_enabled.then(|| settings::local_dir(&root).join("browser")));
    let agents = match (chrome, profile) {
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
    match cli.command {
        Some(Command::Stdio) => novagpt_mcp::run_stdio_with(workspaces, extras).await?,
        Some(Command::Http { port }) => {
            let token = std::env::var("NOVAGPT_HTTP_TOKEN")
                .map_err(|_| "NOVAGPT_HTTP_TOKEN is required for HTTP transport")?;
            novagpt_mcp::run_http_with(
                workspaces,
                SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port.unwrap_or(local.port)),
                token,
                extras,
            )
            .await?;
        }
        Some(Command::VmImport {
            name,
            source,
            cpus,
            memory_mb,
        }) => {
            let vm = extras
                .vm
                .ok_or("--vm-home or local VM configuration required")?;
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
        None => unreachable!("TUI was handled before server startup"),
    }
    Ok(())
}
