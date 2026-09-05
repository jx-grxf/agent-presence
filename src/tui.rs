//! Interactive settings editor.
//!
//! The config file stays the source of truth and hand-editing it is fully supported —
//! this is a front end for it, not a replacement.
//!
//! The point of the live card preview is that `detail` is a privacy decision. Seeing
//! the repository name appear the moment you leave `generic` is worth more than any
//! amount of documentation about it.

use crate::config::{Config, ConfigButton, Detail};
use crate::daemon::presence;
use crate::daemon::registry::Session;
use crate::daemon::registry::Snapshot;
use crate::event::{Activity, Agent};
use crate::ipc;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Padding, Paragraph};
use std::time::{Duration, Instant};

/// Which group a setting belongs to. Eleven flat rows read as a wall; grouped, the two
/// that decide what strangers can see sit together at the top under their own heading.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Section {
    Privacy,
    Behaviour,
    Card,
    Advanced,
}

impl Section {
    fn title(self) -> &'static str {
        match self {
            Section::Privacy => "Privacy",
            Section::Behaviour => "Behaviour",
            Section::Card => "Card",
            Section::Advanced => "Advanced",
        }
    }
}

/// Rows in the settings list, in display order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Detail,
    HiddenPaths,
    ShowModel,
    Enabled,
    FollowFocus,
    IdleTimeout,
    Buttons,
    UpdateCheck,
    AutoUpdate,
    ClientId,
}

impl Field {
    const ALL: [Field; 10] = [
        Field::Detail,
        Field::HiddenPaths,
        Field::ShowModel,
        Field::Enabled,
        Field::FollowFocus,
        Field::IdleTimeout,
        Field::Buttons,
        Field::UpdateCheck,
        Field::AutoUpdate,
        Field::ClientId,
    ];

    fn section(self) -> Section {
        match self {
            Field::Detail | Field::HiddenPaths | Field::ShowModel => Section::Privacy,
            Field::Enabled | Field::FollowFocus | Field::IdleTimeout => Section::Behaviour,
            Field::Buttons => Section::Card,
            Field::UpdateCheck | Field::AutoUpdate | Field::ClientId => Section::Advanced,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Field::Detail => "Detail",
            Field::ShowModel => "Show model",
            Field::FollowFocus => "Follow focus",
            Field::Enabled => "Enabled",
            Field::UpdateCheck => "Update check",
            Field::AutoUpdate => "Auto update",
            Field::IdleTimeout => "Idle timeout",
            Field::HiddenPaths => "Hidden paths",
            Field::Buttons => "Link buttons",
            Field::ClientId => "Application ID",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Field::Detail => "How much of your workspace reaches Discord. generic leaks nothing.",
            Field::ShowModel => "Append the model name, e.g. \"· Opus 4.8\".",
            Field::FollowFocus => "Show the session in the focused terminal window (macOS).",
            Field::Enabled => "Master switch. Off keeps the hooks installed but clears the card.",
            Field::UpdateCheck => "Ask GitHub once a day whether a newer release exists.",
            Field::AutoUpdate => "Install it too, once no session is live. Never self-overwrites.",
            Field::IdleTimeout => "Drop sessions silent this long. Accepts 30s, 15m, 2h.",
            Field::HiddenPaths => "Globs always forced to generic. Comma separated, ~ expands.",
            Field::Buttons => "Up to two links on the card. Label|https://url, comma separated.",
            Field::ClientId => "Your own Discord application. Empty uses the bundled one.",
        }
    }

    /// Whether Enter opens the text editor rather than toggling in place.
    fn is_text(self) -> bool {
        matches!(
            self,
            Field::IdleTimeout | Field::HiddenPaths | Field::Buttons | Field::ClientId
        )
    }
}

enum Mode {
    Browse,
    /// Editing `field` with a scratch buffer, committed on Enter and dropped on Esc.
    Edit {
        field: Field,
        buffer: String,
    },
    Saved(Instant),
    /// The notes for the running version, so "what changed?" has an answer that does not
    /// involve leaving the terminal.
    WhatsNew {
        scroll: u16,
    },
}

