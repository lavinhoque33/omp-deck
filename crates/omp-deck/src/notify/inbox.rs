//! The notification inbox: what happened in tabs the user was not looking at. One entry per tab
//! (a newer event replaces it and moves it to the top); entries leave when the tab is viewed or
//! closed, by their own ✕ or by "Clear all" (one recorded while looking leaves later, see
//! [`Entry::armed`]). Not persisted.

use std::collections::HashSet;
use std::time::SystemTime;

/// What an entry reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A turn finished.
    Finished,
    /// A turn stopped with an error (omp's own message when known).
    Error(Option<String>),
    /// omp waits for the user.
    Attention,
    /// The process ended.
    Exited { code: Option<i32> },
}

impl Kind {
    /// The one-line summary ("Finished its turn", "Stopped with error", …).
    pub fn what(&self) -> String {
        match self {
            Kind::Finished => "Finished its turn".to_owned(),
            Kind::Error(_) => "Stopped with error".to_owned(),
            Kind::Attention => "Needs your attention".to_owned(),
            Kind::Exited { code: Some(0) } => "Session ended".to_owned(),
            Kind::Exited { code: Some(c) } => format!("Process exited with code {c}"),
            Kind::Exited { code: None } => "Process exited".to_owned(),
        }
    }

    /// Needs the user more than a finished turn does (the badge turns to the warn colour).
    pub fn is_warn(&self) -> bool {
        matches!(self, Kind::Error(_) | Kind::Attention)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The tab's uid.
    pub tab: u64,
    pub kind: Kind,
    /// The tab's label when the event happened.
    pub title: String,
    /// Basename of the tab's folder.
    pub project: String,
    /// [`Kind::what`], plus omp's error message on a second line.
    pub body: String,
    pub at: SystemTime,
    /// False for an entry recorded while the user looked at the tab: it stays until they look
    /// away (which arms it) and back, or the next turn starts.
    pub armed: bool,
}

impl Entry {
    pub fn new(tab: u64, kind: Kind, title: &str, project: &str, at: SystemTime) -> Entry {
        let mut body = kind.what();
        if let Kind::Error(Some(msg)) = &kind
            && !msg.is_empty()
        {
            body.push('\n');
            body.push_str(msg);
        }
        Entry {
            tab,
            kind,
            title: title.to_owned(),
            project: project.to_owned(),
            body,
            at,
            armed: true,
        }
    }

    /// Marks the entry as recorded while the user was looking at the tab.
    pub fn while_looking(mut self) -> Entry {
        self.armed = false;
        self
    }
}

#[derive(Default)]
pub struct Inbox {
    /// Newest first.
    entries: Vec<Entry>,
}

impl Inbox {
    /// Records `entry`, replacing the tab's older one and putting it on top.
    pub fn record(&mut self, entry: Entry) {
        self.entries.retain(|e| e.tab != entry.tab);
        self.entries.insert(0, entry);
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Drops the tab's entry; whether there was one.
    pub fn remove(&mut self, tab: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.tab != tab);
        self.entries.len() != before
    }

    /// Per frame: entries of tabs the user does not look at become armed; armed entries of tabs
    /// the user looks at now (window focused, tab in `visible`) are removed. Returns the tabs
    /// whose entry went.
    pub fn sync_viewing(&mut self, visible: &[u64], window_focused: bool) -> Vec<u64> {
        for e in &mut self.entries {
            if !(window_focused && visible.contains(&e.tab)) {
                e.armed = true;
            }
        }
        let mut removed = Vec::new();
        if window_focused && !visible.is_empty() {
            self.entries.retain(|e| {
                let go = e.armed && visible.contains(&e.tab);
                if go {
                    removed.push(e.tab);
                }
                !go
            });
        }
        removed
    }

    /// The tab's next turn began: its entry recorded while looking is stale. Whether it went.
    pub fn turn_started(&mut self, tab: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.tab != tab || e.armed);
        self.entries.len() != before
    }

