//! Wires the hooks into Claude Code and Codex.
//!
//! Both agents use the same nested shape — `hooks.<Event>[].hooks[]` — so one merge
//! routine serves both. Entries are recognised on uninstall by their command pointing
//! at our binary, which means hand-edits and other tools' hooks survive untouched.

use crate::config;
use crate::event::Agent;
use crate::ui;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Events we subscribe to. `PostToolUse` is deliberately absent: it would double the
/// number of hook invocations without changing what the card shows.
///
/// `PermissionRequest` is subscribed alongside `Notification` rather than instead of it.
/// Both report a pending approval, but `Notification` only fires once Claude Code
/// decides the wait is worth interrupting you over, which is several seconds in — so on
/// its own the card was late to say "Waiting for approval".
const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "Stop",
    "SessionEnd",
];

/// Codex's event set, which differs from Claude's only in lacking `Notification`.
const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "SubagentStart",
    "SubagentStop",
    "PreCompact",
    "Stop",
    "SessionEnd",
];

fn events_for(agent: Agent) -> &'static [&'static str] {
    match agent {
        Agent::Claude => CLAUDE_EVENTS,
        Agent::Codex => CODEX_EVENTS,
    }
}

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

/// Which of an agent's events actually carry our hook right now.
#[derive(Debug, Default, Clone)]
pub struct HookStatus {
    pub wired: Vec<&'static str>,
    pub missing: Vec<&'static str>,
}

impl HookStatus {
    pub fn complete(&self) -> bool {
        self.missing.is_empty() && !self.wired.is_empty()
    }
}

/// Read a config file back and report per-event coverage.
///
/// `doctor` needs this rather than a yes/no: a release that subscribes to a new event
/// leaves existing installs partially wired, and "hooks are installed" would hide that
/// the card no longer reports approvals.
pub fn status(path: &Path, agent: Agent) -> HookStatus {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HookStatus::default();
    };
    let Ok(root) = serde_json::from_str::<Value>(&text) else {
        return HookStatus::default();
    };

    let mut status = HookStatus::default();
    for event in events_for(agent) {
        let wired = root["hooks"][*event]
            .as_array()
            .map(|list| list.iter().any(matcher_is_ours))
            .unwrap_or(false);
        if wired {
            status.wired.push(event);
        } else {
            status.missing.push(event);
        }
    }
    status
}

/// What `apply` did to one agent's config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The agent is not installed on this machine.
    Absent,
    Changed,
    /// Already pointing at this binary.
    Unchanged,
}

/// The install itself, with no output of its own, so both the CLI and the onboarding
/// wizard can drive it — a `println!` from inside a full-screen TUI would corrupt it.
pub fn apply(uninstall: bool, only: Option<Agent>) -> Result<Vec<(Agent, Outcome)>> {
    let agents = match only {
        Some(a) => vec![a],
        None => vec![Agent::Claude, Agent::Codex],
    };

    let mut results = Vec::new();
    for agent in agents {
        let path = config_file(agent);
        // Only touch an agent the user actually has installed, unless we are cleaning up.
        let present = path.parent().map(Path::exists).unwrap_or(false);
        if (!uninstall && !present) || (uninstall && !path.exists()) {
            results.push((agent, Outcome::Absent));
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
        }
        results.push((
            agent,
            if changed {
                Outcome::Changed
            } else {
                Outcome::Unchanged
            },
        ));
    }
    Ok(results)
}

