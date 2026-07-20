//! Long-lived process that owns the Discord connection.
//!
//! Exists because Discord permits one activity per connected application while a user
//! runs several agent sessions at once, and because `SET_ACTIVITY` is rate limited
//! (roughly 5 updates per 20s). Hooks fire far faster than that during a busy turn, so
//! updates are coalesced onto a tick instead of being sent one-per-event.

pub mod presence;
pub mod registry;

use crate::config::{self, Config};
use crate::discord::DiscordClient;
use crate::event::HookEvent;
use crate::ipc;
use anyhow::{Context, Result};
use registry::Registry;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

/// Minimum spacing between `SET_ACTIVITY` calls. Comfortably inside Discord's limit.
const TICK: Duration = Duration::from_secs(2);
/// How long to keep running with no sessions before exiting, so an idle machine does
/// not carry a stray process forever.
const SHUTDOWN_AFTER_IDLE: Duration = Duration::from_secs(90);

pub async fn run() -> Result<()> {
    let _lock = SingleInstance::acquire()?;
    let config = Config::load();
    let socket = config::control_socket_path();

    let (tx, mut rx) = mpsc::channel::<HookEvent>(256);

    #[allow(unused_mut)]
    let mut listener = ipc::Listener::bind(&socket).await?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok(stream) => {
                    let tx = tx.clone();
                    // One connection may carry several lines; a slow client must not
                    // hold up the next hook, so each is handled independently.
                    tokio::spawn(async move {
                        let mut lines = BufReader::new(stream).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            match serde_json::from_str::<HookEvent>(&line) {
                                Ok(event) => {
                                    if tx.send(event).await.is_err() {
                                        return;
                                    }
                                }
                                Err(e) => tracing::warn!("unparseable event: {e}"),
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("control socket accept failed: {e:#}");
                    return;
                }
            }
        }
    });

    tracing::info!("daemon listening on {}", socket.display());

    let mut client = DiscordClient::new(config.effective_client_id());
    let mut registry = Registry::default();
    let mut ticker = tokio::time::interval(TICK);
    let mut idle_since: Option<std::time::Instant> = Some(std::time::Instant::now());

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                tracing::debug!(?event.kind, session = %event.session_id, "event");
                registry.apply(event);
            }
            _ = ticker.tick() => {
                registry.expire(config.idle_timeout);

                let desired = registry
                    .snapshot()
                    .filter(|_| config.enabled)
                    .map(|snap| presence::build(&snap, &config));

                if let Err(e) = client.set_activity(desired).await {
                    // Expected whenever Discord is closed. Stay alive and retry.
                    tracing::debug!("presence update deferred: {e:#}");
                }

                if registry.is_empty() {
                    let since = idle_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() > SHUTDOWN_AFTER_IDLE {
                        tracing::info!("no sessions for {SHUTDOWN_AFTER_IDLE:?}, exiting");
                        let _ = client.set_activity(None).await;
                        return Ok(());
                    }
                } else {
                    idle_since = None;
                }
            }
            _ = shutdown_signal() => {
                tracing::info!("shutting down");
                let _ = client.set_activity(None).await;
                return Ok(());
            }
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return std::future::pending().await,
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// Lock file holding the daemon PID, so concurrent `SessionStart` hooks racing to
/// spawn a daemon end up with exactly one.
struct SingleInstance {
    path: std::path::PathBuf,
}

impl SingleInstance {
    fn acquire() -> Result<Self> {
        let path = config::config_dir().join("daemon.pid");
        std::fs::create_dir_all(path.parent().unwrap()).ok();

        if let Ok(existing) = std::fs::read_to_string(&path) {
            if let Ok(pid) = existing.trim().parse::<u32>() {
                if pid != std::process::id() && process_alive(pid) {
                    anyhow::bail!("daemon already running with pid {pid}");
                }
            }
        }
        std::fs::write(&path, std::process::id().to_string())
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(Self { path })
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // Only clear the lock if it is still ours.
        if let Ok(contents) = std::fs::read_to_string(&self.path) {
            if contents.trim() == std::process::id().to_string() {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

pub fn running_pid() -> Option<u32> {
    let contents = std::fs::read_to_string(config::config_dir().join("daemon.pid")).ok()?;
    let pid = contents.trim().parse::<u32>().ok()?;
    process_alive(pid).then_some(pid)
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    // Signal 0 performs the permission and existence checks without delivering.
    unsafe { kill(pid as i32, 0) == 0 }
}

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

#[cfg(windows)]
fn process_alive(pid: u32) -> bool {
    // No cheap syscall without a windows crate; ask the task list instead.
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
        .unwrap_or(false)
}
