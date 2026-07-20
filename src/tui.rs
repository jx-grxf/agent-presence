//! Interactive settings editor.
//!
//! The config file stays the source of truth and hand-editing it is fully supported —
//! this is a front end for it, not a replacement. Anything the editor does not cover
//! (link buttons) is read from and written back untouched.
//!
//! The point of the live card preview is that `detail` is a privacy decision. Seeing
//! the repository name appear the moment you leave `generic` is worth more than any
//! amount of documentation about it.

use crate::config::{Config, Detail};
use crate::daemon::presence;
use crate::daemon::registry::Session;
use crate::daemon::registry::Snapshot;
use crate::event::{Activity, Agent};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph};
use std::time::{Duration, Instant};

/// Rows in the settings list, in display order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    Detail,
    ShowModel,
    FollowFocus,
    Enabled,
    IdleTimeout,
    HiddenPaths,
    ClientId,
}

impl Field {
    const ALL: [Field; 7] = [
        Field::Detail,
        Field::ShowModel,
        Field::FollowFocus,
        Field::Enabled,
        Field::IdleTimeout,
        Field::HiddenPaths,
        Field::ClientId,
    ];

    fn label(self) -> &'static str {
        match self {
            Field::Detail => "Detail",
            Field::ShowModel => "Show model",
            Field::FollowFocus => "Follow focus",
            Field::Enabled => "Enabled",
            Field::IdleTimeout => "Idle timeout",
            Field::HiddenPaths => "Hidden paths",
            Field::ClientId => "Application ID",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Field::Detail => "How much of your workspace reaches Discord. generic leaks nothing.",
            Field::ShowModel => "Append the model name, e.g. \"· Opus 4.8\".",
            Field::FollowFocus => "Show the session in the focused terminal window (macOS).",
            Field::Enabled => "Master switch. Off keeps the hooks installed but clears the card.",
            Field::IdleTimeout => "Drop sessions silent this long. Accepts 30s, 15m, 2h.",
            Field::HiddenPaths => "Globs always forced to generic. One per line, ~ expands.",
            Field::ClientId => "Your own Discord application. Empty uses the bundled one.",
        }
    }

    /// Whether Enter opens the text editor rather than toggling in place.
    fn is_text(self) -> bool {
        matches!(
            self,
            Field::IdleTimeout | Field::HiddenPaths | Field::ClientId
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
}

struct App {
    config: Config,
    original: Config,
    selected: usize,
    mode: Mode,
    error: Option<String>,
    quit: bool,
}

pub fn run() -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "`config` needs a terminal — edit {} directly instead",
            crate::config::config_path().display()
        );
    }

    let config = Config::load();
    let mut app = App {
        original: config.clone(),
        config,
        selected: 0,
        mode: Mode::Browse,
        error: None,
        quit: false,
    };

    let mut terminal = enter()?;
    // Restore the terminal even if drawing panics, otherwise the user is left in raw
    // mode on the alternate screen with no echo.
    let result = (|| -> Result<()> {
        while !app.quit {
            terminal.draw(|frame| draw(frame, &app))?;
            // Polling rather than blocking, so the "saved" flash can expire on its own.
            if event::poll(Duration::from_millis(120))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        handle_key(&mut app, key.code, key.modifiers);
                    }
                }
            }
            if let Mode::Saved(at) = app.mode {
                if at.elapsed() > Duration::from_millis(1200) {
                    app.mode = Mode::Browse;
                }
            }
        }
        Ok(())
    })();
    leave(terminal)?;
    result?;

    if changed(&app.original, &app.config) {
        println!("Left unsaved changes — nothing was written.");
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
                    Err(e) => app.error = Some(e.to_string()),
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

    app.error = None;
    let field = Field::ALL[app.selected];
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
        _ => {}
    }
}

fn current_text(config: &Config, field: Field) -> String {
    match field {
        Field::IdleTimeout => humanize(config.idle_timeout),
        Field::HiddenPaths => config.hidden_paths.join(", "),
        Field::ClientId => config.client_id.clone(),
        _ => String::new(),
    }
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
            config.hidden_paths = value
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
        }
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

