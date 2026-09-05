mod config;
mod daemon;
mod discord;
mod event;
mod hook;
mod install;
mod ipc;
mod onboarding;
mod tui;
mod ui;
mod update;

use anyhow::Result;
use clap::{Parser, Subcommand};
use event::Agent;

/// Default Discord Application ID. Public value, not a secret — Rich Presence
/// authenticates through the logged-in Discord desktop client, never a token.
pub const DEFAULT_CLIENT_ID: &str = "1528707412352172162";

#[derive(Parser)]
#[command(
    name = "agent-presence",
    version,
    about = "Discord Rich Presence for Claude Code and Codex"
)]
struct Cli {
    /// Bare `agent-presence` runs setup. Homebrew's sandbox forbids a formula from
    /// touching `~/.claude`, so the shortest possible first run matters.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Consume one hook event on stdin. Invoked by the agent, not by hand.
    Hook {
        #[arg(long)]
        agent: Agent,
    },
    /// Run the presence daemon in the foreground.
    Daemon,
    /// Install hooks into Claude Code and Codex.
    Install {
        /// Remove previously installed hooks instead.
        #[arg(long)]
        uninstall: bool,
        /// Only touch this agent's config.
        #[arg(long)]
        agent: Option<Agent>,
    },
    /// Edit settings in an interactive menu.
    Config,
    /// Show daemon, config and Discord status.
    Status,
    /// List the agent sessions the daemon is tracking.
    Sessions,
    /// Show the card again, without touching the installed hooks.
    On,
    /// Suppress the card, without touching the installed hooks.
    Off,
    /// Diagnose a setup that is not showing a card.
    Doctor,
    /// Stop a running daemon.
    Stop,
    /// Upgrade to the newest release through whatever installed this binary.
    Update {
        /// Report what is available and exit, without installing anything.
        #[arg(long)]
        check: bool,
    },
    /// Send a one-off activity, to verify the Discord IPC layer.
    DebugActivity {
        #[arg(long, default_value = "Claude Code")]
        details: String,
        #[arg(long, default_value = "Editing code")]
        state: String,
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long, default_value_t = 30)]
        hold: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let Some(command) = Cli::parse().command else {
        init_logging(false);
        // Probe Discord before the wizard takes over the screen: the answer cannot
        // change while it is open, and an async call inside a draw callback cannot.
        return onboarding::run(probe_discord().await);
    };
    init_logging(matches!(command, Command::Hook { .. } | Command::Daemon));

    match command {
        // Never returns an error: a failing hook must not disturb the agent.
        Command::Hook { agent } => hook::run(agent).await,
        Command::Daemon => daemon::run().await?,
        Command::Install { uninstall, agent } => install::run(uninstall, agent)?,
        Command::Config => tui::run().await?,
        Command::Status => status().await?,
        Command::Sessions => sessions().await,
        Command::On => set_enabled(true)?,
        Command::Off => set_enabled(false)?,
        Command::Doctor => doctor().await?,
        Command::Stop => stop(),
        Command::Update { check } => update::run(check)?,
        Command::DebugActivity {
            details,
            state,
            client_id,
            hold,
        } => debug_activity(details, state, client_id, hold).await?,
    }
    Ok(())
}

/// Hooks and the daemon log to a file; everything else logs to stderr. stdout stays
/// clean in all cases — Claude Code feeds hook stdout into the model's context.
fn init_logging(to_file: bool) {
    // Interactive commands print their own report; an INFO line landing mid-spinner
    // would interleave with it. `AGENT_PRESENCE_LOG` still overrides both defaults.
    let default = if to_file {
        "agent_presence=info"
    } else {
        "agent_presence=warn"
    };
    let filter = tracing_subscriber::EnvFilter::try_from_env("AGENT_PRESENCE_LOG")
        .unwrap_or_else(|_| default.into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);

    if to_file {
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(config::log_path())
        {
            builder.with_writer(file).with_ansi(false).init();
            return;
        }
    }
    builder.with_writer(std::io::stderr).init();
}

/// `None` means Discord answered the handshake; `Some` carries why it did not.
pub async fn probe_discord() -> Option<String> {
    let id = config::Config::load().effective_client_id();
    if id.is_empty() {
        return Some("no Application ID configured".into());
    }
    discord::DiscordClient::new(id)
        .connect()
        .await
        .err()
        .map(|e| format!("{e:#}"))
}

/// Flip the master switch from the command line, for when the settings menu is more
/// ceremony than the moment deserves — screen-sharing is about to start, say.
///
/// Takes effect on the daemon's next tick, because it reloads the file it just wrote.
fn set_enabled(enabled: bool) -> Result<()> {
    let mut config = config::Config::load();
    let already = config.enabled == enabled;
    config.enabled = enabled;
    config.save()?;

    ui::heading(if enabled { "Card on" } else { "Card off" });
    let state = if enabled {
        "your agent activity is visible on Discord again"
    } else {
        "the card is cleared — hooks stay installed"
    };
    ui::ok(state);
    if already {
        ui::field("", &ui::dim("(it was already set this way)"));
    }
    if daemon::running_pid().is_some() {
        ui::field(
            "",
            &ui::dim("the running daemon picks this up within seconds"),
        );
    }
    Ok(())
}

