//! The hot path: runs once per agent lifecycle event, inside the agent's process tree.
//!
//! Three rules, all load-bearing:
//!
//! 1. **Never write to stdout.** Claude Code injects hook stdout into the model's
//!    context for `SessionStart` and `UserPromptSubmit`. Anything printed here would
//!    show up as text the model reads.
//! 2. **Always exit 0.** Exit code 2 blocks the agent's tool call outright.
//! 3. **Never block.** Everything is bounded by `HOOK_TIMEOUT`; a missing or wedged
//!    daemon costs the user a few milliseconds, not a stalled session.

use crate::config;
use crate::event::{Agent, EventKind, HookEvent};
use crate::ipc;
use anyhow::Result;
use std::io::Read;

/// Entry point. Returns `Ok` even on failure — see rule 2. Errors are logged only.
pub async fn run(agent: Agent) {
    if let Err(e) = try_run(agent).await {
        tracing::debug!("hook dropped an event: {e:#}");
    }
}

async fn try_run(agent: Agent) -> Result<()> {
    let mut raw = String::new();
    std::io::stdin().read_to_string(&mut raw)?;
    let value: serde_json::Value = serde_json::from_str(&raw)?;

    let mut event = HookEvent::parse(agent, &value)?;
    if event.kind == EventKind::Ignored {
        return Ok(());
    }
    // The agent inherits the terminal we are running under, so our own controlling
    // terminal identifies the window the session lives in.
    event.tty = controlling_tty();

    let socket = config::control_socket_path();
    let deadline = ipc::HOOK_TIMEOUT;

    match tokio::time::timeout(deadline, ipc::send_event(&socket, &event)).await {
        Ok(Ok(())) => Ok(()),
        // No daemon listening. Any event may start one: a daemon can die in the middle
        // of a session — a crash, `agent-presence stop`, a package upgrade replacing the
        // binary underneath it — and restricting the restart to SessionStart left the
        // card dark until the user happened to open a new session. The cooldown below
        // takes over the job that restriction was doing, which was stopping a burst of
        // tool events from spawning a pile of daemons.
        Ok(Err(e)) => {
            if !claim_spawn_slot() {
                return Err(e);
            }
            crate::daemon::spawn_detached(&std::env::current_exe()?)?;
            // Give it a moment to bind, then deliver the event that started it,
            // otherwise this activity would stay invisible until the next tool call.
            tokio::time::sleep(std::time::Duration::from_millis(120)).await;
            let _ = tokio::time::timeout(deadline, ipc::send_event(&socket, &event)).await;
            Ok(())
        }
        Err(_) => anyhow::bail!("daemon did not accept the event within {deadline:?}"),
    }
}

/// Minimum spacing between two spawn attempts. Long enough that a burst of tool events
/// produces one daemon, short enough that a crash is invisible to the user.
const SPAWN_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether this hook is the one allowed to start a daemon right now.
///
/// The file's mtime is the whole state. Losing the race here is harmless — the PID lock
/// in the daemon is what actually guarantees a single instance — so this only has to be
/// good enough to keep a busy turn from forking a hundred processes that all lose it.
fn claim_spawn_slot() -> bool {
    let path = config::config_dir().join("daemon.spawn");
    let recently_tried = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|when| when.elapsed().ok())
        .is_some_and(|since| since < SPAWN_COOLDOWN);
    if recently_tried {
        return false;
    }
    std::fs::create_dir_all(path.parent().unwrap()).ok();
    std::fs::write(&path, b"").is_ok()
}

/// Path of this process's controlling terminal, e.g. `/dev/ttys004`.
///
/// stdin carries the hook payload and stdout/stderr may be redirected, so the terminal
/// is resolved through `/dev/tty` rather than any of the standard descriptors. Returns
/// `None` when the agent runs without a terminal at all (CI, an IDE integration).
#[cfg(unix)]
fn controlling_tty() -> Option<String> {
    use std::ffi::CStr;
    use std::os::fd::AsRawFd;

    let tty = std::fs::File::open("/dev/tty").ok()?;
    let name = unsafe { ttyname(tty.as_raw_fd()) };
    if name.is_null() {
        return None;
    }
    Some(
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned(),
    )
}

#[cfg(not(unix))]
fn controlling_tty() -> Option<String> {
    None
}

#[cfg(unix)]
extern "C" {
    fn ttyname(fd: i32) -> *const std::os::raw::c_char;
}
