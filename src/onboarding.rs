//! First-run wizard: decide the privacy setting, then wire up the agents.
//!
//! This exists because package managers cannot do it. Homebrew sandboxes `post_install`
//! and forbids a formula from writing outside its prefix, so `~/.claude/settings.json`
//! is unreachable from the install itself. Rather than leave the user with a command to
//! copy, bare `agent-presence` walks them through it and verifies the result.
//!
//! Every step is skippable and nothing is written before the install step: quitting
//! early leaves the machine exactly as it was found.

use crate::config::{Config, Detail};
use crate::event::Agent;
use crate::install::{self, Outcome};
use crate::tui;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Welcome,
    Privacy,
    Focus,
    Install,
    Done,
}

impl Step {
    fn next(self) -> Self {
        match self {
            Step::Welcome => Step::Privacy,
            Step::Privacy => Step::Focus,
            Step::Focus => Step::Install,
            Step::Install | Step::Done => Step::Done,
        }
    }
    fn prev(self) -> Self {
        match self {
            Step::Welcome | Step::Privacy => Step::Welcome,
            Step::Focus => Step::Privacy,
            Step::Install => Step::Focus,
            Step::Done => Step::Install,
        }
    }
    fn index(self) -> usize {
        match self {
            Step::Welcome => 0,
            Step::Privacy => 1,
            Step::Focus => 2,
            Step::Install => 3,
            Step::Done => 4,
        }
    }
}

struct Wizard {
    step: Step,
    config: Config,
    /// Which agents exist on this machine, decided before the wizard opens.
    detected: Vec<(Agent, bool)>,
    /// Filled in by the install step, then re-read from disk to confirm.
    installed: Option<Vec<(Agent, Outcome, bool)>>,
    discord_ok: Option<String>,
    error: Option<String>,
    quit: bool,
}

/// `discord` is the result of probing Discord before the alternate screen opens —
/// doing it inside the draw loop would need async in a sync callback, and the answer
/// cannot change while the wizard is open anyway.
pub fn run(discord: Option<String>) -> Result<()> {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        // Non-interactive callers (scripts, Scoop's post_install) get the plain path.
        return install::run(false, None);
    }

    let detected = install::installed_paths()
        .into_iter()
        .map(|(agent, path)| {
            (
                agent,
                path.parent().map(std::path::Path::exists).unwrap_or(false),
            )
        })
        .collect();

    let mut wizard = Wizard {
        step: Step::Welcome,
        config: Config::load(),
        detected,
        installed: None,
        discord_ok: discord,
        error: None,
        quit: false,
    };

    enable_raw_mode()?;
    let mut out = std::io::stdout();
    crossterm::execute!(out, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(out))?;

    let result = (|| -> Result<()> {
        while !wizard.quit {
            terminal.draw(|frame| draw(frame, &wizard))?;
            if event::poll(Duration::from_millis(120))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        handle_key(&mut wizard, key.code, key.modifiers);
                    }
                }
            }
        }
        Ok(())
    })();

    disable_raw_mode()?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result?;

    farewell(&wizard);
    Ok(())
}

/// Leave the terminal with the state the wizard reached, so quitting halfway is not
/// mistaken for a finished setup.
fn farewell(wizard: &Wizard) {
    use crate::ui;
    match &wizard.installed {
        Some(results) if results.iter().any(|(_, _, confirmed)| *confirmed) => {
            ui::heading("You are set up");
            for (agent, _, confirmed) in results {
                if *confirmed {
                    ui::ok(agent.label());
                }
            }
            ui::field("next", "restart your agent — hooks are read at startup");
            ui::field(
                "then",
                &format!("{} to change anything", ui::cyan("agent-presence config")),
            );
        }
        _ => {
            println!(
                "\n{}\n",
                ui::dim("  Setup cancelled — nothing was changed. Run `agent-presence` again.")
            );
        }
    }
}