async fn sessions() {
    ui::heading("Sessions");
    let Some(reply) = ipc::sessions_or_none().await else {
        ui::field(
            "state",
            &ui::dim("no daemon running — nothing is being tracked"),
        );
        return;
    };
    if reply.sessions.is_empty() {
        ui::field("state", &ui::dim("daemon running, no live sessions"));
        return;
    }

    for session in &reply.sessions {
        // The marker, not a heading: several sessions are normal, and which one holds
        // the card is the question this command exists to answer.
        let marker = if session.on_card {
            ui::green("▸")
        } else {
            ui::dim("·")
        };
        let project = session
            .cwd
            .as_deref()
            .and_then(|c| std::path::Path::new(c).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "—".into());
        println!(
            "  {marker} {:<14} {:<22} {}",
            session.agent.label(),
            project,
            ui::dim(session.activity.verb())
        );
        println!(
            "    {}",
            ui::dim(&format!(
                "{}  ·  quiet {}  ·  up {}",
                session.cwd.as_deref().unwrap_or("no working directory"),
                short_duration(session.quiet_secs),
                short_duration(session.age_secs),
            ))
        );
    }

    println!(
        "\n  {}",
        ui::dim(&format!(
            "{} marks the session on the card.",
            if ui::styled() { "▸" } else { "The arrow" }
        ))
    );
    if !reply.card_enabled {
        ui::warn("the card is switched off — `agent-presence on` to show it again");
    }
}

/// One line summarising what the daemon is tracking, for `status` and `doctor`.
fn describe_sessions(reply: &ipc::SessionsReply) -> String {
    let Some(on_card) = reply.sessions.iter().find(|s| s.on_card) else {
        return ui::dim("none live").to_string();
    };
    let project = on_card
        .cwd
        .as_deref()
        .and_then(|c| std::path::Path::new(c).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| on_card.agent.label().to_string());

    let others = reply.sessions.len().saturating_sub(1);
    let rest = match others {
        0 => String::new(),
        1 => ui::dim(" · 1 other"),
        n => ui::dim(&format!(" · {n} others")),
    };
    format!(
        "{} live, showing {}{rest}",
        reply.sessions.len(),
        ui::cyan(&project)
    )
}

