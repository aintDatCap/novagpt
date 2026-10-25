//! NovaGPT command-line entrypoint; never modifies a CodexPro installation.
mod connection;
mod logging;
mod settings;
mod token;
mod tui;
mod tunnel;

use clap::{Parser, Subcommand};
use novagpt_agents::{AgentManager, ChatGptBackend};
use novagpt_browser::{BrowserBridge, ChatGptBrowser};
use novagpt_core::{BashMode, WorkspaceRegistry};
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
    /// Opt in to an already-running, separate local Chrome CDP endpoint; never uses the ChatGPT subagent Chrome.
    #[arg(long, requires = "browser_bridge_origin")]
    browser_bridge_port: Option<u16>,
    /// Explicit HTTPS origin authorized for read-only tab access; never permits other origins.
    #[arg(long, requires = "browser_bridge_port")]
    browser_bridge_origin: Option<String>,
    /// Shell access mode: off, safe (fixed quality commands), or explicitly approved full.
    #[arg(long, value_parser = ["off", "safe", "full"], default_value = "safe")]
    bash_mode: String,
    /// Log format for server diagnostics on stderr: human text or LLM JSON lines.
    #[arg(long, value_parser = ["human", "llm"], default_value = "human")]
    log_format: String,
    /// Explicit acknowledgement required for unrestricted shell execution.
    #[arg(long)]
    allow_full_shell: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Inspect safe workspace information and enabled configuration without launching optional services.
    Doctor {
        /// Emit machine-readable JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
    /// Summarize a bounded and guarded workspace file inventory.
    Inspect {
        #[arg(long, default_value_t = 300)]
        max_entries: usize,
        #[arg(long)]
        json: bool,
    },
    /// Verify an authenticated MCP initialize against the local loopback HTTP endpoint.
    ConnectionTest {
        /// Explicit HTTP port, otherwise the configured workspace port.
        #[arg(long)]
        port: Option<u16>,
        /// Emit bounded machine-readable result.
        #[arg(long)]
        json: bool,
    },
    /// Show safe Git status without a raw diff or a repository write.
    Review {
        #[arg(long)]
        json: bool,
    },
    /// Run the MCP server over stdin/stdout (for MCP clients).
    Stdio,
    /// Start a token-protected Streamable HTTP MCP server on loopback.
    Http {
        /// Explicit port overrides the workspace's saved port.
        #[arg(long)]
        port: Option<u16>,
        /// Explicitly expose token-protected MCP HTTP through an owned ngrok process.
        #[arg(long, conflicts_with = "no_ngrok")]
        ngrok: bool,
        /// Disable ngrok even if imported workspace settings enable it.
        #[arg(long)]
        no_ngrok: bool,
        /// Optional stable ngrok hostname (must already be available to your account).
        #[arg(long, requires = "ngrok")]
        ngrok_domain: Option<String>,
    },
    /// Explicitly reveal the protected ChatGPT connector URL of a running ngrok tunnel.
    /// Never copies to clipboard or writes it to a log.
    ConnectorUrl {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Manage the auto-generated per-workspace MCP HTTP token.
    Token {
        #[command(subcommand)]
        action: TokenCommand,
    },
    /// Inspect the per-workspace persistent metadata-only audit without opening a server.
    Logs {
        #[command(subcommand)]
        action: LogsCommand,
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

#[derive(Subcommand)]
enum TokenCommand {
    /// Reveal the active token to the invoking terminal (handle as a password).
    Show,
    /// Print the local token-file path without revealing the token.
    Path,
}

#[derive(Subcommand)]
enum LogsCommand {
    /// Display the private per-workspace log directory (no token is accessed).
    Path,
    /// Show the latest retained safe audit events, as HUMAN text or structured JSON.
    Tail {
        #[arg(long, default_value_t = 50)]
        lines: usize,
        #[arg(long)]
        json: bool,
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
    if let Some(Command::Logs { action }) = &cli.command {
        let directory = logging::audit_directory(&root)?;
        match action {
            LogsCommand::Path => println!("{}", directory.display()),
            LogsCommand::Tail { lines, json } => {
                let count = (*lines).clamp(1, 128);
                let snapshot = tokio::task::spawn_blocking(move || {
                    novagpt_mcp::AuditTrail::persisted_snapshot(&directory, count)
                })
                .await??;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&snapshot)?);
                } else {
                    for line in &snapshot.human {
                        println!("{line}");
                    }
                    if snapshot.returned_events == 0 {
                        println!("No persisted audit events.");
                    }
                }
            }
        }
        return Ok(());
    }
    if let Some(Command::Token { action }) = &cli.command {
        match action {
            TokenCommand::Show => println!("{}", token::resolve(&root)?.value),
            TokenCommand::Path => println!("{}", token::path(&root)?.display()),
        }
        return Ok(());
    }
    if let Some(action) = &cli.command {
        match action {
            Command::ConnectorUrl { port } => {
                let port = port.unwrap_or(local.port);
                let resolved = token::resolve(&root)?;
                connection::probe(port, &resolved.value).await?;
                let endpoint = tunnel::NgrokTunnel::discover_endpoint(port, None)
                    .await
                    .ok_or("Nessun tunnel ngrok attivo per questa porta")?;
                // Secret revealed only by an explicit user command. No clipboard automation.
                println!("{}", tunnel::connector_url(&endpoint, &resolved.value)?);
                return Ok(());
            }
            Command::ConnectionTest { port, json } => {
                let token = token::resolve(&root)?.value;
                let result = connection::probe(port.unwrap_or(local.port), &token).await?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                } else {
                    println!("Authenticated NovaGPT MCP HTTP is ready on loopback");
                }
                return Ok(());
            }
            Command::Doctor { json } => {
                let registry = WorkspaceRegistry::new(&root, &local.allowed_roots)?;
                let current = registry.default_workspace();
                let report = serde_json::json!({
                    "workspace_id": current.workspace_id,
                    "root": current.root,
                    "allowed_extra_roots": local.allowed_roots.len(),
                    "vm_configured": local.vm_enabled || cli.vm_home.is_some(),
                    "browser_configured": local.browser_enabled ||
                        (cli.chatgpt_chrome.is_some() && cli.chatgpt_profile.is_some()),
                    "http_listen_address": "127.0.0.1",
                    "codexpro_installation_modified": false,
                    "verified_remote_mcp": false,
                    "feature_parity": "INITIAL",
                });
                if *json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    println!(
                        "NovaGPT doctor\nWorkspace: {}\nID: {}\nFeature parity: INITIAL\nVM configured: {}\nBrowser configured: {}",
                        current.root,
                        current.workspace_id,
                        report["vm_configured"],
                        report["browser_configured"]
                    );
                }
                return Ok(());
            }
            Command::Inspect { max_entries, json } => {
                let registry = WorkspaceRegistry::new(&root, &local.allowed_roots)?;
                let report = registry.inspect_workspace(None, *max_entries)?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    println!(
                        "Workspace: {}\nFiles: {}\nDirectories: {}\nEntries limited: {}\nTest files: {}",
                        report.root,
                        report.files,
                        report.directories,
                        report.truncated,
                        report.test_files
                    );
                }
                return Ok(());
            }
            Command::Review { json } => {
                let registry = WorkspaceRegistry::new(&root, &local.allowed_roots)?;
                let status = registry
                    .git_operation(novagpt_core::GitRequest {
                        action: "status".into(),
                        ..Default::default()
                    })
                    .await?;
                if *json {
                    println!("{}", serde_json::to_string_pretty(&status)?);
                } else {
                    println!("Git status:\n{}", status.stdout);
                }
                return Ok(());
            }
            _ => {}
        }
    }
    if cli.setup || cli.command.is_none() {
        let home = cli.codexpro_home.unwrap_or_else(settings::codexpro_home);
        let profiles = settings::discover_codexpro_profiles(&home)?;
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || tui::run(root, local, cli.setup, profiles, handle))
            .await??;
        return Ok(());
    }

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "novagpt=info".into());
    let diagnostics = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter);
    if cli.log_format == "llm" {
        diagnostics
            .json()
            .with_current_span(false)
            .with_span_list(false)
            .init();
    } else {
        diagnostics.init();
    }

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
    let bash_mode = match cli.bash_mode.as_str() {
        "off" => BashMode::Off,
        "safe" => BashMode::Safe,
        "full" if cli.allow_full_shell => BashMode::Full,
        "full" => return Err("full Bash access requires --allow-full-shell".into()),
        _ => return Err("invalid Bash mode".into()),
    };
    let browser_bridge = match (
        cli.browser_bridge_port,
        cli.browser_bridge_origin.as_deref(),
    ) {
        (Some(port), Some(origin)) => Some(BrowserBridge::new(port, origin)?),
        (None, None) => None,
        _ => {
            return Err(
                "Browser Bridge requires --browser-bridge-port and --browser-bridge-origin".into(),
            );
        }
    };
    let mut extras = NovaExtras {
        vm,
        agents,
        browser_bridge,
        output_store: novagpt_mcp::OutputStore::default(),
        bash_mode,
        audit: novagpt_mcp::AuditTrail::persistent(logging::audit_directory(&root)?)?,
        http_public_hostname: None,
    };
    match cli.command {
        Some(Command::Stdio) => novagpt_mcp::run_stdio_with(workspaces, extras).await?,
        Some(Command::Http {
            port,
            ngrok,
            no_ngrok,
            ngrok_domain,
        }) => {
            let port = port.unwrap_or(local.port);
            let ngrok = !no_ngrok && (ngrok || local.ngrok_enabled);
            let ngrok_domain = ngrok_domain.or_else(|| local.ngrok_domain.clone());
            let token = token::resolve(&root)?.value;
            let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
            if ngrok {
                // Own the loopback socket BEFORE starting a public tunnel.
                let audit = extras.audit.clone();
                let listener = match tokio::net::TcpListener::bind(address).await {
                    Ok(listener) => listener,
                    Err(_) => {
                        audit.record("http_bind_failed", true);
                        audit.flush().await;
                        return Err(
                            "Porta MCP occupata: impossibile avviare ngrok verso un altro processo"
                                .into(),
                        );
                    }
                };
                let mut tunnel = match tunnel::NgrokTunnel::start(port, ngrok_domain).await {
                    Ok(tunnel) => tunnel,
                    Err(_) => {
                        audit.record("http_tunnel_start_failed", true);
                        audit.flush().await;
                        return Err("Impossibile avviare ngrok".into());
                    }
                };
                let hostname = match tunnel.public_hostname().await {
                    Some(host) => host,
                    None => {
                        audit.record("http_tunnel_discovery_failed", true);
                        tunnel.stop().await;
                        audit.flush().await;
                        return Err(
                            "ngrok non ha confermato il dominio pubblico della porta MCP".into(),
                        );
                    }
                };
                // Trust only the hostname returned by the locally controlled ngrok
                // agent for the exact loopback port, not a wildcard or forwarded header.
                extras.http_public_hostname = Some(hostname);
                let server =
                    novagpt_mcp::run_http_on_listener_with(workspaces, listener, token, extras);
                tokio::pin!(server);
                let mut published = false;
                loop {
                    tokio::select! {
                        result = &mut server => {
                            tunnel.stop().await;
                            result?;
                            break;
                        }
                        result = tunnel.child.wait() => {
                            let _ = result?;
                            audit.record("http_tunnel_stopped", true);
                            audit.flush().await;
                            return Err("ngrok terminated; public MCP endpoint unavailable".into());
                        }
                        () = tokio::time::sleep(std::time::Duration::from_millis(500)) => {}
                    }
                    if !published && let Some(endpoint) = tunnel.endpoint().await {
                        audit.record("http_tunnel_ready", false);
                        // URL is from the local ngrok API and matched to the configured port.
                        // This does not establish that a real ChatGPT remote MCP client works.
                        eprintln!(
                            "ngrok MCP endpoint pronto: {endpoint} (usa connector-url --port {port} per mostrare l'URL riservato; ChatGPT non ancora verificato)"
                        );
                        published = true;
                    }
                }
            } else {
                novagpt_mcp::run_http_with(workspaces, address, token, extras).await?;
            }
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
        Some(Command::Token { .. } | Command::Logs { .. }) => {
            unreachable!("local commands handled above")
        }
        Some(
            Command::Doctor { .. }
            | Command::Inspect { .. }
            | Command::Review { .. }
            | Command::ConnectionTest { .. }
            | Command::ConnectorUrl { .. },
        ) => {
            unreachable!("read-only commands handled above")
        }
        None => unreachable!("TUI was handled before server startup"),
    }
    Ok(())
}