fn handle_key(wizard: &mut Wizard, code: KeyCode, mods: KeyModifiers) {
    match code {
        KeyCode::Esc | KeyCode::Char('q') => wizard.quit = true,
        KeyCode::Char('c') if mods.contains(KeyModifiers::CONTROL) => wizard.quit = true,
        KeyCode::Left | KeyCode::Backspace if wizard.step != Step::Privacy => {
            wizard.step = wizard.step.prev()
        }
        KeyCode::Enter => match wizard.step {
            Step::Install => perform_install(wizard),
            Step::Done => wizard.quit = true,
            _ => wizard.step = wizard.step.next(),
        },
        // On the choice steps the arrows change the value; navigation is Enter only.
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') => {
            let back = matches!(code, KeyCode::Left | KeyCode::Up);
            match wizard.step {
                Step::Privacy => cycle_detail(&mut wizard.config, back),
                Step::Focus => wizard.config.follow_focus = !wizard.config.follow_focus,
                _ => {}
            }
        }
        _ => {}
    }
}

fn cycle_detail(config: &mut Config, backwards: bool) {
    let order = [Detail::Generic, Detail::Project, Detail::Full];
    let at = order.iter().position(|d| *d == config.detail).unwrap_or(0);
    let next = if backwards {
        (at + order.len() - 1) % order.len()
    } else {
        (at + 1) % order.len()
    };
    config.detail = order[next];
}

/// Save the settings, write the hooks, then read the files back. The read-back is the
/// point: reporting success from the writer's own return value would miss a config the
/// agent will not actually load.
fn perform_install(wizard: &mut Wizard) {
    if let Err(e) = wizard.config.save() {
        wizard.error = Some(format!("could not save settings: {e:#}"));
        return;
    }
    match install::apply(false, None) {
        Ok(results) => {
            let confirmed = results
                .into_iter()
                .map(|(agent, outcome)| {
                    let path = install::config_file(agent);
                    (agent, outcome, install::is_installed(&path))
                })
                .collect();
            wizard.installed = Some(confirmed);
            wizard.error = None;
            wizard.step = Step::Done;
        }
        Err(e) => wizard.error = Some(format!("{e:#}")),
    }
}

fn draw(frame: &mut Frame, wizard: &Wizard) {
    let layout = Layout::vertical([
        Constraint::Length(2), // progress
        Constraint::Min(8),    // body
        Constraint::Length(2), // footer
    ])
    .split(frame.area());

    let dots: Vec<Span> = (0..5)
        .map(|i| {
            let filled = i <= wizard.step.index();
            Span::styled(
                if filled { "● " } else { "○ " },
                if filled {
                    Style::new().fg(Color::Cyan)
                } else {
                    Style::new().dim()
                },
            )
        })
        .collect();
    frame.render_widget(
        Paragraph::new(Line::from(
            [
                vec![Span::styled("agent-presence  ", Style::new().bold())],
                dots,
            ]
            .concat(),
        )),
        layout[0],
    );

    let body = match wizard.step {
        Step::Welcome => welcome(wizard),
        Step::Privacy => privacy(wizard),
        Step::Focus => focus(wizard),
        Step::Install => install_step(wizard),
        Step::Done => done(wizard),
    };
    frame.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::new().dim()),
        ),
        layout[1],
    );

    let hint = match (&wizard.error, wizard.step) {
        (Some(error), _) => Line::from(Span::styled(
            format!("  {error}"),
            Style::new().fg(Color::Red),
        )),
        (_, Step::Welcome) => Line::from(Span::styled(
            "  enter continue · q quit",
            Style::new().dim(),
        )),
        (_, Step::Install) => Line::from(Span::styled(
            "  enter install · ← back · q quit",
            Style::new().dim(),
        )),
        (_, Step::Done) => Line::from(Span::styled("  enter finish", Style::new().dim())),
        _ => Line::from(Span::styled(
            "  ←→ change · enter continue · q quit",
            Style::new().dim(),
        )),
    };
    frame.render_widget(Paragraph::new(hint), layout[2]);
}

fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  {text}"),
        Style::new().bold().fg(Color::Cyan),
    ))
}
fn body(text: &str) -> Line<'static> {
    Line::from(format!("  {text}"))
}
fn muted(text: &str) -> Line<'static> {
    Line::from(Span::styled(format!("  {text}"), Style::new().dim()))
}
fn blank() -> Line<'static> {
    Line::raw("")
}

