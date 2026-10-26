//! Stateless Ratatui widgets and responsive layout (Ratatui 0.29).
use super::model::{
    App, AuthState, Confirm, Field, Level, Modal, ServerState, command_label, config_label,
};
use crate::i18n::{Language, LanguageChoice, Messages};
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
fn state(s: ServerState, lang: Language) -> &'static str {
    match s {
        ServerState::Stopped => Messages::server_stopped(lang),
        ServerState::Starting => Messages::server_starting(lang),
        ServerState::Running => Messages::server_ready(lang),
        ServerState::Failed => Messages::server_failed(lang),
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
fn on(b: bool, lang: Language) -> &'static str {
    if b {
        Messages::enabled(lang)
    } else {
        Messages::disabled_3(lang)
    }
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
    let lang = app.language();
    match index {
        0 => format!(
            "{}{}",
            app.draft.port,
            dirty_mark(app.draft.port != app.persisted.port)
        ),
        1 => format!(
            "{}{}",
            on(app.draft.vm_enabled, lang),
            dirty_mark(app.draft.vm_enabled != app.persisted.vm_enabled)
        ),
        2 => format!(
            "{}{}",
            on(app.draft.browser_enabled, lang),
            dirty_mark(app.draft.browser_enabled != app.persisted.browser_enabled)
        ),
        3 => format!(
            "{}{}",
            app.draft
                .chrome_executable
                .as_ref()
                .map_or(Messages::automatic_detection(lang).into(), |p| p
                    .display()
                    .to_string()),
            dirty_mark(app.draft.chrome_executable != app.persisted.chrome_executable)
        ),
        4 | 5 => format!(
            "{} {}{}",
            app.draft.allowed_roots.len(),
            if app.language() == crate::i18n::Language::English {
                if app.draft.allowed_roots.len() == 1 {
                    "directory"
                } else {
                    "directories"
                }
            } else {
                "directory"
            },
            dirty_mark(app.draft.allowed_roots != app.persisted.allowed_roots)
        ),
        6 => app
            .profiles
            .get(app.profile)
            .map_or(Messages::none(lang).into(), |p| p.root_label.clone()),
        7 => app
            .profiles
            .get(app.profile)
            .and_then(|p| p.port)
            .map_or(Messages::unavailable(lang).into(), |p| {
                Messages::http_port_approval(lang, p)
            }),
        8 => {
            if app.busy {
                Messages::in_progress(lang).into()
            } else if app.dirty() {
                Messages::pending_changes(lang).into()
            } else {
                Messages::current_values(lang).into()
            }
        }
        9 => Messages::restore_last_save(lang).into(),
        10 => match app.auth_state {
            AuthState::NotVerified => Messages::not_verified(lang).into(),
            AuthState::Ready => Messages::ready(lang).into(),
            AuthState::Error => Messages::error(lang).into(),
        },
        11 => format!(
            "{}{}",
            on(app.draft.ngrok_enabled, lang),
            dirty_mark(app.draft.ngrok_enabled != app.persisted.ngrok_enabled)
        ),
        12 => format!(
            "{}{}",
            app.draft
                .ngrok_domain
                .as_deref()
                .unwrap_or(Messages::not_set(lang)),
            dirty_mark(app.draft.ngrok_domain != app.persisted.ngrok_domain)
        ),
        13 => Messages::choose_source_folder(lang).into(),
        15 => match app.draft.language {
            LanguageChoice::English => "English".into(),
            crate::i18n::LanguageChoice::Italian => Messages::language_italian(lang).into(),
            LanguageChoice::Auto => Messages::auto_language(
                lang,
                if app.system_language == crate::i18n::Language::Italian {
                    "Italiano"
                } else {
                    "English"
                },
            ),
        },
        _ => Messages::setup_step(lang, app.step.min(3) + 1),
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
    let lang = app.language();
    let browser = if !app.persisted.browser_enabled {
        Messages::disabled_3(lang)
    } else if app.chrome_valid == Some(false) {
        Messages::invalid_executable(lang)
    } else {
        Messages::configured_session_unverified(lang)
    };
    let tunnel = if app.tunnel_ready {
        Messages::ready(lang)
    } else if app.persisted.ngrok_enabled {
        Messages::configured_not_connected(lang)
    } else {
        Messages::not_configured(lang)
    };
    let next = match app.server {
        ServerState::Running if app.tunnel_ready => Messages::next_show_url(lang),
        ServerState::Running => Messages::next_check_tunnel(lang),
        ServerState::Starting => Messages::waiting_for_mcp_server_verification(lang),
        _ => Messages::next_start_mcp(lang),
    };
    vec![
        Line::from(Span::styled(
            format!("  ●  {}", state(app.server, lang)),
            Style::default()
                .fg(state_color(app.server))
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(Messages::endpoint_label(lang), Style::default().fg(MUTED)),
            Span::raw(format!("127.0.0.1:{}/mcp", app.persisted.port)),
        ]),
        Line::from(vec![
            Span::styled(Messages::browser_label(lang), Style::default().fg(MUTED)),
            Span::raw(browser),
        ]),
        Line::from(vec![
            Span::styled(Messages::tunnel_label(lang), Style::default().fg(MUTED)),
            Span::styled(
                tunnel,
                Style::default().fg(if app.tunnel_ready { OK } else { MUTED }),
            ),
        ]),
        Line::from(Span::styled(
            Messages::the_mcp_token_also_grants_access_to(lang),
            Style::default().fg(WARN),
        )),
        Line::from(Span::styled(
            format!("  {next}"),
            Style::default().fg(ACCENT),
        )),
    ]
}
fn commands(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lang = app.language();
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
                .block(panel(Messages::main_page_header(lang))),
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
                0 | 1 | 7 => state(app.server, lang),
                8 if app.tunnel_ready => Messages::tunnel_available(lang),
                8 => Messages::tunnel_required(lang),
                3 | 4 if !app.persisted.vm_enabled => Messages::vm_disabled(lang),
                5 if !app.persisted.browser_enabled => Messages::browser_disabled(lang),
                9 if app.browser_login_running => Messages::agent_browser_window_open(lang),
                9 if !app.persisted.browser_enabled => Messages::browser_disabled(lang),
                _ => "",
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("  {}  ", command_label(index, lang))),
                Span::styled(suffix, Style::default().fg(MUTED)),
            ]))
        })
        .collect();
    let title = if app.show_advanced {
        Messages::actions_hide_advanced(lang)
    } else {
        Messages::actions_a_show_advanced(lang)
    };
    selected_list(frame, sections[1], title, items, current);
    if wide && split[1].width >= 14 {
        let recent = app.logs.iter().rev().take(5).collect::<Vec<_>>();
        let mut lines = vec![
            Line::from(Span::styled(
                Messages::recent_activity(lang),
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
                    if app.language() == crate::i18n::Language::English {
                        &event.english
                    } else {
                        &event.message
                    }
                ),
                Style::default().fg(color),
            )));
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            Messages::for_all_events_open_activity(lang),
            Style::default().fg(MUTED),
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), split[1]);
    }
}
fn dashboard(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lang = app.language();
    let height = if area.height > 12 { 6 } else { 4 };
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(height), Constraint::Min(2)])
        .split(area);
    let summary = [
        Line::from(Span::styled(
            Messages::mcp_and_component_status(lang),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Messages::status_summary(
            lang,
            state(app.server, lang),
            app.persisted.port,
            if app.tunnel_ready {
                Messages::ready(lang)
            } else {
                Messages::not_verified(lang)
            },
        )),
        Line::from(Messages::component_summary(
            lang,
            if app.persisted.browser_enabled {
                Messages::configured_unverified(lang)
            } else {
                "off"
            },
            if app.persisted.vm_enabled {
                "on"
            } else {
                "off"
            },
        )),
        Line::from(format!(" Workspace: {}", friendly_root(&app.root))),
        Line::from(if app.restart_needed() {
            Messages::settings_saved_mcp_restart_required(lang)
        } else {
            Messages::settings_no_restart_required(lang)
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
                Span::raw(if app.language() == crate::i18n::Language::English {
                    &entry.english
                } else {
                    &entry.message
                }),
            ]))
        })
        .collect::<Vec<_>>();
    let caption = Messages::event_count(lang, end, app.logs.len());
    frame.render_widget(List::new(items).block(panel(&caption)), sections[1]);
}
fn config_help(index: usize, lang: Language) -> &'static str {
    match index {
        0 => Messages::port_on_which_novagpt_exposes_mcp_locally(lang),
        1 => Messages::enable_optional_vm_features_requires_a_server(lang),
        2 => Messages::prepare_novagpt_s_dedicated_browser_chatgpt_login(lang),
        3 => Messages::leave_blank_to_detect_chrome_automatically(lang),
        4 | 5 => Messages::authorized_directories_help(lang),
        6 | 7 => Messages::import_connection_help(lang),
        8 => Messages::save_local_changes_some_settings_require_restarting(lang),
        9 => Messages::discard_all_unsaved_changes(lang),
        10 => Messages::verify_or_generate_the_http_token_without(lang),
        11 => Messages::start_ngrok_with_mcp_when_this_setting(lang),
        12 => Messages::optional_ngrok_hostname_do_not_enter_complete(lang),
        13 => Messages::import_novagpt_help(lang),
        15 => Messages::choose_english_italian_or_automatic_system_language(lang),
        _ => Messages::next_setup_help(lang),
    }
}
fn config(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let lang = app.language();
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
            0 => Messages::setup_workspace(lang),
            1 => Messages::setup_2_4_settings_enter_edit(lang),
            2 => Messages::setup_optional_import(lang),
            3 => Messages::setup_4_4_confirm_and_save(lang),
            _ => Messages::setup_complete(lang),
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
            15 => Messages::settings_general(lang),
            0 | 11 | 12 | 10 => Messages::settings_connection(lang),
            2 | 3 => Messages::settings_browser_heading(lang),
            1 => Messages::virtual_machines(lang),
            4 | 5 => Messages::authorized_directories(lang),
            6 | 7 | 13 => Messages::import(lang),
            8 | 9 => Messages::apply_changes(lang),
            _ => Messages::guided_setup(lang),
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
        let label = clip_label(config_label(i, lang), label_width);
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
        Messages::settings_select_enter_edit(lang),
        rows,
        selected_row,
    );
    if wide && split[1].width > 12 {
        let status = if app.dirty() {
            Messages::unsaved_changes_2(lang)
        } else {
            Messages::settings_saved_2(lang)
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
                Messages::selected_field(lang),
                Style::default().fg(MUTED),
            )),
            Line::from(Span::styled(
                config_label(app.config, lang),
                Style::default().fg(ACCENT),
            )),
            Line::from(""),
            Line::from(config_help(app.config, lang)),
            Line::from(""),
            Line::from(Span::styled(
                Messages::shortcuts(lang),
                Style::default().fg(MUTED),
            )),
            Line::from(Messages::enter_edit_or_run(lang)),
            Line::from(Messages::s_save_changes(lang)),
            Line::from(""),
            Line::from(if app.restart_needed() {
                Messages::mcp_restart_required_to_apply_settings(lang)
            } else {
                Messages::saved_changes_will_apply_at_the_next(lang)
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
    let lang = app.language();
    let Some(modal) = &app.modal else { return };
    let area = frame.area();
    if area.width < 3 || area.height < 3 {
        return;
    }
    let (title, body, footer): (String, String, String) = match modal {
        Modal::Help => (
            Messages::quick_help(lang).into(),
            Messages::help_body(lang).into(),
            Messages::esc_enter_to_close(lang).into(),
        ),
        Modal::Notice { title, body } => (
            title.clone(),
            body.clone(),
            Messages::esc_enter_to_close(lang).into(),
        ),
        Modal::ConnectorUrl(url) => (
            Messages::chatgpt_url_private(lang).into(),
            format!(
                "{}\n\n{url}\n\n{}",
                Messages::in_chatgpt_choose_no_authentication_and_paste(lang),
                Messages::warn_credential_url(lang)
            ),
            Messages::c_copy_to_clipboard_enter_esc_close(lang).into(),
        ),
        Modal::SetupResult(message) => (
            Messages::setup_complete_2(lang).into(),
            message.clone(),
            Messages::enter_esc_to_close(lang).into(),
        ),
        Modal::Edit(field) => {
            let title = match field {
                Field::Port => Messages::edit_http_port(lang),
                Field::Chrome => Messages::chrome_path(lang),
                Field::Root => Messages::add_authorized_directory(lang),
                Field::NgrokDomain => Messages::ngrok_domain_hostname(lang),
                Field::NovaSource => Messages::edit_novagpt_source_path(lang),
            };
            (
                title.into(),
                format!(
                    "{}\n\n{}▏\n\n{}",
                    Messages::edit_input_prompt(lang),
                    app.input,
                    app.input_error.as_deref().unwrap_or("")
                ),
                Messages::enter_apply_esc_cancel(lang).into(),
            )
        }
        Modal::Roots => (
            Messages::remove_directory(lang).into(),
            app.draft
                .allowed_roots
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    format!(
                        "{} {}",
                        if i == app.root_selection { "›" } else { " " },
                        p.display()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Messages::select_enter_confirm_esc_cancel(lang).into(),
        ),
        Modal::ImportPreview => {
            let selected = app.profiles.get(app.profile);
            let body = selected.map_or_else(
                || Messages::no_profiles_available(lang).to_owned(),
                |p| {
                    format!(
                        "{} {}\n{} {}\n{} {}\n{} {} -> {}\n{} {}\n{} {}\n\n{}\n{}",
                        Messages::source(lang),
                        p.origin.display(),
                        Messages::profile(lang),
                        p.source,
                        Messages::profile_workspace(lang),
                        p.root_label,
                        Messages::connector_http_port(lang),
                        app.draft.port,
                        p.port.map_or_else(
                            || Messages::unavailable_2(lang).to_owned(),
                            |port| port.to_string()
                        ),
                        Messages::source_tunnel(lang),
                        p.tunnel.as_deref().unwrap_or("—"),
                        Messages::importable_ngrok_hostname(lang),
                        p.ngrok_domain.as_deref().unwrap_or("—"),
                        Messages::only_port_ngrok_tunnel_and_valid_hostname(lang),
                        Messages::no_changes_until_you_confirm_and_save(lang),
                    )
                },
            );
            (
                Messages::import_preview(lang).into(),
                body,
                Messages::enter_to_confirm_esc_cancel(lang).into(),
            )
        }
        Modal::Confirm(action) => {
            let message = match action {
                Confirm::Save if app.setup => {
                    Messages::save_novagpt_settings_and_install_the_local(lang)
                }
                Confirm::Save => Messages::save_settings_in_novagpt_config_json(lang),
                Confirm::Import => Messages::confirm_import_port(lang),
                Confirm::RemoveRoot => {
                    Messages::revoke_this_authorized_directory_from_the_draft(lang)
                }
                Confirm::Discard => Messages::discard_all_unsaved_changes_2(lang),
                Confirm::Stop => Messages::stop_the_http_server_started_by_this(lang),
                Confirm::Quit => Messages::quit_and_lose_unsaved_changes(lang),
            };
            let choices = format!(
                "  {}    {}  • {}",
                if app.yes {
                    Messages::edit_confirm_yes(lang)
                } else if lang == Language::English {
                    "  YES  "
                } else {
                    "  SI  "
                },
                if app.yes {
                    "  NO  "
                } else {
                    Messages::edit_confirm_no(lang)
                },
                Messages::modal_confirm_short(lang)
            );
            (
                Messages::confirmation_required(lang).into(),
                message.into(),
                choices,
            )
        }
    };
    // Wrap only the already localized text. Dynamic URLs and file paths stay byte-identical.
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
            Modal::ConnectorUrl(_) => Messages::c_copy_esc_close(lang).to_owned(),
            Modal::Confirm(_) => Messages::modal_confirm_short(lang).to_owned(),
            Modal::Edit(_) => Messages::enter_apply_esc_cancel_2(lang).to_owned(),
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
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(ACCENT))
                .title(format!(" {title} ")),
        ),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(WARN)),
        chunks[1],
    );
}
pub(super) fn draw(frame: &mut Frame<'_>, app: &App) {
    let lang = app.language();
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG)), area);
    if area.width < 28 || area.height < 10 {
        frame.render_widget(
            Paragraph::new(Messages::terminal_too_small(lang)).style(Style::default().fg(WARN)),
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
    let tabs = Tabs::new([
        Messages::tab_overview(lang),
        Messages::tab_activity(lang),
        Messages::tab_settings(lang),
    ])
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
        Messages::enter_confirm_esc_cancel(lang)
    } else {
        match app.tab {
            0 => Messages::keys_overview(lang),
            1 => Messages::keys_activity(lang),
            _ => Messages::fields_enter_edit_s_save_tab_views(lang),
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
