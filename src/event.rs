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
    /// Controlling terminal of the agent process, e.g. `/dev/ttys004`. Neither agent
    /// reports this, so the hook fills it in from its own process — see `hook.rs`. Used
    /// only to match the session against the focused terminal window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
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
            tty: None,
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
///
/// Every branch is an allowlist, and deliberately so. Tool inputs carry arbitrary
/// secrets — an API key in `STRIPE_KEY=… deploy`, credentials inside a
/// `postgres://user:pass@host` string, a client's name in an absolute path — and
/// whatever comes back here is published to Discord verbatim at `detail = "full"`.
/// So each branch reduces its input to a shape that *cannot* carry a secret instead
/// of trying to recognize secrets in free-form text, which never holds up.
fn extract_target(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    let label = match tool_name {
        "Bash" | "shell" | "local_shell" => command_label(input["command"].as_str()?)?,
        "WebFetch" => host_of(input["url"].as_str()?)?,
        // A search query is prose the user never meant to publish, and it has no safe
        // reduction — there is no "harmless part" of it. The card just says
        // "Researching".
        "WebSearch" | "web_search" => return None,
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
    let label = label.trim();
    (!label.is_empty()).then(|| label.chars().take(60).collect())
}

/// `git push origin main 2>&1 | tail -3` → `git push`.
///
/// Only the program and, when it is a plain word, its subcommand survive. Arguments are
/// dropped wholesale: they are where the tokens, hosts, paths and connection strings
/// live, and `git push` is what is worth reading on the card anyway.
fn command_label(command: &str) -> Option<String> {
    let mut words = command
        .lines()
        .next()?
        .split_whitespace()
        // Leading `KEY=value` assignments are a common way to hand a secret to a single
        // command. Step over them to reach the program itself.
        .skip_while(|w| w.contains('='));

    // Through `file_name`, so `./scripts/deploy.sh` cannot carry the directories above it.
    let program = plain_word(std::path::Path::new(words.next()?).file_name()?.to_str()?)?;
    Some(match words.next().and_then(plain_word) {
        Some(subcommand) => format!("{program} {subcommand}"),
        None => program.to_string(),
    })
}

/// A bare word: letters, digits, `.`, `-`, `_`. Anything else — a path, a flag, a URL, an
/// assignment, a shell operator, a redirect — is not something we are willing to publish.
fn plain_word(word: &str) -> Option<&str> {
    let plain = !word.is_empty()
        && word.len() <= 24
        && !word.starts_with('-')
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    plain.then_some(word)
}

/// `https://user:tok@api.example.com:8443/v1/x?key=…` → `api.example.com`.
///
/// The path and query are where one-time tokens and presigned signatures live, and the
/// userinfo is credentials outright, so only the host survives.
fn host_of(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = host.split(':').next()?;

    let plain = !host.is_empty()
        && host.len() <= 40
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    plain.then(|| host.to_ascii_lowercase())
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

    /// Everything below is a thing that used to reach Discord verbatim. The assertion
    /// that matters in each case is the `!contains` one — the exact surviving label is
    /// cosmetic, the absence of the secret is not.
    #[test]
    fn command_labels_drop_every_argument() {
        let cases = [
            ("git push origin main 2>&1 | tail -3", Some("git push")),
            ("cargo test --all-features", Some("cargo test")),
            ("npm run build", Some("npm run")),
            // The leak vectors.
            ("STRIPE_KEY=sk_live_9f2 ./deploy.sh", Some("deploy.sh")),
            (
                "curl -H \"Authorization: Bearer tok_x\" https://api.acme.io",
                Some("curl"),
            ),
            ("cat /Users/me/clients/acme/.env", Some("cat")),
            (
                "psql postgres://admin:hunter2@prod-db.internal/app",
                Some("psql"),
            ),
            ("ssh deploy@10.13.7.2", Some("ssh")),
            ("gh api /repos/acme-corp/private-thing", Some("gh api")),
            // Nothing publishable left once the assignment is stepped over.
            ("AWS_SECRET_ACCESS_KEY=wJalr", None),
            // Not a plain program name, so we decline rather than guess.
            ("(cd ~/clients/acme && make)", None),
        ];
        for (command, expected) in cases {
            let label = command_label(command);
            assert_eq!(label.as_deref(), expected, "command: {command}");
            if let Some(label) = &label {
                let leaked = [
                    "sk_live", "tok_x", "hunter2", "acme", "10.13", "wJalr", "clients",
                ]
                .into_iter()
                .find(|secret| label.contains(secret));
                assert_eq!(leaked, None, "leaked from: {command}");
            }
        }
    }

    #[test]
    fn web_fetch_keeps_only_the_host() {
        assert_eq!(
            host_of("https://user:tok_x@api.example.com:8443/v1/keys?token=abc#f").as_deref(),
            Some("api.example.com"),
        );
        assert_eq!(host_of("http://LOCALHOST/x").as_deref(), Some("localhost"));
        assert_eq!(host_of("not a url").as_deref(), None);
    }

    #[test]
    fn web_search_query_is_never_published() {
        let e = parse(
            Agent::Claude,
            r#"{"hook_event_name":"PreToolUse","session_id":"s","tool_name":"WebSearch",
                "tool_input":{"query":"how to fix the acme corp payroll bug"}}"#,
        );
        assert_eq!(e.kind, EventKind::Activity(Activity::Researching));
        assert_eq!(e.target, None);
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
