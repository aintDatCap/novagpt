//! Stateless Ratatui widgets and responsive layout (Ratatui 0.29).
use super::model::{App, COMMANDS, CONFIG_ITEMS, Confirm, Field, Level, Modal, ServerState};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Tabs,
        Wrap,
    },
};
const ACCENT: Color = Color::Cyan;
const MUTED: Color = Color::Gray;
const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const BAD: Color = Color::Red;

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(MUTED))
        .title(title.to_string())
}
fn state(s: ServerState) -> &'static str {
    match s {
        ServerState::Stopped => "FERMO",
        ServerState::Starting => "AVVIO...",
        ServerState::Running => "TCP RAGGIUNGIBILE",
        ServerState::Failed => "ERRORE",
    }
}
fn state_color(s: ServerState) -> Color {
    match s {
        ServerState::Running => OK,
        ServerState::Starting => WARN,
        ServerState::Failed => BAD,
        ServerState::Stopped => MUTED,
    }
}
fn on(b: bool) -> &'static str {
    if b { "ON" } else { "OFF" }
}
fn dirty_mark(b: bool) -> &'static str {
    if b { " *" } else { "" }
}
fn title_line(app: &App) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            " NOVAGPT ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", app.root.display()),
            Style::default().fg(MUTED),
        ),
    ])
}
fn config_value(app: &App, index: usize) -> String {
    match index {
        0 => format!(
            "{}{}",
            app.draft.port,
            dirty_mark(app.draft.port != app.persisted.port)
        ),
        1 => format!(
            "{}{}",
            on(app.draft.vm_enabled),
            dirty_mark(app.draft.vm_enabled != app.persisted.vm_enabled)
        ),
        2 => format!(
            "{}{}",
            on(app.draft.browser_enabled),
            dirty_mark(app.draft.browser_enabled != app.persisted.browser_enabled)
        ),
        3 => format!(
            "{}{}",
            app.draft
                .chrome_executable
                .as_ref()
                .map_or("(automatico/non impostato)".into(), |p| p
                    .display()
                    .to_string()),
            dirty_mark(app.draft.chrome_executable != app.persisted.chrome_executable)
        ),
        4 | 5 => format!(
            "{} directory{}",
            app.draft.allowed_roots.len(),
            dirty_mark(app.draft.allowed_roots != app.persisted.allowed_roots)
        ),
        6 => app
            .profiles
            .get(app.profile)
            .map_or("(nessuno)".into(), |p| p.root_label.clone()),
        7 => app
            .profiles
            .get(app.profile)
            .and_then(|p| p.port)
            .map_or("(non disponibile)".into(), |p| {
                format!("porta {p}, solo con consenso")
            }),
        8 => {
            if app.busy {
                "IN CORSO".into()
            } else if app.dirty() {
                "modifiche in sospeso".into()
            } else {
                "valori attuali".into()
            }
        }
        9 => "ripristina ultimo salvataggio".into(),
        10 => app.auth_state.clone(),
        11 => format!(
            "{}{}",
            on(app.draft.ngrok_enabled),
            dirty_mark(app.draft.ngrok_enabled != app.persisted.ngrok_enabled)
        ),
        12 => format!(
            "{}{}",
            app.draft
                .ngrok_domain
                .as_deref()
                .unwrap_or("(non impostato)"),
            dirty_mark(app.draft.ngrok_domain != app.persisted.ngrok_domain)
        ),
        13 => "scegli cartella sorgente".into(),
        _ => format!("passo {}/4", app.step.min(3) + 1),
    }
}
fn selected_list(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    items: Vec<ListItem<'_>>,
    index: usize,
) {
    if area.width < 3 || area.height < 3 {
        return;
    }
    let list = List::new(items)
        .block(panel(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut list_state = ListState::default();
    list_state.select(Some(index));
    frame.render_stateful_widget(list, area, &mut list_state);
}
fn commands(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let wide = area.width >= 78 && area.height >= 11;
    let parts = if wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(46), Constraint::Percentage(54)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(if area.height > 17 { 7 } else { 0 }),
            ])
            .split(area)
    };
    let items = COMMANDS
        .iter()
        .enumerate()
        .map(|(i, (name, _))| {
            let badge = match i {
                0 | 1 | 7 => state(app.server),
                3 => {
                    if app.persisted.vm_enabled {
                        "ABILITATA"
                    } else {
                        "DISABILITATA"
                    }
                }
                5 => {
                    if app.persisted.browser_enabled {
                        "CONFIGURATO"
                    } else {
                        "NON CONFIGURATO"
                    }
                }
                _ => "",
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", name), Style::default().fg(Color::White)),
                Span::styled(
                    badge.to_owned(),
                    Style::default().fg(if i < 2 || i == 7 {
                        state_color(app.server)
                    } else {
                        MUTED
                    }),
                ),
            ]))
        })
        .collect();
    selected_list(
        frame,
        parts[0],
        " Comandi  ↑↓ seleziona  Enter esegue ",
        items,
        app.command,
    );
    if parts[1].height > 2 {
        let selected = COMMANDS[app.command];
        let lines = vec![
            Line::from(Span::styled(
                selected.0.to_owned(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(selected.1.to_owned()),
            Line::from(""),
            Line::from(format!(
                "HTTP: {}  |  Porta: {}",
                state(app.server),
                app.persisted.port
            )),
            Line::from("Funzioni avanzate: i tool MCP richiedono parametri e client espliciti."),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: true })
                .block(panel(" Dettagli selezione ")),
            parts[1],
        );
    }
}
fn dashboard(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let size = if area.height > 17 {
        10
    } else {
        6.min(area.height / 2)
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(size), Constraint::Min(2)])
        .split(area);
    if sections[0].height >= 3 {
        let vm = if !app.persisted.vm_enabled {
            "Disabilitata".to_string()
        } else {
            app.vm_images
                .map_or("Abilitata • inventario non caricato".to_string(), |n| {
                    format!("Abilitata • {n} immagini approvate")
                })
        };
        let browser = if app.persisted.browser_enabled {
            "Configurato (non verificato)"
        } else {
            "Disabilitato"
        };
        let recent = app
            .logs
            .iter()
            .rev()
            .find(|e| e.level == Level::Error)
            .map_or("Nessuno in questa sessione".to_string(), |e| {
                e.message.clone()
            });
        let rows = vec![
            Row::new::<Vec<ratatui::widgets::Cell>>(vec![
                "Workspace".into(),
                app.root.display().to_string().into(),
            ]),
            Row::new::<Vec<ratatui::widgets::Cell>>(vec![
                "MCP HTTP".into(),
                state(app.server).into(),
            ]),
            Row::new::<Vec<ratatui::widgets::Cell>>(vec![
                "Endpoint".into(),
                format!("http://127.0.0.1:{}/mcp", app.persisted.port).into(),
            ]),
            Row::new::<Vec<ratatui::widgets::Cell>>(vec!["VM".into(), vm.into()]),
            Row::new::<Vec<ratatui::widgets::Cell>>(vec!["Browser".into(), browser.into()]),
            Row::new::<Vec<ratatui::widgets::Cell>>(vec![
                "Riavvio".into(),
                if app.restart_needed() {
                    "NECESSARIO"
                } else {
                    "Non richiesto"
                }
                .into(),
            ]),
            Row::new::<Vec<ratatui::widgets::Cell>>(vec!["Ultimo errore".into(), recent.into()]),
        ];
        let table = Table::new(rows, [Constraint::Length(17), Constraint::Min(1)])
            .block(panel(" Stato dei servizi "))
            .column_spacing(1);
        let mut state = TableState::default();
        frame.render_stateful_widget(table, sections[0], &mut state);
    }
    let visible = sections[1].height.saturating_sub(2) as usize;
    let end = app.logs.len().saturating_sub(app.log_offset);
    let start = end.saturating_sub(visible);
    let lines: Vec<ListItem<'_>> = app
        .logs
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|entry| {
            let (label, color) = match entry.level {
                Level::Info => ("INFO", MUTED),
                Level::Warn => ("WARN", WARN),
                Level::Error => ("ERROR", BAD),
                Level::Success => ("OK", OK),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", entry.time), Style::default().fg(MUTED)),
                Span::styled(
                    format!("{label:<5} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(entry.message.clone()),
            ]))
        })
        .collect();
    let caption = format!(
        " Eventi  ↑↓/PgUp/PgDn scroll  {} / {} ",
        end,
        app.logs.len()
    );
    frame.render_widget(List::new(lines).block(panel(&caption)), sections[1]);
}
fn config(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let top_h = if app.setup {
        if area.height >= 16 { 5 } else { 3 }
    } else {
        0
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(top_h), Constraint::Min(2)])
        .split(area);
    if app.setup && top_h >= 3 {
        let instructions = match app.step {
            0 => "1/4  WORKSPACE  •  Controlla il percorso mostrato nell'header. N: avanti.",
            1 => "2/4  PARAMETRI  •  Seleziona con frecce; Invio modifica. N: avanti.",
            2 => "3/4  IMPORT OPZIONALE  •  Anteprima e consenso esplicito. N: salvataggio.",
            3 => "4/4  REVISIONE  •  Controlla i valori, seleziona Salva e conferma.",
            _ => "COMPLETATO  •  Apri il risultato del setup; q esce.",
        };
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(instructions),
                Line::from("Permessi, token e file dei progetti non vengono importati da CodexPro o NovaGPT."),
            ])
            .wrap(Wrap { trim: true })
            .block(panel(" Setup guidato ")),
            sections[0],
        );
    }
    let wide = sections[1].width >= 95 && sections[1].height >= 10;
    let parts = if wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(56), Constraint::Percentage(44)])
            .split(sections[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(5),
                Constraint::Length(if sections[1].height > 17 { 8 } else { 0 }),
            ])
            .split(sections[1])
    };
    let items = (0..app.config_len())
        .map(|i| {
            let value = config_value(app, i);
            let invalid = (i == 3 || i == 2)
                && app.draft.browser_enabled
                && app.draft.chrome_executable.is_none();
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", CONFIG_ITEMS[i]),
                    Style::default().fg(if invalid { BAD } else { Color::White }),
                ),
                Span::styled(
                    value,
                    Style::default().fg(if invalid { BAD } else { MUTED }),
                ),
            ]))
        })
        .collect();
    selected_list(
        frame,
        parts[0],
        " Config  ↑↓ seleziona  Enter modifica ",
        items,
        app.config,
    );
    if parts[1].height > 2 {
        let selected = app.profiles.get(app.profile);
        let roots = if app.draft.allowed_roots.is_empty() {
            "(solo workspace corrente)".to_string()
        } else {
            app.draft
                .allowed_roots
                .iter()
                .take(4)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let validation = if app.draft.browser_enabled && !app.chrome_valid.unwrap_or(false) {
            "NON VALIDO: seleziona un eseguibile Chrome assoluto."
        } else {
            "Validazione completa al salvataggio."
        };
        let lines = vec![
            Line::from(Span::styled(
                if app.dirty() {
                    "● Modifiche non salvate"
                } else {
                    "✓ Configurazione allineata"
                },
                Style::default().fg(if app.dirty() { WARN } else { OK }),
            )),
            Line::from(if app.restart_needed() {
                "Riavvio HTTP necessario."
            } else {
                "HTTP usa configurazione salvata all'avvio."
            }),
            Line::from(""),
            Line::from(Span::styled(
                "Directory aggiuntive:",
                Style::default().fg(ACCENT),
            )),
            Line::from(roots),
            Line::from(""),
            Line::from(Span::styled(
                "Import CodexPro (solo porta):",
                Style::default().fg(ACCENT),
            )),
            Line::from(selected.map_or("Nessun profilo".into(), |p| {
                format!("{} → {:?}", p.root_label, p.port)
            })),
            Line::from("Altri dati ignorati; nessun accesso in scrittura a CodexPro."),
            Line::from(""),
            Line::from(Span::styled(
                "Token HTTP (mostrato solo su richiesta nella finestra URL):",
                Style::default().fg(ACCENT),
            )),
            Line::from(app.auth_state.clone()),
            Line::from("Per visualizzarlo: novagpt token show"),
            Line::from(""),
            Line::from(Span::styled(
                validation,
                Style::default().fg(if validation.starts_with("NON") {
                    BAD
                } else {
                    MUTED
                }),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(panel(" Riepilogo e controlli ")),
            parts[1],
        );
    }
}
fn centered(area: Rect, percent_w: u16, h: u16) -> Rect {
    let width = area
        .width
        .saturating_mul(percent_w)
        .saturating_div(100)
        .max(1)
        .min(area.width);
    let height = h.min(area.height).max(1);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}
fn modal(frame: &mut Frame<'_>, app: &App) {
    let Some(modal) = &app.modal else {
        return;
    };
    let area = frame.area();
    if area.width < 3 || area.height < 3 {
        return;
    }
    let (title, body, footer): (String, String, String) = match modal {
        Modal::Help => ("Aiuto rapido".into(),
            "Tab / Shift+Tab: cambia scheda\n← →: cambia scheda (fuori dai campi)\n↑ ↓: scegli un comando/campo o scorri i log\nEnter: esegui, modifica o conferma\nEsc: chiudi/indietro   q: esci\n?: aiuto   Ctrl+C: uscita controllata\nStato: PgUp/PgDn, Home/End scorrono i log\nConfig: * indica un campo modificato\nSetup: n avanti, b indietro\nI tasti numerici 1–4 sono opzionali nei Comandi.".into(),
            "Esc / Enter per chiudere".into()),
        Modal::Notice {title, body} => (title.clone(), body.clone(), "Esc / Enter per chiudere".into()),
        Modal::ConnectorUrl(url) => (
            "URL ChatGPT (riservato)".into(),
            format!("In ChatGPT scegli No authentication e incolla questo URL:\n\n{url}\n\nL'URL contiene una credenziale. Non condividerlo."),
            "C = copia adesso negli appunti • Enter / Esc = chiudi (nessuna copia automatica)".into(),
        ),
        Modal::SetupResult(message) => ("Setup terminato".into(), message.clone(), "Invio / Esc per chiudere".into()),
        Modal::Edit(field) => {
            let title = match field { Field::Port => "Modifica porta HTTP", Field::Chrome => "Percorso Chrome", Field::Root => "Aggiungi directory autorizzata", Field::NgrokDomain => "Dominio ngrok (hostname)", Field::NovaSource => "Percorso workspace NovaGPT sorgente" };
            let error = app.input_error.clone().unwrap_or_default();
            (title.into(), format!("Inserisci il valore e premi Invio:\n\n{}▏\n\n{}", app.input, error),
                "Invio applica • Esc annulla".into())
        }
        Modal::Roots => ("Rimuovi directory".into(),
            app.draft.allowed_roots.iter().enumerate().map(|(i, p)|
                format!("{} {}", if i == app.root_selection { "›" } else { " " }, p.display())
            ).collect::<Vec<_>>().join("\n"),
            "↑↓ seleziona • Invio conferma • Esc annulla".into()),
        Modal::ImportPreview => {
            let selected = app.profiles.get(app.profile);
            ("Anteprima importazione".into(), selected.map_or("Nessun profilo.".into(), |p|
                format!("Sorgente: {}\nProfilo {}\nWorkspace profilo: {}\nPorta HTTP: {} -> {:?}\nTunnel sorgente: {}\nDominio ngrok importabile: {}\n\nImporta nella bozza solo porta, tunnel ngrok e hostname valido. Non importa token, file ngrokConfig, root, dati VM o browser.\nNessuna modifica finche' non confermi e salvi.",
                    p.origin.display(), p.source, p.root_label, app.draft.port, p.port,
                    p.tunnel.as_deref().unwrap_or("(assente)"), p.ngrok_domain.as_deref().unwrap_or("(non disponibile)"))),
            "Invio prosegui alla conferma • Esc annulla".into())
        }
        Modal::Confirm(action) => {
            let message = match action {
                Confirm::Save => if app.setup {
                    "Salvare la configurazione NovaGPT e installare il launcher locale? Nessun file esistente sara' sovrascritto."
                } else { "Salvare la configurazione in .novagpt/config.json?" },
                Confirm::Import => "Importare nella bozza porta HTTP e preferenze ngrok supportate? I token non saranno importati.",
                Confirm::RemoveRoot => "Revocare questa directory autorizzata dalla bozza?",
                Confirm::Discard => "Annullare tutte le modifiche non salvate?",
                Confirm::Stop => "Interrompere il server HTTP avviato da questa TUI?",
                Confirm::Quit => "Uscire perdendo le modifiche non salvate?",
            };
            ("Conferma richiesta".into(), message.into(),
                format!("  {}    {}  • ← → seleziona  Enter conferma  Esc annulla",
                    if app.yes { "[ SI ]" } else { "  SI  " },
                    if app.yes { "  NO  " } else { "[ NO ]" }))
        }
    };
    let height = (body.lines().count() as u16).saturating_add(6).max(8);
    let rect = centered(area, if area.width >= 70 { 68 } else { 96 }, height);
    frame.render_widget(Clear, rect);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(2)])
        .split(rect);
    let text = Paragraph::new(body).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(ACCENT))
            .title(format!(" {title} ")),
    );
    frame.render_widget(text, chunks[0]);
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(WARN)),
        chunks[1],
    );
}
pub(super) fn draw(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if area.width < 12 || area.height < 7 {
        frame.render_widget(
            Paragraph::new("NovaGPT: amplia il terminale\nCtrl+C per uscire")
                .style(Style::default().fg(WARN)),
            area,
        );
        return;
    }
    let slots = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Min(2),
            Constraint::Length(2),
        ])
        .split(area);
    frame.render_widget(Paragraph::new(title_line(app)), slots[0]);
    let tabs = Tabs::new([" Comandi ", " Stato ", " Config "])
        .select(app.tab)
        .block(panel(" Navigazione "))
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .divider(" │ ");
    frame.render_widget(tabs, slots[1]);
    match app.tab {
        0 => commands(frame, app, slots[2]),
        1 => dashboard(frame, app, slots[2]),
        _ => config(frame, app, slots[2]),
    }
    let keys = if app.modal.is_some() {
        "Modale: Enter conferma • Esc annulla"
    } else if app.tab == 1 {
        "↑↓ PgUp PgDn log • Tab schede • ? aiuto • q esci"
    } else if app.tab == 2 {
        "↑↓ seleziona • Enter modifica • s salva • Tab schede • ? aiuto • q esci"
    } else {
        "↑↓ seleziona • Enter esegue • Tab schede • ? aiuto • q esci"
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(keys, Style::default().fg(ACCENT))),
            Line::from(app.status.as_str()),
        ]),
        slots[3],
    );
    modal(frame, app);
}
