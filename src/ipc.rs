//! Control channel between the short-lived hook processes and the daemon.
//!
//! Newline-delimited JSON, one message per line. Deliberately trivial: the hook side must
//! never block the agent it is attached to.
//!
//! Most traffic is one-way — a hook reports an event and hangs up. `Request::Sessions` is
//! the exception: the daemon answers on the same connection with the sessions it is
//! tracking, which is what lets `status`, `doctor` and the settings editor show why the
//! card says what it says.

use crate::event::{Activity, Agent, HookEvent};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Hard ceiling on how long a hook may spend talking to the daemon.
pub const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
/// A query waits longer than an event: it is only ever issued by an interactive command,
/// and the daemon has to reach its event loop to answer.
pub const QUERY_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// What a client is asking the daemon to do.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Report one lifecycle event. Fire and forget.
    Event {
        #[serde(flatten)]
        event: Box<HookEvent>,
    },
    /// Ask what the daemon is tracking. Answered with a `SessionsReply`.
    Sessions,
}

/// One live session, as the daemon sees it.
///
/// This never leaves the machine, so unlike the Discord card it carries the real working
/// directory — the whole point is to answer "which of my checkouts is that?". The socket
/// is owner-only for the same reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub agent: Agent,
    pub activity: Activity,
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// Seconds since this session last reported anything.
    pub quiet_secs: u64,
    /// Seconds since it first appeared.
    pub age_secs: u64,
    /// Whether this is the session currently on the card.
    pub on_card: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionsReply {
    pub sessions: Vec<SessionInfo>,
    /// Whether the card is suppressed by `enabled = false`, so a caller can explain an
    /// empty Discord profile that has nothing to do with the sessions listed.
    pub card_enabled: bool,
}

#[cfg(unix)]
pub use unix::{connect, Listener};
#[cfg(windows)]
pub use windows::{connect, Listener};

#[cfg(unix)]
mod unix {
    use super::*;
    use tokio::net::{UnixListener, UnixStream};

    pub struct Listener {
        inner: UnixListener,
        path: std::path::PathBuf,
    }

    impl Listener {
        pub async fn bind(path: &Path) -> Result<Self> {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            // A socket file left behind by a killed daemon would block the bind.
            // Removing it is safe because single-instance locking happens before this.
            let _ = std::fs::remove_file(path);
            let inner = UnixListener::bind(path)
                .with_context(|| format!("binding control socket {}", path.display()))?;

            // Owner-only. On Linux /tmp is shared between accounts, and this socket both
            // hands out working directories and accepts events that drive the card.
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));

            Ok(Self {
                inner,
                path: path.to_path_buf(),
            })
        }

        pub async fn accept(&self) -> Result<UnixStream> {
            Ok(self.inner.accept().await?.0)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub async fn connect(path: &Path) -> Result<UnixStream> {
        Ok(UnixStream::connect(path).await?)
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};

    pub struct Listener {
        name: String,
        next: Option<NamedPipeServer>,
    }

    impl Listener {
        pub async fn bind(path: &Path) -> Result<Self> {
            let name = path.to_string_lossy().into_owned();
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)
                .with_context(|| format!("creating control pipe {name}"))?;
            Ok(Self {
                name,
                next: Some(next),
            })
        }

        /// Named pipes need a fresh server instance per client, created before the
        /// current one is handed off so no connection attempt hits a missing pipe.
        pub async fn accept(&mut self) -> Result<NamedPipeServer> {
            let server = self.next.take().context("listener not initialised")?;
            server.connect().await?;
            self.next = Some(ServerOptions::new().create(&self.name)?);
            Ok(server)
        }
    }

    pub async fn connect(path: &Path) -> Result<tokio::net::windows::named_pipe::NamedPipeClient> {
        Ok(ClientOptions::new().open(path.to_string_lossy().as_ref())?)
    }
}

/// Send one event to the daemon. Caller is responsible for the overall timeout.
pub async fn send_event(path: &Path, event: &HookEvent) -> Result<()> {
    let mut stream = connect(path).await.context("daemon not listening")?;
    let request = Request::Event {
        event: Box::new(event.clone()),
    };
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;
    Ok(())
}

/// Ask the daemon what it is tracking.
pub async fn query_sessions(path: &Path) -> Result<SessionsReply> {
    let mut stream = connect(path).await.context("daemon not listening")?;
    let mut line = serde_json::to_vec(&Request::Sessions)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;

    let mut answer = String::new();
    BufReader::new(&mut stream)
        .read_line(&mut answer)
        .await
        .context("daemon closed the connection without answering")?;
    serde_json::from_str(&answer).context("daemon sent a reply we could not read")
}

/// Convenience for the one-shot commands: the sessions, or `None` if no daemon answers.
pub async fn sessions_or_none() -> Option<SessionsReply> {
    let socket = crate::config::control_socket_path();
    tokio::time::timeout(QUERY_TIMEOUT, query_sessions(&socket))
        .await
        .ok()?
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EventKind;

    fn an_event() -> HookEvent {
        HookEvent {
            agent: Agent::Claude,
            session_id: "s1".into(),
            kind: EventKind::Activity(Activity::Editing),
            cwd: Some("/repo".into()),
            model: None,
            target: None,
            tty: None,
        }
    }

    #[test]
    fn an_event_request_round_trips() {
        let request = Request::Event {
            event: Box::new(an_event()),
        };
        let line = serde_json::to_string(&request).unwrap();
        match serde_json::from_str::<Request>(&line).unwrap() {
            Request::Event { event } => assert_eq!(*event, an_event()),
            Request::Sessions => panic!("wrong variant"),
        }
    }

    #[test]
    fn a_bare_event_still_parses_as_one() {
        // A hook from an older build writes the event with no envelope around it. The
        // binaries are upgraded together but the daemon outlives the upgrade, so the
        // daemon has to keep understanding the old shape.
        let bare = serde_json::to_string(&an_event()).unwrap();
        assert!(serde_json::from_str::<Request>(&bare).is_err());
        assert_eq!(
            serde_json::from_str::<HookEvent>(&bare).unwrap(),
            an_event()
        );
    }

    #[test]
    fn a_sessions_request_is_a_bare_tag() {
        assert_eq!(
            serde_json::to_string(&Request::Sessions).unwrap(),
            r#"{"op":"sessions"}"#
        );
    }
}