fn welcome(wizard: &Wizard) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading("Discord Rich Presence for your coding agents"),
        blank(),
        body("Your Discord profile will show what your agent is doing, live."),
        muted("No bot, no token — the connection is authenticated by the Discord app"),
        muted("you are already signed into."),
        blank(),
        heading("Found on this machine"),
    ];
    for (agent, present) in &wizard.detected {
        lines.push(if *present {
            Line::from(vec![
                Span::styled("  ✓ ", Style::new().fg(Color::Green)),
                Span::raw(agent.label().to_string()),
            ])
        } else {
            muted(&format!("– {} (not installed)", agent.label()))
        });
    }
    if wizard.detected.iter().all(|(_, present)| !present) {
        lines.push(blank());
        lines.push(Line::from(Span::styled(
            "  Neither agent found — there is nothing to wire up yet.",
            Style::new().fg(Color::Yellow),
        )));
    }
    lines
}

fn privacy(wizard: &Wizard) -> Vec<Line<'static>> {
    let (details, state) = tui::preview_card(&wizard.config);
    let (name, explain) = match wizard.config.detail {
        Detail::Generic => (
            "generic",
            "Nothing identifying. No repository name, no branch, no file names.",
        ),
        Detail::Project => (
            "project",
            "Adds the project directory name and the current git branch.",
        ),
        Detail::Full => (
            "full",
            "Adds the file or command being worked on right now.",
        ),
    };
    let leaks = wizard.config.detail != Detail::Generic;

    vec![
        heading("How much should the card reveal?"),
        blank(),
        Line::from(vec![
            Span::raw("  ← "),
            Span::styled(
                format!("{name:^9}"),
                if leaks {
                    Style::new().bold().fg(Color::Yellow)
                } else {
                    Style::new().bold().fg(Color::Green)
                },
            ),
            Span::raw(" →"),
        ]),
        muted(explain),
        blank(),
        muted("This is what everyone on Discord would see:"),
        Line::from(vec![
            Span::raw("    "),
            Span::styled("Agent", Style::new().bold()),
        ]),
        Line::from(format!("    {details}")),
        Line::from(Span::styled(format!("    {state}"), Style::new().dim())),
        blank(),
        if leaks {
            Line::from(Span::styled(
                "  ! Individual repositories can still be hidden later, via hidden_paths.",
                Style::new().fg(Color::Yellow),
            ))
        } else {
            muted("You can raise this later with `agent-presence config`.")
        },
    ]
}

fn focus(wizard: &Wizard) -> Vec<Line<'static>> {
    let on = wizard.config.follow_focus;
    vec![
        heading("Several sessions at once?"),
        blank(),
        Line::from(vec![
            Span::raw("  ← "),
            Span::styled(
                if on {
                    "  follow focus  "
                } else {
                    "  most recent  "
                },
                Style::new()
                    .bold()
                    .fg(if on { Color::Green } else { Color::Gray }),
            ),
            Span::raw(" →"),
        ]),
        blank(),
        muted(if on {
            "The card shows the session in the terminal window you are looking at."
        } else {
            "The card shows whichever session was active most recently."
        }),
        blank(),
        muted("macOS only, and it asks for Automation permission the first time."),
        muted("Denying that is fine — it falls back to most recent."),
    ]
}

fn install_step(wizard: &Wizard) -> Vec<Line<'static>> {
    let mut lines = vec![
        heading("Ready to wire up your agents"),
        blank(),
        body("This adds hook entries to:"),
    ];
    for (agent, present) in &wizard.detected {
        let path = install::config_file(*agent);
        lines.push(if *present {
            muted(&format!("  {}", path.display()))
        } else {
            muted(&format!("– {} not installed, skipping", agent.label()))
        });
    }
    lines.extend([
        blank(),
        muted("Existing hooks and settings in those files are left untouched, and"),
        muted("`agent-presence install --uninstall` removes exactly what was added."),
        blank(),
        Line::from(Span::styled(
            "  Press enter to install.",
            Style::new().bold(),
        )),
    ]);
    lines
}

