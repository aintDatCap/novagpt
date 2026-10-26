//! Stateless Ratatui widgets and responsive layout (Ratatui 0.29).
use super::model::{App, COMMANDS, CONFIG_ITEMS, Confirm, Field, Level, Modal, ServerState};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Tabs, Wrap},
};
const ACCENT: Color = Color::Rgb(104, 203, 220);
const MUTED: Color = Color::Rgb(148, 161, 176);
const OK: Color = Color::Rgb(117, 206, 160);
const WARN: Color = Color::Rgb(239, 189, 96);
const BAD: Color = Color::Rgb(245, 123, 126);
const BG: Color = Color::Rgb(14, 20, 28);
const EDGE: Color = Color::Rgb(57, 74, 91);

fn panel(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(EDGE))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ))
}
fn state(s: ServerState) -> &'static str {
    match s {
        ServerState::Stopped => "Non avviato",
        ServerState::Starting => "Avvio in corso",
        ServerState::Running => "MCP pronto",
        ServerState::Failed => "Errore di avvio",
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
    if b { "Attivo" } else { "Disattivo" }
}
fn dirty_mark(b: bool) -> &'static str {
    if b { " *" } else { "" }
}
fn friendly_root(root: &std::path::Path) -> String {
    root.to_string_lossy()
        .strip_prefix(r"\\?\")
        .unwrap_or(&root.to_string_lossy())
        .to_string()
}
fn shorten(value: &str, width: usize) -> String {
    if width < 4 {
        return String::new();
    }
    if value.chars().count() <= width {
        return value.to_owned();
    }
    let tail: String = value
        .chars()
        .rev()
        .take(width - 2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("… {tail}")
}
fn clip_label(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }
    format!(
        "{}…",
        value
            .chars()
            .take(width.saturating_sub(1))
            .collect::<String>()
    )
}

fn title_line(app: &App, width: u16) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            " NOVAGPT ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "  {}",
                shorten(&friendly_root(&app.root), width.saturating_sub(15) as usize)
            ),
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
                .map_or("(ricerca automatica)".into(), |p| p.display().to_string()),
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
    if area.width < 4 || area.height < 2 {
        return;
    }
    let list = List::new(items)
        .block(Block::default().title(Span::styled(
            format!(" {title} "),
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )))
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .highlight_symbol("› ");
    let mut list_state = ListState::default();
    list_state.select(Some(index));
    frame.render_stateful_widget(list, area, &mut list_state);
}
fn service_lines(app: &App) -> Vec<Line<'static>> {
    let browser = if !app.persisted.browser_enabled {
        "Disattivato"
    } else if app.chrome_valid == Some(false) {
        "Eseguibile non valido"
    } else {
        "Configurato · sessione non verificata"
    };
    let tunnel = if app.tunnel_ready {
        "Pronto"
    } else if app.persisted.ngrok_enabled {
        "Configurato · non connesso"
    } else {
        "Non configurato"
    };
    let next = match app.server {
        ServerState::Running if app.tunnel_ready => "Seleziona «Mostra / copia URL ChatGPT»",
        ServerState::Running => "MCP locale pronto · verifica il tunnel per ChatGPT",
        ServerState::Starting => "Attendi la verifica del server MCP…",
        _ => "Seleziona «Avvia MCP HTTP» per iniziare",
    };
    vec![
        Line::from(Span::styled(
            format!("  ●  {}", state(app.server)),
            Style::default()
                .fg(state_color(app.server))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Endpoint     ", Style::default().fg(MUTED)),
            Span::raw(format!("127.0.0.1:{}/mcp", app.persisted.port)),
        ]),
        Line::from(vec![
            Span::styled("  Browser      ", Style::default().fg(MUTED)),
            Span::raw(browser),
        ]),
        Line::from(vec![
            Span::styled("  Tunnel       ", Style::default().fg(MUTED)),
            Span::styled(
                tunnel,
                Style::default().fg(if app.tunnel_ready { OK } else { MUTED }),
            ),
        ]),
        Line::from(Span::styled(
            "  Il token MCP concede accesso anche ai comandi host.",
            Style::default().fg(WARN),
        )),
        Line::from(Span::styled(
            format!("  {next}"),
            Style::default().fg(ACCENT),
        )),
    ]
}
fn commands(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let wide = area.width >= 104 && area.height >= 14;
    let split = if wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(62), Constraint::Percentage(38)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(8), Constraint::Length(0)])
            .split(area)
    };
    let left = split[0];
    let top_height = 9.min(left.height.saturating_sub(4));
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(top_height), Constraint::Min(3)])
        .split(left);
    if sections[0].height >= 4 {
        frame.render_widget(
            Paragraph::new(service_lines(app))
                .wrap(Wrap { trim: true })
                .block(panel("Panoramica dei servizi")),
            sections[0],
        );
    }
    let actions = app.visible_commands();
    let current = actions
        .iter()
        .position(|&index| index == app.command)
        .unwrap_or(0);
    let items = actions
        .iter()
        .map(|&index| {
            let suffix = match index {
                0 | 1 | 7 => state(app.server),
                8 if app.tunnel_ready => "Tunnel disponibile",
                8 => "Richiede tunnel",
                3 | 4 if !app.persisted.vm_enabled => "VM disattivata",
                5 if !app.persisted.browser_enabled => "Browser disattivato",
                _ => "",
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("  {}  ", COMMANDS[index].0)),
                Span::styled(suffix.to_string(), Style::default().fg(MUTED)),
            ]))
        })
        .collect();
    let title = if app.show_advanced {
        "Azioni · a: nascondi avanzate"
    } else {
        "Azioni · a: mostra avanzate"
    };
    selected_list(frame, sections[1], title, items, current);
    if wide && split[1].width >= 14 {
        let recent = app.logs.iter().rev().take(5).collect::<Vec<_>>();
        let mut lines = vec![
            Line::from(Span::styled(
                " ULTIMA ATTIVITÀ",
                Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
        ];
        for event in recent {
            let color = match event.level {
                Level::Info => MUTED,
                Level::Warn => WARN,
                Level::Error => BAD,
                Level::Success => OK,
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}  {}",
                    event.time.split_whitespace().next().unwrap_or(""),
                    event.message
                ),
                Style::default().fg(color),
            )));
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            "Per tutti gli eventi, apri Attività.",
            Style::default().fg(MUTED),
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), split[1]);
    }
}
fn dashboard(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let height = if area.height > 12 { 6 } else { 4 };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(height), Constraint::Min(2)])
        .split(area);
    let summary = [
        Line::from(Span::styled(
            " STATO MCP E COMPONENTI",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(format!(
            " MCP: {}    Porta: {}    ngrok: {}",
            state(app.server),
            app.persisted.port,
            if app.tunnel_ready {
                "pronto"
            } else {
                "non verificato"
            }
        )),
        Line::from(format!(
            " Chrome: {}    VM: {}",
            if app.persisted.browser_enabled {
                "configurato (non verificato)"
            } else {
                "off"
            },
            if app.persisted.vm_enabled {
                "on"
            } else {
                "off"
            }
        )),
        Line::from(format!(" Workspace: {}", friendly_root(&app.root))),
        Line::from(if app.restart_needed() {
            " Configurazione salvata: riavvio MCP necessario"
        } else {
            " Configurazione: nessun riavvio richiesto"
        }),
    ];
    frame.render_widget(
        Paragraph::new(summary.to_vec()).wrap(Wrap { trim: true }),
        sections[0],
    );
    let visible = sections[1].height.saturating_sub(2) as usize;
    let end = app.logs.len().saturating_sub(app.log_offset);
    let start = end.saturating_sub(visible);
    let items = app
        .logs
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|entry| {
            let (label, color) = match entry.level {
                Level::Info => ("INFO ", MUTED),
                Level::Warn => ("AVVISO", WARN),
                Level::Error => ("ERRORE", BAD),
                Level::Success => ("OK   ", OK),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{}  ", entry.time), Style::default().fg(MUTED)),
                Span::styled(
                    format!("{label}  "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(entry.message.clone()),
            ]))
        })
        .collect::<Vec<_>>();
    let caption = format!(
        "Eventi · {} di {} · ↑↓ PgUp/PgDn per scorrere",
        end,
        app.logs.len()
    );
    frame.render_widget(List::new(items).block(panel(&caption)), sections[1]);
}
fn config_help(index: usize) -> &'static str {
    match index {
        0 => "Porta sulla quale NovaGPT espone MCP localmente.",
        1 => "Abilita le funzioni VM opzionali. Richiede un riavvio del server.",
        2 => "Prepara il browser dedicato NovaGPT. Non verifica l'accesso a ChatGPT.",
        3 => "Lascia vuoto per rilevare Chrome automaticamente.",
        4 | 5 => "Aggiungi o revoca altre directory di lavoro autorizzate.",
        6 | 7 => "Importa solo impostazioni di connessione, dopo anteprima e consenso.",
        8 => "Salva le modifiche locali. Alcune impostazioni richiedono riavvio MCP.",
        9 => "Scarta tutte le modifiche non ancora salvate.",
        10 => "Verifica o genera il token HTTP senza mostrarlo sullo schermo.",
        11 => "Avvia ngrok assieme al server MCP quando la preferenza è attiva.",
        12 => "Hostname ngrok opzionale. Non inserire URL completi o credenziali.",
        13 => "Importa la configurazione pubblica di un altro workspace NovaGPT.",
        _ => "Procedi al passo successivo del setup guidato.",
    }
}
fn config(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let top_h = if app.setup {
        if area.height >= 16 { 4 } else { 2 }
    } else {
        0
    };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(top_h), Constraint::Min(2)])
        .split(area);
    if app.setup && top_h > 0 {
        let prompt = match app.step {
            0 => "SETUP 1/4 · Workspace · n: avanti",
            1 => "SETUP 2/4 · Parametri · Enter: modifica",
            2 => "SETUP 3/4 · Importazione facoltativa · n: avanti",
            3 => "SETUP 4/4 · Conferma e salva",
            _ => "SETUP COMPLETATO",
        };
        frame.render_widget(
            Paragraph::new(prompt).style(Style::default().fg(ACCENT)),
            sections[0],
        );
    }
    let wide = sections[1].width >= 104 && sections[1].height >= 12;
    let split = if wide {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(68), Constraint::Percentage(32)])
            .split(sections[1])
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(0)])
            .split(sections[1])
    };
    let mut rows = Vec::new();
    let mut selected_row = 0;
    let mut section = "";
    for &i in app.config_order() {
        let group = match i {
            0 | 11 | 12 | 10 => "CONNESSIONE",
            2 | 3 => "BROWSER",
            1 => "MACCHINE VIRTUALI",
            4 | 5 => "DIRECTORY AUTORIZZATE",
            6 | 7 | 13 => "IMPORTAZIONE",
            8 | 9 => "APPLICA MODIFICHE",
            _ => "SETUP GUIDATO",
        };
        if section != group {
            if !rows.is_empty() {
                rows.push(ListItem::new(""));
            }
            rows.push(ListItem::new(Line::from(Span::styled(
                format!("  {group}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))));
            section = group;
        }
        if i == app.config {
            selected_row = rows.len();
        }
        let value = config_value(app, i);
        let invalid =
            (i == 2 || i == 3) && app.draft.browser_enabled && app.chrome_valid == Some(false);
        let available = split[0].width.saturating_sub(5) as usize;
        let label_width = if available >= 70 {
            35
        } else {
            (available / 2).clamp(12, 30)
        };
        let value_width = available.saturating_sub(label_width + 2);
        let label = clip_label(CONFIG_ITEMS[i], label_width);
        let label = format!("  {label:<label_width$}  ");
        rows.push(ListItem::new(Line::from(vec![
            Span::styled(
                label,
                Style::default().fg(if invalid { BAD } else { Color::White }),
            ),
            Span::styled(
                shorten(&value, value_width),
                Style::default().fg(if invalid { BAD } else { MUTED }),
            ),
        ])));
    }
    selected_list(
        frame,
        split[0],
        "Impostazioni · ↑↓ seleziona  Enter modifica",
        rows,
        selected_row,
    );
    if wide && split[1].width > 12 {
        let status = if app.dirty() {
            "● Modifiche da salvare"
        } else {
            "✓ Configurazione salvata"
        };
        let status_color = if app.dirty() { WARN } else { OK };
        let lines = vec![
            Line::from(Span::styled(
                status,
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "CAMPO SELEZIONATO",
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                CONFIG_ITEMS[app.config],
                Style::default().fg(ACCENT),
            )),
            Line::from(""),
            Line::from(config_help(app.config)),
            Line::from(""),
            Line::from(Span::styled("SCORCIATOIE", Style::default().fg(MUTED))),
            Line::from("Enter  Modifica o esegui"),
            Line::from("s      Salva modifiche"),
            Line::from(""),
            Line::from(if app.restart_needed() {
                "Riavvio MCP necessario per applicare la configurazione."
            } else {
                "Le modifiche salvate saranno usate al prossimo avvio."
            }),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), split[1]);
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
            "Tab / Shift+Tab: Panoramica, Attività, Impostazioni\n↑ ↓: seleziona o scorri i log\nEnter: esegui, modifica o conferma\na: mostra/nascondi azioni avanzate in Panoramica\ns: salva da Impostazioni\nEsc: chiudi/indietro   q: esci\n?: aiuto   Ctrl+C: uscita controllata\nAttività: PgUp/PgDn, Home/End scorrono gli eventi\nImpostazioni: * indica un valore modificato\nSetup: n avanti, b indietro\nScorciatoie 1–4: operazioni rapide.".into(),
            "Esc / Enter per chiudere".into()),
        Modal::Notice {title, body} => (title.clone(), body.clone(), "Esc / Enter per chiudere".into()),
        Modal::ConnectorUrl(url) => (
            "URL ChatGPT (riservato)".into(),
            format!("In ChatGPT scegli No authentication e incolla questo URL:\n\n{url}\n\nAttenzione: l'URL contiene una credenziale che permette anche di eseguire comandi host tramite MCP. Non condividerlo."),
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
                format!("Sorgente: {}\nProfilo {}\nWorkspace profilo: {}\nPorta HTTP: {} -> {}\nTunnel sorgente: {}\nDominio ngrok importabile: {}\n\nImporta nella bozza solo porta, tunnel ngrok e hostname valido. Non importa token, file ngrokConfig, root, dati VM o browser.\nNessuna modifica finche' non confermi e salvi.",
                    p.origin.display(), p.source, p.root_label, app.draft.port, p.port.map_or_else(|| "non disponibile".to_string(), |port| port.to_string()),
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
    // Account for wrapped lines, especially long connector URLs and help text.
    // Never draw beyond the terminal or expose the URL outside its modal.
    let width_percent: u16 = if area.width >= 100 {
        65
    } else if area.width >= 70 {
        82
    } else {
        96
    };
    let estimated_width = (area.width as usize * width_percent as usize / 100)
        .saturating_sub(2)
        .max(1);
    let body_rows: usize = body
        .lines()
        .map(|line| line.chars().count().max(1).div_ceil(estimated_width))
        .sum();
    let height = (body_rows as u16).saturating_add(5).max(8).min(area.height);
    let rect = centered(area, width_percent, height);
    let footer = if rect.width < 70 {
        match modal {
            Modal::ConnectorUrl(_) => "C copia · Esc chiudi".to_string(),
            Modal::Confirm(_) => "←→ scegli · Enter conferma · Esc annulla".to_string(),
            Modal::Edit(_) => "Enter applica · Esc annulla".to_string(),
            _ => footer,
        }
    } else {
        footer
    };
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
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
    if area.width < 28 || area.height < 10 {
        frame.render_widget(
            Paragraph::new("NovaGPT · amplia il terminale\nCtrl+C per uscire")
                .style(Style::default().fg(WARN)),
            area,
        );
        return;
    }
    let margin = if area.width < 70 { 1 } else { 2 };
    let inner = Rect {
        x: area.x.saturating_add(margin),
        y: area.y,
        width: area.width.saturating_sub(margin * 2),
        height: area.height,
    };
    let slots = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Length(2),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(inner);
    frame.render_widget(Paragraph::new(title_line(app, inner.width)), slots[0]);
    let tabs = Tabs::new([" Panoramica ", " Attività ", " Impostazioni "])
        .select(app.tab)
        .highlight_style(Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))
        .divider("  │  ");
    frame.render_widget(tabs, slots[1]);
    match app.tab {
        0 => commands(frame, app, slots[2]),
        1 => dashboard(frame, app, slots[2]),
        _ => config(frame, app, slots[2]),
    }
    let keys = if app.modal.is_some() {
        "Enter conferma · Esc annulla"
    } else {
        match app.tab {
            0 => "↑↓ azioni   Enter esegui   a avanzate   Tab schede   ? aiuto   q esci",
            1 => "↑↓ / PgUp PgDn eventi   Tab schede   ? aiuto   q esci",
            _ => "↑↓ campi   Enter modifica   s salva   Tab schede   ? aiuto   q esci",
        }
    };
    let foot = vec![
        Line::from(Span::styled(keys, Style::default().fg(ACCENT))),
        Line::from(Span::styled(
            shorten(&app.status, inner.width.saturating_sub(1) as usize),
            Style::default().fg(MUTED),
        )),
    ];
    frame.render_widget(Paragraph::new(foot), slots[3]);
    modal(frame, app);
}
