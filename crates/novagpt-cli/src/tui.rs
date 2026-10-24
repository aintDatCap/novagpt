use crate::settings::{self, CodexProfile, LocalSettings};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs, Wrap},
};
use std::{
    collections::VecDeque,
    fs,
    io::{self, IsTerminal, Stdout},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{runtime::Handle, task::JoinHandle};

type History = Arc<Mutex<VecDeque<String>>>;
const LOG_LIMIT: usize = 150;

fn push(history: &History, message: impl Into<String>) {
    if let Ok(mut list) = history.lock() {
        list.push_back(message.into());
        while list.len() > LOG_LIMIT {
            list.pop_front();
        }
    }
}

/// Bounded in-memory tracing writer: never puts diagnostics on the TUI screen's stderr.
struct EventLogWriter {
    history: History,
    data: Vec<u8>,
}
impl io::Write for EventLogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = 8_192usize.saturating_sub(self.data.len());
        self.data
            .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Drop for EventLogWriter {
    fn drop(&mut self) {
        let rendered = String::from_utf8_lossy(&self.data);
        for line in rendered.lines().take(10) {
            if !line.is_empty() {
                push(&self.history, line.to_owned());
            }
        }
    }
}

struct TerminalGuard;
impl TerminalGuard {
    fn open() -> io::Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen) {
            let _ = disable_raw_mode();
            return Err(error);
        }
        let guard = Self;
        match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => Ok((guard, terminal)),
            Err(error) => {
                drop(guard);
                Err(error)
            }
        }
    }
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

