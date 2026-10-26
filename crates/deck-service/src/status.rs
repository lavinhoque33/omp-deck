//! A session's status as every client shows it (painting is the client's business).

use crate::statusfmt::Activity;

/// What a session's leading indicator shows, in precedence order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TabStatus {
    /// The process ended (`clean` = exit code 0).
    Exited { clean: bool },
    /// omp waits for the user (`π !`).
    Attention,
    /// omp is working (spinner frame in the title).
    Busy,
    /// A bell rang on a background tab.
    Bell,
    /// Nothing to report: the caller's own idle look.
    Idle,
}

impl TabStatus {
    pub fn of(exit_code: Option<i32>, activity: Activity, bell: bool) -> TabStatus {
        match (exit_code, activity) {
            (Some(code), _) => TabStatus::Exited { clean: code == 0 },
            (None, Activity::Attention) => TabStatus::Attention,
            (None, Activity::Busy) => TabStatus::Busy,
            _ if bell => TabStatus::Bell,
            _ => TabStatus::Idle,
        }
    }

    /// Tooltip line for the live omp states.
    pub fn hint(self) -> Option<&'static str> {
        match self {
            TabStatus::Attention => Some("omp is waiting for your input"),
            TabStatus::Busy => Some("omp is working…"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tab_status_precedence() {
        use Activity::*;
        let of = TabStatus::of;
        // Exited wins over everything; the code decides clean/failed.
        assert_eq!(
            of(Some(0), Attention, true),
            TabStatus::Exited { clean: true }
        );
        assert_eq!(of(Some(1), Busy, false), TabStatus::Exited { clean: false });
        // Then attention, busy, the bell, idle.
        assert_eq!(of(None, Attention, true), TabStatus::Attention);
        assert_eq!(of(None, Busy, true), TabStatus::Busy);
        assert_eq!(of(None, Idle, true), TabStatus::Bell);
        assert_eq!(of(None, Unknown, true), TabStatus::Bell);
        assert_eq!(of(None, Idle, false), TabStatus::Idle);
        assert_eq!(of(None, Unknown, false), TabStatus::Idle);
    }
}