/// Everything the header reports that is not a setting. Gathered once before the
/// alternate screen opens, except the session list, which is refreshed on a timer.
struct Status {
    daemon: Option<u32>,
    discord: Option<String>,
    sessions: Option<ipc::SessionsReply>,
    update: Option<String>,
}

struct App {
    config: Config,
    original: Config,
    status: Status,
    selected: usize,
    mode: Mode,
    error: Option<String>,
    quit: bool,
    /// Set by `u`, acted on after the alternate screen is closed — the package manager
    /// writes to the real terminal, and it is worth watching.
    run_update: bool,
}

impl App {
    fn field(&self) -> Field {
        Field::ALL[self.selected]
    }
}

/// How often the header re-asks the daemon what it is tracking.
const SESSION_REFRESH: Duration = Duration::from_secs(2);

pub async fn run() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "`config` needs a terminal — edit {} directly instead",
            crate::config::config_path().display()
        );
    }

    let config = Config::load();
    // Probed before the screen is taken over: an async call inside a draw callback is not
    // possible, and neither answer changes often enough to matter.
    let discord = crate::probe_discord().await;
    let mut app = App {
        status: Status {
            daemon: crate::daemon::running_pid(),
            discord,
            sessions: ipc::sessions_or_none().await,
            update: crate::update::available(),
        },
        original: config.clone(),
        config,
        selected: 0,
        mode: Mode::Browse,
        error: None,
        quit: false,
        run_update: false,
    };

    let mut terminal = enter()?;
    // Restore the terminal even if drawing fails, otherwise the user is left in raw mode
    // on the alternate screen with no echo.
    let result = event_loop(&mut terminal, &mut app).await;
    leave(terminal)?;
    result?;

    if app.run_update {
        return crate::update::run(false);
    }
    if changed(&app.original, &app.config) {
        println!("Left unsaved changes — nothing was written.");
    }
    Ok(())
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let mut refreshed = Instant::now();
    while !app.quit {
        terminal.draw(|frame| draw(frame, app))?;

        // Crossterm's poll blocks the thread, so it goes through `block_in_place` rather
        // than stalling the runtime the session query runs on.
        let key = tokio::task::block_in_place(|| -> Result<Option<_>> {
            // Polling rather than blocking, so the "saved" flash can expire on its own.
            if event::poll(Duration::from_millis(120))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        return Ok(Some((key.code, key.modifiers)));
                    }
                }
            }
            Ok(None)
        })?;
        if let Some((code, mods)) = key {
            handle_key(app, code, mods);
        }

        if refreshed.elapsed() > SESSION_REFRESH {
            refreshed = Instant::now();
            app.status.daemon = crate::daemon::running_pid();
            app.status.sessions = ipc::sessions_or_none().await;
        }
        if let Mode::Saved(at) = app.mode {
            if at.elapsed() > Duration::from_millis(1200) {
                app.mode = Mode::Browse;
            }
        }
    }
    Ok(())
}

fn enter() -> Result<Terminal<CrosstermBackend<std::io::Stdout>>> {
    enable_raw_mode()?;
    let mut out = std::io::stdout();
    crossterm::execute!(out, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(out))?)
}

