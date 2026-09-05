//! Long-lived process that owns the Discord connection.
//!
//! Exists because Discord permits one activity per connected application while a user
//! runs several agent sessions at once, and because `SET_ACTIVITY` is rate limited
//! (roughly 5 updates per 20s). Hooks fire far faster than that during a busy turn, so
//! updates are coalesced onto a tick instead of being sent one-per-event.

pub mod focus;
pub mod presence;
pub mod registry;

use crate::config::{self, Config};
use crate::discord::{self, DiscordClient};
use crate::event::HookEvent;
use crate::ipc;
use anyhow::{Context, Result};
use registry::Registry;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

/// Minimum spacing between `SET_ACTIVITY` calls. Comfortably inside Discord's limit.
const TICK: Duration = Duration::from_secs(2);
/// How long to keep running with no sessions before exiting, so an idle machine does
/// not carry a stray process forever.
const SHUTDOWN_AFTER_IDLE: Duration = Duration::from_secs(90);
/// Ceiling on the focused-window query, so a stalled terminal cannot stall the tick.
const FOCUS_TIMEOUT: Duration = Duration::from_millis(800);
/// How long to wait for Discord to take the card down before exiting anyway.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(3);

/// What reaches the event loop from a control connection.
enum Incoming {
    Event(HookEvent),
    /// A query, with the channel to answer it on. Handled in the loop because that is
    /// where the registry lives.
    Sessions(tokio::sync::oneshot::Sender<ipc::SessionsReply>),
}

/// Read one control connection to the end, forwarding what it carries to the event loop.
async fn serve<S>(stream: S, tx: mpsc::Sender<Incoming>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        // A bare event is what a hook from an older build sends. The binaries upgrade
        // together but the daemon outlives the upgrade, so both shapes have to work.
        let request = match serde_json::from_str::<ipc::Request>(&line) {
            Ok(r) => r,
            Err(envelope_error) => match serde_json::from_str::<HookEvent>(&line) {
                Ok(event) => ipc::Request::Event {
                    event: Box::new(event),
                },
                Err(_) => {
                    tracing::warn!("unparseable control message: {envelope_error}");
                    continue;
                }
            },
        };

        match request {
            ipc::Request::Event { event } => {
                if tx.send(Incoming::Event(*event)).await.is_err() {
                    return Ok(());
                }
            }
            ipc::Request::Sessions => {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if tx.send(Incoming::Sessions(reply_tx)).await.is_err() {
                    return Ok(());
                }
                let reply = reply_rx.await.context("event loop dropped the query")?;
                let mut body = serde_json::to_vec(&reply)?;
                body.push(b'\n');
                writer.write_all(&body).await?;
                writer.flush().await?;
            }
        }
    }
    Ok(())
}

/// The config as the daemon sees it: reloaded when the file changes, with the privacy
/// globs compiled once rather than on every tick.
struct LiveConfig {
    config: Config,
    hidden: globset::GlobSet,
    stamp: Option<std::time::SystemTime>,
}

impl LiveConfig {
    fn load() -> Self {
        let config = Config::load();
        Self {
            hidden: config.hidden_matcher(),
            config,
            stamp: file_stamp(),
        }
    }

    /// Pick up hand edits and `agent-presence config` without a restart. One `stat` per
    /// tick; the file is only read when its mtime actually moved.
    ///
    /// A changed `client_id` needs no handling here: it rides along on every presence
    /// update, and the task that owns the connection reconnects when it sees a new one.
    fn reload_if_changed(&mut self) {
        let stamp = file_stamp();
        if stamp == self.stamp {
            return;
        }
        self.stamp = stamp;
        let config = Config::load();
        self.hidden = config.hidden_matcher();
        self.config = config;
        tracing::info!("config reloaded");
    }
}

fn file_stamp() -> Option<std::time::SystemTime> {
    std::fs::metadata(config::config_path())
        .and_then(|m| m.modified())
        .ok()
}

