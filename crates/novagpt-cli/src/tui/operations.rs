//! Owns async runtime tasks and isolates blocking filesystem/VM actions from input.
use super::launcher::install_launcher;
use super::model::{Effect, Level, OperationEvent, ServerState};
use crate::settings::{self, LocalSettings};
use crate::token;
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
}
impl Controller {
    pub fn new(runtime: Handle, tx: UnboundedSender<OperationEvent>) -> Self {
        Self {
            runtime,
            tx,
            server: None,
        }
    }
    fn report(&self, level: Level, message: impl Into<String>) {
        let _ = self.tx.send(OperationEvent::Log(level, message.into()));
    }
    pub fn execute(&mut self, effect: Effect, root: PathBuf) -> bool {
        match effect {
            Effect::Quit => {
                self.stop();
                return false;
            }
            Effect::Start(settings) => self.start(root, settings),
            Effect::Stop => self.stop(),
            Effect::Inventory => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = novagpt_vm::VmManager::new(settings::local_dir(&root).join("vms"))
                        .and_then(|vm| vm.images());
                    let message = OperationEvent::VmInventory(match result {
                        Ok(images) => Ok(images.image_count),
                        Err(_) => Err(
                            "Impossibile leggere inventario VM; controlla lo store locale.".into(),
                        ),
                    });
                    let _ = tx.send(message);
                });
            }
            Effect::Diagnose(config) => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let location = settings::config_path(&root);
                    let result = novagpt_core::WorkspaceRegistry::new(&root, &config.allowed_roots);
                    let msg = format!(
                        "Diagnostica: workspace {}; config {}; VM {}; browser {}.",
                        if result.is_ok() { "OK" } else { "NON VALIDO" },
                        if location.is_file() {
                            "salvata"
                        } else {
                            "non salvata"
                        },
                        if config.vm_enabled {
                            "abilitata"
                        } else {
                            "disabilitata"
                        },
                        if config.browser_enabled {
                            "abilitato"
                        } else {
                            "disabilitato"
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
                        .map_err(|_| "Directory non valida o non accessibile.".to_string());
                    let _ = tx.send(OperationEvent::RootAdded(result));
                });
            }
            Effect::EnsureToken => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let result = token::resolve(&root)
                        .map(|value| value.description())
                        .map_err(|err| format!("Impossibile preparare il token HTTP: {err}"));
                    let _ = tx.send(OperationEvent::TokenReady(result));
                });
            }
            Effect::Save(settings, setup) => {
                let tx = self.tx.clone();
                self.runtime.spawn_blocking(move || {
                    let outcome = match settings.save(&root) {
                        Ok(path) => {
                            let mut report = format!("Configurazione salvata: {}.", path.display());
                            if setup {
                                match install_launcher(&root) {
                                    Ok(message) => report.push_str(&format!(" {message}")),
                                    Err(_) => {
                                        report.push_str(" ATTENZIONE: launcher non installato.")
                                    }
                                }
                                match token::resolve(&root) {
                                    Ok(resolved) => {
                                        let message = resolved.description();
                                        let _ =
                                            tx.send(OperationEvent::TokenPresent(message.clone()));
                                        report.push_str(&format!(" {message}"));
                                    }
                                    Err(err) => report.push_str(&format!(
                                        " ATTENZIONE: token HTTP non creato: {err}"
                                    )),
                                }
                            }
                            Ok(report)
                        }
                        Err(err) => Err(format!("Salvataggio rifiutato: {err}")),
                    };
                    let _ = tx.send(OperationEvent::Saved { settings, outcome });
                });
            }
        }
        true
    }
    fn start(&mut self, root: PathBuf, config: LocalSettings) {
        if self.server.as_ref().is_some_and(|h| !h.is_finished()) {
            self.report(Level::Warn, "Un server HTTP e' gia avviato.");
            return;
        }
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.port);
        let tx = self.tx.clone();
        let task = self.runtime.spawn(async move {
            let tx_info = tx.clone();
            let setup = tokio::task::spawn_blocking(move || -> Result<_, String> {
                config
                    .validate()
                    .map_err(|_| "Configurazione non valida.".to_string())?;
                let workspaces = novagpt_core::WorkspaceRegistry::new(&root, &config.allowed_roots)
                    .map_err(|_| "Workspace non valido.".to_string())?;
                let vm = if config.vm_enabled {
                    Some(
                        novagpt_vm::VmManager::new(settings::local_dir(&root).join("vms"))
                            .map_err(|_| "Impossibile inizializzare VM.".to_string())?,
                    )
                } else {
                    None
                };
                let agents = if config.browser_enabled {
                    let chrome = config.chrome_executable.ok_or("Chrome non configurato.")?;
                    let browser = novagpt_browser::ChatGptBrowser::new(
                        chrome,
                        settings::local_dir(&root).join("browser"),
                    )
                    .map_err(|_| "Browser non inizializzabile.".to_string())?;
                    Some(novagpt_agents::AgentManager::new(
                        workspaces.clone(),
                        Arc::new(novagpt_agents::ChatGptBackend::new(browser)),
                    ))
                } else {
                    None
                };
                let resolved = token::resolve(&root)
                    .map_err(|err| format!("Token HTTP non disponibile: {err}"))?;
                let _ = tx_info.send(OperationEvent::TokenPresent(resolved.description()));
                Ok((
                    workspaces,
                    novagpt_mcp::NovaExtras {
                        vm,
                        agents,
                        browser_bridge: None,
                        output_store: novagpt_mcp::OutputStore::default(),
                        bash_mode: novagpt_core::BashMode::Safe,
                        audit: novagpt_mcp::AuditTrail::persistent(
                            crate::logging::audit_directory(&root)
                                .map_err(|_| "Percorso audit non valido.".to_string())?,
                        )
                        .map_err(|_| "Archivio audit non disponibile o non sicuro.".to_string())?,
                    },
                    resolved.value,
                ))
            })
            .await;
            match setup {
                Ok(Ok((workspaces, extras, token))) => {
                    let tx_check = tx.clone();
                    let probe = tokio::spawn(async move {
                        // Indicates only that the configured TCP endpoint answers, not authenticated MCP readiness.
                        for _ in 0..25 {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                            if tokio::net::TcpStream::connect(address).await.is_ok() {
                                let _ = tx_check.send(OperationEvent::Server(ServerState::Running));
                                let _ = tx_check.send(OperationEvent::Log(
                                    Level::Success,
                                    format!(
                                        "Porta TCP {} raggiungibile; MCP su http://{address}/mcp",
                                        address.port()
                                    ),
                                ));
                                break;
                            }
                        }
                    });
                    let result =
                        novagpt_mcp::run_http_with(workspaces, address, token, extras).await;
                    probe.abort();
                    if result.is_err() {
                        let _ = tx.send(OperationEvent::Server(ServerState::Failed));
                        let _ = tx.send(OperationEvent::Log(
                            Level::Error,
                            "HTTP terminato: verifica porta occupata, token e configurazione."
                                .into(),
                        ));
                    } else {
                        let _ = tx.send(OperationEvent::Server(ServerState::Stopped));
                        let _ = tx.send(OperationEvent::Log(
                            Level::Info,
                            "Server HTTP terminato.".into(),
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
                        "Avvio fallito: configurazione o sottosistemi non disponibili.".into(),
                    ));
                }
            }
        });
        self.server = Some(task);
    }
    fn stop(&mut self) {
        if let Some(task) = self.server.take() {
            task.abort();
            self.report(Level::Info, "Arresto HTTP richiesto (task cancellato).");
        }
        let _ = self.tx.send(OperationEvent::Server(ServerState::Stopped));
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        if let Some(task) = self.server.take() {
            task.abort();
        }
    }
}
