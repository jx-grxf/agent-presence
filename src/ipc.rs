//! Control channel between the short-lived hook processes and the daemon.
//!
//! Newline-delimited JSON, one `HookEvent` per line. Deliberately trivial: the hook
//! side must never block the agent it is attached to.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::io::AsyncWriteExt;

/// Hard ceiling on how long a hook may spend talking to the daemon.
pub const HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

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
pub async fn send_event(path: &Path, event: &crate::event::HookEvent) -> Result<()> {
    let mut stream = connect(path).await.context("daemon not listening")?;
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    stream.write_all(&line).await?;
    stream.flush().await?;
    Ok(())
}
