//! Platform-specific byte stream to the Discord client.
//!
//! Discord exposes ten endpoints, `discord-ipc-0` through `discord-ipc-9`. Several
//! clients (stable, PTB, canary) can run at once, each taking the lowest free slot, so
//! we probe them in order and keep the first that completes a connect.

use anyhow::{bail, Result};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Transport {
    #[cfg(unix)]
    inner: tokio::net::UnixStream,
    #[cfg(windows)]
    inner: tokio::net::windows::named_pipe::NamedPipeClient,
}

impl Transport {
    /// Probe endpoints 0..=9 and return the first that connects.
    pub async fn connect() -> Result<Self> {
        for slot in 0..10 {
            if let Ok(t) = Self::connect_slot(slot).await {
                tracing::debug!(slot, "connected to discord ipc");
                return Ok(t);
            }
        }
        bail!("no discord-ipc-0..9 endpoint accepted a connection (is Discord running?)")
    }

    #[cfg(unix)]
    async fn connect_slot(slot: u8) -> Result<Self> {
        for dir in candidate_dirs() {
            let path = dir.join(format!("discord-ipc-{slot}"));
            if let Ok(inner) = tokio::net::UnixStream::connect(&path).await {
                return Ok(Self { inner });
            }
        }
        bail!("slot {slot} unavailable")
    }

    #[cfg(windows)]
    async fn connect_slot(slot: u8) -> Result<Self> {
        let path = format!(r"\\.\pipe\discord-ipc-{slot}");
        let inner = tokio::net::windows::named_pipe::ClientOptions::new().open(&path)?;
        Ok(Self { inner })
    }

    /// Write one frame. The header and body MUST go out in a single write —
    /// splitting them corrupts the pipe and Discord silently drops the connection.
    pub async fn write_frame(&mut self, opcode: u32, payload: &[u8]) -> Result<()> {
        let mut frame = Vec::with_capacity(8 + payload.len());
        frame.extend_from_slice(&opcode.to_le_bytes());
        frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        frame.extend_from_slice(payload);
        self.inner.write_all(&frame).await?;
        self.inner.flush().await?;
        Ok(())
    }

    pub async fn read_frame(&mut self) -> Result<(u32, Vec<u8>)> {
        let mut header = [0u8; 8];
        self.inner.read_exact(&mut header).await?;
        let opcode = u32::from_le_bytes(header[0..4].try_into().unwrap());
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        if len > 1 << 20 {
            bail!("discord sent an implausible frame length of {len} bytes");
        }
        let mut body = vec![0u8; len];
        self.inner.read_exact(&mut body).await?;
        Ok((opcode, body))
    }
}

/// Directories that may hold the socket. Plain macOS uses `$TMPDIR`; the Linux
/// entries cover Flatpak and Snap installs, which nest the socket a level deeper.
#[cfg(unix)]
fn candidate_dirs() -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = ["XDG_RUNTIME_DIR", "TMPDIR", "TMP", "TEMP"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect();
    bases.push(PathBuf::from("/tmp"));

    let mut dirs = Vec::new();
    for base in bases {
        dirs.push(base.clone());
        for nested in [
            "snap.discord",
            "app/com.discordapp.Discord",
            ".flatpak/dev.vencord.Vesktop/xdg-run",
        ] {
            dirs.push(base.join(nested));
        }
    }
    dirs
}