enum Editing {
    Port,
    Chrome,
    AllowedRoot,
}
struct App {
    root: PathBuf,
    settings: LocalSettings,
    setup: bool,
    profiles: Vec<CodexProfile>,
    selected_profile: usize,
    tab: usize,
    editing: Option<Editing>,
    input: String,
    history: History,
    handle: Handle,
    server: Option<JoinHandle<()>>,
    saved: bool,
}
impl App {
    fn new(
        root: PathBuf,
        settings: LocalSettings,
        setup: bool,
        profiles: Vec<CodexProfile>,
        handle: Handle,
    ) -> Self {
        let history = Arc::new(Mutex::new(VecDeque::new()));
        push(&history, format!("Workspace: {}", root.display()));
        if setup {
            push(
                &history,
                "Setup TUI: review configuration and press S to save.",
            );
        }
        if profiles.is_empty() {
            push(
                &history,
                "No CodexPro profiles available; importing is optional.",
            );
        }
        Self {
            root,
            settings,
            setup,
            profiles,
            selected_profile: 0,
            tab: if setup { 2 } else { 0 },
            editing: None,
            input: String::new(),
            history,
            handle,
            server: None,
            saved: false,
        }
    }
    fn active(&self) -> bool {
        self.server.as_ref().is_some_and(|s| !s.is_finished())
    }
    fn start(&mut self) {
        if self.active() {
            push(&self.history, "HTTP server already running.");
            return;
        }
        let Ok(token) = std::env::var("NOVAGPT_HTTP_TOKEN") else {
            push(
                &self.history,
                "ERROR: set NOVAGPT_HTTP_TOKEN (24+ characters) before starting HTTP.",
            );
            return;
        };
        if token.len() < 24 {
            push(
                &self.history,
                "ERROR: NOVAGPT_HTTP_TOKEN must contain at least 24 characters.",
            );
            return;
        }
        if let Err(error) = self.settings.validate() {
            push(
                &self.history,
                format!("ERROR: configuration invalid: {error}"),
            );
            return;
        }
        let root = self.root.clone();
        let config = self.settings.clone();
        let history = Arc::clone(&self.history);
        push(
            &history,
            format!("Starting local MCP HTTP on 127.0.0.1:{}...", config.port),
        );
        self.server = Some(self.handle.spawn(async move {
            let result: Result<(), String> = async {
                let workspaces = novagpt_core::WorkspaceRegistry::new(&root, &config.allowed_roots)
                    .map_err(|e| e.to_string())?;
                let vm = if config.vm_enabled {
                    Some(
                        novagpt_vm::VmManager::new(settings::local_dir(&root).join("vms"))
                            .map_err(|e| e.to_string())?,
                    )
                } else {
                    None
                };
                let agents = if config.browser_enabled {
                    let chrome = config
                        .chrome_executable
                        .ok_or("Chrome executable missing")?;
                    let browser = novagpt_browser::ChatGptBrowser::new(
                        chrome,
                        settings::local_dir(&root).join("browser"),
                    )
                    .map_err(|e| e.to_string())?;
                    Some(novagpt_agents::AgentManager::new(
                        workspaces.clone(),
                        Arc::new(novagpt_agents::ChatGptBackend::new(browser)),
                    ))
                } else {
                    None
                };
                push(
                    &history,
                    format!(
                        "Listening at http://127.0.0.1:{}/mcp (loopback only).",
                        config.port
                    ),
                );
                novagpt_mcp::run_http_with(
                    workspaces,
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.port),
                    token,
                    novagpt_mcp::NovaExtras { vm, agents },
                )
                .await
                .map_err(|e| e.to_string())
            }
            .await;
            match result {
                Ok(()) => push(&history, "HTTP server stopped."),
                Err(error) => push(&history, format!("ERROR: HTTP server terminated: {error}")),
            }
        }));
    }
    fn stop(&mut self) {
        if let Some(server) = self.server.take() {
            server.abort();
            push(&self.history, "HTTP server stop requested.");
        } else {
            push(&self.history, "No HTTP server to stop.");
        }
    }
    fn vm_images(&mut self) {
        if !self.settings.vm_enabled {
            push(
                &self.history,
                "VM is disabled. Enable it in Config and save.",
            );
            return;
        }
        match novagpt_vm::VmManager::new(settings::local_dir(&self.root).join("vms"))
            .and_then(|vm| vm.images())
        {
            Ok(images) => push(
                &self.history,
                format!("Approved VM images: {}", images.image_count),
            ),
            Err(e) => push(&self.history, format!("ERROR: VM inventory: {e}")),
        }
    }
    fn diagnostic(&mut self) {
        let config = settings::config_path(&self.root);
        push(
            &self.history,
            format!(
                "Config: {} (exists: {})",
                config.display(),
                config.is_file()
            ),
        );
        push(
            &self.history,
            format!(
                "Server: {} | browser: {} | VM: {}",
                if self.active() { "running" } else { "stopped" },
                self.settings.browser_enabled,
                self.settings.vm_enabled
            ),
        );
    }
    fn save(&mut self) {
        match self.settings.save(&self.root) {
            Ok(path) => {
                self.saved = true;
                push(&self.history, format!("Saved {}", path.display()));
                if self.active() {
                    push(&self.history, "Restart HTTP to apply updated settings.");
                }
                if self.setup {
                    match install_launcher(&self.root) {
                        Ok(message) => push(&self.history, message),
                        Err(e) => push(&self.history, format!("ERROR: launcher installation: {e}")),
                    }
                }
            }
            Err(e) => push(&self.history, format!("ERROR: cannot save settings: {e}")),
        }
    }
    fn import(&mut self) {
        if let Some(profile) = self.profiles.get(self.selected_profile) {
            settings::import_supported(&mut self.settings, profile);
            self.saved = false;
            push(
                &self.history,
                format!(
                    "Imported supported settings from {} (port only). Press S to save.",
                    profile.root_label
                ),
            );
        } else {
            push(&self.history, "No CodexPro profile to import.");
        }
    }
    fn begin_edit(&mut self, which: Editing) {
        self.input = match which {
            Editing::Port => self.settings.port.to_string(),
            Editing::Chrome => self
                .settings
                .chrome_executable
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            Editing::AllowedRoot => String::new(),
        };
        self.editing = Some(which);
    }
    fn complete_edit(&mut self) {
        let Some(which) = self.editing.take() else {
            return;
        };
        let value = self.input.trim();
        match which {
            Editing::Port => match value.parse::<u16>() {
                Ok(port) if port > 0 => {
                    self.settings.port = port;
                    self.saved = false;
                }
                _ => push(&self.history, "ERROR: port must be between 1 and 65535."),
            },
            Editing::Chrome => {
                self.settings.chrome_executable = if value.is_empty() {
                    None
                } else {
                    Some(PathBuf::from(value))
                };
                self.saved = false;
            }
            Editing::AllowedRoot => {
                let candidate = PathBuf::from(value);
                if self.settings.allowed_roots.len() >= 16 {
                    push(&self.history, "ERROR: 16 additional roots maximum.");
                } else if let Ok(canonical) = fs::canonicalize(candidate) {
                    if canonical.is_dir()
                        && !self.settings.allowed_roots.contains(&canonical)
                        && canonical != self.root
                    {
                        self.settings.allowed_roots.push(canonical);
                        self.saved = false;
                    }
                } else {
                    push(&self.history, "ERROR: allowed root must already exist.");
                }
            }
        }
        self.input.clear();
    }
    fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return true;
        }
        if self.editing.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.editing = None;
                    self.input.clear();
                }
                KeyCode::Enter => self.complete_edit(),
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && self.input.len() < 1024 =>
                {
                    self.input.push(ch);
                }
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Tab => self.tab = (self.tab + 1) % 3,
            KeyCode::BackTab => self.tab = (self.tab + 2) % 3,
            KeyCode::Char('1') if self.tab == 0 => self.start(),
            KeyCode::Char('2') if self.tab == 0 => self.stop(),
            KeyCode::Char('3') if self.tab == 0 => self.vm_images(),
            KeyCode::Char('4') if self.tab == 0 => self.diagnostic(),
            KeyCode::Char('e') if self.tab == 2 => self.begin_edit(Editing::Port),
            KeyCode::Char('c') if self.tab == 2 => self.begin_edit(Editing::Chrome),
            KeyCode::Char('a') if self.tab == 2 => self.begin_edit(Editing::AllowedRoot),
            KeyCode::Char('x') if self.tab == 2 => {
                self.settings.allowed_roots.clear();
                self.saved = false;
            }
            KeyCode::Char('v') if self.tab == 2 => {
                self.settings.vm_enabled = !self.settings.vm_enabled;
                self.saved = false;
            }
            KeyCode::Char('b') if self.tab == 2 => {
                self.settings.browser_enabled = !self.settings.browser_enabled;
                self.saved = false;
            }
            KeyCode::Char('p') if self.tab == 2 && !self.profiles.is_empty() => {
                self.selected_profile = (self.selected_profile + 1) % self.profiles.len();
            }
            KeyCode::Char('i') if self.tab == 2 => self.import(),
            KeyCode::Char('s') if self.tab == 2 => self.save(),
            _ => {}
        }
        true
    }
    fn draw(&self, frame: &mut ratatui::Frame<'_>) {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(8),
                Constraint::Length(3),
            ])
            .split(frame.area());
        let tabs = Tabs::new(["Comandi", "Stato / Log", "Config / Setup"])
            .select(self.tab)
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" NovaGPT · {} ", self.root.display())),
            );
        frame.render_widget(tabs, areas[0]);
        let lines: Vec<Line<'_>> = match self.tab {
            0 => vec![
                Line::from("1  Avvia MCP HTTP locale"),
                Line::from("2  Ferma MCP HTTP"),
                Line::from("3  Elenca immagini VM autorizzate"),
                Line::from("4  Diagnostica workspace e servizi"),
                Line::from(""),
                Line::from("Per il protocollo stdio: novagpt --root <path> stdio"),
                Line::from("Per importare dischi: novagpt --vm-home <path> vm-import --help"),
                Line::from("Nessuna pagina web, tunnel o modifica al CodexPro originale."),
                Line::from(format!(
                    "Stato HTTP: {}",
                    if self.active() { "ATTIVO" } else { "FERMO" }
                )),
            ],
            1 => self
                .history
                .lock()
                .map(|history| {
                    let height = usize::from(areas[1].height.saturating_sub(2));
                    history
                        .iter()
                        .skip(history.len().saturating_sub(height))
                        .map(|text| Line::from(text.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            _ => {
                let selected = self.profiles.get(self.selected_profile);
                vec![
                    Line::from(format!(
                        "Config locale: {}",
                        settings::config_path(&self.root).display()
                    )),
                    Line::from(format!("E  Porta HTTP: {}", self.settings.port)),
                    Line::from(format!(
                        "V  VM locale: {} (dati in .novagpt/vms)",
                        self.settings.vm_enabled
                    )),
                    Line::from(format!(
                        "B  ChatGPT browser: {} (profilo dedicato .novagpt/browser)",
                        self.settings.browser_enabled
                    )),
                    Line::from(format!(
                        "C  Chrome: {}",
                        self.settings
                            .chrome_executable
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "(non impostato)".into())
                    )),
                    Line::from(format!(
                        "A  Aggiungi radice autorizzata (ora {})",
                        self.settings.allowed_roots.len()
                    )),
                    Line::from("X  Rimuovi radici aggiuntive"),
                    Line::from(""),
                    Line::from(format!(
                        "P  Profilo CodexPro ({}/{}): {}",
                        if selected.is_some() {
                            self.selected_profile + 1
                        } else {
                            0
                        },
                        self.profiles.len(),
                        selected
                            .map(|p| p.root_label.as_str())
                            .unwrap_or("(nessuno)")
                    )),
                    Line::from(format!(
                        "   Sorgente: {}",
                        selected
                            .map(|p| p.origin.display().to_string())
                            .unwrap_or_else(|| "(nessuna)".into())
                    )),
                    Line::from(format!(
                        "   Anteprima importazione: porta {:?}; altri campi ignorati.",
                        selected.and_then(|p| p.port)
                    )),
                    Line::from(
                        "I  Importa solo opzioni compatibili (in memoria, poi S per salvare)",
                    ),
                    Line::from(format!(
                        "S  Salva{}{}",
                        if self.setup {
                            " e installa launcher nella .novagpt locale"
                        } else {
                            ""
                        },
                        if self.saved {
                            "  [salvato]"
                        } else {
                            "  [modifiche non salvate]"
                        }
                    )),
                ]
            }
        };
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(if self.tab == 1 {
                        " Log ed errori (ultimi 150 eventi) "
                    } else {
                        " Contenuto "
                    }),
            ),
            areas[1],
        );
        let footer = if self.editing.is_some() {
            format!(" Modifica: {}  |  Invio applica  Esc annulla ", self.input)
        } else {
            " Tab/Shift+Tab schede | q esci | HTTP richiede NOVAGPT_HTTP_TOKEN ".into()
        };
        frame.render_widget(
            Paragraph::new(Span::styled(footer, Style::default().fg(Color::Yellow)))
                .block(Block::default().borders(Borders::ALL)),
            areas[2],
        );
    }
}