    /// Forgets the entries of tabs that no longer exist.
    pub fn retain_live(&mut self, live: &HashSet<u64>) {
        self.entries.retain(|e| live.contains(&e.tab));
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Any entry that needs the user (error / attention).
    pub fn warns(&self) -> bool {
        self.entries.iter().any(|e| e.kind.is_warn())
    }

    /// The badge text: `1`–`9`, then `9+`; none when empty.
    pub fn badge(&self) -> Option<&'static str> {
        const DIGITS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];
        match self.entries.len() {
            0 => None,
            n => Some(DIGITS.get(n - 1).copied().unwrap_or("9+")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(tab: u64, kind: Kind) -> Entry {
        Entry::new(tab, kind, "t", "p", SystemTime::UNIX_EPOCH)
    }

    #[test]
    fn a_newer_event_replaces_the_tabs_entry_and_moves_it_to_the_top() {
        let mut i = Inbox::default();
        i.record(entry(1, Kind::Finished));
        i.record(entry(2, Kind::Finished));
        i.record(entry(1, Kind::Attention));
        let order: Vec<(u64, &Kind)> = i.entries().iter().map(|e| (e.tab, &e.kind)).collect();
        assert_eq!(order, vec![(1, &Kind::Attention), (2, &Kind::Finished)]);
    }

    #[test]
    fn entries_leave_when_viewed_closed_dismissed_or_cleared() {
        let mut i = Inbox::default();
        for t in 1..=5 {
            i.record(entry(t, Kind::Finished));
        }
        i.sync_viewing(&[2, 9], true);
        assert_eq!(i.entries().len(), 4);
        i.retain_live(&[1, 3, 4].into_iter().collect());
        assert_eq!(i.entries().len(), 3);
        assert!(i.remove(3));
        assert!(!i.remove(3));
        i.clear();
        assert!(i.entries().is_empty());
    }

    #[test]
    fn an_entry_recorded_while_looking_stays_until_the_user_looks_away_and_back() {
        let mut i = Inbox::default();
        i.record(entry(1, Kind::Finished).while_looking());
        i.record(entry(2, Kind::Finished));
        i.sync_viewing(&[1, 2], true);
        assert_eq!(i.entries().len(), 1, "the armed entry of a viewed tab goes");
        assert_eq!(
            i.entries()[0].tab,
            1,
            "the one recorded while looking stays"
        );
        i.sync_viewing(&[1], true);
        assert_eq!(i.entries().len(), 1, "still looking");
        i.sync_viewing(&[2], true);
        assert_eq!(
            i.entries().len(),
            1,
            "tab 1 is out of view: armed, not removed yet"
        );
        i.sync_viewing(&[1], true);
        assert!(i.entries().is_empty(), "looked away and back");
    }

    #[test]
    fn losing_window_focus_counts_as_looking_away() {
        let mut i = Inbox::default();
        i.record(entry(1, Kind::Finished).while_looking());
        i.sync_viewing(&[1], false);
        assert_eq!(i.entries().len(), 1);
        i.sync_viewing(&[1], true);
        assert!(i.entries().is_empty());
    }

    #[test]
    fn sync_viewing_reports_the_tabs_whose_entry_went() {
        let mut i = Inbox::default();
        i.record(entry(1, Kind::Finished).while_looking());
        i.record(entry(2, Kind::Finished));
        assert_eq!(i.sync_viewing(&[1, 2], true), vec![2]);
        assert!(i.sync_viewing(&[1], true).is_empty(), "still looking");
        assert!(
            i.sync_viewing(&[2], true).is_empty(),
            "armed by looking away"
        );
        assert_eq!(i.sync_viewing(&[1], true), vec![1]);
    }

    #[test]
    fn the_next_turn_removes_only_an_entry_recorded_while_looking() {
        let mut i = Inbox::default();
        i.record(entry(1, Kind::Finished).while_looking());
        i.record(entry(2, Kind::Finished));
        assert!(i.turn_started(1));
        assert!(!i.turn_started(1));
        assert!(!i.turn_started(2), "an armed entry is kept");
        assert_eq!(i.entries().len(), 1);
    }

    #[test]
    fn badge_counts_then_caps_and_hides_at_zero() {
        let mut i = Inbox::default();
        assert_eq!(i.badge(), None);
        for t in 1..=9 {
            i.record(entry(t, Kind::Finished));
            assert_eq!(
                i.badge(),
                Some(["1", "2", "3", "4", "5", "6", "7", "8", "9"][t as usize - 1])
            );
        }
        i.record(entry(10, Kind::Finished));
        assert_eq!(i.badge(), Some("9+"));
    }

    #[test]
    fn only_error_and_attention_entries_warn() {
        let mut i = Inbox::default();
        i.record(entry(1, Kind::Finished));
        i.record(entry(2, Kind::Exited { code: Some(1) }));
        assert!(!i.warns());
        i.record(entry(3, Kind::Error(None)));
        assert!(i.warns());
        i.remove(3);
        i.record(entry(4, Kind::Attention));
        assert!(i.warns());
    }

    #[test]
    fn error_body_carries_omp_message_on_a_second_line() {
        let e = entry(1, Kind::Error(Some("rate limited".into())));
        assert_eq!(e.body, "Stopped with error\nrate limited");
        assert_eq!(entry(1, Kind::Error(None)).body, "Stopped with error");
    }
}
