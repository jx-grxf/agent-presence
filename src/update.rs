//! Version checks and the `update` command.
//!
//! The binary is normally owned by a package manager, so updating means running *that
//! manager* rather than overwriting ourselves in place. A self-replaced Homebrew binary
//! leaves the Cellar disagreeing with the receipt, and every later `brew upgrade` either
//! reverts the change or refuses outright. So this module detects who owns the install
//! and delegates.

use crate::config;
use crate::ui;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const REPO_URL: &str = "https://github.com/jx-grxf/agent-presence";

/// Redirects to the newest tag, which is enough to read the version without touching the
/// GitHub API — and therefore without its 60-requests-per-hour cap on anonymous callers.
const LATEST_RELEASE_URL: &str = "https://github.com/jx-grxf/agent-presence/releases/latest";

/// How long a cached answer stays good.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Ceiling on the network call, so neither the daemon nor `update` can hang on it.
const NETWORK_TIMEOUT: &str = "8";

const NULL_DEVICE: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

// ---------------------------------------------------------------------------
// Who owns this binary
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    Homebrew,
    Scoop,
    Cargo,
    /// A binary the user put there themselves. Nothing owns it, so nothing can upgrade
    /// it on their behalf.
    Standalone(PathBuf),
}

impl Install {
    pub fn detect() -> Self {
        let exe = std::env::current_exe().unwrap_or_default();
        // Homebrew's entry on PATH is a symlink into the Cellar, and the Cellar is where
        // the ownership is actually visible — so resolve before matching.
        let resolved = std::fs::canonicalize(&exe).unwrap_or_else(|_| exe.clone());
        Self::from_paths(&resolved, &exe)
    }

    fn from_paths(resolved: &Path, exe: &Path) -> Self {
        let hay = resolved
            .to_string_lossy()
            .replace('\\', "/")
            .to_ascii_lowercase();
        if hay.contains("/cellar/") || hay.contains("/homebrew/") || hay.contains("/linuxbrew/") {
            Install::Homebrew
        } else if hay.contains("/scoop/apps/") {
            Install::Scoop
        } else if hay.contains("/.cargo/bin/") {
            Install::Cargo
        } else {
            Install::Standalone(exe.to_path_buf())
        }
    }

    pub fn label(&self) -> String {
        match self {
            Install::Homebrew => "Homebrew".into(),
            Install::Scoop => "Scoop".into(),
            Install::Cargo => "cargo install".into(),
            Install::Standalone(path) => format!("standalone ({})", path.display()),
        }
    }

    /// The commands that upgrade this install, in order. `None` when nothing owns the
    /// binary and the user has to replace the file themselves.
    fn upgrade_steps(&self) -> Option<Vec<Vec<String>>> {
        let owned = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match self {
            Install::Homebrew => Some(vec![
                owned(&["brew", "update"]),
                owned(&["brew", "upgrade", "agent-presence"]),
            ]),
            // `scoop` is a PowerShell function, not an executable on PATH.
            Install::Scoop => Some(vec![owned(&[
                "powershell",
                "-NoProfile",
                "-Command",
                "scoop update agent-presence",
            ])]),
            Install::Cargo => Some(vec![owned(&[
                "cargo", "install", "--git", REPO_URL, "--force",
            ])]),
            Install::Standalone(_) => None,
        }
    }
}

// ---------------------------------------------------------------------------
// What the newest version is
// ---------------------------------------------------------------------------

