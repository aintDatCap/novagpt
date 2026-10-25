//! Terminal entry point: RAII lifecycle, event loop and async effect integration.
mod launcher;
mod model;
mod operations;
mod render;

use crate::settings::{CodexProfile, LocalSettings};
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
        assert_eq!(a.command, 1);
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
        let profile = CodexProfile {
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
        for (w, h) in [(120, 32), (65, 18), (35, 11), (12, 7), (8, 4)] {
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
        assert!(screen.contains("MCP HTTP"));
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
        assert!(screen.contains("Porta HTTP"));
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
        assert_eq!(app.auth_state, "Pronto");
        let mut terminal = Terminal::new(TestBackend::new(105, 32)).unwrap();
        terminal.draw(|frame| render::draw(frame, &app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("Genera / verifica token HTTP"));
        assert!(rendered.contains("Pronto"));
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
        assert!(displayed.contains("3 immagini approvate"));
        assert!(displayed.contains("NECESSARIO"));
        let ctrl = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(a.key(ctrl), Some(Effect::Quit)));
    }
}
