//! UI state and pure keyboard transitions. Effects are executed by operations.rs.
use crate::settings::{CodexProfile, LocalSettings};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) const COMMANDS: [(&str, &str); 9] = [
    (
        "Avvia MCP HTTP",
        "Avvia il server autenticato su 127.0.0.1 (token da ambiente).",
    ),
    (
        "Arresta MCP HTTP",
        "Interrompe il server avviato da questa sessione TUI.",
    ),
    (
        "Diagnostica workspace",
        "Controlla configurazione, directory e servizi locali.",
    ),
    (
        "Inventario VM",
        "Elenca le immagini approvate nello store NovaGPT locale.",
    ),
    (
        "Operazioni VM",
        "Create/status/destroy via tool MCP vm; importazione via CLI esplicita.",
    ),
    (
        "Sottoagenti",
        "Spawn/status/message/result/cancel/resume via MCP, se browser attivo.",
    ),
    (
        "Client e CLI",
        "Accesso stdio, HTTP esterno e comando vm-import.",
    ),
    (
        "Avvia MCP + ngrok",
        "Avvia il server e il tunnel pubblico. Non copia l'URL negli appunti.",
    ),
    (
        "Mostra / copia URL ChatGPT",
        "Mostra l'URL riservato solo su richiesta; premi C per copiarlo.",
    ),
];
pub(super) const CONFIG_ITEMS: [&str; 14] = [
    "Porta HTTP",
    "Abilita VM",
    "Abilita ChatGPT Browser",
    "Eseguibile Chrome",
    "Aggiungi directory autorizzata",
    "Rimuovi directory autorizzata",
    "Scegli profilo CodexPro",
    "Importa porta + ngrok (anteprima)",
    "Salva configurazione",
    "Annulla modifiche",
    "Genera / verifica token HTTP",
    "Ngrok automatico (ON/OFF)",
    "Dominio ngrok",
    "Prossimo passo setup",
];
const LOG_CAP: usize = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Level {
    Info,
    Warn,
    Error,
    Success,
}
#[derive(Clone, Debug)]
pub(super) struct LogEntry {
    pub time: String,
    pub level: Level,
    pub message: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ServerState {
    Stopped,
    Starting,
    Running,
    Failed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Field {
    Port,
    Chrome,
    Root,
    NgrokDomain,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Confirm {
    Save,
    Import,
    RemoveRoot,
    Discard,
    Stop,
    Quit,
}
#[derive(Clone, Debug)]
pub(super) enum Modal {
    Help,
    Notice { title: String, body: String },
    Confirm(Confirm),
    Edit(Field),
    Roots,
    ImportPreview,
    SetupResult(String),
    ConnectorUrl(String),
}
#[derive(Clone, Debug)]
pub(super) enum Effect {
    Start(LocalSettings),
    StartNgrok(LocalSettings),
    ShowConnectorUrl(u16),
    CopyConnectorUrl(String),
    Stop,
    Diagnose(LocalSettings),
    Inventory,
    AddRoot(PathBuf),
    ValidateChrome(PathBuf),
    Save(LocalSettings, bool),
    EnsureToken,
    Quit,
}
#[derive(Debug)]
pub(super) enum OperationEvent {
    Log(Level, String),
    Server(ServerState),
    RootAdded(Result<PathBuf, String>),
    ChromeChecked(bool),
    VmInventory(Result<usize, String>),
    TokenReady(Result<String, String>),
    TokenPresent(String),
    ConnectorUrlReady(Result<String, String>),
    CopyFinished(Result<(), String>),
    Saved {
        settings: LocalSettings,
        outcome: Result<String, String>,
    },
}
pub(super) struct App {
    pub root: PathBuf,
    pub draft: LocalSettings,
    pub persisted: LocalSettings,
    pub profiles: Vec<CodexProfile>,
    pub setup: bool,
    pub step: usize,
    pub tab: usize,
    pub command: usize,
    pub config: usize,
    pub profile: usize,
    pub root_selection: usize,
    pub server: ServerState,
    pub vm_images: Option<usize>,
    pub running_config: Option<LocalSettings>,
    pub modal: Option<Modal>,
    pub input: String,
    pub input_error: Option<String>,
    pub chrome_valid: Option<bool>,
    pub auth_state: String,
    pub yes: bool,
    pub busy: bool,
    pub logs: VecDeque<LogEntry>,
    pub log_offset: usize,
    pub status: String,
}
impl App {
    pub fn new(
        root: PathBuf,
        settings: LocalSettings,
        setup: bool,
        profiles: Vec<CodexProfile>,
    ) -> Self {
        let mut app = Self {
            root,
            draft: settings.clone(),
            persisted: settings.clone(),
            profiles,
            setup,
            step: if setup { 0 } else { 4 },
            tab: if setup { 2 } else { 0 },
            command: 0,
            config: 0,
            profile: 0,
            root_selection: 0,
            server: ServerState::Stopped,
            vm_images: None,
            running_config: None,
            modal: None,
            input: String::new(),
            input_error: None,
            chrome_valid: Some(settings.chrome_executable.is_some()),
            auth_state: "Non verificato".to_string(),
            yes: false,
            busy: false,
            logs: VecDeque::new(),
            log_offset: 0,
            status: "Pronto".into(),
        };
        app.log(Level::Info, "NovaGPT avviato. ? apre l'aiuto.");
        app
    }
    pub fn dirty(&self) -> bool {
        self.draft != self.persisted
    }
    pub fn restart_needed(&self) -> bool {
        self.server == ServerState::Running
            && self
                .running_config
                .as_ref()
                .is_some_and(|c| *c != self.persisted)
    }
    pub fn config_len(&self) -> usize {
        if self.setup { 14 } else { 13 }
    }
    pub fn log(&mut self, level: Level, message: impl Into<String>) {
        let message = message.into();
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            % 86400;
        let time = format!(
            "{:02}:{:02}:{:02} UTC",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        );
        if self.log_offset > 0 {
            self.log_offset = self.log_offset.saturating_add(1);
        }
        self.logs.push_back(LogEntry {
            time,
            level,
            message: message.clone(),
        });
        if self.logs.len() > LOG_CAP {
            self.logs.pop_front();
        }
        self.log_offset = self.log_offset.min(self.logs.len().saturating_sub(1));
        self.status = message;
    }
    pub fn on_event(&mut self, event: OperationEvent) {
        match event {
            OperationEvent::Log(level, text) => self.log(level, text),
            OperationEvent::Server(s) => {
                self.server = s;
                if matches!(s, ServerState::Stopped | ServerState::Failed) {
                    self.running_config = None;
                }
            }
            OperationEvent::VmInventory(result) => {
                self.busy = false;
                match result {
                    Ok(count) => {
                        self.vm_images = Some(count);
                        self.log(
                            Level::Success,
                            format!("Inventario VM: {count} immagini approvate."),
                        );
                    }
                    Err(message) => self.log(Level::Error, message),
                }
            }
            OperationEvent::ConnectorUrlReady(result) => {
                self.busy = false;
                match result {
                    Ok(url) => {
                        self.modal = Some(Modal::ConnectorUrl(url));
                        self.log(
                            Level::Info,
                            "URL ChatGPT visualizzato solo in finestra riservata.",
                        );
                    }
                    Err(message) => self.log(Level::Warn, message),
                }
            }
            OperationEvent::CopyFinished(result) => match result {
                Ok(()) => self.log(Level::Success, "URL copiato negli appunti su richiesta."),
                Err(message) => self.log(Level::Error, message),
            },
            OperationEvent::TokenPresent(message) => {
                self.auth_state = "Pronto".to_string();
                self.log(Level::Info, message);
            }
            OperationEvent::TokenReady(result) => {
                self.busy = false;
                match result {
                    Ok(message) => {
                        self.auth_state = "Pronto".to_string();
                        self.log(Level::Success, message);
                    }
                    Err(message) => {
                        self.auth_state = "ERRORE".to_string();
                        self.log(Level::Error, message);
                    }
                }
            }
            OperationEvent::ChromeChecked(valid) => {
                self.chrome_valid = Some(valid);
                if !valid {
                    self.log(
                        Level::Error,
                        "Eseguibile Chrome non valido o non accessibile.",
                    );
                }
            }
            OperationEvent::RootAdded(result) => {
                self.busy = false;
                match result {
                    Ok(root) => {
                        if root == self.root || self.draft.allowed_roots.contains(&root) {
                            self.log(Level::Warn, "Directory gia autorizzata.");
                        } else if self.draft.allowed_roots.len() >= 16 {
                            self.log(Level::Error, "Massimo 16 directory aggiuntive.");
                        } else {
                            self.draft.allowed_roots.push(root);
                            self.log(
                                Level::Success,
                                "Directory autorizzata aggiunta (non salvata).",
                            );
                        }
                    }
                    Err(msg) => self.log(Level::Error, msg),
                }
            }
            OperationEvent::Saved { settings, outcome } => {
                self.busy = false;
                match outcome {
                    Ok(text) => {
                        self.persisted = settings;
                        let level = if text.contains("ATTENZIONE") {
                            Level::Warn
                        } else {
                            Level::Success
                        };
                        self.log(level, text.clone());
                        if self.setup {
                            self.step = 4;
                            self.modal = Some(Modal::SetupResult(text));
                        }
                        if self.restart_needed() {
                            self.log(Level::Warn, "Riavvia HTTP per applicare la configurazione.");
                        }
                    }
                    Err(msg) => self.log(Level::Error, msg),
                }
            }
        }
    }
    fn activate_command(&mut self, index: usize) -> Option<Effect> {
        self.command = index;
        match index {
            0 => {
                if self.server == ServerState::Starting || self.server == ServerState::Running {
                    self.log(Level::Warn, "Server gia in esecuzione o in avvio.");
                    None
                } else if self.dirty() {
                    self.log(Level::Warn, "Salva le modifiche prima di avviare HTTP.");
                    None
                } else {
                    self.server = ServerState::Starting;
                    self.running_config = Some(self.persisted.clone());
                    self.log(
                        Level::Info,
                        format!(
                            "Avvio HTTP richiesto sulla porta {}...",
                            self.persisted.port
                        ),
                    );
                    Some(if self.persisted.ngrok_enabled {
                        Effect::StartNgrok(self.persisted.clone())
                    } else {
                        Effect::Start(self.persisted.clone())
                    })
                }
            }
            1 => {
                if matches!(self.server, ServerState::Starting | ServerState::Running) {
                    self.confirm(Confirm::Stop);
                } else {
                    self.log(Level::Info, "Nessun server TUI da arrestare.");
                }
                None
            }
            2 => Some(Effect::Diagnose(self.persisted.clone())),
            3 => {
                if !self.persisted.vm_enabled {
                    self.log(
                        Level::Warn,
                        "VM non abilitata nella configurazione salvata.",
                    );
                    None
                } else {
                    self.busy = true;
                    self.log(Level::Info, "Lettura inventario VM...");
                    Some(Effect::Inventory)
                }
            }
            4 => {
                self.notice("Operazioni VM", "Tool MCP 'vm': images, create (image), status (id), destroy (id, distruttivo). Importazione di dischi approvati: novagpt vm-import --name N --source FILE [--cpus 2 --memory-mb 4096]. Le operazioni host richiedono un client MCP con parametri espliciti.");
                None
            }
            5 => {
                self.notice("Sottoagenti MCP", "Tool disponibili: subagent_spawn, subagent_status, subagent_message, subagent_result, subagent_cancel, subagent_resume, subagent_browser_diagnostics. Servono browser ChatGPT abilitato, Chrome dedicato e autenticazione manuale; nessuna automazione e' avviata da questo menu.");
                None
            }
            6 => {
                self.notice("Accesso CLI", "MCP stdio: novagpt stdio\nHTTP da CLI: novagpt http --ngrok\nMostra URL: novagpt connector-url\nVM import: novagpt vm-import --name N --source FILE");
                None
            }
            7 => {
                if matches!(self.server, ServerState::Starting | ServerState::Running) {
                    self.log(
                        Level::Warn,
                        "Arresta il server HTTP prima di avviare ngrok.",
                    );
                    None
                } else if self.dirty() {
                    self.log(
                        Level::Warn,
                        "Salva la configurazione prima di avviare ngrok.",
                    );
                    None
                } else {
                    self.server = ServerState::Starting;
                    self.running_config = Some(self.persisted.clone());
                    self.log(Level::Info, "Avvio del tunnel ngrok richiesto...");
                    Some(Effect::StartNgrok(self.persisted.clone()))
                }
            }
            8 => {
                if self.server != ServerState::Running {
                    self.log(Level::Warn, "Avvia prima MCP + ngrok.");
                    None
                } else if self.busy {
                    None
                } else {
                    self.busy = true;
                    Some(Effect::ShowConnectorUrl(self.persisted.port))
                }
            }
            _ => None,
        }
    }
    fn notice(&mut self, title: &str, body: &str) {
        self.modal = Some(Modal::Notice {
            title: title.into(),
            body: body.into(),
        });
    }
    fn confirm(&mut self, action: Confirm) {
        self.yes = false; // Default sicuro: No.
        self.modal = Some(Modal::Confirm(action));
    }
    fn start_edit(&mut self, field: Field) {
        self.input = match field {
            Field::Port => self.draft.port.to_string(),
            Field::Chrome => self
                .draft
                .chrome_executable
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            Field::Root => String::new(),
            Field::NgrokDomain => self.draft.ngrok_domain.clone().unwrap_or_default(),
        };
        self.input_error = None;
        self.modal = Some(Modal::Edit(field));
    }
    fn complete_edit(&mut self, field: Field) -> Option<Effect> {
        let value = self.input.trim();
        match field {
            Field::Port => match value.parse::<u16>() {
                Ok(port) if port != 0 => self.draft.port = port,
                _ => {
                    self.input_error = Some("Usa una porta tra 1 e 65535.".into());
                    return None;
                }
            },
            Field::Chrome => {
                if !value.is_empty() && !PathBuf::from(value).is_absolute() {
                    self.input_error = Some("Serve un percorso assoluto.".into());
                    return None;
                }
                let path = (!value.is_empty()).then(|| PathBuf::from(value));
                self.draft.chrome_executable = path.clone();
                self.chrome_valid = if path.is_some() { None } else { Some(false) };
                self.modal = None;
                self.input.clear();
                return path.map(Effect::ValidateChrome);
            }
            Field::NgrokDomain => {
                if !value.is_empty() && !crate::tunnel::valid_domain(value) {
                    self.input_error =
                        Some("Inserisci un hostname valido, senza schema o percorso.".into());
                    return None;
                }
                self.draft.ngrok_domain = (!value.is_empty()).then(|| value.to_string());
            }
            Field::Root => {
                if value.is_empty() {
                    self.input_error = Some("Inserisci una directory.".into());
                    return None;
                }
                if self.draft.allowed_roots.len() >= 16 {
                    self.input_error = Some("Massimo 16 directory autorizzate.".into());
                    return None;
                }
                let path = PathBuf::from(value);
                self.modal = None;
                self.input.clear();
                self.busy = true;
                return Some(Effect::AddRoot(path));
            }
        }
        self.modal = None;
        self.input.clear();
        self.log(
            Level::Info,
            "Valore modificato; salva per renderlo persistente.",
        );
        None
    }
    fn config_action(&mut self) -> Option<Effect> {
        match self.config {
            0 => self.start_edit(Field::Port),
            1 => {
                self.draft.vm_enabled = !self.draft.vm_enabled;
                self.log(Level::Info, "Opzione VM modificata.");
            }
            2 => {
                self.draft.browser_enabled = !self.draft.browser_enabled;
                self.log(Level::Info, "Opzione browser modificata.");
            }
            3 => self.start_edit(Field::Chrome),
            4 => self.start_edit(Field::Root),
            5 => {
                if !self.draft.allowed_roots.is_empty() {
                    self.root_selection = 0;
                    self.modal = Some(Modal::Roots);
                } else {
                    self.log(Level::Warn, "Nessuna directory aggiuntiva.");
                }
            }
            6 => {
                if !self.profiles.is_empty() {
                    self.profile = (self.profile + 1) % self.profiles.len();
                } else {
                    self.log(Level::Warn, "Nessun profilo CodexPro disponibile.");
                }
            }
            7 => {
                if self.profiles.get(self.profile).is_some_and(|p| {
                    p.port.is_some() || p.ngrok_domain.is_some() || p.tunnel.is_some()
                }) {
                    self.modal = Some(Modal::ImportPreview);
                } else {
                    self.log(Level::Warn, "Nessuna porta importabile nel profilo.");
                }
            }
            8 => {
                if self.setup && self.step < 3 {
                    self.step = 3;
                    self.log(Level::Info, "Passo di revisione: conferma il salvataggio.");
                } else {
                    self.confirm(Confirm::Save);
                }
            }
            9 => {
                if self.dirty() {
                    self.confirm(Confirm::Discard);
                }
            }
            10 => {
                if !self.busy {
                    self.busy = true;
                    return Some(Effect::EnsureToken);
                }
            }
            11 => {
                self.draft.ngrok_enabled = !self.draft.ngrok_enabled;
                self.log(
                    Level::Info,
                    "Preferenza ngrok modificata; salva per confermare.",
                );
            }
            12 => self.start_edit(Field::NgrokDomain),
            13 if self.setup && self.step < 4 => {
                self.step = (self.step + 1).min(3);
            }
            _ => {}
        }
        None
    }
    fn accepted(&mut self, action: Confirm) -> Option<Effect> {
        match action {
            Confirm::Stop => {
                self.server = ServerState::Stopped;
                self.running_config = None;
                Some(Effect::Stop)
            }
            Confirm::Save => {
                if self.busy {
                    self.log(Level::Warn, "Operazione in corso.");
                    None
                } else {
                    self.busy = true;
                    self.log(Level::Info, "Validazione e salvataggio in corso...");
                    Some(Effect::Save(self.draft.clone(), self.setup))
                }
            }
            Confirm::Import => {
                if let Some(profile) = self.profiles.get(self.profile) {
                    crate::settings::import_supported(&mut self.draft, profile);
                    self.log(
                        Level::Success,
                        "Porta e impostazioni ngrok consentite importate nella bozza; salva per applicarle.",
                    );
                }
                None
            }
            Confirm::RemoveRoot => {
                if self.root_selection < self.draft.allowed_roots.len() {
                    self.draft.allowed_roots.remove(self.root_selection);
                    self.log(Level::Success, "Directory rimossa dalla bozza.");
                }
                None
            }
            Confirm::Discard => {
                self.draft = self.persisted.clone();
                self.chrome_valid = Some(self.draft.chrome_executable.is_some());
                self.log(Level::Info, "Modifiche annullate.");
                None
            }
            Confirm::Quit => Some(Effect::Quit),
        }
    }
    pub fn key(&mut self, key: KeyEvent) -> Option<Effect> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Effect::Quit);
        }
        if let Some(modal) = self.modal.clone() {
            match modal {
                Modal::Edit(field) => match key.code {
                    KeyCode::Esc => {
                        self.modal = None;
                        self.input.clear();
                        self.input_error = None;
                    }
                    KeyCode::Enter => return self.complete_edit(field),
                    KeyCode::Backspace => {
                        self.input.pop();
                        self.input_error = None;
                    }
                    KeyCode::Char(ch)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                            && self.input.len() < 1024 =>
                    {
                        self.input.push(ch);
                        self.input_error = None;
                    }
                    _ => {}
                },
                Modal::Confirm(action) => match key.code {
                    KeyCode::Esc => self.modal = None,
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                        self.yes = !self.yes
                    }
                    KeyCode::Char('y') => {
                        self.modal = None;
                        return self.accepted(action);
                    }
                    KeyCode::Char('n') => self.modal = None,
                    KeyCode::Enter => {
                        self.modal = None;
                        if self.yes {
                            return self.accepted(action);
                        }
                    }
                    _ => {}
                },
                Modal::ImportPreview => match key.code {
                    KeyCode::Esc | KeyCode::Char('n') => self.modal = None,
                    KeyCode::Enter | KeyCode::Char('y') => {
                        self.modal = None;
                        self.confirm(Confirm::Import);
                    }
                    _ => {}
                },
                Modal::Roots => match key.code {
                    KeyCode::Esc => self.modal = None,
                    KeyCode::Up => self.root_selection = self.root_selection.saturating_sub(1),
                    KeyCode::Down => {
                        self.root_selection = (self.root_selection + 1)
                            .min(self.draft.allowed_roots.len().saturating_sub(1))
                    }
                    KeyCode::Enter => self.confirm(Confirm::RemoveRoot),
                    _ => {}
                },
                Modal::ConnectorUrl(url) => match key.code {
                    KeyCode::Char('c') | KeyCode::Char('C') => {
                        return Some(Effect::CopyConnectorUrl(url));
                    }
                    KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.modal = None,
                    _ => {}
                },
                Modal::Help | Modal::Notice { .. } | Modal::SetupResult(_) => {
                    if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q')) {
                        self.modal = None;
                    }
                }
            }
            return None;
        }
        match key.code {
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Tab => self.tab = (self.tab + 1) % 3,
            KeyCode::BackTab => self.tab = (self.tab + 2) % 3,
            KeyCode::Left => self.tab = (self.tab + 2) % 3,
            KeyCode::Right => self.tab = (self.tab + 1) % 3,
            KeyCode::Char('q') => {
                if self.dirty() {
                    self.confirm(Confirm::Quit);
                } else {
                    return Some(Effect::Quit);
                }
            }
            KeyCode::Esc => {
                if self.setup && self.tab == 2 && self.step > 0 && self.step < 4 {
                    self.step -= 1;
                } else if self.tab != 0 {
                    self.tab = 0;
                }
            }
            KeyCode::Char('n') if self.setup && self.tab == 2 && self.step < 4 => {
                self.step = (self.step + 1).min(3)
            }
            KeyCode::Char('b') if self.setup && self.tab == 2 => {
                self.step = self.step.saturating_sub(1)
            }
            KeyCode::Up => match self.tab {
                0 => self.command = self.command.saturating_sub(1),
                1 => self.log_offset = (self.log_offset + 1).min(self.logs.len().saturating_sub(1)),
                _ => self.config = self.config.saturating_sub(1),
            },
            KeyCode::Down => match self.tab {
                0 => self.command = (self.command + 1).min(COMMANDS.len() - 1),
                1 => self.log_offset = self.log_offset.saturating_sub(1),
                _ => self.config = (self.config + 1).min(self.config_len() - 1),
            },
            KeyCode::PageUp if self.tab == 1 => {
                self.log_offset = (self.log_offset + 10).min(self.logs.len().saturating_sub(1))
            }
            KeyCode::PageDown if self.tab == 1 => {
                self.log_offset = self.log_offset.saturating_sub(10)
            }
            KeyCode::Home if self.tab == 1 => self.log_offset = self.logs.len().saturating_sub(1),
            KeyCode::End if self.tab == 1 => self.log_offset = 0,
            KeyCode::Enter if self.tab == 0 => return self.activate_command(self.command),
            KeyCode::Enter if self.tab == 2 && !self.busy => return self.config_action(),
            KeyCode::Char(ch @ '1'..='4') if self.tab == 0 => {
                let index = match ch {
                    '1' => 0,
                    '2' => 1,
                    '3' => 3,
                    _ => 2,
                };
                return self.activate_command(index);
            }
            KeyCode::Char('s') if self.tab == 2 && !self.busy => {
                self.config = 8;
                return self.config_action();
            }
            _ => {}
        }
        None
    }
}
