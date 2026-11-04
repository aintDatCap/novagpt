//! Owns async runtime tasks and isolates blocking filesystem/VM actions from input.
use super::launcher::install_launcher;
use super::model::{BrowserOpenFailure, Effect, Level, OperationEvent, ServerState};
use crate::settings::{self, LocalSettings};
use crate::token;
use crate::{connection, tunnel};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{runtime::Handle, sync::mpsc::UnboundedSender, task::JoinHandle};

pub(super) struct Controller {
    runtime: Handle,
    tx: UnboundedSender<OperationEvent>,
    server: Option<JoinHandle<()>>,
    browser_login: Option<JoinHandle<()>>,
}
impl Controller {
    pub fn new(runtime: Handle, tx: UnboundedSender<OperationEvent>) -> Self {
        Self {
            runtime,
            tx,
            server: None,
            browser_login: None,
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
                return false;
            }
            Effect::Start(settings) => self.start(root, settings, false),
            Effect::StartNgrok(settings) => self.start(root, settings, true),
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
                    use std::{
                        io::Write,
                        process::{Command, Stdio},
                    };
                    let program = if cfg!(windows) {
                        "clip.exe"
                    } else if cfg!(target_os = "macos") {
                        "pbcopy"
                    } else {
                        "wl-copy"
                    };
                    let result = (|| -> Result<(), String> {
                        let mut process = Command::new(program)
                            .stdin(Stdio::piped())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()
                            .map_err(|_| {
                                "Clipboard unavailable: select the URL manually".to_string()
                            })?;
                        process
                            .stdin
                            .take()
                            .ok_or("Clipboard standard input unavailable".to_string())?
                            .write_all(url.as_bytes())
                            .map_err(|_| "Clipboard write failed".to_string())?;
                        if !process
                            .wait()
                            .map_err(|_| "Clipboard operation failed".to_string())?
                            .success()
                        {
                            return Err("Clipboard operation failed".into());
                        }
                        Ok(())
                    })();
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
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.port);
        let tx = self.tx.clone();
        let task = self.runtime.spawn(async move {
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
                            settings::global_dir()
                                .map_err(|_| "Global VM directory unavailable".to_string())?
                                .join("vms"),
                        )
                        .map_err(|_| "Unable to initialize VM.".to_string())?,
                    )
                } else {
                    None
                };
                let browser = if config.browser_enabled {
                    let chrome = crate::chrome::discover(config.chrome_executable.as_deref())
                        .map_err(|_| "Chrome is unavailable.")?;
                    let browser_tx = tx_info.clone();
                    Some(
                        novagpt_browser::ChatGptBrowser::new(
                            chrome,
                            settings::global_dir()
                                .map_err(|_| "Global browser directory unavailable".to_string())?
                                .join("browser"),
                        )
                        .map_err(|_| "Unable to initialize browser.".to_string())?
                        .with_diagnostic_reporter(move |event| {
                            let _ = browser_tx.send(OperationEvent::Log(
                                Level::Info,
                                format!("Browser: {event}"),
                            ));
                        }),
                    )
                } else {
                    None
                };
                let agents = browser.as_ref().map(|browser| {
                    novagpt_agents::AgentManager::new(
                        workspaces.clone(),
                        Arc::new(novagpt_agents::ChatGptBackend::new(browser.clone())),
                    )
                });
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
                        audit: novagpt_mcp::AuditTrail::persistent(
                            crate::logging::audit_directory(&root)
                                .map_err(|_| "Invalid audit directory.".to_string())?,
                        )
                        .map_err(|_| "Audit storage unavailable or unsafe.".to_string())?,
                    },
                    resolved.value,
                ))
            })
            .await;
            match setup {
                Ok(Ok((workspaces, mut extras, token))) => {
                    let tx_check = tx.clone();
                    let probe_token = token.clone();
                    let probe = tokio::spawn(async move {
                        // An unrelated listener (e.g. CodexPro) must never mark NovaGPT ready.
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
                                        if let Some(hostname) = tunnel.public_hostname().await {
                                            // Same validated public host used by the tunnel; never disable host validation.
                                            extras.http_public_hostname = Some(hostname);
                                            let server = async {
                                                novagpt_mcp::run_http_on_listener_with(
                                                    workspaces, listener, token, extras
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
                                        } else {
                                            audit.record("http_tunnel_discovery_failed", true);
                                            tunnel.stop().await;
                                            audit.flush().await;
                                            Err("The ngrok hostname for the MCP port could not be verified".into())
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
                        novagpt_mcp::run_http_with(workspaces, address, token, extras).await
                    };
                    probe.abort();
                    if result.is_err() {
                        let _ = tx.send(OperationEvent::Server(ServerState::Failed));
                        let _ = tx.send(OperationEvent::Log(
                            Level::Error,
                            "HTTP stopped: check port, token and settings."
                                .into(),
                        ));
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
        if let Some(task) = self.server.take() {
            task.abort();
            self.report(Level::Info, "HTTP stop requested (task cancelled).");
        }
        let _ = self.tx.send(OperationEvent::Server(ServerState::Stopped));
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(task) = self.server.take() {
            task.abort();
        }
        if let Some(task) = self.browser_login.take() {
            task.abort();
        }
    }
}
