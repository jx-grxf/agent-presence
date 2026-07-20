mod config;
mod daemon;
mod discord;
mod event;
mod hook;
mod install;
mod ipc;
mod tui;
mod ui;

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
    #[command(subcommand)]
    command: Command,
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
    /// Diagnose a setup that is not showing a card.
    Doctor,
    /// Stop a running daemon.
    Stop,
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
    let command = Cli::parse().command;
    init_logging(matches!(command, Command::Hook { .. } | Command::Daemon));

    match command {
        // Never returns an error: a failing hook must not disturb the agent.
        Command::Hook { agent } => hook::run(agent).await,
        Command::Daemon => daemon::run().await?,
        Command::Install { uninstall, agent } => install::run(uninstall, agent)?,
        Command::Config => tui::run()?,
        Command::Status => status()?,
        Command::Doctor => doctor().await?,
        Command::Stop => stop()?,
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

fn status() -> Result<()> {
    let config = config::Config::load();

    ui::heading("Daemon");
    match daemon::running_pid() {
        Some(pid) => ui::ok(&format!("running {}", ui::dim(&format!("pid {pid}")))),
        None => ui::field(
            "state",
            &ui::dim("not running — starts with your next session"),
        ),
    }

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
        if install::is_installed(&path) {
            any = true;
            ui::ok(&format!(
                "{} {}",
                agent.label(),
                ui::dim(&path.display().to_string())
            ));
        } else if present {
            ui::fail(&format!(
                "{} found, but no hooks — run `agent-presence install`",
                agent.label()
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
        None => ui::warn("not running — it starts itself with your next session"),
    }

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

fn stop() -> Result<()> {
    match daemon::running_pid() {
        Some(pid) => {
            #[cfg(unix)]
            unsafe {
                terminate(pid as i32, 15);
            }
            #[cfg(windows)]
            {
                std::process::Command::new("taskkill")
                    .args(["/PID", &pid.to_string()])
                    .output()?;
            }
            println!("stopped daemon (pid {pid})");
        }
        None => println!("no daemon running"),
    }
    Ok(())
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
