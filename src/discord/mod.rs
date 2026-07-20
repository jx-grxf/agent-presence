//! Minimal Discord Rich Presence client.
//!
//! Speaks the local IPC protocol directly rather than going through the (deprecated)
//! native SDK. Only a public **Application ID** is required — Rich Presence needs no
//! bot and no token, because the connection is authenticated by the Discord desktop
//! client the user is already logged into.
//!
//! Wire format: `[u32 LE opcode][u32 LE length][utf8 JSON]`.

mod transport;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::time::Duration;
use transport::Transport;

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;
const OP_CLOSE: u32 = 2;
const OP_PING: u32 = 3;
const OP_PONG: u32 = 4;

/// Discord rejects `details`/`state` shorter than 2 bytes and truncates past 128.
const FIELD_MIN: usize = 2;
const FIELD_MAX: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Activity {
    /// 0 = "Playing". The only type a non-verified app may set over IPC.
    #[serde(rename = "type")]
    pub kind: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamps: Option<Timestamps>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assets: Option<Assets>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buttons: Option<Vec<Button>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Timestamps {
    /// Unix seconds. Discord renders this as a live "XX:XX elapsed" counter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct Assets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub large_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub large_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Button {
    pub label: String,
    pub url: String,
}

impl Activity {
    /// Clamp every user-visible string to what Discord accepts. A field that would be
    /// rejected is dropped rather than sent, since one bad field fails the whole call.
    pub fn sanitized(mut self) -> Self {
        self.details = self.details.and_then(clamp_field);
        self.state = self.state.and_then(clamp_field);
        if let Some(assets) = self.assets.as_mut() {
            assets.large_text = assets.large_text.take().and_then(clamp_field);
            assets.small_text = assets.small_text.take().and_then(clamp_field);
        }
        if let Some(buttons) = self.buttons.as_mut() {
            buttons.truncate(2);
        }
        self
    }
}

fn clamp_field(s: String) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.len() < FIELD_MIN {
        return None;
    }
    if trimmed.len() <= FIELD_MAX {
        return Some(trimmed.to_string());
    }
    // Truncate on a char boundary, leaving room for the ellipsis.
    let mut end = FIELD_MAX - 1;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Some(format!("{}…", &trimmed[..end]))
}

pub struct DiscordClient {
    client_id: String,
    conn: Option<Transport>,
    /// Last activity we successfully sent, so repeated ticks are no-ops.
    last_sent: Option<Option<Activity>>,
}

impl DiscordClient {
    pub fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            conn: None,
            last_sent: None,
        }
    }

    /// Drop the connection so the next call re-handshakes. Used when Discord quits.
    fn reset(&mut self) {
        self.conn = None;
        self.last_sent = None;
    }

    pub async fn connect(&mut self) -> Result<()> {
        if self.conn.is_some() {
            return Ok(());
        }
        let mut transport = Transport::connect().await?;

        let handshake = serde_json::json!({ "v": 1, "client_id": self.client_id });
        transport
            .write_frame(OP_HANDSHAKE, handshake.to_string().as_bytes())
            .await
            .context("handshake write failed")?;

        // Discord answers with DISPATCH/READY, or closes with an error payload if the
        // client_id is not a real application.
        let (opcode, body) = tokio::time::timeout(Duration::from_secs(5), transport.read_frame())
            .await
            .context("discord did not answer the handshake within 5s")??;

        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        if opcode == OP_CLOSE {
            bail!(
                "discord refused the handshake: {}",
                parsed["message"]
                    .as_str()
                    .unwrap_or("unknown reason — is the Application ID correct?")
            );
        }
        if parsed["evt"] != "READY" {
            bail!("unexpected handshake reply: {parsed}");
        }

        let user = parsed["data"]["user"]["username"]
            .as_str()
            .unwrap_or("?")
            .to_string();
        tracing::info!(user, "discord ready");
        self.conn = Some(transport);
        Ok(())
    }

    /// Push an activity, or `None` to clear the card. Skipped when unchanged.
    pub async fn set_activity(&mut self, activity: Option<Activity>) -> Result<()> {
        let activity = activity.map(Activity::sanitized);
        if self.last_sent.as_ref() == Some(&activity) {
            return Ok(());
        }
        self.connect().await?;

        let payload = serde_json::json!({
            "cmd": "SET_ACTIVITY",
            "nonce": uuid::Uuid::new_v4().to_string(),
            "args": { "pid": std::process::id(), "activity": activity },
        });

        match self.roundtrip(payload).await {
            Ok(()) => {
                self.last_sent = Some(activity);
                Ok(())
            }
            Err(e) => {
                // A broken pipe means Discord quit; forget the connection so the next
                // tick reconnects instead of erroring forever.
                self.reset();
                Err(e)
            }
        }
    }

    async fn roundtrip(&mut self, payload: serde_json::Value) -> Result<()> {
        let conn = self.conn.as_mut().context("not connected")?;
        conn.write_frame(OP_FRAME, payload.to_string().as_bytes())
            .await?;

        // Discord echoes every command. Answer keepalive pings encountered on the way.
        loop {
            let (opcode, body) =
                tokio::time::timeout(Duration::from_secs(5), conn.read_frame()).await??;
            match opcode {
                OP_PING => conn.write_frame(OP_PONG, &body).await?,
                OP_CLOSE => bail!("discord closed the connection"),
                _ => {
                    let parsed: serde_json::Value =
                        serde_json::from_slice(&body).unwrap_or_default();
                    if parsed["evt"] == "ERROR" {
                        bail!("SET_ACTIVITY rejected: {}", parsed["data"]["message"]);
                    }
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_fields_discord_would_reject() {
        let a = Activity {
            details: Some("x".into()),
            state: Some("ok".into()),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(a.details, None, "1-char details must be dropped, not sent");
        assert_eq!(a.state.as_deref(), Some("ok"));
    }

    #[test]
    fn truncates_long_fields_on_char_boundary() {
        let a = Activity {
            details: Some("ü".repeat(200)),
            ..Default::default()
        }
        .sanitized();
        let details = a.details.unwrap();
        assert!(details.len() <= FIELD_MAX + 3);
        assert!(details.ends_with('…'));
    }

    #[test]
    fn omits_empty_fields_from_the_wire_format() {
        let json = serde_json::to_string(&Activity {
            kind: 0,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(json, r#"{"type":0}"#);
    }
}
