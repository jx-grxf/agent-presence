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

/// How much fresher a rival has to be before it takes the card away from the session
/// currently on it.
///
/// Without this the card simply showed whichever session fired most recently, and four
/// concurrent sessions all firing tool events traded the lead several times a minute —
/// the card flipped between projects, agents and icons on every tick. Real work produces
/// an event every few seconds, so a session that has been quiet this long has genuinely
/// stopped, while two busy sessions stay within the margin of each other forever and the
/// incumbent keeps the card.
const SWITCH_MARGIN: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct Registry {
    sessions: HashMap<String, Session>,
    /// Session the card is currently describing, so the choice is stable across ticks.
    primary: Option<String>,
}

impl Registry {
    pub fn apply(&mut self, event: HookEvent) {
        let now = Instant::now();
        match event.kind {
            EventKind::SessionEnd => {
                self.sessions.remove(&event.session_id);
                if self.primary.as_deref() == Some(event.session_id.as_str()) {
                    self.primary = None;
                }
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
    pub fn snapshot(&mut self) -> Option<Snapshot> {
        self.snapshot_focused(None)
    }

    /// Pick the session the card describes.
    ///
    /// Three rules, in order. The focused terminal wins outright — that is the user
    /// pointing at a window, and obeying it instantly is the whole point of the feature.
    /// Failing that the session already on the card keeps it, unless a rival has been
    /// active `SWITCH_MARGIN` longer, which is what stops several busy sessions from
    /// trading the card back and forth. Only with no incumbent does the most recently
    /// active session simply win.
    pub fn snapshot_focused(&mut self, hint: Option<&FocusHint>) -> Option<Snapshot> {
        let chosen = self.choose(hint)?;
        self.primary = Some(chosen.clone());
        let primary = self.sessions.get(&chosen)?.clone();

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

    fn choose(&self, hint: Option<&FocusHint>) -> Option<String> {
        // The pool the card may be drawn from: the focused window's sessions when we can
        // identify them, everything otherwise. A focused window running no agent must not
        // blank the card, so an empty match falls through rather than winning.
        let matching: Vec<(&String, &Session)> = match hint {
            Some(h) => self
                .sessions
                .iter()
                .filter(|(_, s)| matches_hint(s, h))
                .collect(),
            None => Vec::new(),
        };
        let pool: Vec<(&String, &Session)> = if matching.is_empty() {
            self.sessions.iter().collect()
        } else {
            matching
        };

        // Ties are broken by start time so the result is deterministic rather than
        // dependent on HashMap ordering.
        let (best_id, best) = pool.iter().max_by_key(|(_, s)| (s.last_seen, s.started))?;

        // A hint naming exactly one session leaves nothing to stabilise, but Ghostty
        // reports a working directory rather than a terminal, so two sessions in one repo
        // still tie — and the incumbent deserves that tie-break too.
        let incumbent = self
            .primary
            .as_ref()
            .and_then(|id| pool.iter().find(|(pid, _)| *pid == id));

        match incumbent {
            Some((id, current))
                if best.last_seen.saturating_duration_since(current.last_seen) < SWITCH_MARGIN =>
            {
                Some((*id).clone())
            }
            _ => Some((*best_id).clone()),
        }
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

    /// Backdate a session's activity, to stand in for one that has genuinely gone quiet.
    fn quieten(r: &mut Registry, session: &str, by: Duration) {
        let s = r.sessions.get_mut(session).unwrap();
        s.last_seen -= by;
    }

    #[test]
    fn busy_sessions_do_not_trade_the_card_back_and_forth() {
        // The reported bug: four concurrent sessions all firing tool events meant the
        // card showed whichever fired last, so it flipped project, agent and icon on
        // every 2s tick.
        let mut r = Registry::default();
        for id in ["a", "b", "c", "d"] {
            r.apply(ev(id, EventKind::Activity(Activity::Editing)));
        }
        let first = r.snapshot().unwrap().primary.clone();

        // Every other session reports in, each one now the most recent.
        for _ in 0..5 {
            for id in ["b", "c", "d", "a"] {
                std::thread::sleep(Duration::from_millis(1));
                r.apply(ev(id, EventKind::Activity(Activity::Reading)));
                let now = r.snapshot().unwrap();
                assert_eq!(
                    now.primary.started, first.started,
                    "the card changed session while every rival was equally busy"
                );
            }
        }
    }

    #[test]
    fn a_session_that_goes_quiet_hands_the_card_over() {
        let mut r = Registry::default();
        r.apply(ev("working", EventKind::Activity(Activity::Editing)));
        r.snapshot().unwrap();

        r.apply(ev("other", EventKind::Activity(Activity::Reading)));
        // Still within the margin, so the incumbent keeps it.
        assert_eq!(r.snapshot().unwrap().primary.activity, Activity::Editing);

        // The incumbent stops for longer than the margin.
        quieten(&mut r, "working", SWITCH_MARGIN * 2);
        assert_eq!(
            r.snapshot().unwrap().primary.activity,
            Activity::Reading,
            "a session that has genuinely stopped must not hold the card"
        );
    }

    #[test]
    fn focus_still_wins_instantly_over_the_incumbent() {
        // Hysteresis must not blunt the focus feature: switching windows is the user
        // saying which session they mean, and that has to take effect on the next tick.
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
        // "back" is freshest, so it takes the card first.
        assert_eq!(r.snapshot().unwrap().primary.activity, Activity::Editing);

        let hint = FocusHint::Tty("/dev/ttys001".into());
        assert_eq!(
            r.snapshot_focused(Some(&hint)).unwrap().primary.activity,
            Activity::Reading,
            "the focused window must override the incumbent immediately"
        );
    }

    #[test]
    fn ending_the_shown_session_releases_the_card() {
        let mut r = Registry::default();
        r.apply(ev("a", EventKind::Activity(Activity::Editing)));
        r.apply(ev("b", EventKind::Activity(Activity::Reading)));
        r.snapshot().unwrap();

        r.apply(ev("a", EventKind::SessionEnd));
        r.apply(ev("b", EventKind::SessionEnd));
        r.apply(ev("c", EventKind::Activity(Activity::Researching)));
        assert_eq!(
            r.snapshot().unwrap().primary.activity,
            Activity::Researching,
            "a departed incumbent must not keep the card out of reach"
        );
    }
}
