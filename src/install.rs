//! Wires the hooks into Claude Code and Codex.
//!
//! Both agents use the same nested shape — `hooks.<Event>[].hooks[]` — so one merge
//! routine serves both. Entries are recognised on uninstall by their command pointing
//! at our binary, which means hand-edits and other tools' hooks survive untouched.

use crate::config;
use crate::event::Agent;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Events we subscribe to. `PostToolUse` is deliberately absent: it would double the
/// number of hook invocations without changing what the card shows.
const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "Notification",
    "Stop",
    "SessionEnd",
];

/// Codex fires no `SessionEnd`; those sessions are reaped by the daemon's idle timeout.
const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "SubagentStart",
    "Stop",
];

pub fn config_file(agent: Agent) -> PathBuf {
    match agent {
        // Claude Code merges hooks from settings.json.
        Agent::Claude => config::home().join(".claude").join("settings.json"),
        // Codex reads hooks.json next to config.toml.
        Agent::Codex => config::home().join(".codex").join("hooks.json"),
    }
}

pub fn installed_paths() -> Vec<(Agent, PathBuf)> {
    vec![
        (Agent::Claude, config_file(Agent::Claude)),
        (Agent::Codex, config_file(Agent::Codex)),
    ]
}

/// True if the file already contains at least one of our hook entries.
pub fn is_installed(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    text.contains("agent-presence")
}

pub fn run(uninstall: bool, only: Option<Agent>) -> Result<()> {
    let agents = match only {
        Some(a) => vec![a],
        None => vec![Agent::Claude, Agent::Codex],
    };

    for agent in agents {
        let path = config_file(agent);
        // Only touch an agent the user actually has installed, unless we are cleaning up.
        if !uninstall && !path.parent().map(Path::exists).unwrap_or(false) {
            println!(
                "- {} not found, skipping ({})",
                agent.label(),
                path.display()
            );
            continue;
        }
        if uninstall && !path.exists() {
            continue;
        }

        let mut root = read_json(&path)?;
        let changed = if uninstall {
            remove_hooks(&mut root)
        } else {
            add_hooks(&mut root, agent)?
        };

        if changed {
            write_json(&path, &root)?;
            let verb = if uninstall {
                "removed from"
            } else {
                "installed into"
            };
            println!("✓ {} hooks {verb} {}", agent.label(), path.display());
        } else {
            println!("· {} already up to date", agent.label());
        }
    }

    if !uninstall {
        println!("\nRun `agent-presence doctor` to verify, then start a session.");
    }
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) if text.trim().is_empty() => Ok(json!({})),
        Ok(text) => serde_json::from_str(&text).with_context(|| {
            format!(
                "{} is not valid JSON — fix or move it first",
                path.display()
            )
        }),
        Err(_) => Ok(json!({})),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write via a temp file so an interrupted write cannot truncate the user's settings.
    let tmp = path.with_extension("agent-presence.tmp");
    std::fs::write(&tmp, format!("{}\n", serde_json::to_string_pretty(value)?))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn add_hooks(root: &mut Value, agent: Agent) -> Result<bool> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let events = match agent {
        Agent::Claude => CLAUDE_EVENTS,
        Agent::Codex => CODEX_EVENTS,
    };

    let hooks = root
        .as_object_mut()
        .context("settings root is not a JSON object")?
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks
        .as_object_mut()
        .context("`hooks` is not a JSON object")?;

    let mut changed = false;
    for event in events {
        let entry = hook_entry(agent, &exe);
        let list = hooks.entry(*event).or_insert_with(|| json!([]));
        let Some(list) = list.as_array_mut() else {
            continue;
        };

        // Replace an older entry of ours in place, so upgrading a moved binary works.
        if let Some(existing) = list.iter_mut().find(|m| matcher_is_ours(m)) {
            if *existing != entry {
                *existing = entry;
                changed = true;
            }
        } else {
            list.push(entry);
            changed = true;
        }
    }
    Ok(changed)
}

fn hook_entry(agent: Agent, exe: &str) -> Value {
    let handler = match agent {
        // Claude Code supports the exec form, which sidesteps shell quoting entirely.
        Agent::Claude => json!({
            "type": "command",
            "command": exe,
            "args": ["hook", "--agent", "claude"],
            "timeout": 5
        }),
        // Codex takes a single command string.
        Agent::Codex => json!({
            "type": "command",
            "command": format!("\"{exe}\" hook --agent codex"),
            "timeout": 5
        }),
    };
    json!({ "hooks": [handler] })
}