fn humanize(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        s if s % 3600 == 0 && s > 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 && s > 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
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
        Field::Enabled => onoff(config.enabled),
        Field::IdleTimeout => humanize(config.idle_timeout),
        Field::HiddenPaths => {
            if config.hidden_paths.is_empty() {
                "none".into()
            } else {
                config.hidden_paths.join(", ")
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
    let activity = presence::build(&snapshot, config).sanitized();
    (
        activity.details.unwrap_or_default(),
        activity.state.unwrap_or_default(),
    )
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let layout = Layout::vertical([
        Constraint::Length(3),                           // title
        Constraint::Length(Field::ALL.len() as u16 + 2), // settings
        Constraint::Length(8),                           // preview
        Constraint::Min(3),                              // help + status
    ])
    .split(area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("agent-presence", Style::new().bold()),
            Span::raw("  "),
            Span::styled(
                crate::config::config_path().display().to_string(),
                Style::new().dim(),
            ),
        ]))
        .block(Block::default().borders(Borders::BOTTOM)),
        layout[0],
    );

    let rows: Vec<Line> = Field::ALL
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let selected = i == app.selected;
            let marker = if selected { "▸ " } else { "  " };
            let value = match &app.mode {
                Mode::Edit { field: f, buffer } if *f == *field => format!("{buffer}▌"),
                _ => value_of(&app.config, *field),
            };
            let value_style = match field {
                // The one setting where the value carries a warning.
                Field::Detail if app.config.detail != Detail::Generic => {
                    Style::new().fg(Color::Yellow)
                }
                _ if selected => Style::new().fg(Color::Cyan),
                _ => Style::new(),
            };
            Line::from(vec![
                Span::styled(
                    format!("{marker}{:<16}", field.label()),
                    if selected {
                        Style::new().bold()
                    } else {
                        Style::new().dim()
                    },
                ),
                Span::styled(value, value_style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), layout[1]);

    let (details, state) = preview_card(&app.config);
    let leaks = app.config.detail != Detail::Generic;
    let preview_block = Block::default()
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
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled("Agent", Style::new().bold())),
            Line::from(details),
            Line::from(Span::styled(state, Style::new().dim())),
            Line::from(Span::styled("12:34 elapsed", Style::new().dim())),
        ])
        .block(preview_block),
        layout[2],
    );

    let footer = match (&app.mode, &app.error) {
        (_, Some(error)) => Line::from(Span::styled(
            format!("  {error}"),
            Style::new().fg(Color::Red),
        )),
        (Mode::Saved(_), _) => Line::from(Span::styled(
            "  ✓ saved — restart the daemon with `agent-presence stop`",
            Style::new().fg(Color::Green),
        )),
        (Mode::Edit { .. }, _) => Line::from(Span::styled(
            "  enter save field · esc cancel",
            Style::new().dim(),
        )),
        _ => Line::from(Span::styled(
            "  ↑↓ move · ←→/space change · enter edit · s save · r defaults · q quit",
            Style::new().dim(),
        )),
    };
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                format!("  {}", Field::ALL[app.selected].help()),
                Style::new().dim(),
            )),
            Line::raw(""),
            footer,
        ]),
        layout[3],
    );
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
            original: config.clone(),
            config,
            selected: 0,
            mode: Mode::Browse,
            error: None,
            quit: false,
        }
    }

    fn render_at(width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        let app = app_for_test();
        terminal.draw(|frame| draw(frame, &app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn renders_the_settings_and_the_preview() {
        let screen = render_at(90, 30);
        assert!(screen.contains("Detail"));
        assert!(screen.contains("generic"));
        assert!(screen.contains("Claude Code"), "preview must be drawn");
        assert!(screen.contains("s save"));
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
        assert_eq!(humanize(Duration::from_secs(900)), "15m");
        assert_eq!(humanize(Duration::from_secs(7200)), "2h");
        assert_eq!(humanize(Duration::from_secs(45)), "45s");
    }
}