fn done(wizard: &Wizard) -> Vec<Line<'static>> {
    let mut lines = vec![heading("Done"), blank()];

    match &wizard.installed {
        Some(results) => {
            for (agent, outcome, confirmed) in results {
                lines.push(match (outcome, confirmed) {
                    (Outcome::Absent, _) => muted(&format!("– {} not installed", agent.label())),
                    // Verified by reading the file back, not by trusting the writer.
                    (_, true) => Line::from(vec![
                        Span::styled("  ✓ ", Style::new().fg(Color::Green)),
                        Span::raw(format!("{} hooks verified", agent.label())),
                    ]),
                    (_, false) => Line::from(Span::styled(
                        format!(
                            "  ✗ {} — hooks were written but did not read back",
                            agent.label()
                        ),
                        Style::new().fg(Color::Red),
                    )),
                });
            }
        }
        None => lines.push(muted("  Nothing was installed.")),
    }

    lines.push(blank());
    lines.push(match &wizard.discord_ok {
        None => Line::from(vec![
            Span::styled("  ✓ ", Style::new().fg(Color::Green)),
            Span::raw("Discord reachable"),
        ]),
        Some(error) => Line::from(Span::styled(
            format!("  ! Discord: {error}"),
            Style::new().fg(Color::Yellow),
        )),
    });
    if wizard.discord_ok.is_some() {
        lines.push(muted(
            "    Start the desktop app and run `agent-presence doctor`.",
        ));
    }

    lines.extend([
        blank(),
        body("Restart your agent — both read their hook config at startup."),
        muted("Then just work. The daemon starts and stops on its own."),
    ]);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wizard() -> Wizard {
        Wizard {
            step: Step::Welcome,
            config: Config::default(),
            detected: vec![(Agent::Claude, true), (Agent::Codex, false)],
            installed: None,
            discord_ok: None,
            error: None,
            quit: false,
        }
    }

    fn render(wizard: &Wizard, width: u16, height: u16) -> String {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, wizard)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn every_step_renders_at_common_sizes() {
        let mut w = wizard();
        w.installed = Some(vec![(Agent::Claude, Outcome::Changed, true)]);
        for step in [
            Step::Welcome,
            Step::Privacy,
            Step::Focus,
            Step::Install,
            Step::Done,
        ] {
            w.step = step;
            for (width, height) in [(100, 30), (80, 24), (40, 12)] {
                render(&w, width, height);
            }
        }
    }

    #[test]
    fn welcome_names_only_the_agents_that_exist() {
        let screen = render(&wizard(), 100, 30);
        assert!(screen.contains("✓ Claude Code"));
        assert!(screen.contains("Codex (not installed)"));
    }

    #[test]
    fn the_privacy_step_previews_what_would_leak() {
        let mut w = wizard();
        w.step = Step::Privacy;
        let generic = render(&w, 100, 30);
        assert!(generic.contains("generic"));

        cycle_detail(&mut w.config, false);
        let project = render(&w, 100, 30);
        assert!(project.contains("project"));
        assert_ne!(generic, project, "the preview must react to the choice");
    }

    #[test]
    fn quitting_before_the_install_step_changes_nothing() {
        let mut w = wizard();
        handle_key(&mut w, KeyCode::Enter, KeyModifiers::NONE);
        handle_key(&mut w, KeyCode::Char('q'), KeyModifiers::NONE);
        assert!(w.quit);
        assert!(
            w.installed.is_none(),
            "no install may happen without reaching the install step"
        );
    }

    #[test]
    fn enter_walks_forward_and_left_walks_back() {
        let mut w = wizard();
        handle_key(&mut w, KeyCode::Enter, KeyModifiers::NONE);
        assert!(w.step == Step::Privacy);
        // Left on a choice step changes the value rather than navigating.
        handle_key(&mut w, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(w.config.detail, Detail::Full, "cycled backwards, wrapping");
        assert!(w.step == Step::Privacy);

        handle_key(&mut w, KeyCode::Enter, KeyModifiers::NONE);
        assert!(w.step == Step::Focus);
        handle_key(&mut w, KeyCode::Backspace, KeyModifiers::NONE);
        assert!(w.step == Step::Privacy, "backspace steps back");
    }

    #[test]
    fn a_failed_read_back_is_reported_as_failure() {
        let mut w = wizard();
        w.step = Step::Done;
        w.installed = Some(vec![(Agent::Claude, Outcome::Changed, false)]);
        let screen = render(&w, 100, 30);
        assert!(
            screen.contains("did not read back"),
            "a write that cannot be confirmed must not read as success"
        );
    }
}