fn install_launcher(root: &Path) -> io::Result<String> {
    let dir = settings::local_dir(root);
    let bin = dir.join("bin");
    if bin.exists() {
        let meta = fs::symlink_metadata(&bin)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe installation directory",
            ));
        }
    } else {
        fs::create_dir(&bin)?;
    }
    let filename = if cfg!(windows) {
        "novagpt.exe"
    } else {
        "novagpt"
    };
    let destination = bin.join(filename);
    let source = std::env::current_exe()?;
    match fs::symlink_metadata(&destination) {
        Ok(m) if m.file_type().is_symlink() || !m.is_file() => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe installed binary",
            ));
        }
        Ok(_) => {} // Existing executable is never overwritten while it might be running.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if source != destination {
                fs::copy(&source, &destination)?;
            }
        }
        Err(e) => return Err(e),
    }
    let script = if cfg!(windows) {
        dir.join("start.cmd")
    } else {
        dir.join("start.sh")
    };
    if !script.exists() {
        let content = if cfg!(windows) {
            "@echo off\r\n\"%~dp0bin\\novagpt.exe\" --root \"%~dp0..\" %*\r\n"
        } else {
            "#!/bin/sh\nHERE=\"$(CDPATH= cd -- \"$(dirname -- \"$0\")\" && pwd)\"\nexec \"$HERE/bin/novagpt\" --root \"$HERE/..\" \"$@\"\n"
        };
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&script)?;
        use std::io::Write;
        file.write_all(content.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(format!(
        "Local launcher: {} (existing binaries are not overwritten)",
        script.display()
    ))
}