fn leave(mut terminal: Terminal<CrosstermBackend<std::io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

fn changed(a: &Config, b: &Config) -> bool {
    // No PartialEq on Config, and serialising is exactly the comparison that matters:
    // two configs are the same if they would write the same file.
    toml::to_string(a).ok() != toml::to_string(b).ok()
}

fn handle_key(app: &mut App, code: KeyCode, mods: KeyModifiers) {
    if let Mode::Edit { field, buffer } = &mut app.mode {
        let (field, buffer) = (*field, buffer);
        match code {
            KeyCode::Esc => app.mode = Mode::Browse,
            KeyCode::Enter => {
                let value = buffer.clone();
                match commit(&mut app.config, field, &value) {
                    Ok(()) => {
                        app.error = None;
                        app.mode = Mode::Browse;
                    }
                    // Stay in the editor on a bad value, so the typing is not lost.
                    Err(e) => app.error = Some(format!("{e:#}")),
                }
            }
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Char(c) => buffer.push(c),
            _ => {}
        }
        return;
    }

    if let Mode::WhatsNew { scroll } = &mut app.mode {
        match code {
            KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
            _ => app.mode = Mode::Browse,
        }
        return;
    }

    app.error = None;
    let field = app.field();
    match code {
        KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => app.quit = true,
        KeyCode::Up | KeyCode::Char('k') => {
            app.selected = app.selected.checked_sub(1).unwrap_or(Field::ALL.len() - 1)
        }
        KeyCode::Down | KeyCode::Char('j') => app.selected = (app.selected + 1) % Field::ALL.len(),
        KeyCode::Char('r') => {
            app.config = Config::default();
        }
        KeyCode::Char('w') => app.mode = Mode::WhatsNew { scroll: 0 },
        // Only offered when there is something to install, so it cannot be a surprise.
        KeyCode::Char('u') if app.status.update.is_some() => {
            app.run_update = true;
            app.quit = true;
        }
        KeyCode::Char('s') => match app.config.save() {
            Ok(()) => {
                app.original = app.config.clone();
                app.mode = Mode::Saved(Instant::now());
            }
            Err(e) => app.error = Some(format!("{e:#}")),
        },
        KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => {
            if field.is_text() {
                app.mode = Mode::Edit {
                    field,
                    buffer: current_text(&app.config, field),
                };
            } else {
                toggle(&mut app.config, field, code == KeyCode::Left);
            }
        }
        _ => {}
    }
}

fn toggle(config: &mut Config, field: Field, backwards: bool) {
    match field {
        Field::Detail => {
            let order = [Detail::Generic, Detail::Project, Detail::Full];
            let at = order.iter().position(|d| *d == config.detail).unwrap_or(0);
            let next = if backwards {
                (at + order.len() - 1) % order.len()
            } else {
                (at + 1) % order.len()
            };
            config.detail = order[next];
        }
        Field::ShowModel => config.show_model = !config.show_model,
        Field::FollowFocus => config.follow_focus = !config.follow_focus,
        Field::Enabled => config.enabled = !config.enabled,
        Field::UpdateCheck => config.update_check = !config.update_check,
        Field::AutoUpdate => config.auto_update = !config.auto_update,
        // The text fields, which Enter opens an editor for instead.
        _ => {}
    }
}

fn current_text(config: &Config, field: Field) -> String {
    match field {
        Field::IdleTimeout => crate::config::humanize(config.idle_timeout),
        Field::HiddenPaths => config.hidden_paths.join(", "),
        Field::Buttons => format_buttons(&config.buttons),
        Field::ClientId => config.client_id.clone(),
        _ => String::new(),
    }
}

fn format_buttons(buttons: &[ConfigButton]) -> String {
    buttons
        .iter()
        .map(|b| format!("{}|{}", b.label, b.url))
        .collect::<Vec<_>>()
        .join(", ")
}

fn commit(config: &mut Config, field: Field, value: &str) -> Result<()> {
    let value = value.trim();
    match field {
        Field::IdleTimeout => {
            // Round-trip through the same parser the config file uses, so the editor
            // can never accept a value that would fail to load later.
            let parsed: Config = toml::from_str(&format!("idle_timeout = \"{value}\""))
                .map_err(|_| anyhow::anyhow!("expected something like 30s, 15m or 2h"))?;
            anyhow::ensure!(
                !parsed.idle_timeout.is_zero(),
                "an idle timeout of zero would drop every session immediately"
            );
            config.idle_timeout = parsed.idle_timeout;
        }
        Field::HiddenPaths => {
            let patterns = crate::config::split_globs(value);
            // Reject here rather than at load time. A glob that fails to compile is
            // dropped by the matcher, which means the user walks away believing a
            // repository is hidden when it is not — the one mistake this field cannot
            // be allowed to make quietly.
            for pattern in &patterns {
                crate::config::compile_glob(pattern)?;
            }
            config.hidden_paths = patterns;
        }
        Field::Buttons => config.buttons = parse_buttons(value)?,
        Field::ClientId => {
            anyhow::ensure!(
                value.is_empty() || value.chars().all(|c| c.is_ascii_digit()),
                "an Application ID is all digits"
            );
            config.client_id = value.to_string();
        }
        _ => {}
    }
    Ok(())
}

/// `GitHub|https://github.com/x/y, Docs|https://x.dev` → two buttons.
///
/// Discord rejects the whole activity if a button is malformed, so every rule it enforces
/// is enforced here instead — a card that silently stops appearing is far harder to
/// diagnose than a message in the editor.
fn parse_buttons(value: &str) -> Result<Vec<ConfigButton>> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    let mut buttons = Vec::new();
    for entry in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (label, url) = entry
            .split_once('|')
            .ok_or_else(|| anyhow::anyhow!("expected `Label|https://url`, got {entry:?}"))?;
        let (label, url) = (label.trim(), url.trim());

        anyhow::ensure!(!label.is_empty(), "a button needs a label");
        anyhow::ensure!(
            label.chars().count() <= 32,
            "Discord caps a button label at 32 characters"
        );
        anyhow::ensure!(
            url.starts_with("https://") || url.starts_with("http://"),
            "a button URL has to start with https://"
        );
        buttons.push(ConfigButton {
            label: label.to_string(),
            url: url.to_string(),
        });
    }
    anyhow::ensure!(buttons.len() <= 2, "Discord shows at most two buttons");
    Ok(buttons)
}

