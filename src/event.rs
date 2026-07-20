//! One event model for both agents.
//!
//! Claude Code and Codex both deliver a single JSON object on stdin and agree on the
//! core field names (`session_id`, `cwd`, `model`, `hook_event_name`, `tool_name`,
//! `tool_input`), so one parser covers both. They differ only in a few event names,
//! which `EventKind::parse` reconciles.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn label(self) -> &'static str {
        match self {
            Agent::Claude => "Claude Code",
            Agent::Codex => "Codex",
        }
    }

    /// Art-asset key uploaded to the Discord application.
    pub fn asset_key(self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }
}

impl std::str::FromStr for Agent {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "claude" | "claude-code" => Ok(Agent::Claude),
            "codex" => Ok(Agent::Codex),
            other => anyhow::bail!("unknown agent {other:?} (expected 'claude' or 'codex')"),
        }
    }
}

/// What the agent is doing, already collapsed to the handful of states worth showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Activity {
    Starting,
    Thinking,
    Editing,
    RunningCommands,
    Reading,
    Researching,
    Delegating,
    AwaitingApproval,
    Idle,
}

impl Activity {
    pub fn verb(self) -> &'static str {
        match self {
            Activity::Starting => "Starting up",
            Activity::Thinking => "Thinking",
            Activity::Editing => "Editing code",
            Activity::RunningCommands => "Running commands",
            Activity::Reading => "Reading code",
            Activity::Researching => "Researching",
            Activity::Delegating => "Delegating to subagents",
            Activity::AwaitingApproval => "Waiting for approval",
            Activity::Idle => "Idle",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    SessionStart,
    Activity(Activity),
    SessionEnd,
    /// A hook we install but do not act on. Kept so the daemon can still refresh the
    /// session's last-seen timestamp.
    Ignored,
}

/// A normalized event, ready to send to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HookEvent {
    pub agent: Agent,
    pub session_id: String,
    pub kind: EventKind,
    /// Absolute path of the session's working directory. The daemon applies the
    /// privacy filter — this is never sent to Discord as-is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the current tool is acting on (file name, command). Only surfaced at
    /// `detail = "full"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

impl HookEvent {
    pub fn parse(agent: Agent, raw: &serde_json::Value) -> anyhow::Result<Self> {
        let event_name = raw["hook_event_name"].as_str().unwrap_or_default();
        let tool_name = raw["tool_name"].as_str().unwrap_or_default();

        let kind = match event_name {
            "SessionStart" => EventKind::SessionStart,
            "SessionEnd" => EventKind::SessionEnd,
            "UserPromptSubmit" => EventKind::Activity(Activity::Thinking),
            "Stop" => EventKind::Activity(Activity::Idle),
            "PreToolUse" => EventKind::Activity(classify_tool(tool_name)),
            // Codex names this event directly; Claude Code routes it through Notification.
            "PermissionRequest" => EventKind::Activity(Activity::AwaitingApproval),
            "Notification" => match raw["notification_type"].as_str() {
                Some("permission_prompt") | Some("elicitation_dialog") => {
                    EventKind::Activity(Activity::AwaitingApproval)
                }
                Some("idle_prompt") => EventKind::Activity(Activity::Idle),
                _ => EventKind::Ignored,
            },
            "SubagentStart" => EventKind::Activity(Activity::Delegating),
            _ => EventKind::Ignored,
        };

        let session_id = raw["session_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            // Codex omits session_id on some events; fall back to the cwd so those
            // still land on the right session instead of spawning a phantom one.
            .or_else(|| raw["cwd"].as_str().map(|c| format!("cwd:{c}")))
            .ok_or_else(|| anyhow::anyhow!("event has neither session_id nor cwd"))?;

        Ok(Self {
            agent,
            session_id,
            kind,
            cwd: raw["cwd"].as_str().map(str::to_owned),
            model: raw["model"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
            target: extract_target(tool_name, &raw["tool_input"]),
        })
    }
}

fn classify_tool(tool_name: &str) -> Activity {
    // MCP tools arrive as `mcp__<server>__<tool>`; treat them as generic work.
    match tool_name {
        "Edit" | "Write" | "NotebookEdit" | "MultiEdit" | "apply_patch" => Activity::Editing,
        "Bash" | "BashOutput" | "shell" | "local_shell" => Activity::RunningCommands,
        "Read" | "Grep" | "Glob" | "read_file" | "LSP" => Activity::Reading,
        "WebSearch" | "WebFetch" | "web_search" => Activity::Researching,
        "Task" | "Agent" | "Workflow" => Activity::Delegating,
        _ => Activity::Thinking,
    }
}

/// Pull a short human-readable target out of the tool input.
fn extract_target(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    let raw = match tool_name {
        "Bash" | "shell" | "local_shell" => input["command"].as_str()?.lines().next()?.to_string(),
        "WebSearch" | "web_search" => input["query"].as_str()?.to_string(),
        "WebFetch" => input["url"].as_str()?.to_string(),
        _ => {
            let path = input["file_path"]
                .as_str()
                .or_else(|| input["path"].as_str())?;
            // Only the file name — never the full path, which would leak directory
            // structure even before the privacy filter runs.
            std::path::Path::new(path)
                .file_name()?
                .to_string_lossy()
                .into_owned()
        }
    };
    let raw = raw.trim();
    (!raw.is_empty()).then(|| raw.chars().take(60).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(agent: Agent, json: &str) -> HookEvent {
        HookEvent::parse(agent, &serde_json::from_str(json).unwrap()).unwrap()
    }

    #[test]
    fn parses_claude_pretooluse_edit() {
        let e = parse(
            Agent::Claude,
            r#"{"hook_event_name":"PreToolUse","session_id":"s1","cwd":"/a/b",
                "model":"claude-opus-4-8","tool_name":"Edit",
                "tool_input":{"file_path":"/a/b/src/main.rs"}}"#,
        );
        assert_eq!(e.kind, EventKind::Activity(Activity::Editing));
        assert_eq!(
            e.target.as_deref(),
            Some("main.rs"),
            "must reduce to the file name only"
        );
        assert_eq!(e.session_id, "s1");
    }

    #[test]
    fn parses_codex_shell_the_same_way() {
        let e = parse(
            Agent::Codex,
            r#"{"hook_event_name":"PreToolUse","session_id":"s2","cwd":"/a",
                "tool_name":"shell","tool_input":{"command":"cargo test\nsecond line"}}"#,
        );
        assert_eq!(e.kind, EventKind::Activity(Activity::RunningCommands));
        assert_eq!(
            e.target.as_deref(),
            Some("cargo test"),
            "only the first line"
        );
    }

    #[test]
    fn maps_both_approval_shapes() {
        let claude = parse(
            Agent::Claude,
            r#"{"hook_event_name":"Notification","notification_type":"permission_prompt","session_id":"s"}"#,
        );
        let codex = parse(
            Agent::Codex,
            r#"{"hook_event_name":"PermissionRequest","session_id":"s"}"#,
        );
        assert_eq!(claude.kind, EventKind::Activity(Activity::AwaitingApproval));
        assert_eq!(codex.kind, claude.kind);
    }

    #[test]
    fn unknown_events_are_ignored_not_errors() {
        let e = parse(
            Agent::Claude,
            r#"{"hook_event_name":"PreCompact","session_id":"s"}"#,
        );
        assert_eq!(e.kind, EventKind::Ignored);
    }

    #[test]
    fn falls_back_to_cwd_when_session_id_missing() {
        let e = parse(
            Agent::Codex,
            r#"{"hook_event_name":"Stop","cwd":"/some/repo"}"#,
        );
        assert_eq!(e.session_id, "cwd:/some/repo");
    }

    #[test]
    fn unknown_tools_do_not_panic() {
        let e = parse(
            Agent::Claude,
            r#"{"hook_event_name":"PreToolUse","session_id":"s","tool_name":"mcp__railway__deploy","tool_input":{}}"#,
        );
        assert_eq!(e.kind, EventKind::Activity(Activity::Thinking));
        assert_eq!(e.target, None);
    }
}