/// Ask GitHub which release is newest.
///
/// Shells out to `curl` on purpose: bundling an HTTPS stack would multiply the binary
/// size for one request a day, and curl ships with macOS, every mainstream Linux, and
/// Windows 10 onwards. Only the redirect target is read, never a response body.
pub fn fetch_latest() -> Result<String> {
    let output = Command::new("curl")
        .args([
            "-sSLI",
            "--max-time",
            NETWORK_TIMEOUT,
            "-o",
            NULL_DEVICE,
            "-w",
            "%{url_effective}",
            LATEST_RELEASE_URL,
        ])
        .output()
        .context("running curl — is it on PATH?")?;

    anyhow::ensure!(
        output.status.success(),
        "curl failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );

    let url = String::from_utf8_lossy(&output.stdout);
    version_from_url(url.trim())
        .with_context(|| format!("no version in the redirect target {:?}", url.trim()))
}

/// `https://github.com/o/r/releases/tag/v0.2.3` → `0.2.3`.
fn version_from_url(url: &str) -> Option<String> {
    let tag = url.rsplit('/').next()?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    parse_version(version).map(|_| version.to_string())
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    // Pre-release and build metadata do not participate in the comparison.
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// Whether `candidate` is a release worth moving to. Unparseable input is not — a
/// version we cannot read must never nag the user.
pub fn is_newer(candidate: &str, installed: &str) -> bool {
    match (parse_version(candidate), parse_version(installed)) {
        (Some(new), Some(old)) => new > old,
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// The cached daily check
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Cache {
    checked_unix: u64,
    latest: String,
}

fn cache_path() -> PathBuf {
    config::config_dir().join("update-check.json")
}

fn read_cache() -> Option<Cache> {
    serde_json::from_str(&std::fs::read_to_string(cache_path()).ok()?).ok()
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// An update worth telling the user about, or `None`.
///
/// Reads the cache only — never the network — so `status` and `doctor` stay instant and
/// behave the same on a machine that is offline.
pub fn available() -> Option<String> {
    let cache = read_cache()?;
    is_newer(&cache.latest, current()).then_some(cache.latest)
}

/// Refresh the cache if a day has passed. Called from the daemon, off the event loop.
///
/// A failed check still stamps the cache. Otherwise a machine without network would run
/// a fresh curl on every daemon start, which on a busy day is every few minutes.
pub fn refresh_if_stale() {
    let previous = read_cache();
    if let Some(cache) = &previous {
        let age = unix_now().saturating_sub(cache.checked_unix);
        if age < CHECK_INTERVAL.as_secs() {
            return;
        }
    }

    let latest = match fetch_latest() {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("update check failed: {e:#}");
            previous
                .map(|c| c.latest)
                .unwrap_or_else(|| current().into())
        }
    };
    if let Err(e) = write_cache(&latest) {
        tracing::debug!("could not cache the update check: {e:#}");
    }
}

fn write_cache(latest: &str) -> Result<()> {
    let path = cache_path();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let body = serde_json::to_vec(&Cache {
        checked_unix: unix_now(),
        latest: latest.to_string(),
    })?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// `agent-presence update`
// ---------------------------------------------------------------------------

pub fn run(check_only: bool) -> Result<()> {
    let install = Install::detect();

    ui::heading("Update");
    ui::field("installed", current());
    ui::field("source", &ui::dim(&install.label()));

    let spinner = ui::Spinner::start("checking for a newer release…");
    let latest = match fetch_latest() {
        Ok(v) => {
            spinner.succeed(&format!("latest release is {v}"));
            v
        }
        Err(e) => {
            spinner.fail_with(&format!("{e:#}"));
            return Ok(());
        }
    };
    // Even a same-version answer is worth caching: it pushes the daemon's next check out
    // by a day and keeps `status` from claiming an update the user just installed.
    let _ = write_cache(&latest);

    if !is_newer(&latest, current()) {
        println!("\n{}", ui::dim("  Already up to date."));
        return Ok(());
    }
    if check_only {
        println!(
            "\n  {} {}",
            ui::yellow("!"),
            format_args!("v{latest} is available — run `agent-presence update` to install it")
        );
        return Ok(());
    }

    let Some(steps) = install.upgrade_steps() else {
        ui::heading("Next step");
        ui::warn("nothing owns this binary, so it has to be replaced by hand");
        println!(
            "  {}",
            ui::dim(&format!(
                "{REPO_URL}/releases/latest — then run `agent-presence stop`"
            ))
        );
        return Ok(());
    };

    ui::heading("Upgrading");
    for step in &steps {
        let shown = step.join(" ");
        ui::field("run", &ui::cyan(&shown));
        let status = Command::new(&step[0])
            .args(&step[1..])
            .status()
            .with_context(|| format!("running `{shown}`"))?;
        anyhow::ensure!(status.success(), "`{shown}` failed");
    }

    restart_daemon();
    println!(
        "\n{}",
        ui::dim("  Done. The card comes back on the next tool call.")
    );
    Ok(())
}

/// Stop the old daemon and start the new one.
///
/// The step that made the last upgrade look broken. A running daemon keeps executing the
/// *old* binary — the package manager has already replaced the file underneath it — and
/// nothing used to bring it back, so the card stayed dark until the user happened to
/// open a fresh agent session.
fn restart_daemon() {
    // Nothing was running, so there is nothing to bring back — the next hook event
    // starts one on its own.
    if crate::stop_daemon().is_none() {
        return;
    }

    let exe = daemon_binary();
    match crate::daemon::spawn_detached(&exe) {
        Ok(()) => ui::ok("restarted the daemon on the new binary"),
        // Not fatal: the next hook event starts one anyway, now that any event can.
        Err(e) => ui::warn(&format!(
            "could not restart the daemon ({e:#}) — the next tool call will"
        )),
    }
}

/// Which binary to launch the new daemon from.
///
/// Not `current_exe`: that resolves to the file this process was started from, which for
/// a Homebrew install is a Cellar path that `brew upgrade` has just deleted. The name is
/// resolved through PATH first, so it lands on whatever was installed a moment ago.
fn daemon_binary() -> PathBuf {
    let locator = if cfg!(windows) { "where" } else { "which" };
    let from_path = Command::new(locator)
        .arg("agent-presence")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .map(|line| PathBuf::from(line.trim()))
        })
        .filter(|p| p.exists());

    from_path
        .or_else(|| std::env::current_exe().ok().filter(|p| p.exists()))
        .unwrap_or_else(|| PathBuf::from("agent-presence"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_version_out_of_a_release_redirect() {
        assert_eq!(
            version_from_url("https://github.com/jx-grxf/agent-presence/releases/tag/v0.2.3")
                .as_deref(),
            Some("0.2.3")
        );
        // The un-redirected URL carries no version, and must not be mistaken for one.
        assert_eq!(version_from_url(LATEST_RELEASE_URL), None);
        assert_eq!(version_from_url("https://example.com/tag/nightly"), None);
    }

    #[test]
    fn compares_versions_numerically_not_lexically() {
        assert!(
            is_newer("0.10.0", "0.9.0"),
            "10 > 9 despite the string order"
        );
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.2.2", "0.2.2"));
        assert!(!is_newer("0.2.1", "0.2.2"), "never offer a downgrade");
        // A version we cannot read must not nag.
        assert!(!is_newer("nightly", "0.2.2"));
        assert!(!is_newer("0.2.3.4", "0.2.2"));
    }

    #[test]
    fn detects_who_owns_the_binary() {
        let cases = [
            (
                "/opt/homebrew/Cellar/agent-presence/0.2.2/bin/agent-presence",
                Install::Homebrew,
            ),
            (
                "/home/linuxbrew/.linuxbrew/Cellar/agent-presence/0.2.2/bin/agent-presence",
                Install::Homebrew,
            ),
            (
                "C:/Users/j/scoop/apps/agent-presence/current/agent-presence.exe",
                Install::Scoop,
            ),
            ("/Users/j/.cargo/bin/agent-presence", Install::Cargo),
        ];
        for (path, expected) in cases {
            let p = Path::new(path);
            assert_eq!(Install::from_paths(p, p), expected, "path: {path}");
        }

        let loose = Path::new("/usr/local/bin/agent-presence");
        assert_eq!(
            Install::from_paths(loose, loose),
            Install::Standalone(loose.to_path_buf()),
            "an unowned binary has no manager to delegate to"
        );
    }

    #[test]
    fn every_managed_install_has_a_runnable_step() {
        for install in [Install::Homebrew, Install::Scoop, Install::Cargo] {
            let steps = install.upgrade_steps().expect("managed install");
            assert!(!steps.is_empty());
            assert!(
                steps.iter().all(|s| !s.is_empty()),
                "a step with no program would panic on step[0]"
            );
        }
        assert!(Install::Standalone(PathBuf::from("/tmp/x"))
            .upgrade_steps()
            .is_none());
    }
}