pub fn run(uninstall: bool, only: Option<Agent>) -> Result<()> {
    ui::heading(if uninstall {
        "Removing hooks"
    } else {
        "Wiring up your agents"
    });

    // A spinner for a few file writes is theatre; it resolves immediately unless the
    // filesystem cache is cold, which is exactly when the feedback is worth having.
    let spinner = ui::Spinner::start("reading agent configuration…");
    let results = apply(uninstall, only)?;
    drop(spinner);

    let mut touched = 0;
    for (agent, outcome) in results {
        let path = config_file(agent);
        match outcome {
            Outcome::Absent => ui::field(
                "",
                &ui::dim(&format!("{} not installed, skipping", agent.label())),
            ),
            Outcome::Changed => {
                touched += 1;
                let verb = if uninstall { "removed from" } else { "→" };
                ui::ok(&format!(
                    "{} {verb} {}",
                    agent.label(),
                    ui::dim(&path.display().to_string())
                ));
            }
            Outcome::Unchanged => ui::ok(&format!(
                "{} {}",
                agent.label(),
                ui::dim("already up to date")
            )),
        }
    }

    if uninstall {
        println!(
            "\n{}",
            ui::dim(
                "  Hooks gone. Config and logs are untouched — delete them by hand if you want."
            )
        );
        return Ok(());
    }

    ui::heading("Next");
    if touched > 0 {
        ui::field("1", "restart the agent — it reads hooks at startup");
    }
    ui::field(
        "2",
        &format!("{} to verify", ui::cyan("agent-presence doctor")),
    );
    ui::field(
        "3",
        &format!(
            "{} to change what is shown",
            ui::cyan("agent-presence config")
        ),
    );
    println!(
        "\n  {} {}",
        ui::green("✓"),
        ui::dim("nothing identifying is shown by default.")
    );
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

/// Path to write into the agents' hook config.
///
/// `current_exe` resolves symlinks, which for a package-managed install points into a
/// versioned directory — `…/Cellar/agent-presence/0.1.0/bin/…` — that the next upgrade
/// deletes, silently breaking every hook. So prefer whichever `PATH` entry leads to
/// this same binary: package managers keep those stable across versions.
fn stable_exe_path() -> Result<String> {
    let exe = std::env::current_exe()?;
    let target = exe.canonicalize().unwrap_or_else(|_| exe.clone());
    let name = exe.file_name().unwrap_or_default().to_owned();
    let path = std::env::var_os("PATH").unwrap_or_default();

    Ok(pick_stable(
        &exe,
        &target,
        std::env::split_paths(&path).map(|d| d.join(&name)),
    )
    .to_string_lossy()
    .into_owned())
}

/// Split out from `stable_exe_path` so the choice is testable without touching `PATH`.
fn pick_stable(exe: &Path, target: &Path, candidates: impl Iterator<Item = PathBuf>) -> PathBuf {
    for candidate in candidates {
        // Compare canonicalized, so a symlink counts but a same-named copy does not.
        if candidate.canonicalize().is_ok_and(|c| c == target) {
            return candidate;
        }
    }
    exe.to_path_buf()
}

fn add_hooks(root: &mut Value, agent: Agent) -> Result<bool> {
    let exe = stable_exe_path()?;
    let events = events_for(agent);

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
    fn prefers_a_stable_path_entry_over_the_versioned_one() {
        let dir = std::env::temp_dir().join(format!("ap-stable-{}", std::process::id()));
        let versioned = dir.join("Cellar/1.0/bin");
        let stable = dir.join("bin");
        std::fs::create_dir_all(&versioned).unwrap();
        std::fs::create_dir_all(&stable).unwrap();

        let real = versioned.join("agent-presence");
        std::fs::write(&real, b"binary").unwrap();
        let link = stable.join("agent-presence");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(windows)]
        std::fs::copy(&real, &link).unwrap();

        let target = real.canonicalize().unwrap();
        let picked = pick_stable(&real, &target, [link.clone()].into_iter());
        std::fs::remove_dir_all(&dir).ok();

        #[cfg(unix)]
        assert_eq!(picked, link, "the symlinked PATH entry survives an upgrade");
        #[cfg(windows)]
        let _ = picked;
    }

    #[test]
    fn falls_back_to_the_real_path_when_nothing_on_path_matches() {
        let exe = PathBuf::from("/opt/somewhere/agent-presence");
        let picked = pick_stable(
            &exe,
            &exe,
            [PathBuf::from("/usr/bin/agent-presence")].into_iter(),
        );
        assert_eq!(picked, exe);
    }

    #[test]
    fn unrelated_settings_keep_their_original_order() {
        // serde_json's default map is a BTreeMap, which sorted the user's whole
        // settings.json alphabetically on every install — a huge diff in a file we were
        // only meant to add one key to.
        let original = r#"{"model":"opus","includeCoAuthoredBy":false,"env":{"A":"1"}}"#;
        let mut root: Value = serde_json::from_str(original).unwrap();
        add_hooks(&mut root, Agent::Claude).unwrap();

        let rendered = serde_json::to_string(&root).unwrap();
        let model = rendered.find("model").unwrap();
        let coauthored = rendered.find("includeCoAuthoredBy").unwrap();
        let env = rendered.find(r#""env""#).unwrap();
        assert!(
            model < coauthored && coauthored < env,
            "install reordered the user's settings: {rendered}"
        );
    }

    #[test]
    fn status_reports_a_partially_wired_install() {
        // What an older install looks like after a release subscribes to a new event.
        let dir = std::env::temp_dir().join(format!("ap-status-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        let mut root = json!({});
        add_hooks(&mut root, Agent::Claude).unwrap();
        root["hooks"].as_object_mut().unwrap().remove("PreCompact");
        std::fs::write(&path, serde_json::to_string(&root).unwrap()).unwrap();

        let status = status(&path, Agent::Claude);
        std::fs::remove_dir_all(&dir).ok();

        assert!(!status.complete(), "a missing event must not read as done");
        assert_eq!(status.missing, vec!["PreCompact"]);
        assert!(status.wired.contains(&"PreToolUse"));
    }

    #[test]
    fn status_of_a_file_without_our_hooks_is_empty_not_partial() {
        let dir = std::env::temp_dir().join(format!("ap-status-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, r#"{"model":"opus"}"#).unwrap();

        let status = status(&path, Agent::Claude);
        std::fs::remove_dir_all(&dir).ok();

        assert!(status.wired.is_empty());
        assert!(!status.complete());
    }

    /// The plugin wires the same events without going through this module, so nothing
    /// stops the two drifting apart — and when they do, plugin users quietly lose
    /// whatever the new event reported. It shipped a version behind once already.
    #[test]
    fn the_plugin_subscribes_to_the_same_events_as_the_installer() {
        let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/plugin/hooks/hooks.json");
        let text = std::fs::read_to_string(manifest).expect("plugin hooks.json");
        let root: Value = serde_json::from_str(&text).expect("valid JSON");
        let hooks = root["hooks"].as_object().expect("hooks object");

        let mut declared: Vec<&str> = hooks.keys().map(String::as_str).collect();
        let mut expected: Vec<&str> = CLAUDE_EVENTS.to_vec();
        declared.sort_unstable();
        expected.sort_unstable();
        assert_eq!(
            declared, expected,
            "plugin/hooks/hooks.json is out of step with CLAUDE_EVENTS"
        );
    }

    #[test]
    fn the_plugin_version_tracks_the_crate() {
        let manifest = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/plugin/.claude-plugin/plugin.json"
        );
        let text = std::fs::read_to_string(manifest).expect("plugin.json");
        let root: Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(
            root["version"].as_str(),
            Some(env!("CARGO_PKG_VERSION")),
            "bump plugin/.claude-plugin/plugin.json with the crate version"
        );
    }

    #[test]
    fn both_agents_subscribe_to_session_end() {
        // Codex grew SessionEnd; without it, finished Codex sessions sat on the card
        // until the idle timeout reaped them a quarter of an hour later.
        for agent in [Agent::Claude, Agent::Codex] {
            assert!(
                events_for(agent).contains(&"SessionEnd"),
                "{} must be told when a session ends",
                agent.label()
            );
        }
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