pub fn run(
    root: PathBuf,
    settings: LocalSettings,
    setup: bool,
    profiles: Vec<CodexProfile>,
    handle: Handle,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TUI needs an interactive terminal; use 'novagpt stdio' for MCP clients",
        ));
    }
    let (_guard, mut terminal) = TerminalGuard::open()?;
    let mut app = App::new(root, settings, setup, profiles, handle);
    let trace_log = Arc::clone(&app.history);
    if let Err(error) = tracing_subscriber::fmt()
        .with_writer(move || EventLogWriter {
            history: Arc::clone(&trace_log),
            data: Vec::new(),
        })
        .with_env_filter("novagpt=info")
        .try_init()
    {
        push(
            &app.history,
            format!("Tracing subscriber unavailable: {error}"),
        );
    }
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        if event::poll(Duration::from_millis(120))?
            && let Event::Key(key) = event::read()?
            && !app.key(key)
        {
            break;
        }
    }
    app.stop();
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn importing_requires_explicit_selection_and_save() {
        let dir = tempfile::tempdir().unwrap();
        let handle = Handle::current();
        let profile = CodexProfile {
            origin: PathBuf::from("profile.json"),
            root_label: "example".into(),
            port: Some(9191),
        };
        let mut app = App::new(
            dir.path().to_path_buf(),
            LocalSettings::default(),
            true,
            vec![profile],
            handle,
        );
        assert_eq!(app.settings.port, 8787);
        app.import();
        assert_eq!(app.settings.port, 9191);
        assert!(!settings::config_path(dir.path()).exists());
    }
}
