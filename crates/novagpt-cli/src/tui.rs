//! Terminal entry point: RAII lifecycle, event loop and async effect integration.
mod launcher;
mod model;
mod operations;
mod render;

use crate::settings::{ImportProfile, LocalSettings};
use crossterm::{
    event::{self, Event},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, IsTerminal, Stdout},
    path::PathBuf,
    time::Duration,
};
use tokio::{runtime::Handle, sync::mpsc};

/// Single owner of terminal restoration, even when drawing/event-reading panics.
trait TerminalOps {
    fn leave_alternate(&mut self);
    fn restore_raw(&mut self);
}
struct LiveTerminal;
impl TerminalOps for LiveTerminal {
    fn leave_alternate(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
    fn restore_raw(&mut self) {
        let _ = disable_raw_mode();
    }
}
struct Lifecycle<T: TerminalOps> {
    operations: T,
    raw: bool,
    alternate: bool,
}
impl<T: TerminalOps> Drop for Lifecycle<T> {
    fn drop(&mut self) {
        if self.alternate {
            self.operations.leave_alternate();
        }
        if self.raw {
            self.operations.restore_raw();
        }
    }
}
struct TerminalGuard {
    _lifecycle: Lifecycle<LiveTerminal>,
}
impl TerminalGuard {
    fn open() -> io::Result<(Self, Terminal<CrosstermBackend<Stdout>>)> {
        enable_raw_mode()?;
        let mut lifecycle = Lifecycle {
            operations: LiveTerminal,
            raw: true,
            alternate: false,
        };
        execute!(io::stdout(), EnterAlternateScreen)?;
        lifecycle.alternate = true;
        let terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        Ok((
            Self {
                _lifecycle: lifecycle,
            },
            terminal,
        ))
    }
}

pub fn run(
    root: PathBuf,
    settings: LocalSettings,
    setup: bool,
    profiles: Vec<ImportProfile>,
    handle: Handle,
) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "TUI needs an interactive terminal; use 'novagpt stdio' for MCP clients",
        ));
    }
    let (_guard, mut terminal) = TerminalGuard::open()?;
    let mut app = model::App::new(root, settings, setup, profiles);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut controller = operations::Controller::new(handle, tx);
    loop {
        // UI stays responsive while filesystem, VM and server work off-thread.
        while let Ok(message) = rx.try_recv() {
            app.on_event(message);
        }
        terminal.draw(|frame| render::draw(frame, &app))?;
        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                Event::Key(key) => {
                    if let Some(effect) = app.key(key)
                        && !controller.execute(effect, app.root.clone())
                    {
                        break;
                    }
                }
                Event::Resize(_, _) => {
                    terminal.autoresize()?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
    use model::{App, Effect, Field, Level, Modal, OperationEvent, ServerState};
    use ratatui::backend::TestBackend;
    use std::sync::{Arc, Mutex};
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn app() -> App {
        App::new(PathBuf::from("."), LocalSettings::default(), false, vec![])
    }
    #[test]
    fn navigation_and_selection_and_number_shortcuts() {
        let mut a = app();
        a.key(key(KeyCode::Down));
        assert_eq!(a.command, 7);
        a.key(key(KeyCode::Up));
        assert_eq!(a.command, 0);
        assert!(matches!(a.key(key(KeyCode::Enter)), Some(Effect::Start(_))));
        assert_eq!(a.server, ServerState::Starting);
        a.key(key(KeyCode::Tab));
        assert_eq!(a.tab, 1);
        a.key(key(KeyCode::Tab));
        assert_eq!(a.tab, 2);
        a.key(key(KeyCode::Tab));
        assert_eq!(a.tab, 0);
        a.key(key(KeyCode::BackTab));
        assert_eq!(a.tab, 2);
        a.key(key(KeyCode::Left));
        assert_eq!(a.tab, 1);
        a.key(key(KeyCode::Right));
        assert_eq!(a.tab, 2);
        a.key(key(KeyCode::Tab));
        let selected = a.key(key(KeyCode::Char('4')));
        assert_eq!(a.command, 2);
        assert!(matches!(selected, Some(Effect::Diagnose(_))));
        a.key(key(KeyCode::Char('3')));
        assert_eq!(a.command, 3);
        a.key(key(KeyCode::Char('2')));
        assert_eq!(a.command, 1);
        assert!(matches!(
            a.modal,
            Some(Modal::Confirm(model::Confirm::Stop))
        ));
    }
    #[test]
    fn editing_traps_focus_and_rejects_invalid_port() {
        let mut a = app();
        a.tab = 2;
        a.key(key(KeyCode::Enter));
        assert!(matches!(a.modal, Some(Modal::Edit(Field::Port))));
        a.key(key(KeyCode::Tab));
        assert_eq!(a.tab, 2);
        for _ in 0..4 {
            a.key(key(KeyCode::Backspace));
        }
        a.key(key(KeyCode::Char('0')));
        a.key(key(KeyCode::Enter));
        assert!(a.modal.is_some());
        assert!(a.input_error.is_some());
        a.key(key(KeyCode::Backspace));
        for ch in "9393".chars() {
            a.key(key(KeyCode::Char(ch)));
        }
        a.key(key(KeyCode::Enter));
        assert_eq!(a.draft.port, 9393);
        assert!(a.dirty());
        a.key(key(KeyCode::Enter));
        a.key(key(KeyCode::Esc));
        assert!(a.modal.is_none());
        assert_eq!(a.draft.port, 9393);
        let release = KeyEvent {
            kind: KeyEventKind::Release,
            ..key(KeyCode::Char('q'))
        };
        assert!(a.key(release).is_none());
    }
    #[test]
    fn logs_scroll_and_help_and_cancel() {
        let mut a = app();
        for i in 0..32 {
            a.log(Level::Info, format!("event {i}"));
        }
        a.tab = 1;
        a.key(key(KeyCode::PageUp));
        assert_eq!(a.log_offset, 10);
        a.key(key(KeyCode::Down));
        assert_eq!(a.log_offset, 9);
        a.key(key(KeyCode::End));
        assert_eq!(a.log_offset, 0);
        a.key(key(KeyCode::Char('?')));
        assert!(matches!(a.modal, Some(Modal::Help)));
        a.key(key(KeyCode::Esc));
        assert!(a.modal.is_none());
    }
    #[test]
    fn save_is_confirmed_and_validated_by_settings() {
        let dir = tempfile::tempdir().expect("workspace");
        let mut a = App::new(
            dir.path().to_path_buf(),
            LocalSettings::default(),
            false,
            vec![],
        );
        a.draft.port = 9191;
        a.tab = 2;
        a.config = 8;
        assert!(a.key(key(KeyCode::Enter)).is_none());
        assert!(matches!(
            a.modal,
            Some(Modal::Confirm(model::Confirm::Save))
        ));
        a.key(key(KeyCode::Esc));
        assert!(!crate::settings::config_path(dir.path()).exists());
        a.key(key(KeyCode::Enter));
        let effect = a.key(key(KeyCode::Char('y')));
        let Some(Effect::Save(saved, false)) = effect else {
            panic!("expected save");
        };
        assert_eq!(saved.port, 9191);
        let path = saved.save(dir.path()).expect("save fixture");
        a.on_event(OperationEvent::Saved {
            settings: saved.clone(),
            outcome: Ok(format!("Saved {}", path.display())),
        });
        assert_eq!(
            crate::settings::LocalSettings::load(dir.path()).unwrap(),
            saved
        );
        assert!(!a.dirty());
    }
    #[test]
    fn import_port_ngrok_and_only_after_approval() {
        let dir = tempfile::tempdir().unwrap();
        let profile = ImportProfile {
            source: "CodexPro",
            origin: PathBuf::from("fixture.json"),
            root_label: "sample".into(),
            port: Some(9393),
            tunnel: Some("ngrok".into()),
            ngrok_domain: Some("nova-fixture.ngrok-free.dev".into()),
        };
        let mut a = App::new(
            dir.path().to_path_buf(),
            LocalSettings::default(),
            true,
            vec![profile],
        );
        a.step = 2;
        a.config = 7;
        a.key(key(KeyCode::Enter));
        assert_eq!(a.draft.port, 8787);
        assert!(!a.draft.ngrok_enabled);
        a.key(key(KeyCode::Esc));
        assert_eq!(a.draft.port, 8787);
        a.key(key(KeyCode::Enter));
        a.key(key(KeyCode::Enter));
        assert_eq!(a.draft.port, 8787);
        a.key(key(KeyCode::Char('y')));
        assert_eq!(a.draft.port, 9393);
        assert!(a.draft.ngrok_enabled);
        assert_eq!(
            a.draft.ngrok_domain.as_deref(),
            Some("nova-fixture.ngrok-free.dev")
        );
        assert!(!a.draft.vm_enabled);
        assert!(!a.draft.browser_enabled);
        assert!(!crate::settings::config_path(dir.path()).exists());
        // Imported ngrok preference is honored by the normal start command.
        a.tab = 0;
        assert!(a.key(key(KeyCode::Enter)).is_none()); // Unsaved changes block launch.
        a.tab = 2;
        a.key(key(KeyCode::Char('n')));
        assert_eq!(a.step, 3);
    }
    #[test]
    fn nova_workspace_selection_previews_then_imports_only_after_confirmation() {
        let mut app = app();
        app.tab = 2;
        app.config = 13;
        assert!(app.key(key(KeyCode::Enter)).is_none());
        assert!(matches!(app.modal, Some(Modal::Edit(Field::NovaSource))));
        let path = if cfg!(windows) {
            "C:\\Projects\\example"
        } else {
            "/tmp/example"
        };
        for ch in path.chars() {
            app.key(key(KeyCode::Char(ch)));
        }
        assert!(matches!(
            app.key(key(KeyCode::Enter)),
            Some(Effect::LoadNovaSource(_))
        ));
        assert!(app.busy);
        app.on_event(OperationEvent::NovaSourceLoaded(Ok(ImportProfile {
            source: "NovaGPT",
            origin: PathBuf::from("source/.novagpt/config.json"),
            root_label: "source".into(),
            port: Some(9977),
            tunnel: Some("none".into()),
            ngrok_domain: None,
        })));
        assert!(matches!(app.modal, Some(Modal::ImportPreview)));
        assert_eq!(app.draft.port, 8787);
        app.key(key(KeyCode::Enter));
        assert!(matches!(
            app.modal,
            Some(Modal::Confirm(model::Confirm::Import))
        ));
        app.key(key(KeyCode::Char('y')));
        assert_eq!(app.draft.port, 9977);
        assert!(app.dirty());
    }
    #[test]
    fn saved_ngrok_preference_controls_normal_http_start() {
        let settings = LocalSettings {
            ngrok_enabled: true,
            ngrok_domain: Some("fixture.ngrok-free.dev".into()),
            ..LocalSettings::default()
        };
        let mut a = App::new(PathBuf::from("."), settings, false, vec![]);
        assert!(matches!(a.key(key(KeyCode::Enter)),
            Some(Effect::StartNgrok(config)) if config.ngrok_domain.as_deref() == Some("fixture.ngrok-free.dev")));
        let mut b = app();
        b.tab = 2;
        b.config = 11;
        b.key(key(KeyCode::Enter));
        assert!(b.draft.ngrok_enabled);
        b.config = 12;
        b.key(key(KeyCode::Enter));
        assert!(matches!(b.modal, Some(Modal::Edit(Field::NgrokDomain))));
        for character in "https://bad.example/path".chars() {
            b.key(key(KeyCode::Char(character)));
        }
        b.key(key(KeyCode::Enter));
        assert!(b.input_error.is_some());
        b.key(key(KeyCode::Esc));
        assert!(b.draft.ngrok_domain.is_none());
    }

    #[test]
    fn ngrok_url_is_revealed_only_on_demand_and_copy_is_explicit() {
        let mut a = app();
        a.command = 7;
        assert!(matches!(
            a.key(key(KeyCode::Enter)),
            Some(Effect::StartNgrok(_))
        ));
        assert!(a.modal.is_none());
        a.on_event(OperationEvent::Server(ServerState::Running));
        a.command = 8;
        assert!(matches!(
            a.key(key(KeyCode::Enter)),
            Some(Effect::ShowConnectorUrl(8787))
        ));
        let token = "T".repeat(40);
        let link = format!(
            "https://example.ngrok-free.app/mcp?{}={token}",
            "codexpro_token"
        );
        a.on_event(OperationEvent::ConnectorUrlReady(Ok(link.clone())));
        assert!(matches!(a.modal, Some(Modal::ConnectorUrl(_))));
        assert!(a.logs.iter().all(|entry| !entry.message.contains(&token)));
        assert!(a.key(key(KeyCode::Char('x'))).is_none());
        assert!(matches!(a.key(key(KeyCode::Char('c'))),
            Some(Effect::CopyConnectorUrl(value)) if value == link));
        a.on_event(OperationEvent::CopyFinished(Ok(())));
        assert!(a.logs.iter().all(|entry| !entry.message.contains(&token)));
        a.key(key(KeyCode::Esc));
        assert!(a.modal.is_none());
    }
    #[test]
    fn resize_and_tiny_rendering_with_testbackend() {
        let mut a = app();
        a.tab = 0;
        for (w, h) in [
            (148, 42),
            (120, 32),
            (80, 24),
            (65, 18),
            (35, 11),
            (12, 7),
            (8, 4),
        ] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| render::draw(f, &a)).unwrap();
            assert_eq!(t.size().unwrap().width, w);
        }
        a.tab = 1;
        let mut t = Terminal::new(TestBackend::new(70, 19)).unwrap();
        t.draw(|f| render::draw(f, &a)).unwrap();
        let screen = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("MCP AND COMPONENT STATUS"));
        assert!(screen.contains("INFO"));
        a.tab = 2;
        t.draw(|f| render::draw(f, &a)).unwrap();
        let screen = t
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(screen.contains("HTTP port"));
    }
    #[test]
    fn home_actions_and_tunnel_status_follow_real_server_events() {
        let mut a = app();
        assert_eq!(a.visible_commands(), vec![0, 7, 9, 2]);
        assert_eq!(a.command, 0);
        a.key(key(KeyCode::Char('a')));
        assert!(a.show_advanced);
        assert_eq!(a.visible_commands(), vec![0, 7, 9, 2, 3, 4, 5, 6]);
        a.key(key(KeyCode::Char('a')));
        assert!(!a.show_advanced);
        a.on_event(OperationEvent::Server(ServerState::Running));
        assert_eq!(a.visible_commands(), vec![8, 1, 2]);
        assert_eq!(a.command, 8);
        assert!(!a.tunnel_ready);
        a.on_event(OperationEvent::TunnelReady);
        assert!(a.tunnel_ready);
        a.on_event(OperationEvent::Server(ServerState::Stopped));
        assert!(!a.tunnel_ready);
        assert_eq!(a.command, 0);
    }

    #[test]
    fn agent_browser_requires_saved_opt_in_and_mcp_stopped() {
        let mut app = app();
        app.command = 9;
        assert!(app.visible_commands().contains(&9));
        assert!(app.key(key(KeyCode::Enter)).is_none());
        assert!(!app.browser_login_running);
        assert!(!app.busy);

        // Enabling an unsaved draft does not opt in to starting Chrome.
        app.draft.browser_enabled = true;
        assert!(app.key(key(KeyCode::Enter)).is_none());
        assert!(!app.browser_login_running);

        app.persisted = app.draft.clone();
        app.draft.port = 9191;
        assert!(app.key(key(KeyCode::Enter)).is_none());
        assert!(!app.browser_login_running);

        app.draft = app.persisted.clone();
        app.on_event(OperationEvent::Server(ServerState::Running));
        assert!(!app.visible_commands().contains(&9));
        app.command = 9;
        assert!(app.key(key(KeyCode::Enter)).is_none());
        assert!(!app.browser_login_running);
    }

    #[test]
    fn agent_browser_open_is_async_single_flight_and_blocks_server_start() {
        let settings = LocalSettings {
            browser_enabled: true,
            ..LocalSettings::default()
        };
        let mut app = App::new(PathBuf::from("."), settings, false, vec![]);
        app.command = 9;
        let effect = app.key(key(KeyCode::Enter));
        assert!(matches!(effect, Some(Effect::OpenAgentBrowser(config)) if config.browser_enabled));
        assert!(app.browser_login_running);
        assert!(app.busy);
        assert!(app.key(key(KeyCode::Enter)).is_none()); // no second launch
        app.command = 0;
        assert!(app.key(key(KeyCode::Enter)).is_none()); // cannot start MCP
        assert_eq!(app.server, ServerState::Stopped);
        app.command = 7;
        assert!(app.key(key(KeyCode::Enter)).is_none()); // cannot start ngrok
        assert_eq!(app.server, ServerState::Stopped);

        app.on_event(OperationEvent::BrowserLoginFinished(Err(
            model::BrowserOpenFailure::ProfileBusy,
        )));
        assert!(!app.browser_login_running);
        assert!(!app.busy);
        assert!(app.status.contains("profile") || app.status.contains("profilo"));
        app.command = 9;
        assert!(matches!(
            app.key(key(KeyCode::Enter)),
            Some(Effect::OpenAgentBrowser(_))
        ));
        app.on_event(OperationEvent::BrowserLoginFinished(Ok(())));
        assert!(!app.browser_login_running);
        assert!(!app.busy);
        assert!(matches!(
            app.key(key(KeyCode::Char('q'))),
            Some(Effect::Quit)
        ));
    }

    #[test]
    fn agent_browser_action_is_visible_and_localized() {
        let mut app = app();
        let mut terminal = Terminal::new(TestBackend::new(132, 37)).unwrap();
        terminal.draw(|frame| render::draw(frame, &app)).unwrap();
        let shown = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        assert!(shown(&terminal).contains("Open agent browser"));
        app.draft.language = crate::i18n::LanguageChoice::Italian;
        terminal.draw(|frame| render::draw(frame, &app)).unwrap();
        assert!(shown(&terminal).contains("Apri browser agenti"));
    }

    #[test]
    fn grouped_settings_navigation_and_home_rendering() {
        let mut a = app();
        a.tab = 2;
        a.key(key(KeyCode::Up));
        assert_eq!(a.config, 15);
        a.key(key(KeyCode::Down));
        assert_eq!(a.config, 0);
        a.key(key(KeyCode::Down));
        assert_eq!(a.config, 11);

        let mut t = Terminal::new(TestBackend::new(124, 36)).unwrap();
        t.draw(|f| render::draw(f, &a)).unwrap();
        let screen = |terminal: &Terminal<TestBackend>| {
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let shown = screen(&t);
        assert!(shown.contains("CONNECTION"));
        a.config = 2;
        t.draw(|f| render::draw(f, &a)).unwrap();
        assert!(screen(&t).contains("BROWSER"));
        assert!(shown.contains("APPLY CHANGES"));

        a.tab = 0;
        t.draw(|f| render::draw(f, &a)).unwrap();
        let shown = screen(&t);
        assert!(shown.contains("Service overview"));
        assert!(shown.contains("MCP token also grants access"));
        assert!(!shown.contains("Operazioni VM"));
        a.key(key(KeyCode::Char('a')));
        t.draw(|f| render::draw(f, &a)).unwrap();
        assert!(screen(&t).contains("VM operations"));
    }

    #[test]
    fn interface_language_changes_live_and_auto_uses_system_locale() {
        use crate::i18n::{Language, LanguageChoice};
        let mut a = app();
        assert_eq!(a.language(), Language::English);
        let mut terminal = Terminal::new(TestBackend::new(130, 35)).expect("terminal");
        terminal
            .draw(|f| render::draw(f, &a))
            .expect("render English");
        let shown = |t: &Terminal<TestBackend>| {
            t.backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        assert!(shown(&terminal).contains("Service overview"));
        assert!(!shown(&terminal).contains("Panoramica dei servizi"));

        a.tab = 2;
        a.config = 15;
        a.key(key(KeyCode::Enter));
        assert_eq!(a.draft.language, LanguageChoice::Italian);
        assert_eq!(a.language(), Language::Italian);
        assert!(a.dirty());
        a.tab = 0;
        terminal
            .draw(|f| render::draw(f, &a))
            .expect("render Italian");
        assert!(shown(&terminal).contains("Panoramica dei servizi"));

        a.tab = 2;
        a.config = 15;
        a.system_language = Language::Italian;
        a.key(key(KeyCode::Enter));
        assert_eq!(a.draft.language, LanguageChoice::Auto);
        assert_eq!(a.language(), Language::Italian);
        a.system_language = Language::English;
        assert_eq!(a.language(), Language::English);
        a.key(key(KeyCode::Enter));
        assert_eq!(a.draft.language, LanguageChoice::English);
    }

    #[test]
    fn changing_only_language_does_not_require_mcp_restart() {
        use crate::i18n::LanguageChoice;
        let mut a = app();
        a.on_event(OperationEvent::Server(ServerState::Running));
        a.running_config = Some(a.persisted.clone());
        assert!(!a.restart_needed());
        a.draft.language = LanguageChoice::Italian;
        let saved = a.draft.clone();
        a.on_event(OperationEvent::Saved {
            settings: saved,
            outcome: Ok("saved".into()),
        });
        assert!(!a.restart_needed());
    }

    #[test]
    fn connector_credential_remains_byte_identical_when_localized() {
        let mut a = app();
        let url = "https://example.test/mcp?secret=CODE_123456789";
        a.modal = Some(Modal::ConnectorUrl(url.to_string()));
        let mut terminal = Terminal::new(TestBackend::new(136, 38)).expect("terminal");
        for language in [
            crate::i18n::LanguageChoice::English,
            crate::i18n::LanguageChoice::Italian,
        ] {
            a.draft.language = language;
            terminal.draw(|f| render::draw(f, &a)).expect("render URL");
            let screen = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(screen.contains(url), "connector URL must remain unchanged");
        }
    }

    struct FakeTerminal(Arc<Mutex<Vec<&'static str>>>);
    impl TerminalOps for FakeTerminal {
        fn leave_alternate(&mut self) {
            self.0.lock().unwrap().push("leave");
        }
        fn restore_raw(&mut self) {
            self.0.lock().unwrap().push("raw_off");
        }
    }
    #[test]
    fn terminal_restores_on_unwind_and_partial_failure() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let copy = Arc::clone(&events);
        let _ = std::panic::catch_unwind(move || {
            let _guard = Lifecycle {
                operations: FakeTerminal(copy),
                raw: true,
                alternate: true,
            };
            panic!("injected draw failure");
        });
        assert_eq!(*events.lock().unwrap(), vec!["leave", "raw_off"]);
        events.lock().unwrap().clear();
        {
            let _partial = Lifecycle {
                operations: FakeTerminal(Arc::clone(&events)),
                raw: true,
                alternate: false,
            };
        }
        assert_eq!(*events.lock().unwrap(), vec!["raw_off"]);
    }
    #[test]
    fn launcher_never_overwrites_existing_binary_or_script() {
        let dir = tempfile::tempdir().unwrap();
        let local = crate::settings::local_dir(dir.path());
        std::fs::create_dir_all(local.join("bin")).unwrap();
        let exe = local.join("bin").join(if cfg!(windows) {
            "novagpt.exe"
        } else {
            "novagpt"
        });
        let script = local.join(if cfg!(windows) {
            "start.cmd"
        } else {
            "start.sh"
        });
        std::fs::write(&exe, "sentinel binary").unwrap();
        std::fs::write(&script, "sentinel script").unwrap();
        launcher::install_launcher(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(exe).unwrap(), "sentinel binary");
        assert_eq!(std::fs::read_to_string(script).unwrap(), "sentinel script");
    }
    #[test]
    fn chrome_validation_and_root_revocation_require_explicit_actions() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = App::new(
            dir.path().to_path_buf(),
            LocalSettings::default(),
            false,
            vec![],
        );
        a.tab = 2;
        a.config = 2;
        a.key(key(KeyCode::Enter));
        assert!(a.draft.browser_enabled);
        a.config = 3;
        a.key(key(KeyCode::Enter));
        assert!(matches!(a.modal, Some(Modal::Edit(Field::Chrome))));
        let invalid = if cfg!(windows) {
            "C:\\unknown\\browser.exe"
        } else {
            "/unknown/browser"
        };
        for ch in invalid.chars() {
            a.key(key(KeyCode::Char(ch)));
        }
        assert!(matches!(
            a.key(key(KeyCode::Enter)),
            Some(Effect::ValidateChrome(_))
        ));
        a.on_event(OperationEvent::ChromeChecked(false));
        assert_eq!(a.chrome_valid, Some(false));
        assert!(a.draft.validate().is_err());
        a.draft.allowed_roots.push(dir.path().to_path_buf());
        a.config = 5;
        a.key(key(KeyCode::Enter));
        assert!(matches!(a.modal, Some(Modal::Roots)));
        a.key(key(KeyCode::Esc));
        assert_eq!(a.draft.allowed_roots.len(), 1);
        a.key(key(KeyCode::Enter));
        a.key(key(KeyCode::Enter));
        assert!(matches!(
            a.modal,
            Some(Modal::Confirm(model::Confirm::RemoveRoot))
        ));
        a.key(key(KeyCode::Char('y')));
        assert!(a.draft.allowed_roots.is_empty());
    }
    #[test]
    fn config_can_generate_token_without_exposing_its_value() {
        let mut app = app();
        app.tab = 2;
        app.config = 10;
        assert!(matches!(
            app.key(key(KeyCode::Enter)),
            Some(Effect::EnsureToken)
        ));
        assert!(app.busy);
        app.on_event(OperationEvent::TokenReady(Ok(
            "Token pronto (valore nascosto).".into(),
        )));
        assert!(!app.busy);
        assert_eq!(app.auth_state, model::AuthState::Ready);
        let mut terminal = Terminal::new(TestBackend::new(105, 32)).unwrap();
        terminal.draw(|frame| render::draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Generate / verify HTTP token"));
        assert!(rendered.contains("Ready"));
    }
    #[test]
    fn status_inventory_restart_and_ctrl_c() {
        let mut a = app();
        a.persisted.vm_enabled = true;
        a.draft.vm_enabled = true;
        a.on_event(OperationEvent::VmInventory(Ok(3)));
        assert_eq!(a.vm_images, Some(3));
        a.server = ServerState::Running;
        a.running_config = Some(a.persisted.clone());
        let mut saved = a.persisted.clone();
        saved.port = 9500;
        a.on_event(OperationEvent::Saved {
            settings: saved.clone(),
            outcome: Ok("saved".to_string()),
        });
        assert!(a.restart_needed());
        a.tab = 1;
        let mut terminal = Terminal::new(TestBackend::new(105, 29)).unwrap();
        terminal.draw(|f| render::draw(f, &a)).unwrap();
        let displayed = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(displayed.contains("3 approved images"));
        assert!(displayed.contains("MCP restart required"));
        let ctrl = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(a.key(ctrl), Some(Effect::Quit)));
    }
}
