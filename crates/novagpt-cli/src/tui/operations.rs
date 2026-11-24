//! Owns async runtime tasks and isolates blocking filesystem/VM actions from input.
use super::launcher::install_launcher;
use super::model::{BrowserOpenFailure, Effect, Level, OperationEvent, ServerState};
use crate::settings::{self, LocalSettings};
use crate::token;
use crate::{connection, tunnel};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{runtime::Handle, sync::mpsc::UnboundedSender, task::JoinHandle};

pub(super) struct Controller {
    runtime: Handle,
    tx: UnboundedSender<OperationEvent>,
    server: Option<JoinHandle<()>>,
    server_stop: Option<tokio::sync::watch::Sender<bool>>,
    browser_login: Option<JoinHandle<()>>,
    agent_browser: Arc<Mutex<Option<novagpt_browser::ChatGptBrowser>>>,
    installer: Option<JoinHandle<()>>,
}
impl Controller {
    pub fn new(runtime: Handle, tx: UnboundedSender<OperationEvent>) -> Self {
        Self {
            runtime,
            tx,
            server: None,
            server_stop: None,
            browser_login: None,
            agent_browser: Arc::new(Mutex::new(None)),
            installer: None,
        }
    }
    fn report(&self, level: Level, message: impl Into<String>) {
        let _ = self.tx.send(OperationEvent::Log(level, message.into()));
    }
    pub fn execute(&mut self, effect: Effect, root: PathBuf) -> bool {
        match effect {
            Effect::Quit => {
                self.stop();
                if let Some(task) = self.browser_login.take() {
                    task.abort();
                }
                if let Some(task) = self.installer.take() {
                    task.abort();
                }
                return false;
            }
            Effect::Start(settings) => self.start(root, settings, false),
            Effect::StartNgrok(settings) => self.start(root, settings, true),
            Effect::InstallNgrok => {
                if self
                    .installer
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
                {
                    return true;
                }
                let tx = self.tx.clone();
                self.installer = Some(self.runtime.spawn(async move {
                    let result = crate::ngrok_install::install().await.map_err(str::to_owned);
                    let _ = tx.send(OperationEvent::NgrokInstallFinished(result));
                }));
            }
            Effect::LoadUsage => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = crate::logging::audit_directory(&root)
                        .and_then(|dir| novagpt_mcp::UsageProfiler::persisted_report(&dir))
                        .map_err(|_| "Saved usage report unavailable".to_string());
                    let _ = tx.send(OperationEvent::UsageLoaded(result));
                });
            }
            Effect::LoadAudit => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = crate::logging::audit_directory(&root)
                        .and_then(|dir| novagpt_mcp::AuditTrail::persisted_events(&dir, 2_000))
                        .map_err(|_| "Saved audit history unavailable".to_string());
                    let _ = tx.send(OperationEvent::AuditLoaded(result));
                });
            }
            Effect::LoadSessions => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let sessions = crate::sessions::SessionStore::open_default()
                        .map(|store| store.load())
                        .unwrap_or_default();
                    let _ = tx.send(OperationEvent::SessionsLoaded(sessions));
                });
            }
            Effect::ReopenSession { url, settings } => {
                let tx = self.tx.clone();
                let slot = Arc::clone(&self.agent_browser);
                self.runtime.spawn(async move {
                    let result = async {
                        let browser = session_browser(&slot, &settings).await?;
                        browser.browse_open().await.map_err(|e| e.to_string())?;
                        browser
                            .browse_navigate(&url)
                            .await
                            .map_err(|e| e.to_string())?;
                        Ok(())
                    }
                    .await;
                    let _ = tx.send(OperationEvent::SessionReopened { result });
                });
            }
            Effect::ExportAudit => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = crate::logging::audit_directory(&root)
                        .and_then(|dir| {
                            let destination = crate::logging::support_bundle_destination()?;
                            novagpt_mcp::AuditTrail::export_compressed(&dir, &destination)?;
                            Ok(destination)
                        })
                        .map_err(|_| "Unable to export private audit metadata".to_string());
                    let _ = tx.send(OperationEvent::AuditExported(result));
                });
            }
            Effect::ShowConnectorUrl(port) => {
                let tx = self.tx.clone();
                self.runtime.spawn(async move {
                    let result: Result<String, String> = async {
                        let resolved = tokio::task::spawn_blocking(token::resolve)
                            .await
                            .map_err(|_| "HTTP token unavailable".to_string())?
                            .map_err(|_| "HTTP token unavailable".to_string())?;
                        connection::probe(port, &resolved.value)
                            .await
                            .map_err(|_| {
                                "MCP server unreachable or authentication failed".to_string()
                            })?;
                        let endpoint = tunnel::NgrokTunnel::discover_endpoint(port, None)
                            .await
                            .ok_or("ngrok tunnel not ready".to_string())?;
                        tunnel::connector_url(&endpoint, &resolved.value)
                            .map_err(|_| "Invalid tunnel URL".to_string())
                    }
                    .await;
                    let _ = tx.send(OperationEvent::ConnectorUrlReady(result));
                });
            }
            Effect::CopyConnectorUrl(url) => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = crate::clipboard::copy(&url);
                    let _ = tx.send(OperationEvent::CopyFinished(result));
                });
            }
            Effect::LoadNovaSource(source) => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = settings::discover_novagpt_workspace(&source).map_err(|_| {
                        "Unable to import folder: check the path and .novagpt/config.json."
                            .to_string()
                    });
                    let _ = tx.send(OperationEvent::NovaSourceLoaded(result));
                });
            }
            Effect::ShowSubagentChrome => {
                // Borrow only a cloned handle: no lock is held during Chrome I/O.
                let browser = self
                    .agent_browser
                    .lock()
                    .ok()
                    .and_then(|guard| guard.clone());
                let tx = self.tx.clone();
                self.runtime.spawn(async move {
                    let result = match browser {
                        Some(browser) => browser.show_subagent_chrome().await.map_err(|_| ()),
                        None => Err(()),
                    };
                    let _ = tx.send(OperationEvent::BrowserShowFinished(result));
                });
            }
            Effect::OpenAgentBrowser(config) => {
                if self
                    .browser_login
                    .as_ref()
                    .is_some_and(|task| !task.is_finished())
                {
                    self.report(Level::Warn, "Dedicated browser login is already running.");
                } else {
                    let tx = self.tx.clone();
                    self.browser_login = Some(self.runtime.spawn(async move {
                        // Chrome discovery checks the filesystem; keep it off Tokio workers.
                        let executable = tokio::task::spawn_blocking(move || {
                            crate::chrome::discover(config.chrome_executable.as_deref())
                        })
                        .await
                        .map_err(|_| BrowserOpenFailure::ChromeMissing)
                        .and_then(|result| result.map_err(|_| BrowserOpenFailure::ChromeMissing));
                        let result = match executable {
                            Ok(executable) => {
                                let profile = match settings::global_dir() {
                                    Ok(dir) => dir.join("browser"),
                                    Err(_) => {
                                        let _ = tx.send(OperationEvent::BrowserLoginFinished(Err(
                                            BrowserOpenFailure::ProfileInvalid,
                                        )));
                                        return;
                                    }
                                };
                                novagpt_browser::ChatGptBrowser::manual_login(executable, profile)
                                    .await
                                    .map_err(|error| match error {
                                        novagpt_browser::BrowserError::ProfileBusy => {
                                            BrowserOpenFailure::ProfileBusy
                                        }
                                        novagpt_browser::BrowserError::InvalidProfile => {
                                            BrowserOpenFailure::ProfileInvalid
                                        }
                                        _ => BrowserOpenFailure::LaunchFailed,
                                    })
                            }
                            Err(error) => Err(error),
                        };
                        let _ = tx.send(OperationEvent::BrowserLoginFinished(result));
                    }));
                }
            }
            Effect::Stop => self.stop(),
            Effect::Inventory => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = settings::global_dir()
                        .map_err(|e| e.to_string())
                        .and_then(|dir| {
                            novagpt_vm::VmManager::new(dir.join("vms")).map_err(|e| e.to_string())
                        })
                        .and_then(|vm| vm.images().map_err(|e| e.to_string()));
                    let message = OperationEvent::VmInventory(match result {
                        Ok(images) => Ok(images.image_count),
                        Err(_) => Err("Unable to list VM images; check the local store.".into()),
                    });
                    let _ = tx.send(message);
                });
            }
            Effect::Diagnose(config) => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let location = settings::config_path();
                    let result = novagpt_core::WorkspaceRegistry::new(&root, &config.allowed_roots);
                    let msg = format!(
                        "Diagnostics: workspace {}; settings {}; VM {}; browser {}.",
                        if result.is_ok() { "OK" } else { "INVALID" },
                        if location.as_ref().is_ok_and(|path| path.is_file()) {
                            "saved"
                        } else {
                            "not saved"
                        },
                        if config.vm_enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                        if config.browser_enabled {
                            "enabled"
                        } else {
                            "disabled"
                        },
                    );
                    let level = if result.is_ok() {
                        Level::Success
                    } else {
                        Level::Error
                    };
                    let _ = tx.send(OperationEvent::Log(level, msg));
                });
            }
            Effect::ValidateChrome(path) => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let valid = path.is_absolute() && path.is_file();
                    let _ = tx.send(OperationEvent::ChromeChecked(valid));
                });
            }
            Effect::AddRoot(candidate) => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = std::fs::canonicalize(candidate)
                        .and_then(|p| {
                            if p.is_dir() {
                                Ok(p)
                            } else {
                                Err(std::io::Error::new(
                                    std::io::ErrorKind::InvalidInput,
                                    "not a directory",
                                ))
                            }
                        })
                        .map_err(|_| "Invalid or inaccessible directory.".to_string());
                    let _ = tx.send(OperationEvent::RootAdded(result));
                });
            }
            Effect::EnsureToken => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = token::resolve()
                        .map(|value| value.description())
                        .map_err(|err| format!("Unable to prepare HTTP token: {err}"));
                    let _ = tx.send(OperationEvent::TokenReady(result));
                });
            }
            Effect::Save(settings, setup) => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let outcome = match settings.save() {
                        Ok(path) => {
                            let mut report = format!("Settings saved: {}.", path.display());
                            if setup {
                                match install_launcher(&root) {
                                    Ok(message) => report.push_str(&format!(" {message}")),
                                    Err(_) => report.push_str(" WARNING: launcher not installed."),
                                }
                                match token::resolve() {
                                    Ok(resolved) => {
                                        let message = resolved.description();
                                        let _ =
                                            tx.send(OperationEvent::TokenPresent(message.clone()));
                                        report.push_str(&format!(" {message}"));
                                    }
                                    Err(err) => report.push_str(&format!(
                                        " WARNING: HTTP token could not be created: {err}"
                                    )),
                                }
                            }
                            Ok(report)
                        }
                        Err(err) => Err(format!("Settings save rejected: {err}")),
                    };
                    let _ = tx.send(OperationEvent::Saved { settings, outcome });
                });
            }
        }
        true
    }
    fn start(&mut self, root: PathBuf, config: LocalSettings, ngrok: bool) {
        if self.server.as_ref().is_some_and(|h| !h.is_finished()) {
            self.report(Level::Warn, "An HTTP server is already running.");
            return;
        }
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        self.server_stop = Some(stop_tx.clone());
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.port);
        let tx = self.tx.clone();
        let agent_browser = Arc::clone(&self.agent_browser);
        let session_runtime = self.runtime.clone();
        // A browser already opened for session history owns the dedicated profile.
        // The server must reuse that same handle instead of launching a second
        // Chrome on the same profile, which would fail with ProfileBusy.
        let existing_browser = self.agent_browser.lock().ok().and_then(|slot| slot.clone());
        let task = self.runtime.spawn(async move {
            if ngrok && !crate::ngrok_install::available().await {
                let _ = tx.send(OperationEvent::NgrokMissing);
                return;
            }
            let tx_info = tx.clone();
            let ngrok_domain = config.ngrok_domain.clone();
            let setup = tokio::task::spawn_blocking(move || -> Result<_, String> {
                config
                    .validate()
                    .map_err(|_| "Invalid configuration.".to_string())?;
                let workspaces = novagpt_core::WorkspaceRegistry::new(&root, &config.allowed_roots)
                    .map_err(|_| "Invalid workspace.".to_string())?;
                let vm = if config.vm_enabled {
                    Some(
                        novagpt_vm::VmManager::new(
                            config.vm_home.clone().unwrap_or(
                                settings::global_dir()
                                    .map_err(|_| "Global VM directory unavailable".to_string())?
                                    .join("vms"),
                            ),
                        )
                        .map_err(|_| "Unable to initialize VM.".to_string())?,
                    )
                } else {
                    None
                };
                let browser = if config.browser_enabled {
                    let browser = match existing_browser {
                        // Reuse the session-history browser and the same profile.
                        Some(browser) => browser,
                        None => {
                            let chrome =
                                crate::chrome::discover(config.chrome_executable.as_deref())
                                    .map_err(|_| "Chrome is unavailable.")?;
                            let browser_tx = tx_info.clone();
                            novagpt_browser::ChatGptBrowser::new(
                                chrome,
                                settings::global_dir()
                                    .map_err(|_| {
                                        "Global browser directory unavailable".to_string()
                                    })?
                                    .join("browser"),
                            )
                            .map_err(|_| "Unable to initialize browser.".to_string())?
                            .with_diagnostic_reporter(move |event| {
                                let _ = browser_tx.send(OperationEvent::Log(
                                    Level::Info,
                                    format!("Browser: {event}"),
                                ));
                            })
                        }
                    };
                    browser
                        .set_project_url(config.chatgpt_project_url.as_deref())
                        .map_err(|_| "Invalid ChatGPT project URL.".to_string())?;
                    Some(browser)
                } else {
                    None
                };
                let audit = novagpt_mcp::AuditTrail::persistent(
                    crate::logging::audit_directory(&root)
                        .map_err(|_| "Invalid audit directory.".to_string())?,
                )
                .map_err(|_| "Audit storage unavailable or unsafe.".to_string())?;
                let usage = if config.usage_profiler_enabled {
                    novagpt_mcp::UsageProfiler::persistent(
                        crate::logging::audit_directory(&root)
                            .map_err(|_| "Invalid usage directory.".to_string())?,
                    ).map_err(|_| "Usage storage unavailable or unsafe.".to_string())?
                } else {
                    novagpt_mcp::UsageProfiler::disabled()
                };
                let agents = browser.as_ref().map(|browser| -> Result<_, String> {
                    let agent_audit = audit.clone();
                    let agent_usage = usage.clone();
                    let runtime = session_runtime.clone();
                    Ok(novagpt_agents::AgentManager::new(
                        workspaces.clone(),
                        Arc::new(novagpt_agents::ChatGptBackend::new(browser.clone())),
                    ).with_history(crate::logging::audit_directory(&root).map_err(|_| "Invalid audit directory.")?.join("agent-history.json")).map_err(|_| "Agent history unavailable or locked.")?.with_diagnostic_reporter(move |id, stage| {
                        agent_audit.record_agent(stage.audit_name(), stage.is_failure(), id);
                    })
                    .with_usage_reporter(move |id, repository_id, input_chars, output_chars, failed| {
                        agent_usage.observe_model_turn(id, repository_id, input_chars, output_chars, failed);
                    })
                    .with_conversation_recorder(move |id, url| {
                        // Owner-private index only: prompts and replies are never stored.
                        let id = id.to_owned();
                        let url = url.to_owned();
                        runtime.spawn_blocking(move || {
                            if let Ok(store) = crate::sessions::SessionStore::open_default() {
                                let _ = store.record(
                                    crate::sessions::SessionKind::Subagent,
                                    &id,
                                    &url,
                                    None,
                                );
                            }
                        });
                    }))
                }).transpose()?;
                let resolved = token::resolve()
                    .map_err(|err| format!("HTTP token unavailable: {err}"))?;
                let _ = tx_info.send(OperationEvent::TokenPresent(resolved.description()));
                Ok((
                    workspaces,
                    novagpt_mcp::NovaExtras {
                        vm,
                        agents,
                        browser,
                        browser_bridge: None,
                        output_store: novagpt_mcp::OutputStore::default(),
                        http_public_hostname: None,
                        audit,
                        usage,
                    },
                    resolved.value,
                ))
            })
            .await;
            match setup {
                Ok(Ok((workspaces, mut extras, token))) => {
                    if *stop_rx.borrow() {
                        let _ = tx.send(OperationEvent::Server(ServerState::Stopped));
                        return;
                    }
                    if let Ok(mut handle) = agent_browser.lock() {
                        *handle = extras.browser.clone();
                    }
                    let tx_check = tx.clone();
                    let probe_token = token.clone();
                    let probe = tokio::spawn(async move {
                        // An unrelated listener must never mark NovaGPT ready.
                        for _ in 0..120 {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            if connection::probe(address.port(), &probe_token).await.is_ok() {
                                let _ = tx_check.send(OperationEvent::Server(ServerState::Running));
                                let _ = tx_check.send(OperationEvent::Log(
                                    Level::Success,
                                    "Authenticated NovaGPT MCP server ready on loopback.".into(),
                                ));
                                break;
                            }
                        }
                    });
                    let result: Result<(), Box<dyn std::error::Error>> = if ngrok {
                        let audit = extras.audit.clone();
                        match tokio::net::TcpListener::bind(address).await {
                            Ok(listener) => {
                                // Do not start a public tunnel unless NovaGPT owns this port.
                                match tunnel::NgrokTunnel::start(address.port(), ngrok_domain).await {
                                    Ok(mut tunnel) => {
                                        match tunnel.public_hostname().await {
                                            Ok(hostname) => {
                                            // Same validated public host used by the tunnel; never disable host validation.
                                            extras.http_public_hostname = Some(hostname);
                                            let server = async {
                                                novagpt_mcp::run_http_on_listener_with_shutdown(
                                                    workspaces, listener, token, extras, wait_for_stop(stop_rx.clone())
                                                ).await.is_ok()
                                            };
                                            tokio::pin!(server);
                                            let mut published = false;
                                            loop {
                                                tokio::select! {
                                                    output = &mut server => {
                                                        tunnel.stop().await;
                                                        break if output { Ok(()) } else { Err("MCP server stopped".into()) };
                                                    }
                                                    output = tunnel.child.wait() => {
                                                        let _ = output;
                                                        audit.record("http_tunnel_stopped", true);
                                                        let _ = stop_tx.send(true);
                                                        let _ = (&mut server).await;
                                                        break Err("The ngrok tunnel closed".into());
                                                    }
                                                    () = tokio::time::sleep(Duration::from_millis(450)) => {
                                                        if !published && tunnel.endpoint().await.is_some() {
                                                            audit.record("http_tunnel_ready", false);
                                                            published = true;
                                                            let _ = tx.send(OperationEvent::TunnelReady);
                                                            let _ = tx.send(OperationEvent::Log(Level::Success,
                                                                "Tunnel ngrok pronto. Seleziona 'Mostra / copia URL ChatGPT'.".into()));
                                                        }
                                                    }
                                                }
                                            }
                                            }
                                            Err(reason) => {
                                                audit.record(reason.audit_event(), true);
                                                tunnel.stop().await;
                                                audit.flush().await;
                                                Err(reason.message().into())
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        audit.record("http_tunnel_start_failed", true);
                                        audit.flush().await;
                                        Err("ngrok unavailable or account configuration invalid".into())
                                    }
                                }
                            }
                            Err(_) => {
                                audit.record("http_bind_failed", true);
                                audit.flush().await;
                                Err("HTTP port occupied: refused to expose another server through ngrok".into())
                            }
                        }
                    } else {
                        match tokio::net::TcpListener::bind(address).await {
                            Ok(listener) => novagpt_mcp::run_http_on_listener_with_shutdown(workspaces, listener, token, extras, wait_for_stop(stop_rx.clone())).await,
                            Err(error) => {
                                extras.audit.record("http_bind_failed", true);
                                extras.audit.flush().await;
                                Err(error.into())
                            }
                        }
                    };
                    probe.abort();
                    if let Ok(mut handle) = agent_browser.lock() {
                        *handle = None;
                    }
                    if let Err(error) = result {
                        let _ = tx.send(OperationEvent::Server(ServerState::Failed));
                        // Only the ngrok startup branch exposes this diagnostic:
                        // its errors are fixed, credential-free messages.
                        let message = if ngrok {
                            format!("HTTP tunnel stopped: {error}")
                        } else {
                            "HTTP stopped: check port, token and settings.".to_string()
                        };
                        let _ = tx.send(OperationEvent::Log(Level::Error, message));
                    } else {
                        let _ = tx.send(OperationEvent::Server(ServerState::Stopped));
                        let _ = tx.send(OperationEvent::Log(
                            Level::Info,
                            "HTTP server stopped.".into(),
                        ));
                    }
                }
                Ok(Err(error)) => {
                    let _ = tx.send(OperationEvent::Server(ServerState::Failed));
                    let _ = tx.send(OperationEvent::Log(Level::Error, error));
                }
                _ => {
                    let _ = tx.send(OperationEvent::Server(ServerState::Failed));
                    let _ = tx.send(OperationEvent::Log(
                        Level::Error,
                        "Startup failed: settings or subsystems unavailable.".into(),
                    ));
                }
            }
        });
        self.server = Some(task);
    }
    fn stop(&mut self) {
        if let Some(stop) = self.server_stop.as_ref() {
            let _ = stop.send(true);
            self.report(Level::Info, "HTTP graceful shutdown requested.");
        } else {
            let _ = self.tx.send(OperationEvent::Server(ServerState::Stopped));
        }
    }

    /// Called from the blocking TUI thread, never from the Tokio executor.
    pub fn finish(&mut self) -> std::io::Result<()> {
        self.stop();
        if let Some(mut task) = self.server.take() {
            self.runtime.block_on(async {
                if tokio::time::timeout(Duration::from_secs(40), &mut task)
                    .await
                    .is_err()
                {
                    task.abort();
                    let _ = task.await;
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "TUI server shutdown timed out",
                    ));
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}

async fn wait_for_stop(mut stop: tokio::sync::watch::Receiver<bool>) {
    while !*stop.borrow_and_update() {
        if stop.changed().await.is_err() {
            break;
        }
    }
}

/// Reuse the server-owned browser when one exists (the same dedicated Chrome
/// and profile), otherwise create one lazily for session history.
async fn session_browser(
    slot: &Arc<Mutex<Option<novagpt_browser::ChatGptBrowser>>>,
    settings: &LocalSettings,
) -> Result<novagpt_browser::ChatGptBrowser, String> {
    if let Some(existing) = slot.lock().ok().and_then(|guard| guard.clone()) {
        return Ok(existing);
    }
    let override_path = settings.chrome_executable.clone();
    let executable =
        tokio::task::spawn_blocking(move || crate::chrome::discover(override_path.as_deref()))
            .await
            .map_err(|_| "Chrome discovery failed".to_string())?
            .map_err(|_| "Chrome is unavailable".to_string())?;
    let profile = settings::global_dir()
        .map_err(|_| "Global browser directory unavailable".to_string())?
        .join("browser");
    let browser = novagpt_browser::ChatGptBrowser::new(executable, profile)
        .map_err(|_| "Unable to initialize browser".to_string())?;
    browser
        .set_project_url(settings.chatgpt_project_url.as_deref())
        .map_err(|_| "Invalid ChatGPT project URL".to_string())?;
    if let Ok(mut handle) = slot.lock() {
        *handle = Some(browser.clone());
    }
    Ok(browser)
}

impl Drop for Controller {
    fn drop(&mut self) {
        if let Ok(mut handle) = self.agent_browser.lock() {
            *handle = None;
        }
        if let Some(task) = self.server.take() {
            task.abort();
        }
        if let Some(task) = self.browser_login.take() {
            task.abort();
        }
        if let Some(task) = self.installer.take() {
            task.abort();
        }
    }
}
