//! Tracks every live agent session and decides which one the card represents.

use super::focus::{self, FocusHint};
use crate::event::{Activity, Agent, EventKind, HookEvent};
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Session {
    pub agent: Agent,
    pub activity: Activity,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub target: Option<String>,
    /// Terminal the session runs in, when the hook could determine one.
    pub tty: Option<String>,
    /// Wall-clock start, as unix seconds, for Discord's elapsed timer.
    pub started_unix: u64,
    pub started: Instant,
    pub last_seen: Instant,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    /// The session the card describes: the most recently active one.
    pub primary: Session,
    /// How many other sessions are live.
    pub others: usize,
    /// Start of the oldest live session, so the timer spans the whole coding stretch.
    pub oldest_start_unix: u64,
}

#[derive(Default)]
pub struct Registry {
    sessions: HashMap<String, Session>,
}

impl Registry {
    pub fn apply(&mut self, event: HookEvent) {
        let now = Instant::now();
        match event.kind {
            EventKind::SessionEnd => {
                self.sessions.remove(&event.session_id);
            }
            EventKind::Ignored => {
                // Still counts as a sign of life.
                if let Some(s) = self.sessions.get_mut(&event.session_id) {
                    s.last_seen = now;
                }
            }
            EventKind::SessionStart | EventKind::Activity(_) => {
                let activity = match event.kind {
                    EventKind::Activity(a) => a,
                    _ => Activity::Starting,
                };
                let entry = self
                    .sessions
                    .entry(event.session_id)
                    .or_insert_with(|| Session {
                        agent: event.agent,
                        activity,
                        cwd: event.cwd.clone(),
                        model: event.model.clone(),
                        target: event.target.clone(),
                        tty: event.tty.clone(),
                        started_unix: unix_now(),
                        started: now,
                        last_seen: now,
                    });
                entry.activity = activity;
                entry.last_seen = now;
                entry.target = event.target;
                // Later events carry the authoritative model/cwd; SessionStart may not.
                if event.cwd.is_some() {
                    entry.cwd = event.cwd;
                }
                if event.model.is_some() {
                    entry.model = event.model;
                }
                if event.tty.is_some() {
                    entry.tty = event.tty;
                }
            }
        }
    }

