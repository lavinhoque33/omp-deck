//! Live-control policy for the Agent Hub: what the link to omp's control extension allows, how
//! the chat draft turns into a message, and the per-agent request bookkeeping the widgets read.

use std::collections::HashMap;
use std::time::Duration;

use deck_core::ctl::LiveStatus;

/// How long a tab launched with the extension may take to open its socket before it counts as
/// read-only (omp itself needs a moment to start).
pub const CONNECT_GRACE: Duration = Duration::from_secs(8);

pub const CHAT_TIP: &str = "Enter sends, Shift+Enter adds a line.\n\
    A running agent is steered: the text reaches it at its next step.\n\
    An idle agent gets a new prompt; a parked agent is woken first.";

/// State of the control link of the active tab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    Live,
    /// This launch created the session with the extension; the socket is not up yet.
    Connecting,
    /// Never had live control (started without the extension, or it did not come up).
    ReadOnly,
    /// Had live control, but the connection is gone while the tab still runs.
    Lost,
    /// The tab's process exited (the session ended, omp quit, ...).
    Ended,
}

pub struct LinkFacts {
    pub connected: bool,
    /// The tab's terminal process exited.
    pub ended: bool,
    /// The socket was connected at some point.
    pub seen: bool,
    /// How long the tab has been waiting for a socket; `None` when no socket is expected (an
    /// attach to an existing session, or no extension).
    pub waited: Option<Duration>,
}

pub fn link_state(f: &LinkFacts) -> Link {
    if f.connected {
        Link::Live
    } else if f.ended {
        Link::Ended
    } else if f.seen {
        Link::Lost
    } else if f.waited.is_some_and(|w| w < CONNECT_GRACE) {
        Link::Connecting
    } else {
        Link::ReadOnly
    }
}

/// The "Restart session with live control" action.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restart {
    /// Not applicable (connected, no backend, no session file, no extension).
    Hidden,
    /// Offered, but omp is busy (restarting would abort its turn).
    Busy,
    Ready,
}

pub struct RestartFacts {
    pub link: Link,
    pub backend: bool,
    pub has_session_file: bool,
    pub extension: bool,
    /// The tab's activity is exactly idle.
    pub idle: bool,
}

pub fn restart_offer(f: &RestartFacts) -> Restart {
    if f.link != Link::ReadOnly || !f.backend || !f.has_session_file || !f.extension {
        Restart::Hidden
    } else if f.idle {
        Restart::Ready
    } else {
        Restart::Busy
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReqKind {
    Kill,
    Revive,
    Chat,
}

/// A command sent and not answered yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Req {
    pub agent: String,
    pub kind: ReqKind,
}

/// What the widgets know about the control link.
pub struct CtlView<'a> {
    pub link: Link,
    pub restart: Restart,
    pub reqs: &'a HashMap<u64, Req>,
    /// Last chat error per agent id.
    pub errors: &'a HashMap<String, String>,
}

impl CtlView<'_> {
    pub fn pending(&self, agent: &str) -> Option<ReqKind> {
        self.reqs
            .values()
            .find(|r| r.agent == agent)
            .map(|r| r.kind)
    }

    pub fn error(&self, agent: &str) -> Option<&str> {
        self.errors.get(agent).map(String::as_str)
    }
}

/// Kill works on running and idle agents (omp's hub `x`).
pub fn can_kill(link: Link, live: Option<LiveStatus>) -> bool {
    link == Link::Live && matches!(live, Some(LiveStatus::Running | LiveStatus::Idle))
}

pub fn can_revive(link: Link, live: Option<LiveStatus>) -> bool {
    link == Link::Live && live == Some(LiveStatus::Parked)
}

/// Chat steers running agents, prompts idle ones and wakes parked ones.
pub fn can_chat(link: Link, live: Option<LiveStatus>) -> bool {
    link == Link::Live
        && matches!(
            live,
            Some(LiveStatus::Running | LiveStatus::Idle | LiveStatus::Parked)
        )
}

/// Why the composer is disabled; `None` when it is usable.
pub fn chat_disabled_reason(link: Link, live: Option<LiveStatus>) -> Option<&'static str> {
    if can_chat(link, live) {
        return None;
    }
    Some(match (link, live) {
        (Link::Connecting, _) => "Connecting to omp…",
        (Link::ReadOnly, _) => "Read-only: this tab was started without live control",
        (Link::Lost, _) => "Live control disconnected",
        (Link::Ended, _) => "This session has ended",
        (Link::Live, Some(LiveStatus::Aborted)) => "This agent was stopped",
        (Link::Live, _) => "omp no longer tracks this agent",
    })
}