fn value_of(config: &Config, field: Field) -> String {
    let onoff = |b: bool| if b { "on" } else { "off" }.to_string();
    match field {
        Field::Detail => match config.detail {
            Detail::Generic => "generic",
            Detail::Project => "project",
            Detail::Full => "full",
        }
        .to_string(),
        Field::ShowModel => onoff(config.show_model),
        Field::FollowFocus => onoff(config.follow_focus),
        Field::UpdateCheck => onoff(config.update_check),
        Field::AutoUpdate => onoff(config.auto_update),
        Field::Enabled => onoff(config.enabled),
        Field::IdleTimeout => crate::config::humanize(config.idle_timeout),
        Field::HiddenPaths => {
            if config.hidden_paths.is_empty() {
                "none".into()
            } else {
                config.hidden_paths.join(", ")
            }
        }
        Field::Buttons => {
            if config.buttons.is_empty() {
                "none".into()
            } else {
                format_buttons(&config.buttons)
            }
        }
        Field::ClientId => {
            if config.client_id.is_empty() {
                "bundled".into()
            } else {
                config.client_id.clone()
            }
        }
    }
}

/// What the card would look like right now, built by the real presence code so the
/// preview cannot drift from what actually gets sent. Shared with `doctor`.
pub fn preview_card(config: &Config) -> (String, String) {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/your/project".into());
    let now = Instant::now();
    let snapshot = Snapshot {
        primary: Session {
            agent: Agent::Claude,
            activity: Activity::Editing,
            cwd: Some(cwd),
            model: Some("claude-opus-4-8".into()),
            target: Some("main.rs".into()),
            tty: None,
            started_unix: 0,
            started: now,
            last_seen: now,
        },
        others: 0,
        oldest_start_unix: 0,
    };
    let activity = presence::build(&snapshot, config, &config.hidden_matcher()).sanitized();
    (
        activity.details.unwrap_or_default(),
        activity.state.unwrap_or_default(),
    )
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// Below this the settings and the preview stop fitting side by side and stack instead.
const TWO_COLUMN_WIDTH: u16 = 92;

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Length(3), // title and status
        Constraint::Min(6),    // body
        Constraint::Length(2), // help and footer
    ])
    .split(area);

    frame.render_widget(header(app), layout[0]);

    if let Mode::WhatsNew { scroll } = app.mode {
        frame.render_widget(whats_new(scroll), layout[1]);
    } else {
        draw_body(frame, app, layout[1]);
    }

    frame.render_widget(footer(app), layout[2]);
}

