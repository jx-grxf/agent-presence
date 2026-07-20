//! Tracks every live agent session and decides which one the card represents.

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

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        // Most recent activity wins the card. Ties are broken by start time so the
        // result is deterministic rather than dependent on HashMap ordering.
        let primary = self
            .sessions
            .values()
            .max_by_key(|s| (s.last_seen, s.started))?
            .clone();
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
    fn empty_registry_yields_no_card() {
        assert!(Registry::default().snapshot().is_none());
    }
}