pub async fn run() -> Result<()> {
    let _lock = SingleInstance::acquire()?;
    let mut live = LiveConfig::load();
    let socket = config::control_socket_path();

    let (tx, mut rx) = mpsc::channel::<Incoming>(256);

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
                        if let Err(e) = serve(stream, tx).await {
                            tracing::debug!("control connection ended: {e:#}");
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

    if live.config.update_check {
        // Off the event loop and off the critical path: curl can sit on a DNS timeout
        // for seconds, and a presence tick must not wait behind it. The result only
        // lands in a cache file that `status` and `doctor` read later.
        tokio::spawn(async {
            let _ = tokio::task::spawn_blocking(crate::update::refresh_if_stale).await;
        });
    }

    // Discord talks over a socket that can take seconds to answer — probing ten IPC slots
    // with the app closed, or waiting out a handshake — and every one of those seconds was
    // spent inside the `select!` arm, so the daemon serviced no events and no queries
    // while it happened. `agent-presence sessions` timed out against a daemon that was
    // running perfectly well. The connection now lives in its own task and the loop only
    // publishes what it wants shown.
    let (presence_tx, presence_rx) = mpsc::channel::<PresenceUpdate>(4);
    let presence = tokio::spawn(drive_presence(presence_rx));

    let mut registry = Registry::default();
    let mut ticker = tokio::time::interval(TICK);
    let mut idle_since: Option<std::time::Instant> = Some(std::time::Instant::now());

    loop {
        tokio::select! {
            Some(incoming) = rx.recv() => {
                match incoming {
                    Incoming::Event(event) => {
                        tracing::debug!(?event.kind, session = %event.session_id, "event");
                        registry.apply(event);
                    }
                    // Answering from here rather than from the connection task is what
                    // keeps the registry single-owner and lock-free.
                    Incoming::Sessions(reply) => {
                        let _ = reply.send(ipc::SessionsReply {
                            sessions: registry.describe(),
                            card_enabled: live.config.enabled,
                        });
                    }
                }
            }
            _ = ticker.tick() => {
                live.reload_if_changed();
                let config = &live.config;
                registry.expire(config.idle_timeout);

                // Only worth asking the window server when there is a choice to make.
                let hint = if config.follow_focus && registry.has_multiple() {
                    resolve_focus().await
                } else {
                    None
                };

                let desired = registry
                    .snapshot_focused(hint.as_ref())
                    .filter(|_| config.enabled)
                    .map(|snap| presence::build(&snap, config, &live.hidden));

                // `try_send`, so a Discord round trip that has not finished cannot
                // hold up the tick. Presence is latest-wins: if the queue is full the
                // task is already behind and the next tick supersedes this one anyway.
                let _ = presence_tx.try_send(PresenceUpdate {
                    client_id: config.effective_client_id(),
                    activity: desired,
                });

                if registry.is_empty() {
                    let since = idle_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() > SHUTDOWN_AFTER_IDLE {
                        tracing::info!("no sessions for {SHUTDOWN_AFTER_IDLE:?}, exiting");
                        clear_card(presence_tx, presence).await;
                        // On the way out is the safest possible moment to replace the
                        // binary: no session is live, and we are about to release the
                        // lock anyway, so the next hook event starts the new build.
                        if live.config.auto_update && live.config.update_check {
                            let _ = tokio::task::spawn_blocking(
                                crate::update::auto_install_if_available,
                            )
                            .await;
                        }
                        return Ok(());
                    }
                } else {
                    idle_since = None;
                }
            }
            _ = shutdown_signal() => {
                tracing::info!("shutting down");
                clear_card(presence_tx, presence).await;
                return Ok(());
            }
        }
    }
}

/// What the event loop wants shown, handed to the task that owns the Discord connection.
struct PresenceUpdate {
    /// Carried per update so a `client_id` changed in the config takes effect without the
    /// loop having to reach into the connection.
    client_id: String,
    activity: Option<discord::Activity>,
}

/// Own the Discord connection and push whatever the loop last asked for.
///
/// Every slow thing about Discord lives in here: connecting, the handshake, waiting for
/// the echo of a `SET_ACTIVITY`, and reconnecting after the app quits. None of it can
/// delay an event or a query any more.
async fn drive_presence(mut updates: mpsc::Receiver<PresenceUpdate>) {
    let mut client: Option<(String, DiscordClient)> = None;

    while let Some(mut update) = updates.recv().await {
        // A delayed Discord reply must not replay obsolete cards after reconnecting.
        while let Ok(newer) = updates.try_recv() {
            update = newer;
        }
        let reconnect = client
            .as_ref()
            .is_none_or(|(id, _)| *id != update.client_id);
        if reconnect {
            // A different application is a different connection; clear the old card
            // first so it does not linger under the previous identity.
            if let Some((_, old)) = client.as_mut() {
                let _ = old.set_activity(None).await;
            }
            client = Some((
                update.client_id.clone(),
                DiscordClient::new(update.client_id),
            ));
        }

        if let Some((_, client)) = client.as_mut() {
            if let Err(e) = client.set_activity(update.activity).await {
                // Expected whenever Discord is closed. Stay alive and retry next tick.
                tracing::debug!("presence update deferred: {e:#}");
            }
        }
    }
}

/// Take the card down and wait for Discord to acknowledge it, within reason.
///
/// Worth waiting for: leaving a stale card up is exactly what a user notices, and the
/// process is about to exit anyway. Not worth waiting forever, since Discord being
/// unreachable is the ordinary reason this would hang.
async fn clear_card(tx: mpsc::Sender<PresenceUpdate>, mut task: tokio::task::JoinHandle<()>) {
    // The budget includes enqueueing: a full queue behind a stalled Discord connection
    // must not delay shutdown indefinitely before the timeout even starts.
    let shutdown = async {
        let _ = tx
            .send(PresenceUpdate {
                client_id: Config::load().effective_client_id(),
                activity: None,
            })
            .await;
        drop(tx);
        let _ = (&mut task).await;
    };
    if tokio::time::timeout(CLEAR_TIMEOUT, shutdown).await.is_err() {
        task.abort();
        let _ = task.await;
    }
}

/// Resolve the focused terminal off the event loop. The query shells out to
/// `osascript`, which can hang on a wedged app, so it is capped well below the tick.
async fn resolve_focus() -> Option<focus::FocusHint> {
    let query = tokio::task::spawn_blocking(focus::focused_target);
    match tokio::time::timeout(FOCUS_TIMEOUT, query).await {
        Ok(Ok(hint)) => hint,
        Ok(Err(e)) => {
            tracing::debug!("focus query panicked: {e}");
            None
        }
        Err(_) => {
            tracing::debug!("focus query timed out, keeping last-active session");
            None
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

/// Lock file holding the daemon PID, so concurrent hooks racing to spawn a daemon end
/// up with exactly one.
///
/// Losing this race has to be reliable: both winners would bind the control socket, and
/// `Listener::bind` unlinks whatever it finds — so a second daemon silently steals every
/// event from the first, and each clears the other's card.
struct SingleInstance {
    /// Keep the same file open until shutdown. Never unlink it: another process may
    /// already have opened that inode while waiting to claim it.
    locked: std::fs::File,
}

impl SingleInstance {
    fn acquire() -> Result<Self> {
        use std::io::Write;

        let path = config::config_dir().join("daemon.pid");
        std::fs::create_dir_all(config::config_dir()).context("creating daemon state directory")?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);

        // Windows keeps this writer exclusive until its handle closes, including on
        // process death. Readers (status/doctor) remain allowed, but writers and file
        // deletion are refused. No stale-file detection or staging file is needed.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            const FILE_SHARE_READ: u32 = 0x0000_0001;
            options.share_mode(FILE_SHARE_READ);
        }

        let mut file = options.open(&path).with_context(|| {
            format!(
                "opening daemon lock {} (another daemon may hold it)",
                path.display()
            )
        })?;

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            const LOCK_EX: i32 = 2;
            const LOCK_NB: i32 = 4;
            if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .context("acquiring daemon lock (another daemon may hold it)");
            }
        }

        // Only the lock holder may replace the informational PID.
        file.set_len(0)?;
        file.write_all(std::process::id().to_string().as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
        file.flush()?;
        Ok(Self { locked: file })
    }
}

#[cfg(unix)]
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

/// Launch `exe daemon` fully detached, so it outlives whatever started it.
///
/// Shared by the hook (which starts a daemon when none is listening) and `update`
/// (which has to bring one back after replacing the binary underneath it).
pub fn spawn_detached(exe: &std::path::Path) -> Result<()> {
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(unix)]
    {
        // New session, so closing the terminal does not SIGHUP the daemon.
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                if setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    cmd.spawn()
        .with_context(|| format!("spawning {}", exe.display()))?;
    Ok(())
}

#[cfg(unix)]
extern "C" {
    fn setsid() -> i32;
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        // Clear the informational PID while still holding the lock. Keeping the file
        // in place prevents two contenders from locking different inodes at this path.
        if let Err(error) = self.locked.set_len(0) {
            tracing::warn!("could not clear daemon PID: {error}");
        }
    }
}

