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
    /// Let the daemon ask GitHub once a day whether a newer release exists, so `status`
    /// and `doctor` can say so. Nothing is installed unless `auto_update` says so.
    pub update_check: bool,
    /// Install a newer release on the daemon's own initiative.
    ///
    /// Off by default, and it never overwrites the binary itself — it runs whatever
    /// package manager owns the install, the same way `agent-presence update` does, and
    /// only while no session is live so it cannot pull the binary out from under a turn
    /// in progress. A standalone binary has no owner to delegate to, so nothing happens.
    pub auto_update: bool,
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
            update_check: true,
            auto_update: false,
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
            match compile_glob(pattern) {
                Ok(g) => {
                    builder.add(g);
                }
                Err(e) => tracing::warn!("ignoring invalid hidden_paths glob {pattern:?}: {e:#}"),
            }
        }
        builder.build().unwrap_or_else(|_| GlobSet::empty())
    }
}

/// One `hidden_paths` entry, tilde expanded and compiled.
///
/// Shared with the settings editor so a pattern that would be dropped at load time is
/// rejected while it is being typed instead. A glob that silently fails to compile is
/// the worst outcome available here: the user believes a repository is hidden, and it
/// is not.
pub fn compile_glob(pattern: &str) -> Result<Glob> {
    Glob::new(&expand_tilde(pattern)).with_context(|| format!("bad glob {pattern:?}"))
}

/// Split a comma-separated list of globs without cutting brace alternations in half.
///
/// `~/{work,clients}/**` is one pattern, not two — splitting it naively produced `~/{work`
/// and `clients}/**`, neither of which compiles, so both were dropped and the paths the
/// user meant to hide were published.
pub fn split_globs(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for c in raw.chars() {
        match c {
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(c);
            }
            ',' if depth == 0 => out.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    out.push(current);
    out.into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `900s` → `15m`. Written back into the config file so a hand-edited `"15m"` does not
/// come back as `"900s"` the next time the editor saves.
pub fn humanize(d: Duration) -> String {
    let secs = d.as_secs();
    match secs {
        s if s % 3600 == 0 && s > 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 && s > 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
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
///
/// `AGENT_PRESENCE_HOME` moves this too, not just the config file. An instance pointed at
/// its own home has to be genuinely isolated — otherwise a second one binds the socket
/// the first is listening on, and `Listener::bind` unlinks whatever it finds.
pub fn control_socket_path() -> PathBuf {
    if let Ok(explicit) = std::env::var("AGENT_PRESENCE_HOME") {
        #[cfg(windows)]
        {
            // Named pipes are not filesystem paths, so isolate by name instead.
            let key = short_key(&explicit);
            return PathBuf::from(format!(r"\\.\pipe\agent-presence-{key}"));
        }
        #[cfg(unix)]
        {
            let inside = PathBuf::from(&explicit).join("control.sock");
            // A socket path is copied into `sockaddr_un.sun_path`, which is 104 bytes on
            // macOS and 108 on Linux — shorter than plenty of legitimate directories, and
            // exceeding it fails the bind rather than truncating. So a deep home falls
            // back to a short name derived from it, which is still unique per home.
            if inside.as_os_str().len() < 100 {
                return inside;
            }
            return system_temp_dir().join(format!("agent-presence-{}.sock", short_key(&explicit)));
        }
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\agent-presence")
    }
    #[cfg(unix)]
    {
        // Include the user so two accounts on one machine never collide. On macOS
        // $TMPDIR is already per-user, but Linux /tmp is shared.
        let user: String = std::env::var("USER")
            .unwrap_or_else(|_| "default".into())
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        system_temp_dir().join(format!("agent-presence-{user}.sock"))
    }
}

#[cfg(unix)]
fn system_temp_dir() -> PathBuf {
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// A short, stable, filename-safe key for an arbitrary string.
///
/// FNV-1a, because this only has to avoid collisions between a handful of directories on
/// one machine — nothing here is security-sensitive, and a hashing dependency for it would
/// be absurd in a binary this size.
fn short_key(value: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// `idle_timeout = "15m"` in TOML, `Duration` in Rust.
mod humantime_secs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::humanize(*d))
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
    fn brace_alternations_survive_the_comma_split() {
        // Splitting naively produced `~/work/{a` and `b}/**`, neither of which compiles,
        // so both were dropped — and the repositories the user meant to hide were
        // published instead.
        assert_eq!(
            split_globs("~/work/{acme,globex}/**, ~/clients/**"),
            vec!["~/work/{acme,globex}/**", "~/clients/**"]
        );
        assert_eq!(
            split_globs("  ~/a/** , , ~/b/**  "),
            vec!["~/a/**", "~/b/**"]
        );
        assert!(split_globs("").is_empty());
    }

    #[test]
    fn a_brace_glob_actually_hides_the_path() {
        let c = Config {
            hidden_paths: split_globs("~/work/{acme,globex}/**"),
            ..Default::default()
        };
        let m = c.hidden_matcher();
        assert!(m.is_match(home().join("work/acme/billing")));
        assert!(m.is_match(home().join("work/globex/api")));
        assert!(!m.is_match(home().join("work/personal/blog")));
    }

    #[test]
    fn an_uncompilable_glob_is_reported_rather_than_dropped() {
        assert!(compile_glob("~/work/**").is_ok());
        assert!(
            compile_glob("~/work/{unclosed").is_err(),
            "the editor has to be able to refuse this instead of silently ignoring it"
        );
    }

    #[test]
    fn durations_round_trip_in_the_unit_they_were_written() {
        let c = Config {
            idle_timeout: Duration::from_secs(900),
            ..Default::default()
        };
        let text = toml::to_string(&c).unwrap();
        assert!(
            text.contains(r#"idle_timeout = "15m""#),
            "a hand-written 15m must not come back as 900s: {text}"
        );
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.idle_timeout, c.idle_timeout);
    }

    /// `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux. Overrunning it does
    /// not truncate, it fails the bind — and a daemon that cannot bind exits silently.
    #[cfg(unix)]
    #[test]
    fn a_deep_home_still_yields_a_bindable_socket_path() {
        let deep = std::env::temp_dir().join("a".repeat(120));
        std::env::set_var("AGENT_PRESENCE_HOME", &deep);
        let path = control_socket_path();
        std::env::remove_var("AGENT_PRESENCE_HOME");

        assert!(
            path.as_os_str().len() < 104,
            "socket path is {} bytes: {}",
            path.as_os_str().len(),
            path.display()
        );
    }

    #[test]
    fn two_homes_never_share_a_socket() {
        assert_ne!(short_key("/one/home"), short_key("/another/home"));
        assert_eq!(short_key("/one/home"), short_key("/one/home"));
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
