//! User configuration and the paths everything else agrees on.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// How much of the local workspace may appear on the Discord card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Detail {
    /// Activity and model only. No project name, no branch, no file names.
    #[default]
    Generic,
    /// Adds the project directory name and git branch.
    Project,
    /// Adds the current file or command.
    Full,
}

/// Prepended on save, since `toml` drops the doc comments below.
const CONFIG_HEADER: &str = "\
# agent-presence — edit by hand, or run `agent-presence config` for a menu.
# https://github.com/jx-grxf/agent-presence#privacy
";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub detail: Detail,
    pub show_model: bool,
    /// Globs of paths always forced back to `Generic`, whatever `detail` says.
    pub hidden_paths: Vec<String>,
    /// Discord Application ID. Empty means use the compiled-in default.
    pub client_id: String,
    /// Sessions silent for this long are dropped, in case a hook never fired.
    #[serde(with = "humantime_secs")]
    pub idle_timeout: Duration,
    pub buttons: Vec<ConfigButton>,
    /// Master switch, so presence can be turned off without uninstalling hooks.
    pub enabled: bool,
    /// With several sessions live, show the one in the terminal window you are looking
    /// at. Turn off to always show the most recently active session instead.
    pub follow_focus: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfigButton {
    pub label: String,
    pub url: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            detail: Detail::Generic,
            show_model: true,
            hidden_paths: Vec::new(),
            client_id: String::new(),
            idle_timeout: Duration::from_secs(15 * 60),
            buttons: Vec::new(),
            enabled: true,
            follow_focus: true,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(c) => c,
            Err(e) => {
                // A broken config must never take presence down entirely.
                tracing::warn!("using defaults, config unreadable: {e:#}");
                Self::default()
            }
        }
    }

    fn try_load() -> Result<Self> {
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Write the config back out. Only reached from the editor, so a partial write
    /// would cost the user their settings — hence the temp-file swap.
    pub fn save(&self) -> Result<()> {
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap())?;
        let body = format!("{}\n{}", CONFIG_HEADER, toml::to_string_pretty(self)?);

        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    pub fn effective_client_id(&self) -> String {
        if !self.client_id.is_empty() {
            return self.client_id.clone();
        }
        if let Ok(from_env) = std::env::var("AGENT_PRESENCE_CLIENT_ID") {
            if !from_env.is_empty() {
                return from_env;
            }
        }
        crate::DEFAULT_CLIENT_ID.to_string()
    }

    /// Compile `hidden_paths` once. Invalid globs are skipped with a warning rather
    /// than failing closed, but a glob that fails to compile must not silently widen
    /// what is shown — so we log loudly.
    pub fn hidden_matcher(&self) -> GlobSet {
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.hidden_paths {
            let expanded = expand_tilde(pattern);
            match Glob::new(&expanded) {
                Ok(g) => {
                    builder.add(g);
                }
                Err(e) => tracing::warn!("ignoring invalid hidden_paths glob {pattern:?}: {e}"),
            }
        }
        builder.build().unwrap_or_else(|_| GlobSet::empty())
    }
}

fn expand_tilde(pattern: &str) -> String {
    match pattern.strip_prefix("~/") {
        Some(rest) => home().join(rest).to_string_lossy().into_owned(),
        None => pattern.to_string(),
    }
}

pub fn home() -> PathBuf {
    directories::UserDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    if let Ok(explicit) = std::env::var("AGENT_PRESENCE_HOME") {
        return PathBuf::from(explicit);
    }
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return PathBuf::from(appdata).join("agent-presence");
        }
    }
    home().join(".config").join("agent-presence")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn log_path() -> PathBuf {
    config_dir().join("agent-presence.log")
}

/// Control socket shared by the hook processes and the daemon.
pub fn control_socket_path() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\agent-presence")
    }
    #[cfg(unix)]
    {
        let dir = std::env::var_os("TMPDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        // Include the user so two accounts on one machine never collide. On macOS
        // $TMPDIR is already per-user, but Linux /tmp is shared.
        let user: String = std::env::var("USER")
            .unwrap_or_else(|_| "default".into())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        dir.join(format!("agent-presence-{user}.sock"))
    }
}

/// `idle_timeout = "15m"` in TOML, `Duration` in Rust.
mod humantime_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&format!("{}s", d.as_secs()))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let raw = String::deserialize(d)?;
        parse(&raw).ok_or_else(|| serde::de::Error::custom(format!("bad duration {raw:?}")))
    }

    fn parse(s: &str) -> Option<Duration> {
        let s = s.trim();
        let (num, mult) = match s.chars().last()? {
            'h' => (&s[..s.len() - 1], 3600),
            'm' => (&s[..s.len() - 1], 60),
            's' => (&s[..s.len() - 1], 1),
            _ => (s, 1),
        };
        Some(Duration::from_secs(num.trim().parse::<u64>().ok()? * mult))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_privacy_generic() {
        let c = Config::default();
        assert_eq!(
            c.detail,
            Detail::Generic,
            "the safe default must survive refactors"
        );
        assert!(c.enabled);
    }

    #[test]
    fn parses_a_full_config() {
        let c: Config = toml::from_str(
            r#"
            detail = "project"
            show_model = false
            hidden_paths = ["~/work/**"]
            idle_timeout = "5m"
            [[buttons]]
            label = "GitHub"
            url = "https://github.com/x/y"
            "#,
        )
        .unwrap();
        assert_eq!(c.detail, Detail::Project);
        assert!(!c.show_model);
        assert_eq!(c.idle_timeout, Duration::from_secs(300));
        assert_eq!(c.buttons.len(), 1);
    }

    #[test]
    fn empty_config_file_yields_defaults() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.detail, Detail::Generic);
    }

    #[test]
    fn hidden_glob_matches_expanded_home() {
        let c = Config {
            hidden_paths: vec!["~/work/**".into()],
            ..Default::default()
        };
        let path = home().join("work/secret-thing");
        assert!(c.hidden_matcher().is_match(&path));
    }
}
