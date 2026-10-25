//! The notification inbox: what happened in tabs the user was not looking at. One entry per tab
//! (a newer event replaces it and moves it to the top); entries leave when the tab is viewed or
//! closed, by their own ✕ or by "Clear all". Not persisted.

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
        }
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

    /// The user looks at these tabs now.
    pub fn remove_viewed(&mut self, visible: &[u64]) {
        if !visible.is_empty() {
            self.entries.retain(|e| !visible.contains(&e.tab));
        }
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
        i.remove_viewed(&[2, 9]);
        assert_eq!(i.entries().len(), 4);
        i.retain_live(&[1, 3, 4].into_iter().collect());
        assert_eq!(i.entries().len(), 3);
        assert!(i.remove(3));
        assert!(!i.remove(3));
        i.clear();
        assert!(i.entries().is_empty());
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
