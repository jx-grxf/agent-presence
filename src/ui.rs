//! Presentation for the one-shot commands.
//!
//! Everything here degrades to plain ASCII-with-no-escapes when stdout is not a
//! terminal, so `agent-presence doctor > report.txt` and piping into `grep` stay
//! readable. `NO_COLOR` is honoured the same way.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Braille frames: one glyph wide in every terminal, unlike block or emoji spinners.
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const FRAME_TIME: Duration = Duration::from_millis(80);

pub fn styled() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
}

fn paint(code: &str, text: &str) -> String {
    if styled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn dim(text: &str) -> String {
    paint("2", text)
}
pub fn bold(text: &str) -> String {
    paint("1", text)
}
pub fn green(text: &str) -> String {
    paint("32", text)
}
pub fn red(text: &str) -> String {
    paint("31", text)
}
pub fn yellow(text: &str) -> String {
    paint("33", text)
}
pub fn cyan(text: &str) -> String {
    paint("36", text)
}

pub fn heading(text: &str) {
    println!("\n{}", bold(text));
}

/// `label   value` with the labels lined up in a column.
pub fn field(label: &str, value: &str) {
    println!("  {:<10} {value}", dim(label));
}

pub fn ok(text: &str) {
    println!("  {} {text}", green("✓"));
}
pub fn fail(text: &str) {
    println!("  {} {text}", red("✗"));
}
pub fn warn(text: &str) {
    println!("  {} {text}", yellow("!"));
}

/// A spinner that animates on a background thread until it is resolved.
///
/// Only spins on a terminal: under a pipe it prints nothing and the eventual
/// `ok`/`fail` line carries the whole message, so log files do not fill up with
/// half-overwritten frames.
pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    pub fn start(message: impl Into<String>) -> Self {
        let message = message.into();
        let stop = Arc::new(AtomicBool::new(false));
        if !styled() {
            return Self { stop, handle: None };
        }

        let flag = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut out = std::io::stdout();
            for frame in FRAMES.iter().cycle() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                // \r and clear-to-end, so a shorter frame never leaves debris behind.
                let _ = write!(out, "\r\x1b[K  {} {message}", paint("36", frame));
                let _ = out.flush();
                std::thread::sleep(FRAME_TIME);
            }
            let _ = write!(out, "\r\x1b[K");
            let _ = out.flush();
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn clear(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    pub fn succeed(mut self, text: &str) {
        self.clear();
        ok(text);
    }

    pub fn fail_with(mut self, text: &str) {
        self.clear();
        fail(text);
    }
}

impl Drop for Spinner {
    /// An early return or a `?` must never leave the terminal spinning.
    fn drop(&mut self) {
        self.clear();
    }
}

/// Render a Discord card the way Discord stacks it.
///
/// Padding is applied *before* colouring: escape sequences have no display width, so
/// padding a styled string would misalign every box by the length of its escapes.
pub fn card(app: &str, details: &str, state: &str, elapsed: &str) -> Vec<String> {
    let inner = [app, details, state, elapsed]
        .into_iter()
        .map(str::chars)
        .map(Iterator::count)
        .max()
        .unwrap_or(0)
        .max(30);

    let row = |text: &str, style: fn(&str) -> String| {
        format!("│ {} │", style(&format!("{text:<inner$}")))
    };
    let line = "─".repeat(inner + 2);
    vec![
        format!("┌{line}┐"),
        row(app, bold),
        row(details, |s| s.to_string()),
        row(state, dim),
        row(elapsed, dim),
        format!("└{line}┘"),
    ]
}