pub fn running_pid() -> Option<u32> {
    let contents = std::fs::read_to_string(config::config_dir().join("daemon.pid")).ok()?;
    let pid = contents.trim().parse::<u32>().ok()?;
    process_alive(pid).then_some(pid)
}

/// Block until `pid` is gone, or `budget` runs out. Returns whether it actually exited.
///
/// A stopped daemon does not die at the instant it is signalled: it still has to clear
/// the Discord card, which is a round trip to a socket that may not answer. Starting a
/// replacement before that finishes means the replacement loses the lock and exits, and
/// nothing is left running at all — the exact failure `update` used to report as success.
pub fn await_exit(pid: u32, budget: Duration) -> bool {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if !process_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !process_alive(pid)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shutdown_is_bounded_even_with_a_full_presence_queue() {
        let (tx, rx) = mpsc::channel(1);
        tx.send(PresenceUpdate {
            client_id: String::new(),
            activity: None,
        })
        .await
        .unwrap();
        let task = tokio::spawn(async move {
            let _rx = rx;
            std::future::pending::<()>().await;
        });
        let handle = task.abort_handle();
        let result =
            tokio::time::timeout(CLEAR_TIMEOUT + Duration::from_secs(2), clear_card(tx, task))
                .await;
        if result.is_err() {
            handle.abort();
        }
        assert!(
            result.is_ok(),
            "shutdown must bound the send and the Discord task"
        );
        assert!(
            handle.is_finished(),
            "a timed-out Discord task must be stopped"
        );
    }
}
