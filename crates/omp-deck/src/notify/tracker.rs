//! When a tab's status change deserves a desktop notification: a pure state machine over
//! [`TabStatus`] transitions, the window's focus, which tab is active and the user's toggles.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use deck_core::config::NotifyConfig;

use crate::tabs::TabStatus;

/// A turn must have run at least this long before its end is worth a notification.
pub const MIN_BUSY: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// omp finished a turn (busy → idle).
    TurnFinished,
    /// omp waits for the user (`π !`).
    Attention,
    /// The process ended.
    Exited { code: Option<i32> },
}

/// The notification-relevant state of a tab (the bell is not one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Idle,
    Busy,
    Attention,
    Exited,
}

impl Kind {
    fn of(status: TabStatus) -> Kind {
        match status {
            TabStatus::Exited { .. } => Kind::Exited,
            TabStatus::Attention => Kind::Attention,
            TabStatus::Busy => Kind::Busy,
            TabStatus::Bell | TabStatus::Idle => Kind::Idle,
        }
    }
}

/// One tab as seen this frame.
#[derive(Clone, Copy, Debug)]
pub struct Sighting {
    pub status: TabStatus,
    pub exit_code: Option<i32>,
    /// The tab is the active one in the strip.
    pub active: bool,
}

struct Memory {
    kind: Kind,
    /// When the current busy stretch was first seen.
    busy_since: Option<Instant>,
}

#[derive(Default)]
pub struct Tracker {
    tabs: HashMap<u64, Memory>,
    /// Tabs with a notification still on screen.
    shown: HashSet<u64>,
}

/// Whether `event` is wanted now: its toggle is on and the user is not already looking at the tab.
pub fn wanted(cfg: &NotifyConfig, event: Event, window_focused: bool, active: bool) -> bool {
    let toggle = match event {
        Event::TurnFinished => cfg.turn_finished,
        Event::Attention => cfg.attention,
        Event::Exited { .. } => cfg.exited,
    };
    if !cfg.enabled || !toggle {
        return false;
    }
    if cfg.only_when_unfocused {
        !window_focused
    } else {
        !(window_focused && active)
    }
}

impl Tracker {
    /// Feed one tab's current state. Returns the event to notify about, once per transition
    /// (a transition that is gated out is consumed all the same).
    pub fn observe(
        &mut self,
        uid: u64,
        now: Instant,
        seen: Sighting,
        window_focused: bool,
        cfg: &NotifyConfig,
    ) -> Option<Event> {
        let kind = Kind::of(seen.status);
        let Some(mem) = self.tabs.get_mut(&uid) else {
            // First sight (a restored or just-opened tab): nothing happened yet.
            self.tabs.insert(
                uid,
                Memory {
                    kind,
                    busy_since: (kind == Kind::Busy).then_some(now),
                },
            );
            return None;
        };
        let prev = mem.kind;
        if prev == kind {
            return None;
        }
        let busy_for = mem
            .busy_since
            .map_or(Duration::ZERO, |since| now.duration_since(since));
        mem.kind = kind;
        mem.busy_since = (kind == Kind::Busy).then_some(now);
        let event = match (prev, kind) {
            (Kind::Busy, Kind::Idle) if busy_for >= MIN_BUSY => Some(Event::TurnFinished),
            (_, Kind::Attention) => Some(Event::Attention),
            (_, Kind::Exited) => Some(Event::Exited {
                code: seen.exit_code,
            }),
            _ => None,
        };
        event.filter(|e| wanted(cfg, *e, window_focused, seen.active))
    }

    /// A notification for `uid` was sent.
    pub fn shown(&mut self, uid: u64) {
        self.shown.insert(uid);
    }

    /// The notification for `uid` is gone (dismissed, expired, clicked).
    pub fn gone(&mut self, uid: u64) {
        self.shown.remove(&uid);
    }

    /// The tab whose notification should be withdrawn now: the user looks at it (window focused,
    /// tab active).
    pub fn take_seen(&mut self, active: Option<u64>, window_focused: bool) -> Option<u64> {
        let uid = active.filter(|_| window_focused)?;
        self.shown.remove(&uid).then_some(uid)
    }

    /// Forget tabs that no longer exist; returns those that still had a notification up.
    pub fn retain(&mut self, live: &HashSet<u64>) -> Vec<u64> {
        self.tabs.retain(|uid, _| live.contains(uid));
        let stale: Vec<u64> = self
            .shown
            .iter()
            .copied()
            .filter(|u| !live.contains(u))
            .collect();
        for u in &stale {
            self.shown.remove(u);
        }
        stale
    }

    /// Some tab is busy right now (the caller keeps repainting to see it end).
    pub fn any_busy(&self) -> bool {
        self.tabs.values().any(|m| m.kind == Kind::Busy)
    }
}

