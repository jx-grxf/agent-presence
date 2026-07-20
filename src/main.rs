mod config;
mod daemon;
mod discord;
mod event;
mod hook;
mod install;
mod ipc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use event::Agent;

/// Default Discord Application ID. Public value, not a secret — Rich Presence
/// authenticates through the logged-in Discord desktop client, never a token.
pub const DEFAULT_CLIENT_ID: &str = "";

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
    let filter = tracing_subscriber::EnvFilter::try_from_env("AGENT_PRESENCE_LOG")
        .unwrap_or_else(|_| "agent_presence=info".into());
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
    match daemon::running_pid() {
        Some(pid) => println!("daemon:   running (pid {pid})"),
        None => println!("daemon:   not running"),
    }
    println!("config:   {}", config::config_path().display());
    println!("detail:   {:?}", config.detail);
    println!("enabled:  {}", config.enabled);
    println!(
        "client:   {}",
        if config.effective_client_id().is_empty() {
            "NOT SET — see README, create a Discord application".to_string()
        } else {
            config.effective_client_id()
        }
    );
    println!("log:      {}", config::log_path().display());
    Ok(())
}

async fn doctor() -> Result<()> {
    status()?;
    println!();

    let id = config::Config::load().effective_client_id();
    if id.is_empty() {
        println!("✗ no Discord Application ID configured");
        return Ok(());
    }
    match discord::DiscordClient::new(id).connect().await {
        Ok(()) => println!("✓ Discord IPC reachable and handshake accepted"),
        Err(e) => println!("✗ Discord: {e:#}"),
    }
    for (agent, path) in install::installed_paths() {
        let state = if install::is_installed(&path) {
            "✓ hooks installed"
        } else {
            "✗ hooks missing"
        };
        println!("{state} for {} ({})", agent.label(), path.display());
    }
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
