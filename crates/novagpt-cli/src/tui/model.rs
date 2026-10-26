//! UI state and pure keyboard transitions. Effects are executed by operations.rs.
use crate::i18n::{self, Language};
use crate::settings::{ImportProfile, LocalSettings};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    collections::VecDeque,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub(super) fn command_label(index: usize, lang: Language) -> &'static str {
    match index {
        0 => i18n::Messages::start_mcp_http(lang),
        1 => i18n::Messages::stop_mcp_http(lang),
        2 => i18n::Messages::workspace_diagnostics(lang),
        3 => i18n::Messages::vm_inventory(lang),
        4 => i18n::Messages::vm_operations(lang),
        5 => i18n::Messages::subagents(lang),
        6 => i18n::Messages::clients_and_cli(lang),
        7 => i18n::Messages::start_mcp_ngrok(lang),
        8 => i18n::Messages::show_chatgpt_url(lang),
        _ => "",
    }
}

pub(super) const CONFIG_ORDER: [usize; 16] = [15, 0, 11, 12, 10, 2, 3, 1, 4, 5, 6, 7, 13, 8, 9, 14];
pub(super) fn config_label(index: usize, lang: Language) -> &'static str {
    match index {
        0 => i18n::Messages::http_port(lang),
        1 => i18n::Messages::enable_vm(lang),
        2 => i18n::Messages::enable_chatgpt_browser(lang),
        3 => i18n::Messages::chrome_executable(lang),
        4 => i18n::Messages::add_authorized_directory(lang),
        5 => i18n::Messages::remove_authorized_directory(lang),
        6 => i18n::Messages::choose_codexpro_novagpt_profile(lang),
        7 => i18n::Messages::import_profile_preview(lang),
        8 => i18n::Messages::save_settings(lang),
        9 => i18n::Messages::discard_changes(lang),
        10 => i18n::Messages::generate_verify_http_token(lang),
        11 => i18n::Messages::automatic_ngrok_on_off(lang),
        12 => i18n::Messages::ngrok_hostname(lang),
        13 => i18n::Messages::import_novagpt_folder(lang),
        14 => i18n::Messages::next_setup_step(lang),
        15 => i18n::Messages::settings_language(lang),
        _ => "",
    }
}
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
    pub english: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AuthState {
    NotVerified,
    Ready,
    Error,
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
    NovaSource,
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
    LoadNovaSource(PathBuf),
    Quit,
}
#[derive(Debug)]
pub(super) enum OperationEvent {
    Log(Level, String),
    Server(ServerState),
    TunnelReady,
    RootAdded(Result<PathBuf, String>),
    ChromeChecked(bool),
    VmInventory(Result<usize, String>),
    TokenReady(Result<String, String>),
    TokenPresent(String),
    ConnectorUrlReady(Result<String, String>),
    CopyFinished(Result<(), String>),
    NovaSourceLoaded(Result<ImportProfile, String>),
    Saved {
        settings: LocalSettings,
        outcome: Result<String, String>,
    },
}
pub(super) struct App {
    pub root: PathBuf,
    pub draft: LocalSettings,
    pub persisted: LocalSettings,
    pub profiles: Vec<ImportProfile>,
    pub setup: bool,
    pub step: usize,
    pub tab: usize,
    pub command: usize,
    pub show_advanced: bool,
    pub tunnel_ready: bool,
    pub config: usize,
    pub system_language: Language,
    pub profile: usize,
    pub root_selection: usize,
    pub server: ServerState,
    pub vm_images: Option<usize>,
    pub running_config: Option<LocalSettings>,
    pub modal: Option<Modal>,
    pub input: String,
    pub input_error: Option<String>,
    pub chrome_valid: Option<bool>,
    pub auth_state: AuthState,
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
        profiles: Vec<ImportProfile>,
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
            show_advanced: false,
            tunnel_ready: false,
            config: 0,
            system_language: i18n::detect_system(),
            profile: 0,
            root_selection: 0,
            server: ServerState::Stopped,
            vm_images: None,
            running_config: None,
            modal: None,
            input: String::new(),
            input_error: None,
            chrome_valid: Some(
                crate::chrome::discover(settings.chrome_executable.as_deref()).is_ok(),
            ),
            auth_state: AuthState::NotVerified,
            yes: false,
            busy: false,
            logs: VecDeque::new(),
            log_offset: 0,
            status: "Pronto".into(),
        };
        app.log(Level::Info, "NovaGPT avviato. ? apre l'aiuto.");
        app
    }
    pub fn language(&self) -> Language {
        self.draft.language.resolved(self.system_language)
    }
    pub fn dirty(&self) -> bool {
        self.draft != self.persisted
    }
    pub fn restart_needed(&self) -> bool {
        // Language is a TUI-only preference. It never changes MCP startup state.
        self.server == ServerState::Running
            && self.running_config.as_ref().is_some_and(|running| {
                let mut current = self.persisted.clone();
                current.language = running.language;
                *running != current
            })
    }
    pub fn config_len(&self) -> usize {
        if self.setup { 16 } else { 15 }
    }

    pub fn config_order(&self) -> &'static [usize] {
        &CONFIG_ORDER[..self.config_len()]
    }

    fn select_config(&mut self, direction: i32) {
        let order = self.config_order();
        let position = order
            .iter()
            .position(|&index| index == self.config)
            .unwrap_or(0);
        let next = if direction < 0 {
            position.saturating_sub(1)
        } else {
            (position + 1).min(order.len().saturating_sub(1))
        };
        self.config = order[next];
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
            english: message.clone(),
            message: message.clone(),
        });
        if self.logs.len() > LOG_CAP {
            self.logs.pop_front();
        }
        self.log_offset = self.log_offset.min(self.logs.len().saturating_sub(1));
        self.status = message;
    }
    fn log_localized(&mut self, level: Level, message: fn(Language) -> &'static str) {
        self.log(level, message(Language::Italian));
        if let Some(entry) = self.logs.back_mut() {
            entry.english = message(Language::English).to_string();
        }
        self.status = message(self.language()).to_string();
    }

    fn log_localized_dynamic(&mut self, level: Level, english: String, italian: String) {
        self.log(level, italian);
        if let Some(entry) = self.logs.back_mut() {
            entry.english = english.clone();
        }
        self.status = if self.language() == Language::English {
            english
        } else {
            self.logs
                .back()
                .map_or(String::new(), |entry| entry.message.clone())
        };
    }

    pub fn on_event(&mut self, event: OperationEvent) {
        match event {
            OperationEvent::Log(level, text) => self.log(level, text),
            OperationEvent::Server(s) => {
                self.server = s;
                if matches!(s, ServerState::Stopped | ServerState::Failed) {
                    self.running_config = None;
                    self.tunnel_ready = false;
                    if !self.show_advanced {
                        self.command = 0;
                    }
                } else if s == ServerState::Running && !self.show_advanced {
                    self.command = 8;
                }
            }
            OperationEvent::TunnelReady => self.tunnel_ready = true,
            OperationEvent::VmInventory(result) => {
                self.busy = false;
                match result {
                    Ok(count) => {
                        self.vm_images = Some(count);
                        self.log_localized_dynamic(
                            Level::Success,
                            i18n::Messages::vm_image_count(Language::English, count),
                            i18n::Messages::vm_image_count(Language::Italian, count),
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
                        self.log_localized(
                            Level::Info,
                            i18n::Messages::chatgpt_url_shown_only_in_the_private,
                        );
                    }
                    Err(message) => self.log(Level::Warn, message),
                }
            }
            OperationEvent::CopyFinished(result) => match result {
                Ok(()) => self.log_localized(
                    Level::Success,
                    i18n::Messages::url_copied_to_clipboard_on_request,
                ),
                Err(message) => self.log(Level::Error, message),
            },
            OperationEvent::NovaSourceLoaded(result) => {
                self.busy = false;
                match result {
                    Ok(profile) => {
                        self.profiles.push(profile);
                        self.profile = self.profiles.len().saturating_sub(1);
                        self.modal = Some(Modal::ImportPreview);
                        self.log_localized(Level::Info, i18n::Messages::nova_folder_verified);
                    }
                    Err(message) => self.log(Level::Error, message),
                }
            }
            OperationEvent::TokenPresent(message) => {
                self.auth_state = AuthState::Ready;
                self.log(Level::Info, message);
            }
            OperationEvent::TokenReady(result) => {
                self.busy = false;
                match result {
                    Ok(message) => {
                        self.auth_state = AuthState::Ready;
                        self.log(Level::Success, message);
                    }
                    Err(message) => {
                        self.auth_state = AuthState::Error;
                        self.log(Level::Error, message);
                    }
                }
            }
            OperationEvent::ChromeChecked(valid) => {
                self.chrome_valid = Some(valid);
                if !valid {
                    self.log_localized(Level::Error, i18n::Messages::invalid_chrome_binary);
                }
            }
            OperationEvent::RootAdded(result) => {
                self.busy = false;
                match result {
                    Ok(root) => {
                        if root == self.root || self.draft.allowed_roots.contains(&root) {
                            self.log_localized(
                                Level::Warn,
                                i18n::Messages::directory_already_authorized,
                            );
                        } else if self.draft.allowed_roots.len() >= 16 {
                            self.log_localized(
                                Level::Error,
                                i18n::Messages::maximum_16_additional_directories,
                            );
                        } else {
                            self.draft.allowed_roots.push(root);
                            self.log_localized(
                                Level::Success,
                                i18n::Messages::authorized_directory_added_not_saved,
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
                            self.log_localized(
                                Level::Warn,
                                i18n::Messages::restart_http_to_apply_the_settings,
                            );
                        }
                    }
                    Err(msg) => self.log(Level::Error, msg),
                }
            }
        }
    }
    pub fn visible_commands(&self) -> Vec<usize> {
        let mut commands = if matches!(self.server, ServerState::Running | ServerState::Starting) {
            vec![8, 1, 2]
        } else {
            vec![0, 7, 2]
        };
        if self.show_advanced {
            commands.extend([3, 4, 5, 6]);
        }
        commands
    }

    fn select_command(&mut self, direction: i32) {
        let commands = self.visible_commands();
        let position = commands
            .iter()
            .position(|&index| index == self.command)
            .unwrap_or(0);
        let next = if direction < 0 {
            position.saturating_sub(1)
        } else {
            (position + 1).min(commands.len().saturating_sub(1))
        };
        self.command = commands[next];
    }

    fn activate_command(&mut self, index: usize) -> Option<Effect> {
        self.command = index;
        match index {
            0 => {
                if self.server == ServerState::Starting || self.server == ServerState::Running {
                    self.log_localized(
                        Level::Warn,
                        i18n::Messages::server_is_already_running_or_starting,
                    );
                    None
                } else if self.dirty() {
                    self.log_localized(Level::Warn, i18n::Messages::save_before_http);
                    None
                } else {
                    self.server = ServerState::Starting;
                    if !self.show_advanced {
                        self.command = 8;
                    }
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
                    self.log_localized(Level::Info, i18n::Messages::no_tui_server_to_stop);
                }
                None
            }
            2 => Some(Effect::Diagnose(self.persisted.clone())),
            3 => {
                if !self.persisted.vm_enabled {
                    self.log_localized(
                        Level::Warn,
                        i18n::Messages::vm_is_disabled_in_the_saved_configuration,
                    );
                    None
                } else {
                    self.busy = true;
                    self.log_localized(Level::Info, i18n::Messages::reading_vm_inventory);
                    Some(Effect::Inventory)
                }
            }
            4 => {
                self.notice(
                    i18n::Messages::vm_operations(self.language()),
                    i18n::Messages::notice_vm_body(self.language()),
                );
                None
            }
            5 => {
                self.notice(
                    i18n::Messages::mcp_subagents(self.language()),
                    i18n::Messages::notice_agents_body(self.language()),
                );
                None
            }
            6 => {
                self.notice(
                    i18n::Messages::cli_access(self.language()),
                    i18n::Messages::notice_cli_body(self.language()),
                );
                None
            }
            7 => {
                if matches!(self.server, ServerState::Starting | ServerState::Running) {
                    self.log_localized(Level::Warn, i18n::Messages::stop_http_before_ngrok);
                    None
                } else if self.dirty() {
                    self.log_localized(Level::Warn, i18n::Messages::save_before_ngrok);
                    None
                } else {
                    self.server = ServerState::Starting;
                    if !self.show_advanced {
                        self.command = 8;
                    }
                    self.running_config = Some(self.persisted.clone());
                    self.log_localized(Level::Info, i18n::Messages::ngrok_tunnel_startup_requested);
                    Some(Effect::StartNgrok(self.persisted.clone()))
                }
            }
            8 => {
                if self.server != ServerState::Running {
                    self.log_localized(Level::Warn, i18n::Messages::start_mcp_ngrok_first);
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
            Field::NovaSource => String::new(),
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
                self.chrome_valid = Some(crate::chrome::discover(path.as_deref()).is_ok());
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
            Field::NovaSource => {
                let folder = PathBuf::from(value);
                if !folder.is_absolute() {
                    self.input_error =
                        Some("Indica un percorso assoluto della cartella NovaGPT.".into());
                    return None;
                }
                self.modal = None;
                self.input.clear();
                self.busy = true;
                return Some(Effect::LoadNovaSource(folder));
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
        self.log_localized(
            Level::Info,
            i18n::Messages::value_changed_save_to_persist_it,
        );
        None
    }
    fn config_action(&mut self) -> Option<Effect> {
        match self.config {
            15 => {
                self.draft.language = self.draft.language.next();
                self.log_localized(
                    Level::Info,
                    i18n::Messages::language_changed_save_to_keep_it_after,
                );
            }
            0 => self.start_edit(Field::Port),
            1 => {
                self.draft.vm_enabled = !self.draft.vm_enabled;
                self.log_localized(Level::Info, i18n::Messages::vm_setting_changed);
            }
            2 => {
                self.draft.browser_enabled = !self.draft.browser_enabled;
                self.chrome_valid =
                    Some(crate::chrome::discover(self.draft.chrome_executable.as_deref()).is_ok());
                self.log_localized(Level::Info, i18n::Messages::browser_setting_changed);
            }
            3 => self.start_edit(Field::Chrome),
            4 => self.start_edit(Field::Root),
            5 => {
                if !self.draft.allowed_roots.is_empty() {
                    self.root_selection = 0;
                    self.modal = Some(Modal::Roots);
                } else {
                    self.log_localized(Level::Warn, i18n::Messages::no_additional_directories);
                }
            }
            6 => {
                if !self.profiles.is_empty() {
                    self.profile = (self.profile + 1) % self.profiles.len();
                } else {
                    self.log_localized(Level::Warn, i18n::Messages::no_import_profiles);
                }
            }
            7 => {
                if self.profiles.get(self.profile).is_some_and(|p| {
                    p.port.is_some() || p.ngrok_domain.is_some() || p.tunnel.is_some()
                }) {
                    self.modal = Some(Modal::ImportPreview);
                } else {
                    self.log_localized(Level::Warn, i18n::Messages::no_import_port);
                }
            }
            8 => {
                if self.setup && self.step < 3 {
                    self.step = 3;
                    self.log_localized(Level::Info, i18n::Messages::setup_review_step);
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
                self.log_localized(
                    Level::Info,
                    i18n::Messages::ngrok_preference_changed_save_to_confirm,
                );
            }
            12 => self.start_edit(Field::NgrokDomain),
            13 => {
                if !self.busy {
                    self.start_edit(Field::NovaSource);
                }
            }
            14 if self.setup && self.step < 4 => {
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
                self.tunnel_ready = false;
                if !self.show_advanced {
                    self.command = 0;
                }
                self.running_config = None;
                Some(Effect::Stop)
            }
            Confirm::Save => {
                if self.busy {
                    self.log_localized(Level::Warn, i18n::Messages::operation_in_progress);
                    None
                } else {
                    self.busy = true;
                    self.log_localized(Level::Info, i18n::Messages::validation_saving);
                    Some(Effect::Save(self.draft.clone(), self.setup))
                }
            }
            Confirm::Import => {
                if let Some(profile) = self.profiles.get(self.profile) {
                    crate::settings::import_supported(&mut self.draft, profile);
                    self.log_localized(
                        Level::Success,
                        i18n::Messages::port_and_approved_ngrok_settings_imported_into,
                    );
                }
                None
            }
            Confirm::RemoveRoot => {
                if self.root_selection < self.draft.allowed_roots.len() {
                    self.draft.allowed_roots.remove(self.root_selection);
                    self.log_localized(
                        Level::Success,
                        i18n::Messages::directory_removed_from_draft,
                    );
                }
                None
            }
            Confirm::Discard => {
                self.draft = self.persisted.clone();
                self.chrome_valid =
                    Some(crate::chrome::discover(self.draft.chrome_executable.as_deref()).is_ok());
                self.log_localized(Level::Info, i18n::Messages::changes_discarded);
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
            KeyCode::Char('a') if self.tab == 0 => {
                self.show_advanced = !self.show_advanced;
                if !self.visible_commands().contains(&self.command) {
                    self.command = self.visible_commands()[0];
                }
            }
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
                0 => self.select_command(-1),
                1 => self.log_offset = (self.log_offset + 1).min(self.logs.len().saturating_sub(1)),
                _ => self.select_config(-1),
            },
            KeyCode::Down => match self.tab {
                0 => self.select_command(1),
                1 => self.log_offset = self.log_offset.saturating_sub(1),
                _ => self.select_config(1),
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
