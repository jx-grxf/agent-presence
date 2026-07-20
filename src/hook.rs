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

    let event = HookEvent::parse(agent, &value)?;
    if event.kind == EventKind::Ignored {
        return Ok(());
    }

    let socket = config::control_socket_path();
    let deadline = ipc::HOOK_TIMEOUT;

    match tokio::time::timeout(deadline, ipc::send_event(&socket, &event)).await {
        Ok(Ok(())) => Ok(()),
        // No daemon listening. Start one — but only for SessionStart, so a burst of
        // tool events can never spawn a pile of daemons racing for the same lock.
        Ok(Err(e)) => {
            if event.kind == EventKind::SessionStart {
                spawn_daemon()?;
                // Give it a moment to bind, then deliver the event that started it,
                // otherwise the session would stay invisible until the next tool call.
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                let _ = tokio::time::timeout(deadline, ipc::send_event(&socket, &event)).await;
                Ok(())
            } else {
                Err(e)
            }
        }
        Err(_) => anyhow::bail!("daemon did not accept the event within {deadline:?}"),
    }
}

/// Launch the daemon fully detached, so it outlives this hook and the agent session.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe()?;
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
                if libc_setsid() == -1 {
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

    cmd.spawn()?;
    Ok(())
}

#[cfg(unix)]
extern "C" {
    #[link_name = "setsid"]
    fn libc_setsid() -> i32;
}