/// The message to send for a draft: surrounding blank space trimmed, `None` when empty.
pub fn prepare_message(draft: &str) -> Option<String> {
    let t = draft.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// Placeholder text of the composer for an agent in `live`.
pub fn composer_hint(live: Option<LiveStatus>) -> &'static str {
    match live {
        Some(LiveStatus::Running) => "Steer this agent…",
        Some(LiveStatus::Parked) => "Message to wake this agent…",
        _ => "Message this agent…",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Option<LiveStatus>; 5] = [
        None,
        Some(LiveStatus::Running),
        Some(LiveStatus::Idle),
        Some(LiveStatus::Parked),
        Some(LiveStatus::Aborted),
    ];

    fn facts(connected: bool, waited: Option<Duration>) -> LinkFacts {
        LinkFacts {
            connected,
            ended: false,
            seen: false,
            waited,
        }
    }

    #[test]
    fn link_waits_for_the_socket_only_when_launched_with_it() {
        let s = Duration::from_secs;
        assert_eq!(link_state(&facts(true, None)), Link::Live);
        assert_eq!(link_state(&facts(true, Some(s(100)))), Link::Live);
        assert_eq!(link_state(&facts(false, Some(s(1)))), Link::Connecting);
        assert_eq!(
            link_state(&facts(false, Some(CONNECT_GRACE))),
            Link::ReadOnly
        );
        assert_eq!(link_state(&facts(false, None)), Link::ReadOnly);
    }

    #[test]
    fn a_gone_connection_is_not_a_missing_one() {
        let mut f = facts(false, None);
        f.seen = true;
        assert_eq!(link_state(&f), Link::Lost);
        f.ended = true;
        assert_eq!(link_state(&f), Link::Ended);
        let mut never = facts(false, None);
        never.ended = true;
        assert_eq!(link_state(&never), Link::Ended);
        f.connected = true;
        assert_eq!(link_state(&f), Link::Live);
    }

    #[test]
    fn controls_exist_only_while_live() {
        for live in ALL {
            for link in [Link::Connecting, Link::ReadOnly, Link::Lost, Link::Ended] {
                assert!(!can_kill(link, live));
                assert!(!can_revive(link, live));
                assert!(!can_chat(link, live));
                assert!(chat_disabled_reason(link, live).is_some());
            }
        }
    }

    #[test]
    fn live_controls_follow_the_agent_state() {
        let l = Link::Live;
        assert!(can_kill(l, Some(LiveStatus::Running)));
        assert!(can_kill(l, Some(LiveStatus::Idle)));
        assert!(!can_kill(l, Some(LiveStatus::Parked)));
        assert!(!can_kill(l, Some(LiveStatus::Aborted)));
        assert!(!can_kill(l, None));
        assert!(can_revive(l, Some(LiveStatus::Parked)));
        assert!(!can_revive(l, Some(LiveStatus::Idle)));
        for ok in [LiveStatus::Running, LiveStatus::Idle, LiveStatus::Parked] {
            assert!(can_chat(l, Some(ok)));
            assert_eq!(chat_disabled_reason(l, Some(ok)), None);
        }
        assert!(!can_chat(l, Some(LiveStatus::Aborted)));
        assert!(!can_chat(l, None));
        assert!(chat_disabled_reason(l, None).is_some());
    }

    #[test]
    fn restart_is_offered_only_to_read_only_idle_backend_tabs_with_a_file() {
        let base = RestartFacts {
            link: Link::ReadOnly,
            backend: true,
            has_session_file: true,
            extension: true,
            idle: true,
        };
        assert_eq!(restart_offer(&base), Restart::Ready);
        assert_eq!(
            restart_offer(&RestartFacts {
                idle: false,
                ..base
            }),
            Restart::Busy
        );
        for hidden in [
            RestartFacts {
                link: Link::Live,
                ..base
            },
            RestartFacts {
                link: Link::Connecting,
                ..base
            },
            RestartFacts {
                link: Link::Lost,
                ..base
            },
            RestartFacts {
                link: Link::Ended,
                ..base
            },
            RestartFacts {
                backend: false,
                ..base
            },
            RestartFacts {
                has_session_file: false,
                ..base
            },
            RestartFacts {
                extension: false,
                ..base
            },
        ] {
            assert_eq!(restart_offer(&hidden), Restart::Hidden);
        }
    }

    #[test]
    fn drafts_are_trimmed_and_blank_ones_are_not_sent() {
        assert_eq!(
            prepare_message("  hi there \n"),
            Some("hi there".to_owned())
        );
        assert_eq!(prepare_message("a\n\nb"), Some("a\n\nb".to_owned()));
        assert_eq!(prepare_message(" \n\t "), None);
        assert_eq!(prepare_message(""), None);
    }

    #[test]
    fn pending_and_errors_are_looked_up_per_agent() {
        let mut reqs = HashMap::new();
        reqs.insert(
            7,
            Req {
                agent: "A".into(),
                kind: ReqKind::Chat,
            },
        );
        let mut errors = HashMap::new();
        errors.insert("B".to_owned(), "no such subagent".to_owned());
        let v = CtlView {
            link: Link::Live,
            restart: Restart::Hidden,
            reqs: &reqs,
            errors: &errors,
        };
        assert_eq!(v.pending("A"), Some(ReqKind::Chat));
        assert_eq!(v.pending("B"), None);
        assert_eq!(v.error("B"), Some("no such subagent"));
        assert_eq!(v.error("A"), None);
    }

    #[test]
    fn composer_hint_names_the_effect() {
        assert!(composer_hint(Some(LiveStatus::Running)).contains("Steer"));
        assert!(composer_hint(Some(LiveStatus::Parked)).contains("wake"));
        assert!(composer_hint(Some(LiveStatus::Idle)).contains("Message"));
    }
}