    /// Drop sessions whose agent died without firing `SessionEnd`.
    pub fn expire(&mut self, idle_timeout: Duration) -> usize {
        let now = Instant::now();
        let before = self.sessions.len();
        self.sessions
            .retain(|_, s| now.duration_since(s.last_seen) < idle_timeout);
        before - self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Whether resolving the focused window is worth the cost this tick.
    pub fn has_multiple(&self) -> bool {
        self.sessions.len() > 1
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.snapshot_focused(None)
    }

    /// Pick the session the card describes. The focused terminal wins when we can
    /// identify it; otherwise the most recently active session does, which is also the
    /// fallback when the focused window holds no agent session at all.
    pub fn snapshot_focused(&self, hint: Option<&FocusHint>) -> Option<Snapshot> {
        let focused = hint.and_then(|h| {
            self.sessions
                .values()
                .filter(|s| matches_hint(s, h))
                // Several sessions can share one cwd; the busier one is the better guess.
                .max_by_key(|s| (s.last_seen, s.started))
        });

        // Ties are broken by start time so the result is deterministic rather than
        // dependent on HashMap ordering.
        let primary = match focused {
            Some(s) => s.clone(),
            None => self
                .sessions
                .values()
                .max_by_key(|s| (s.last_seen, s.started))?
                .clone(),
        };
        let oldest_start_unix = self
            .sessions
            .values()
            .map(|s| s.started_unix)
            .min()
            .unwrap_or(primary.started_unix);
        Some(Snapshot {
            primary,
            others: self.sessions.len() - 1,
            oldest_start_unix,
        })
    }
}

fn matches_hint(session: &Session, hint: &FocusHint) -> bool {
    match hint {
        FocusHint::Tty(tty) => session.tty.as_deref() == Some(tty.as_str()),
        FocusHint::Cwd(cwd) => session
            .cwd
            .as_deref()
            .map(focus::normalize_cwd)
            .is_some_and(|c| c == *cwd),
    }
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(session: &str, kind: EventKind) -> HookEvent {
        HookEvent {
            agent: Agent::Claude,
            session_id: session.into(),
            kind,
            cwd: Some("/repo".into()),
            model: Some("claude-opus-4-8".into()),
            target: None,
            tty: None,
        }
    }

    fn ev_in(session: &str, kind: EventKind, tty: &str) -> HookEvent {
        HookEvent {
            tty: Some(tty.into()),
            ..ev(session, kind)
        }
    }

    #[test]
    fn tracks_and_ends_a_session() {
        let mut r = Registry::default();
        r.apply(ev("a", EventKind::SessionStart));
        assert_eq!(r.len(), 1);
        r.apply(ev("a", EventKind::SessionEnd));
        assert!(r.is_empty());
    }

    #[test]
    fn latest_activity_drives_the_card() {
        let mut r = Registry::default();
        r.apply(ev("a", EventKind::Activity(Activity::Reading)));
        std::thread::sleep(Duration::from_millis(2));
        r.apply(ev("b", EventKind::Activity(Activity::Editing)));

        let snap = r.snapshot().unwrap();
        assert_eq!(snap.primary.activity, Activity::Editing);
        assert_eq!(
            snap.others, 1,
            "the other session must be counted, not shown"
        );
    }

    #[test]
    fn timer_spans_the_oldest_session() {
        let mut r = Registry::default();
        r.apply(ev("old", EventKind::SessionStart));
        r.apply(ev("new", EventKind::SessionStart));
        if let Some(s) = r.sessions.get_mut("new") {
            s.started_unix += 500;
        }
        let snap = r.snapshot().unwrap();
        assert!(snap.oldest_start_unix <= snap.primary.started_unix);
    }

    #[test]
    fn expires_sessions_that_never_said_goodbye() {
        let mut r = Registry::default();
        r.apply(ev("zombie", EventKind::SessionStart));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(r.expire(Duration::from_millis(1)), 1);
        assert!(r.snapshot().is_none());
    }

    #[test]
    fn focused_window_beats_recent_activity() {
        let mut r = Registry::default();
        r.apply(ev_in(
            "front",
            EventKind::Activity(Activity::Reading),
            "/dev/ttys001",
        ));
        std::thread::sleep(Duration::from_millis(2));
        r.apply(ev_in(
            "back",
            EventKind::Activity(Activity::Editing),
            "/dev/ttys002",
        ));

        let hint = FocusHint::Tty("/dev/ttys001".into());
        let snap = r.snapshot_focused(Some(&hint)).unwrap();
        assert_eq!(
            snap.primary.activity,
            Activity::Reading,
            "the focused window wins even though the other session is newer"
        );
    }

    #[test]
    fn unmatched_hint_falls_back_to_last_active() {
        let mut r = Registry::default();
        r.apply(ev_in(
            "a",
            EventKind::Activity(Activity::Reading),
            "/dev/ttys001",
        ));
        std::thread::sleep(Duration::from_millis(2));
        r.apply(ev_in(
            "b",
            EventKind::Activity(Activity::Editing),
            "/dev/ttys002",
        ));

        // A focused terminal running no agent must not blank the card.
        let hint = FocusHint::Tty("/dev/ttys009".into());
        let snap = r.snapshot_focused(Some(&hint)).unwrap();
        assert_eq!(snap.primary.activity, Activity::Editing);
    }

    #[test]
    fn cwd_hint_matches_despite_trailing_slash() {
        let mut r = Registry::default();
        r.apply(ev("a", EventKind::Activity(Activity::Reading)));
        let hint = FocusHint::Cwd("/repo".into());
        assert!(matches_hint(&r.sessions["a"], &hint));
    }

    #[test]
    fn empty_registry_yields_no_card() {
        assert!(Registry::default().snapshot().is_none());
    }
}