fn header(app: &App) -> Paragraph<'static> {
    let title = Line::from(vec![
        Span::styled("agent-presence", Style::new().bold()),
        Span::raw("  "),
        Span::styled(format!("v{}", crate::update::current()), Style::new().dim()),
        Span::raw("  "),
        Span::styled(
            crate::config::config_path().display().to_string(),
            Style::new().dim(),
        ),
    ]);

    let mut status: Vec<Span> = Vec::new();
    match app.status.daemon {
        Some(pid) => {
            status.push(Span::styled("● ", Style::new().fg(Color::Green)));
            status.push(Span::styled(
                format!("daemon {pid}"),
                Style::new().fg(Color::Green),
            ));
        }
        None => {
            status.push(Span::styled("○ ", Style::new().dim()));
            status.push(Span::styled("daemon idle", Style::new().dim()));
        }
    }
    status.push(Span::styled("  ·  ", Style::new().dim()));
    status.push(match &app.status.discord {
        None => Span::styled("Discord reachable", Style::new().dim()),
        Some(_) => Span::styled("Discord unreachable", Style::new().fg(Color::Yellow)),
    });
    if let Some(sessions) = &app.status.sessions {
        status.push(Span::styled("  ·  ", Style::new().dim()));
        status.push(Span::styled(
            match sessions.sessions.len() {
                0 => "no live sessions".to_string(),
                1 => "1 live session".to_string(),
                n => format!("{n} live sessions"),
            },
            Style::new().dim(),
        ));
    }
    if let Some(latest) = &app.status.update {
        status.push(Span::styled("  ·  ", Style::new().dim()));
        status.push(Span::styled(
            format!("v{latest} available — u to install"),
            Style::new().fg(Color::Cyan).bold(),
        ));
    }

    Paragraph::new(vec![title, Line::from(status)]).block(Block::default().borders(Borders::BOTTOM))
}

fn draw_body(frame: &mut Frame, app: &App, area: Rect) {
    // Side by side when there is room; the preview is what makes `detail` legible, so it
    // is the last thing to be given up.
    let (left, right) = if area.width >= TWO_COLUMN_WIDTH {
        let cols = Layout::horizontal([Constraint::Percentage(52), Constraint::Percentage(48)])
            .split(area);
        (cols[0], Some(cols[1]))
    } else {
        let rows = Layout::vertical([Constraint::Min(6), Constraint::Length(7)]).split(area);
        (rows[0], Some(rows[1]))
    };

    let (rows, selected_row) = settings_rows(app);
    frame.render_widget(
        Paragraph::new(rows).scroll((scroll_for(selected_row, left.height), 0)),
        left,
    );

    if let Some(right) = right {
        let split = Layout::vertical([Constraint::Length(7), Constraint::Min(0)]).split(right);
        frame.render_widget(preview(app), split[0]);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                app.field().help().to_string(),
                Style::new().dim(),
            )))
            .wrap(ratatui::widgets::Wrap { trim: false })
            // Padding rather than a literal indent, so a wrapped second line lines up
            // with the first instead of starting at the margin.
            .block(Block::default().padding(Padding::new(2, 1, 0, 0))),
            split[1],
        );
    }
}

/// Keep the selected row on screen when the settings do not fit.
///
/// A short terminal clipped the last section away entirely, and the selection moved into
/// it invisibly — arrow keys changed a setting the user could not see.
fn scroll_for(selected_row: u16, height: u16) -> u16 {
    selected_row.saturating_sub(height.saturating_sub(1))
}

/// The rendered rows, and which of them carries the selection.
fn settings_rows(app: &App) -> (Vec<Line<'static>>, u16) {
    let mut rows: Vec<Line> = Vec::new();
    let mut section: Option<Section> = None;
    let mut selected_row = 0;

    for (i, field) in Field::ALL.iter().enumerate() {
        if section != Some(field.section()) {
            section = Some(field.section());
            rows.push(Line::from(Span::styled(
                format!("  {}", field.section().title().to_uppercase()),
                Style::new().fg(Color::Blue).bold(),
            )));
        }

        let selected = i == app.selected;
        if selected {
            selected_row = rows.len() as u16;
        }
        let value = match &app.mode {
            Mode::Edit { field: f, buffer } if *f == *field => format!("{buffer}▌"),
            _ => value_of(&app.config, *field),
        };
        let value_style = match field {
            // The one setting where the value itself carries a warning.
            Field::Detail if app.config.detail != Detail::Generic => Style::new().fg(Color::Yellow),
            Field::Enabled if !app.config.enabled => Style::new().fg(Color::Yellow),
            _ if selected => Style::new().fg(Color::Cyan),
            _ => Style::new(),
        };
        rows.push(Line::from(vec![
            Span::styled(
                if selected { "  ▸ " } else { "    " },
                Style::new().fg(Color::Cyan),
            ),
            Span::styled(
                format!("{:<16}", field.label()),
                if selected {
                    Style::new().bold()
                } else {
                    Style::new().dim()
                },
            ),
            Span::styled(value, value_style),
        ]));
    }
    (rows, selected_row)
}