/// Ours if any handler underneath points at our binary.
fn matcher_is_ours(matcher: &Value) -> bool {
    matcher["hooks"]
        .as_array()
        .map(|hs| hs.iter().any(handler_is_ours))
        .unwrap_or(false)
}

/// Identify our entries by the `hook --agent <x>` invocation signature rather than by
/// the binary's file name, so a renamed or relocated binary still uninstalls cleanly.
fn handler_is_ours(handler: &Value) -> bool {
    let command = handler["command"].as_str().unwrap_or_default();
    if command.contains("agent-presence") || command.contains("agent_presence") {
        return true;
    }
    let exec_form = handler["args"]
        .as_array()
        .map(|args| {
            let joined: Vec<&str> = args.iter().filter_map(Value::as_str).collect();
            joined.contains(&"hook") && joined.contains(&"--agent")
        })
        .unwrap_or(false);
    exec_form || (command.contains("hook") && command.contains("--agent"))
}

/// Strip our entries and tidy up any containers we emptied.
fn remove_hooks(root: &mut Value) -> bool {
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return false;
    };

    let mut changed = false;
    for (_event, list) in hooks.iter_mut() {
        if let Some(list) = list.as_array_mut() {
            let before = list.len();
            list.retain(|m| !matcher_is_ours(m));
            changed |= list.len() != before;
        }
    }
    hooks.retain(|_, list| !list.as_array().map(|a| a.is_empty()).unwrap_or(false));

    if hooks.is_empty() {
        root.as_object_mut().map(|o| o.remove("hooks"));
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unrelated_hooks_and_settings() {
        let mut root = json!({
            "model": "opus",
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
                    "hooks": [{"type": "command", "command": "/usr/local/bin/my-linter"}]
                }]
            }
        });
        add_hooks(&mut root, Agent::Claude).unwrap();

        assert_eq!(root["model"], "opus", "unrelated settings must survive");
        let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2, "existing hook kept, ours appended");
        assert_eq!(pre[0]["hooks"][0]["command"], "/usr/local/bin/my-linter");
    }

    #[test]
    fn install_is_idempotent() {
        let mut root = json!({});
        assert!(add_hooks(&mut root, Agent::Claude).unwrap());
        let after_first = root.clone();
        assert!(
            !add_hooks(&mut root, Agent::Claude).unwrap(),
            "second run must be a no-op"
        );
        assert_eq!(root, after_first);
    }

    #[test]
    fn uninstall_removes_only_ours() {
        let mut root = json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{"type": "command", "command": "/usr/local/bin/my-linter"}]
                }]
            }
        });
        add_hooks(&mut root, Agent::Claude).unwrap();
        assert!(remove_hooks(&mut root));

        let pre = root["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["hooks"][0]["command"], "/usr/local/bin/my-linter");
        assert!(
            root["hooks"].get("SessionStart").is_none(),
            "emptied events are pruned"
        );
    }

    #[test]
    fn uninstall_from_a_clean_file_changes_nothing() {
        let mut root = json!({"model": "opus"});
        assert!(!remove_hooks(&mut root));
        assert_eq!(root, json!({"model": "opus"}));
    }

    #[test]
    fn upgrading_a_moved_binary_replaces_the_old_entry() {
        let mut root = json!({});
        add_hooks(&mut root, Agent::Claude).unwrap();
        root["hooks"]["Stop"][0]["hooks"][0]["command"] = json!("/old/path/agent-presence");

        assert!(add_hooks(&mut root, Agent::Claude).unwrap());
        let stop = root["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "must replace in place, not duplicate");
        assert_ne!(stop[0]["hooks"][0]["command"], "/old/path/agent-presence");
    }

    #[test]
    fn codex_uses_a_single_command_string() {
        let mut root = json!({});
        add_hooks(&mut root, Agent::Codex).unwrap();
        let handler = &root["hooks"]["PreToolUse"][0]["hooks"][0];
        assert!(handler["args"].is_null(), "Codex takes no args array");
        assert!(handler["command"]
            .as_str()
            .unwrap()
            .contains("--agent codex"));
    }
}
