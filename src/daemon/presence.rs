//! Turns a registry snapshot into a Discord activity.
//!
//! This is the **only** place that reads `cwd`, and therefore the only place that can
//! leak a project name off the machine. The privacy filter lives here on purpose.

use crate::config::{Config, Detail};
use crate::daemon::registry::Snapshot;
use crate::discord::{self, Assets, Button, Timestamps};
use std::path::Path;

pub fn build(snapshot: &Snapshot, config: &Config) -> discord::Activity {
    let session = &snapshot.primary;
    let detail = effective_detail(session.cwd.as_deref(), config);

    // Line 1: who is working, and on what (if allowed).
    let details = match (detail, project_label(session.cwd.as_deref())) {
        (Detail::Generic, _) | (_, None) => session.agent.label().to_string(),
        (_, Some(project)) => match git_branch(session.cwd.as_deref()) {
            Some(branch) => format!("{project} · {branch}"),
            None => project,
        },
    };

    // Line 2: what it is doing, plus context that survives the filter.
    let mut state = session.activity.verb().to_string();
    if detail == Detail::Full {
        if let Some(target) = &session.target {
            state = format!("{state}: {target}");
        }
    }
    if config.show_model {
        if let Some(model) = session.model.as_deref().map(pretty_model) {
            state = format!("{state} · {model}");
        }
    }
    if snapshot.others > 0 {
        state = format!("{state} · +{} more", snapshot.others);
    }

    discord::Activity {
        kind: 0,
        details: Some(details),
        state: Some(state),
        timestamps: Some(Timestamps {
            start: Some(snapshot.oldest_start_unix),
        }),
        assets: Some(Assets {
            large_image: Some(session.agent.asset_key().to_string()),
            large_text: Some(session.agent.label().to_string()),
            small_image: None,
            small_text: None,
        }),
        buttons: (!config.buttons.is_empty()).then(|| {
            config
                .buttons
                .iter()
                .take(2)
                .map(|b| Button {
                    label: b.label.clone(),
                    url: b.url.clone(),
                })
                .collect()
        }),
    }
}

/// A hidden path is forced back to `Generic` no matter what `detail` says.
fn effective_detail(cwd: Option<&str>, config: &Config) -> Detail {
    match cwd {
        Some(path) if config.hidden_matcher().is_match(path) => Detail::Generic,
        _ => config.detail,
    }
}

fn project_label(cwd: Option<&str>) -> Option<String> {
    let name = Path::new(cwd?).file_name()?.to_string_lossy().into_owned();
    (!name.is_empty()).then_some(name)
}

/// Read the branch straight from `.git/HEAD` — no subprocess, no repo scan.
fn git_branch(cwd: Option<&str>) -> Option<String> {
    let mut dir = Path::new(cwd?);
    loop {
        let head = dir.join(".git").join("HEAD");
        if let Ok(contents) = std::fs::read_to_string(&head) {
            let branch = contents
                .trim()
                .strip_prefix("ref: refs/heads/")?
                .to_string();
            return (!branch.is_empty()).then_some(branch);
        }
        dir = dir.parent()?;
    }
}

/// `claude-opus-4-8` → `Opus 4.8`; `gpt-5-codex` → `GPT-5 Codex`.
fn pretty_model(raw: &str) -> String {
    let cleaned = raw.strip_prefix("claude-").unwrap_or(raw);
    // Drop dated suffixes like `-20251001`.
    let parts: Vec<&str> = cleaned
        .split('-')
        .filter(|p| !(p.len() >= 6 && p.chars().all(|c| c.is_ascii_digit())))
        .collect();

    // Only *trailing* numeric segments are a version: `opus-4-8` → Opus 4.8, while
    // `gpt-5-codex` keeps "codex" as part of the name.
    let split = parts
        .iter()
        .rposition(|p| !p.chars().all(|c| c.is_ascii_digit() || c == '.'))
        .map_or(0, |i| i + 1);
    let (name, version) = parts.split_at(split);

    let name = name
        .iter()
        .map(|p| {
            let upper = p.to_uppercase();
            if upper == "GPT" {
                upper
            } else {
                let mut c = p.chars();
                match c.next() {
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ");

    let version: Vec<&str> = version.iter().copied().filter(|v| v.len() < 5).collect();
    if version.is_empty() {
        name
    } else {
        format!("{name} {}", version.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::registry::Session;
    use crate::event::{Activity, Agent};
    use std::time::Instant;

    fn snapshot(cwd: &str, others: usize) -> Snapshot {
        let now = Instant::now();
        Snapshot {
            primary: Session {
                agent: Agent::Claude,
                activity: Activity::Editing,
                cwd: Some(cwd.into()),
                model: Some("claude-opus-4-8".into()),
                target: Some("main.rs".into()),
                tty: None,
                started_unix: 1000,
                started: now,
                last_seen: now,
            },
            others,
            oldest_start_unix: 900,
        }
    }

    #[test]
    fn generic_default_never_leaks_the_project_name() {
        let a = build(&snapshot("/Users/me/secret-thing", 0), &Config::default());
        let rendered = format!("{:?}", a);
        assert!(
            !rendered.contains("secret-thing"),
            "default config leaked the repo name"
        );
        assert_eq!(a.details.as_deref(), Some("Claude Code"));
        assert_eq!(a.state.as_deref(), Some("Editing code · Opus 4.8"));
    }

    #[test]
    fn project_detail_shows_the_directory() {
        let config = Config {
            detail: Detail::Project,
            ..Default::default()
        };
        let a = build(&snapshot("/Users/me/secret-thing", 0), &config);
        assert_eq!(a.details.as_deref(), Some("secret-thing"));
    }

    #[test]
    fn hidden_paths_override_the_detail_setting() {
        let config = Config {
            detail: Detail::Full,
            hidden_paths: vec!["/Users/me/work/**".into()],
            ..Default::default()
        };
        let a = build(&snapshot("/Users/me/work/client-repo", 0), &config);
        assert_eq!(
            a.details.as_deref(),
            Some("Claude Code"),
            "hidden path must fall back to generic"
        );
    }

    #[test]
    fn full_detail_appends_the_target() {
        let config = Config {
            detail: Detail::Full,
            ..Default::default()
        };
        let a = build(&snapshot("/Users/me/repo", 0), &config);
        assert!(a.state.unwrap().starts_with("Editing code: main.rs"));
    }

    #[test]
    fn concurrent_sessions_are_counted() {
        let a = build(&snapshot("/Users/me/repo", 2), &Config::default());
        assert!(a.state.unwrap().ends_with("+2 more"));
    }

    #[test]
    fn model_names_are_humanised() {
        assert_eq!(pretty_model("claude-opus-4-8"), "Opus 4.8");
        assert_eq!(pretty_model("claude-sonnet-5"), "Sonnet 5");
        assert_eq!(pretty_model("gpt-5-codex"), "GPT 5 Codex");
        assert_eq!(
            pretty_model("claude-haiku-4-5-20251001"),
            "Haiku 4.5",
            "date suffix dropped"
        );
    }

    #[test]
    fn model_can_be_suppressed() {
        let config = Config {
            show_model: false,
            ..Default::default()
        };
        let a = build(&snapshot("/Users/me/repo", 0), &config);
        assert_eq!(a.state.as_deref(), Some("Editing code"));
    }
}