fn preview(app: &App) -> Paragraph<'static> {
    let (details, state) = preview_card(&app.config);
    let leaks = app.config.detail != Detail::Generic;
    let block = Block::default()
        .borders(Borders::ALL)
        .title(if leaks {
            " preview — visible to everyone on Discord "
        } else {
            " preview "
        })
        .border_style(if leaks {
            Style::new().fg(Color::Yellow)
        } else {
            Style::new().dim()
        });

    let mut lines = vec![
        Line::from(Span::styled("Agent", Style::new().bold())),
        Line::from(details),
        Line::from(Span::styled(state, Style::new().dim())),
        Line::from(Span::styled("12:34 elapsed", Style::new().dim())),
    ];
    if !app.config.buttons.is_empty() {
        lines.push(Line::from(
            app.config
                .buttons
                .iter()
                .take(2)
                .map(|b| Span::styled(format!("[ {} ] ", b.label), Style::new().fg(Color::Blue)))
                .collect::<Vec<_>>(),
        ));
    }
    if !app.config.enabled {
        lines.push(Line::from(Span::styled(
            "the card is switched off",
            Style::new().fg(Color::Yellow),
        )));
    }
    Paragraph::new(lines).block(block)
}

fn whats_new(scroll: u16) -> Paragraph<'static> {
    let version = crate::update::current();
    let body = crate::update::notes_for(version)
        .unwrap_or("No notes shipped with this build.")
        .to_string();

    // The notes are Markdown meant for a GitHub release page. Rendering it properly is
    // not worth a dependency; stripping the two marks that actually appear is.
    let mut lines: Vec<Line> = Vec::new();
    for raw in body.lines() {
        let text = raw.replace("**", "").replace('`', "");
        if text.trim().is_empty() {
            // Wrapped paragraphs already read as separated; a run of blanks just wastes
            // rows in a panel that has to scroll.
            if matches!(lines.last(), Some(l) if l.width() == 0) {
                continue;
            }
            lines.push(Line::raw(""));
            continue;
        }
        let bold = raw.trim_start().starts_with("**");
        lines.push(Line::from(Span::styled(
            text,
            if bold {
                Style::new().bold()
            } else {
                Style::new()
            },
        )));
    }

    Paragraph::new(lines)
        .scroll((scroll, 0))
        .wrap(ratatui::widgets::Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" what's new in v{version} "))
                .border_style(Style::new().fg(Color::Blue))
                .padding(Padding::new(2, 2, 0, 0)),
        )
}