/// Title and body of the notification for `event` on a tab named `tab` in project `project`.
pub fn describe(event: Event, tab: &str, project: &str) -> (String, String) {
    let what = match event {
        Event::TurnFinished => "Finished its turn".to_owned(),
        Event::Attention => "Needs your attention".to_owned(),
        Event::Exited { code: Some(0) } => "Session ended".to_owned(),
        Event::Exited { code: Some(c) } => format!("Process exited with code {c}"),
        Event::Exited { code: None } => "Process exited".to_owned(),
    };
    let body = if project.is_empty() {
        what
    } else {
        format!("{what} · {project}")
    };
    (tab.to_owned(), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(status: TabStatus, active: bool) -> Sighting {
        Sighting {
            status,
            exit_code: match status {
                TabStatus::Exited { clean } => Some(if clean { 0 } else { 2 }),
                _ => None,
            },
            active,
        }
    }

    const BUSY: TabStatus = TabStatus::Busy;
    const IDLE: TabStatus = TabStatus::Idle;

    /// Busy for `secs`, then `end`; returns the event at the end. Window state as given.
    fn run(
        end: TabStatus,
        secs: u64,
        focused: bool,
        active: bool,
        cfg: &NotifyConfig,
    ) -> Option<Event> {
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(IDLE, active), focused, cfg);
        t.observe(1, t0, seen(BUSY, active), focused, cfg);
        t.observe(
            1,
            t0 + Duration::from_secs(secs),
            seen(end, active),
            focused,
            cfg,
        )
    }

    #[test]
    fn turn_finished_needs_three_busy_seconds() {
        let cfg = NotifyConfig::default();
        assert_eq!(run(IDLE, 2, false, false, &cfg), None);
        assert_eq!(run(IDLE, 3, false, false, &cfg), Some(Event::TurnFinished));
        assert_eq!(run(IDLE, 90, false, false, &cfg), Some(Event::TurnFinished));
    }

    #[test]
    fn attention_and_exit_fire_regardless_of_busy_time() {
        let cfg = NotifyConfig::default();
        assert_eq!(
            run(TabStatus::Attention, 0, false, false, &cfg),
            Some(Event::Attention)
        );
        assert_eq!(
            run(TabStatus::Exited { clean: false }, 0, false, false, &cfg),
            Some(Event::Exited { code: Some(2) })
        );
        assert_eq!(
            run(TabStatus::Exited { clean: true }, 0, false, false, &cfg),
            Some(Event::Exited { code: Some(0) })
        );
    }

    #[test]
    fn idle_to_attention_fires_too() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let now = Instant::now();
        t.observe(1, now, seen(IDLE, false), false, &cfg);
        assert_eq!(
            t.observe(1, now, seen(TabStatus::Attention, false), false, &cfg),
            Some(Event::Attention)
        );
    }

    #[test]
    fn answering_attention_then_finishing_notifies_once_for_each_transition() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        t.observe(1, at(0), seen(BUSY, false), false, &cfg);
        assert_eq!(
            t.observe(1, at(5), seen(TabStatus::Attention, false), false, &cfg),
            Some(Event::Attention)
        );
        assert_eq!(
            t.observe(1, at(6), seen(TabStatus::Attention, false), false, &cfg),
            None
        );
        // busy time restarts when the user answers
        assert_eq!(t.observe(1, at(7), seen(BUSY, false), false, &cfg), None);
        assert_eq!(t.observe(1, at(8), seen(IDLE, false), false, &cfg), None);
        t.observe(1, at(9), seen(BUSY, false), false, &cfg);
        assert_eq!(
            t.observe(1, at(13), seen(IDLE, false), false, &cfg),
            Some(Event::TurnFinished)
        );
    }

    #[test]
    fn a_transition_is_reported_only_once() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(BUSY, false), false, &cfg);
        let end = t0 + Duration::from_secs(10);
        assert_eq!(
            t.observe(1, end, seen(IDLE, false), false, &cfg),
            Some(Event::TurnFinished)
        );
        assert_eq!(t.observe(1, end, seen(IDLE, false), false, &cfg), None);
        assert_eq!(
            t.observe(
                1,
                end + Duration::from_secs(1),
                seen(IDLE, false),
                false,
                &cfg
            ),
            None
        );
    }

    #[test]
    fn first_sight_of_a_tab_never_notifies() {
        let cfg = NotifyConfig::default();
        for s in [
            IDLE,
            BUSY,
            TabStatus::Attention,
            TabStatus::Exited { clean: false },
        ] {
            let mut t = Tracker::default();
            assert_eq!(
                t.observe(7, Instant::now(), seen(s, false), false, &cfg),
                None
            );
        }
    }

    #[test]
    fn bell_counts_as_idle() {
        let cfg = NotifyConfig::default();
        assert_eq!(
            run(TabStatus::Bell, 10, false, false, &cfg),
            Some(Event::TurnFinished)
        );
    }

    #[test]
    fn toggles_gate_each_event_and_the_master_switch_gates_all() {
        let mut cfg = NotifyConfig {
            turn_finished: false,
            ..NotifyConfig::default()
        };
        assert_eq!(run(IDLE, 10, false, false, &cfg), None);
        assert_eq!(
            run(TabStatus::Attention, 0, false, false, &cfg),
            Some(Event::Attention)
        );
        cfg.attention = false;
        assert_eq!(run(TabStatus::Attention, 0, false, false, &cfg), None);
        cfg.exited = false;
        assert_eq!(
            run(TabStatus::Exited { clean: false }, 0, false, false, &cfg),
            None
        );
        cfg = NotifyConfig {
            enabled: false,
            ..NotifyConfig::default()
        };
        assert_eq!(run(IDLE, 10, false, false, &cfg), None);
        assert_eq!(run(TabStatus::Attention, 0, false, false, &cfg), None);
    }

    #[test]
    fn unfocused_only_mode_ignores_background_tabs_while_the_window_has_focus() {
        let cfg = NotifyConfig::default();
        assert!(cfg.only_when_unfocused);
        assert_eq!(
            run(IDLE, 10, true, false, &cfg),
            None,
            "focused window, background tab"
        );
        assert_eq!(
            run(IDLE, 10, true, true, &cfg),
            None,
            "focused window, active tab"
        );
        assert_eq!(
            run(IDLE, 10, false, true, &cfg),
            Some(Event::TurnFinished),
            "window away, active tab"
        );
        assert_eq!(run(IDLE, 10, false, false, &cfg), Some(Event::TurnFinished));
    }

    #[test]
    fn background_mode_also_notifies_for_background_tabs_of_a_focused_window() {
        let cfg = NotifyConfig {
            only_when_unfocused: false,
            ..NotifyConfig::default()
        };
        assert_eq!(run(IDLE, 10, true, false, &cfg), Some(Event::TurnFinished));
        assert_eq!(
            run(IDLE, 10, true, true, &cfg),
            None,
            "the user is looking at it"
        );
        assert_eq!(run(IDLE, 10, false, true, &cfg), Some(Event::TurnFinished));
    }

    #[test]
    fn a_gated_out_transition_is_consumed() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(BUSY, false), true, &cfg);
        let end = t0 + Duration::from_secs(10);
        assert_eq!(t.observe(1, end, seen(IDLE, false), true, &cfg), None);
        // the window loses focus later: the old transition must not fire now
        assert_eq!(
            t.observe(
                1,
                end + Duration::from_secs(1),
                seen(IDLE, false),
                false,
                &cfg
            ),
            None
        );
    }

    #[test]
    fn tabs_are_tracked_independently() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(BUSY, false), false, &cfg);
        t.observe(2, t0, seen(IDLE, false), false, &cfg);
        let end = t0 + Duration::from_secs(5);
        assert_eq!(t.observe(2, end, seen(IDLE, false), false, &cfg), None);
        assert_eq!(
            t.observe(1, end, seen(IDLE, false), false, &cfg),
            Some(Event::TurnFinished)
        );
    }

    #[test]
    fn activating_the_tab_withdraws_its_notification_only_when_the_window_is_focused() {
        let mut t = Tracker::default();
        t.shown(4);
        assert_eq!(t.take_seen(Some(4), false), None, "window away");
        assert_eq!(t.take_seen(Some(5), true), None, "other tab");
        assert_eq!(t.take_seen(None, true), None);
        assert_eq!(t.take_seen(Some(4), true), Some(4));
        assert_eq!(t.take_seen(Some(4), true), None, "withdrawn once");
    }

    #[test]
    fn a_dismissed_notification_is_not_withdrawn_again() {
        let mut t = Tracker::default();
        t.shown(4);
        t.gone(4);
        assert_eq!(t.take_seen(Some(4), true), None);
    }

    #[test]
    fn closed_tabs_are_forgotten_and_their_notifications_reported() {
        let cfg = NotifyConfig::default();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        t.observe(1, t0, seen(BUSY, false), false, &cfg);
        t.observe(2, t0, seen(IDLE, false), false, &cfg);
        t.shown(1);
        let live: HashSet<u64> = [2].into_iter().collect();
        assert_eq!(t.retain(&live), vec![1]);
        assert!(!t.any_busy());
        // a reused uid starts from scratch: first sight, no event
        assert_eq!(t.observe(1, t0, seen(IDLE, false), false, &cfg), None);
    }

    #[test]
    fn describe_names_the_event_and_project() {
        assert_eq!(
            describe(Event::TurnFinished, "fix bug", "omp-deck"),
            ("fix bug".into(), "Finished its turn · omp-deck".into())
        );
        assert_eq!(
            describe(Event::Attention, "t", "").1,
            "Needs your attention"
        );
        assert_eq!(
            describe(Event::Exited { code: Some(137) }, "t", "p").1,
            "Process exited with code 137 · p"
        );
        assert_eq!(
            describe(Event::Exited { code: Some(0) }, "t", "p").1,
            "Session ended · p"
        );
        assert_eq!(
            describe(Event::Exited { code: None }, "t", "p").1,
            "Process exited · p"
        );
    }
}
