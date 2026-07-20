//! Which terminal window is in front, so the card follows the window you are looking at.
//!
//! Terminals expose their state over AppleScript, but not uniformly: Terminal.app and
//! iTerm2 hand out the tty of the focused tab, Ghostty does not — it only exposes the
//! working directory. So the hint is one of two shapes and the registry matches on
//! whichever it gets.
//!
//! Only macOS is implemented. Everywhere else this returns `None` and the daemon falls
//! back to its last-active policy, which is also what happens when the user denies the
//! Automation permission macOS asks for on first use.

/// How the focused window identifies itself.
// Nothing constructs these off macOS yet; the registry still matches against them.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FocusHint {
    /// Device path of the focused tab's terminal, e.g. `/dev/ttys004`.
    Tty(String),
    /// Working directory of the focused tab. Coarser: two sessions in one repo tie.
    Cwd(String),
}

impl FocusHint {
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn parse(line: &str) -> Option<Self> {
        let (kind, value) = line.trim().split_once('\t')?;
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        match kind {
            "tty" => Some(Self::Tty(value.to_string())),
            "cwd" => Some(Self::Cwd(normalize_cwd(value))),
            _ => None,
        }
    }
}

/// Trailing slashes and `file://` prefixes vary between terminals; strip both so the
/// hint compares equal to the `cwd` the agent reported.
pub fn normalize_cwd(raw: &str) -> String {
    let raw = raw.strip_prefix("file://").unwrap_or(raw);
    raw.trim_end_matches('/').to_string()
}

/// Ask the frontmost terminal what it is showing. Blocking — call from a blocking task.
#[cfg(target_os = "macos")]
pub fn focused_target() -> Option<FocusHint> {
    // Each app is guarded by `is running` so the query never launches anything, and by
    // `frontmost` so a background terminal cannot claim focus. System Events is
    // deliberately not used: it would require a second, much broader TCC grant.
    const SCRIPT: &str = r#"
if application "Ghostty" is running then
  tell application "Ghostty"
    if frontmost then
      try
        return "cwd	" & (working directory of focused terminal of selected tab of front window)
      end try
    end if
  end tell
end if
if application "iTerm2" is running then
  tell application "iTerm2"
    if frontmost then
      try
        return "tty	" & (tty of current session of current window)
      end try
    end if
  end tell
end if
if application "Terminal" is running then
  tell application "Terminal"
    if frontmost then
      try
        return "tty	" & (tty of selected tab of front window)
      end try
    end if
  end tell
end if
return ""
"#;

    let out = std::process::Command::new("osascript")
        .args(["-e", SCRIPT])
        .output()
        .ok()?;
    if !out.status.success() {
        // Denied Automation permission lands here. Degrading to last-active is the
        // right answer, so this stays at debug level rather than nagging every tick.
        tracing::debug!(
            "focus query failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    FocusHint::parse(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(target_os = "macos"))]
pub fn focused_target() -> Option<FocusHint> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_hint_shapes() {
        assert_eq!(
            FocusHint::parse("tty\t/dev/ttys004\n"),
            Some(FocusHint::Tty("/dev/ttys004".into()))
        );
        assert_eq!(
            FocusHint::parse("cwd\t/a/repo/\n"),
            Some(FocusHint::Cwd("/a/repo".into())),
            "trailing slash must be normalized away"
        );
    }

    #[test]
    fn no_terminal_in_front_yields_no_hint() {
        assert_eq!(FocusHint::parse(""), None);
        assert_eq!(FocusHint::parse("cwd\t"), None);
    }
}