fn short_duration(secs: u64) -> String {
    match secs {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

async fn status() -> Result<()> {
    let config = config::Config::load();

    ui::heading("Daemon");
    match daemon::running_pid() {
        Some(pid) => ui::ok(&format!("running {}", ui::dim(&format!("pid {pid}")))),
        None => ui::field(
            "state",
            &ui::dim("not running — starts with your next session"),
        ),
    }
    if let Some(reply) = ipc::sessions_or_none().await {
        ui::field("sessions", &describe_sessions(&reply));
    }
    report_update(&config);

    ui::heading("Settings");
    ui::field("detail", &format!("{:?}", config.detail).to_lowercase());
    ui::field("model", if config.show_model { "shown" } else { "hidden" });
    ui::field(
        "focus",
        if config.follow_focus {
            "follows the focused window"
        } else {
            "most recent session"
        },
    );
    let enabled = if config.enabled {
        "yes".to_string()
    } else {
        ui::yellow("no — card is suppressed")
    };
    ui::field("enabled", &enabled);
    ui::field("app id", &config.effective_client_id());

    ui::heading("Paths");
    ui::field(
        "config",
        &ui::dim(&config::config_path().display().to_string()),
    );
    ui::field("log", &ui::dim(&config::log_path().display().to_string()));

    println!(
        "\n{}",
        ui::dim("  Change any of this with `agent-presence config`.")
    );
    Ok(())
}

/// One line about a newer release, from the daemon's cached check.
///
/// Never reaches the network: `status` and `doctor` have to stay instant, and a machine
/// with no connectivity must not sit here waiting for a timeout.
fn report_update(config: &config::Config) {
    if !config.update_check {
        return;
    }
    if let Some(latest) = update::available() {
        ui::warn(&format!(
            "v{latest} available {}",
            ui::dim("— run `agent-presence update`")
        ));
    }
}

async fn doctor() -> Result<()> {
    let config = config::Config::load();

    ui::heading("Discord");
    let id = config.effective_client_id();
    if id.is_empty() {
        ui::fail("no Application ID configured — see README");
    } else {
        let spinner = ui::Spinner::start("connecting to the Discord desktop client…");
        match discord::DiscordClient::new(id).connect().await {
            Ok(()) => spinner.succeed("IPC reachable, handshake accepted"),
            Err(e) => {
                spinner.fail_with(&format!("{e:#}"));
                println!(
                    "{}",
                    ui::dim(
                        "    The browser client has no IPC socket — the desktop app is required."
                    )
                );
            }
        }
    }

    ui::heading("Hooks");
    let mut any = false;
    for (agent, path) in install::installed_paths() {
        let present = path.parent().map(std::path::Path::exists).unwrap_or(false);
        let status = install::status(&path, agent);
        if status.complete() {
            any = true;
            ui::ok(&format!(
                "{} {}",
                agent.label(),
                ui::dim(&path.display().to_string())
            ));
        } else if !status.wired.is_empty() {
            // A release that subscribes to a new event leaves older installs partially
            // wired. Saying "installed" here would hide a card that has quietly stopped
            // reporting approvals or compaction.
            any = true;
            ui::warn(&format!(
                "{} wired for {} of {} events — run `agent-presence install` to add {}",
                agent.label(),
                status.wired.len(),
                status.wired.len() + status.missing.len(),
                status.missing.join(", ")
            ));
        } else if present {
            // Not an error: the Claude Code plugin wires the same hooks without touching
            // this file at all, and that install is perfectly valid.
            ui::warn(&format!(
                "{} has no hooks in {} — run `agent-presence install`, or ignore this if you use the plugin",
                agent.label(),
                ui::dim(&path.display().to_string())
            ));
        } else {
            ui::field(
                "",
                &ui::dim(&format!("{} not installed on this machine", agent.label())),
            );
        }
    }
    if !any {
        ui::warn("no agent is wired up yet — run `agent-presence install`");
    }

    ui::heading("Daemon");
    match daemon::running_pid() {
        Some(pid) => ui::ok(&format!("running {}", ui::dim(&format!("pid {pid}")))),
        None => ui::warn("not running — it starts itself with your next tool call"),
    }
    if let Some(reply) = ipc::sessions_or_none().await {
        ui::field("sessions", &describe_sessions(&reply));
        if !reply.card_enabled {
            ui::warn("the card is switched off — `agent-presence on` to show it again");
        }
    }
    ui::field("version", update::current());
    report_update(&config);

    ui::heading("Card preview");
    let (details, state) = tui::preview_card(&config);
    for line in ui::card("Agent", &details, &state, "12:34 elapsed") {
        println!("  {line}");
    }
    if config.detail != config::Detail::Generic {
        println!(
            "\n  {} {}",
            ui::yellow("!"),
            ui::dim("detail is not generic — the project name above is visible to everyone.")
        );
    }

    println!(
        "\n{}",
        ui::dim("  Still no card? Discord → Settings → Activity Privacy → \"Display current activity\".")
    );
    Ok(())
}

fn stop() {
    let Some(pid) = stop_daemon() else {
        println!("no daemon running");
        return;
    };
    // Signalling is not stopping — the daemon still has to clear the card. Waiting for
    // it means this command does not claim more than it did, and that a `stop` followed
    // immediately by a fresh session cannot race the departing daemon for the lock.
    if daemon::await_exit(pid, std::time::Duration::from_secs(10)) {
        println!("stopped daemon (pid {pid})");
    } else {
        println!("daemon (pid {pid}) was signalled but is still shutting down");
    }
}

/// Terminate a running daemon, returning the pid it had. Also used by `update`, which
/// has to take the old binary out of the way before starting the new one.
pub fn stop_daemon() -> Option<u32> {
    let pid = daemon::running_pid()?;
    #[cfg(unix)]
    unsafe {
        terminate(pid as i32, 15);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .output();
    }
    Some(pid)
}

#[cfg(unix)]
extern "C" {
    #[link_name = "kill"]
    fn terminate(pid: i32, sig: i32) -> i32;
}

async fn debug_activity(
    details: String,
    state: String,
    client_id: Option<String>,
    hold: u64,
) -> Result<()> {
    let id = client_id.unwrap_or_else(|| config::Config::load().effective_client_id());
    anyhow::ensure!(
        !id.is_empty(),
        "no Discord Application ID — pass --client-id or set it in {}",
        config::config_path().display()
    );

    let mut client = discord::DiscordClient::new(id);
    client
        .set_activity(Some(discord::Activity {
            kind: 0,
            details: Some(details),
            state: Some(state),
            timestamps: Some(discord::Timestamps {
                start: Some(daemon::registry::unix_now()),
            }),
            ..Default::default()
        }))
        .await?;

    tracing::info!("activity set — holding {hold}s, check your Discord profile");
    tokio::time::sleep(std::time::Duration::from_secs(hold)).await;
    client.set_activity(None).await?;
    tracing::info!("activity cleared");
    Ok(())
}