fn footer(app: &App) -> Paragraph<'static> {
    let hint = match (&app.mode, &app.error) {
        (_, Some(error)) => Line::from(Span::styled(
            format!("  {error}"),
            Style::new().fg(Color::Red),
        )),
        (Mode::Saved(_), _) => Line::from(Span::styled(
            "  ✓ saved — the running daemon picks it up within a couple of seconds",
            Style::new().fg(Color::Green),
        )),
        (Mode::Edit { .. }, _) => Line::from(Span::styled(
            "  enter save field · esc cancel",
            Style::new().dim(),
        )),
        (Mode::WhatsNew { .. }, _) => Line::from(Span::styled(
            "  ↑↓ scroll · any other key returns",
            Style::new().dim(),
        )),
        _ => {
            let mut keys = String::from(
                "  ↑↓ move · ←→ change · enter edit · s save · r defaults · w what's new",
            );
            if app.status.update.is_some() {
                keys.push_str(" · u update");
            }
            keys.push_str(" · q quit");
            Line::from(Span::styled(keys, Style::new().dim()))
        }
    };
    Paragraph::new(vec![Line::raw(""), hint])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_timeout_the_config_loader_would_refuse() {
        let mut c = Config::default();
        assert!(commit(&mut c, Field::IdleTimeout, "soon").is_err());
        assert!(commit(&mut c, Field::IdleTimeout, "0s").is_err());
        assert!(commit(&mut c, Field::IdleTimeout, "45m").is_ok());
        assert_eq!(c.idle_timeout, Duration::from_secs(2700));
    }

    #[test]
    fn rejects_a_non_numeric_application_id() {
        let mut c = Config::default();
        assert!(commit(&mut c, Field::ClientId, "not-an-id").is_err());
        assert!(
            commit(&mut c, Field::ClientId, "").is_ok(),
            "empty means bundled"
        );
        assert!(commit(&mut c, Field::ClientId, "1528707412352172162").is_ok());
    }

    #[test]
    fn hidden_paths_round_trip_through_the_editor() {
        let mut c = Config::default();
        commit(&mut c, Field::HiddenPaths, "~/work/**, , ~/clients/**").unwrap();
        assert_eq!(c.hidden_paths, vec!["~/work/**", "~/clients/**"]);
        assert_eq!(
            current_text(&c, Field::HiddenPaths),
            "~/work/**, ~/clients/**"
        );
    }

    #[test]
    fn detail_cycles_both_ways() {
        let mut c = Config::default();
        toggle(&mut c, Field::Detail, false);
        assert_eq!(c.detail, Detail::Project);
        toggle(&mut c, Field::Detail, true);
        assert_eq!(c.detail, Detail::Generic);
        toggle(&mut c, Field::Detail, true);
        assert_eq!(c.detail, Detail::Full, "wraps around");
    }

    #[test]
    fn preview_honours_the_privacy_filter() {
        let generic = preview_card(&Config::default());
        assert_eq!(
            generic.0, "Claude Code",
            "generic must not name the project"
        );

        let revealing = preview_card(&Config {
            detail: Detail::Project,
            ..Default::default()
        });
        assert_ne!(
            revealing.0, generic.0,
            "project detail must change the card"
        );
    }

    fn app_for_test() -> App {
        let config = Config::default();
        App {
            status: Status {
                daemon: Some(4821),
                discord: None,
                sessions: None,
                update: None,
            },
            original: config.clone(),
            config,
            selected: 0,
            mode: Mode::Browse,
            error: None,
            quit: false,
            run_update: false,
        }
    }

    fn render(app: &App, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn render_at(width: u16, height: u16) -> String {
        render(&app_for_test(), width, height)
    }

    #[test]
    fn renders_the_settings_and_the_preview() {
        let screen = render_at(100, 30);
        assert!(screen.contains("Detail"));
        assert!(screen.contains("generic"));
        assert!(screen.contains("Claude Code"), "preview must be drawn");
        assert!(screen.contains("s save"));
    }

    #[test]
    fn every_setting_sits_under_a_section_heading() {
        let screen = render_at(100, 34);
        for heading in ["PRIVACY", "BEHAVIOUR", "CARD", "ADVANCED"] {
            assert!(screen.contains(heading), "missing section {heading}");
        }
        // Every field has to be reachable, so every field has to be drawn.
        for field in Field::ALL {
            assert!(
                screen.contains(field.label()),
                "{} is not on screen",
                field.label()
            );
        }
    }

    #[test]
    fn the_header_reports_what_the_daemon_is_doing() {
        let mut app = app_for_test();
        app.status.sessions = Some(ipc::SessionsReply {
            sessions: Vec::new(),
            card_enabled: true,
        });
        let screen = render(&app, 100, 30);
        assert!(screen.contains("daemon 4821"));
        assert!(screen.contains("Discord reachable"));
        assert!(screen.contains("no live sessions"));
    }

    #[test]
    fn an_available_update_is_offered_only_when_there_is_one() {
        let quiet = render_at(100, 30);
        assert!(!quiet.contains("u update"), "nothing to install, no offer");

        let mut app = app_for_test();
        app.status.update = Some("9.9.9".into());
        let screen = render(&app, 100, 30);
        assert!(screen.contains("v9.9.9 available"));
        assert!(screen.contains("u update"));
    }

    #[test]
    fn u_only_starts_an_update_when_one_is_available() {
        let mut app = app_for_test();
        handle_key(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);
        assert!(
            !app.run_update && !app.quit,
            "u must be inert with no update"
        );

        app.status.update = Some("9.9.9".into());
        handle_key(&mut app, KeyCode::Char('u'), KeyModifiers::NONE);
        assert!(app.run_update && app.quit, "u must leave the TUI to update");
    }

    #[test]
    fn whats_new_shows_the_running_version() {
        let mut app = app_for_test();
        handle_key(&mut app, KeyCode::Char('w'), KeyModifiers::NONE);
        let screen = render(&app, 100, 30);
        assert!(screen.contains(&format!("what's new in v{}", crate::update::current())));
        assert!(
            screen.contains("scroll"),
            "the footer must explain the panel"
        );

        // Any key that is not a scroll returns to the settings.
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(app.mode, Mode::Browse));
    }

    #[test]
    fn buttons_round_trip_through_the_editor() {
        let mut c = Config::default();
        commit(
            &mut c,
            Field::Buttons,
            "GitHub|https://github.com/x/y, Docs|https://x.dev",
        )
        .unwrap();
        assert_eq!(c.buttons.len(), 2);
        assert_eq!(c.buttons[0].label, "GitHub");
        assert_eq!(c.buttons[1].url, "https://x.dev");
        assert_eq!(
            current_text(&c, Field::Buttons),
            "GitHub|https://github.com/x/y, Docs|https://x.dev"
        );

        commit(&mut c, Field::Buttons, "").unwrap();
        assert!(c.buttons.is_empty(), "empty clears them");
    }

    #[test]
    fn buttons_discord_would_reject_are_refused_here() {
        // Discord fails the whole activity on a malformed button, so the card would
        // simply stop appearing with nothing to explain why.
        let mut c = Config::default();
        for bad in [
            "no pipe here",
            "Label|ftp://example.com",
            "Label|example.com",
            "|https://x.dev",
            "a|https://x.dev, b|https://x.dev, c|https://x.dev",
        ] {
            assert!(
                commit(&mut c, Field::Buttons, bad).is_err(),
                "accepted {bad:?}"
            );
        }
        let long = format!("{}|https://x.dev", "x".repeat(33));
        assert!(commit(&mut c, Field::Buttons, &long).is_err(), "label cap");
    }

    #[test]
    fn hidden_paths_reject_a_glob_that_would_be_dropped() {
        let mut c = Config::default();
        assert!(commit(&mut c, Field::HiddenPaths, "~/work/{unclosed").is_err());
        assert!(
            c.hidden_paths.is_empty(),
            "a rejected value must not be half-applied"
        );
        commit(&mut c, Field::HiddenPaths, "~/work/{a,b}/**").unwrap();
        assert_eq!(
            c.hidden_paths,
            vec!["~/work/{a,b}/**"],
            "a brace alternation is one pattern, not two"
        );
    }

    #[test]
    fn survives_a_terminal_too_small_to_fit() {
        // Layout constraints that overflow must clip, not panic. An 80x24 terminal is
        // the floor people actually have; 20x6 is the pathological case.
        for (w, h) in [(80, 24), (40, 12), (20, 6)] {
            render_at(w, h);
        }
    }

    #[test]
    fn editing_a_field_shows_the_buffer_not_the_stored_value() {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(90, 30)).unwrap();
        let mut app = app_for_test();
        app.selected = Field::ALL
            .iter()
            .position(|f| *f == Field::ClientId)
            .unwrap();
        handle_key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut app, KeyCode::Char('7'), KeyModifiers::NONE);

        terminal.draw(|frame| draw(frame, &app)).unwrap();
        let screen: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        // The buffer plus its cursor, which only the edit mode draws. Checking for
        // "bundled" being gone would pass on the help text, which also mentions it.
        assert!(
            screen.contains("7▌"),
            "the edit buffer and cursor must replace the stored value"
        );
    }

    #[test]
    fn quitting_never_writes_the_file() {
        let mut app = app_for_test();
        toggle(&mut app.config, Field::Detail, false);
        handle_key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(app.quit);
        assert!(
            changed(&app.original, &app.config),
            "an unsaved change must still be reported as unsaved"
        );
    }

    #[test]
    fn durations_render_in_the_unit_they_were_written() {
        assert_eq!(crate::config::humanize(Duration::from_secs(900)), "15m");
        assert_eq!(crate::config::humanize(Duration::from_secs(7200)), "2h");
        assert_eq!(crate::config::humanize(Duration::from_secs(45)), "45s");
    }
}
