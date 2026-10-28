//! The pairing window: one at a time, one device connection at a time. Pure state (no I/O): the
//! engine feeds it relay events and the desktop's decisions and acts on the answers.
//!
//! Stages: `Waiting` (invite shown) → `Claimed` (a device connected with the invite's token) →
//! `Approval` (its message 1 proved the PSK; the desktop decides) → `Accepted` (message 2 sent;
//! the invite token stays registered until that connection ends, so the relay does not cut it).
//!
//! A window serves at most one verified device: the desktop's approval is given while looking at
//! that device's fingerprint, so it must never apply to another one. A claim that ends before
//! proving the PSK goes back to `Waiting` (a wrong guess does not use the window up); a verified
//! candidate that leaves before the decision ends the window. Waiting and claims time out with
//! the window; the approval wait and the accepted connection have their own limit. Transitions
//! check the deadline themselves, not only the engine's timer.

use std::time::{Duration, Instant};

use deck_remote::noise::{DeviceHello, PSK_LEN, Psk, PublicKey, fingerprint};
use deck_remote::relay::{self, Hash, PeerId, Token};
use zeroize::Zeroizing;

use super::{RemoteError, hex, random};
use crate::gateway::protocol::{PairingState, PairingView};

/// Default length of the window and of the approval wait.
pub const DEFAULT_TTL: Duration = Duration::from_secs(120);
/// Overrides [`DEFAULT_TTL`] in milliseconds (verification only).
pub const TTL_ENV: &str = "OMP_DECK_PAIRING_TTL_MS";

/// The configured TTL ([`TTL_ENV`] or [`DEFAULT_TTL`]).
pub fn ttl() -> Duration {
    std::env::var(TTL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map_or(DEFAULT_TTL, Duration::from_millis)
}

/// A device that proved the PSK and waits for the desktop.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub peer: PeerId,
    pub public: PublicKey,
    pub hello: DeviceHello,
}

#[derive(Debug)]
enum Stage {
    Waiting,
    Claimed {
        peer: PeerId,
    },
    Approval {
        candidate: Candidate,
        until: Deadline,
    },
    Accepted {
        peer: PeerId,
        until: Deadline,
    },
}

#[derive(Clone, Copy, Debug)]
struct Deadline {
    at: Instant,
    unix_ms: u64,
}

impl Deadline {
    fn after(ttl: Duration, now: Instant, now_unix_ms: u64) -> Deadline {
        Deadline {
            at: now + ttl,
            unix_ms: now_unix_ms.saturating_add(u64::try_from(ttl.as_millis()).unwrap_or(u64::MAX)),
        }
    }
}

/// What a device connection on the invite's token gets.
#[derive(Debug, PartialEq, Eq)]
pub enum Admit {
    /// Not the invite's token.
    NotOurs,
    /// This connection now holds the window.
    Claimed,
    /// Another connection holds it (or it is over): close this one `UNAUTHORIZED`.
    Refuse,
}

/// What a connection's end did to the window.
#[derive(Debug, PartialEq, Eq)]
pub enum Left {
    NotOurs,
    /// It held the window without proving the PSK: open again for another device.
    Released,
    /// A verified candidate left before the decision: the window is over (drop the invite
    /// token; an approval given for it applies to nobody).
    Abandoned,
    /// The paired device's connection ended: the window is over (drop the invite token).
    Finished,
}

/// One open pairing window. Secrets (token, PSK) never leave it except through
/// [`Window::invite`] and the link's PSK copy.
pub struct Window {
    id: String,
    token: Token,
    token_hash: Hash,
    psk: Zeroizing<Psk>,
    ttl: Duration,
    window: Deadline,
    stage: Stage,
}

impl std::fmt::Debug for Window {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Window")
            .field("id", &self.id)
            .field("stage", &self.stage)
            .finish_non_exhaustive()
    }
}

impl Window {
    pub fn open(ttl: Duration, now: Instant, now_unix_ms: u64) -> Result<Window, RemoteError> {
        let token: Token = random()?;
        let psk = Zeroizing::new(random::<PSK_LEN>()?);
        Ok(Window {
            id: hex(&random::<8>()?),
            token_hash: relay::hash(&token),
            token,
            psk,
            ttl,
            window: Deadline::after(ttl, now, now_unix_ms),
            stage: Stage::Waiting,
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn token(&self) -> &Token {
        &self.token
    }

    pub fn token_hash(&self) -> Hash {
        self.token_hash
    }

    pub fn psk(&self) -> &Zeroizing<Psk> {
        &self.psk
    }

    pub fn expires_unix_ms(&self) -> u64 {
        self.window.unix_ms
    }

    /// The connection currently on the window, if any.
    pub fn peer(&self) -> Option<PeerId> {
        match &self.stage {
            Stage::Waiting => None,
            Stage::Claimed { peer } | Stage::Accepted { peer, .. } => Some(*peer),
            Stage::Approval { candidate, .. } => Some(candidate.peer),
        }
    }

    /// The device waiting for the desktop's decision, while that wait has not timed out.
    pub fn candidate(&self, now: Instant) -> Option<&Candidate> {
        match &self.stage {
            Stage::Approval { candidate, until } if now < until.at => Some(candidate),
            _ => None,
        }
    }

    /// The device was answered; only its connection keeps the window alive.
    pub fn accepted(&self) -> bool {
        matches!(self.stage, Stage::Accepted { .. })
    }

    /// A device connected with `token`.
    pub fn on_open(&mut self, peer: PeerId, token: &Hash) -> Admit {
        if *token != self.token_hash {
            return Admit::NotOurs;
        }
        match self.stage {
            Stage::Waiting => {
                self.stage = Stage::Claimed { peer };
                Admit::Claimed
            }
            _ => Admit::Refuse,
        }
    }

    /// The claiming connection's message 1 proved the PSK: the desktop decides now. `false` =
    /// not the claiming connection, or the window timed out (close it).
    pub fn on_hello(
        &mut self,
        peer: PeerId,
        public: PublicKey,
        hello: DeviceHello,
        now: Instant,
        now_unix_ms: u64,
    ) -> bool {
        if !matches!(self.stage, Stage::Claimed { peer: p } if p == peer) || now >= self.window.at {
            return false;
        }
        self.stage = Stage::Approval {
            candidate: Candidate {
                peer,
                public,
                hello,
            },
            until: Deadline::after(self.ttl, now, now_unix_ms),
        };
        true
    }

    /// Approves the waiting device (unless its wait timed out): its connection is answered and
    /// kept until it ends.
    pub fn approve(&mut self, now: Instant, now_unix_ms: u64) -> Option<Candidate> {
        let candidate = self.candidate(now)?.clone();
        self.stage = Stage::Accepted {
            peer: candidate.peer,
            until: Deadline::after(self.ttl, now, now_unix_ms),
        };
        Some(candidate)
    }

    /// A device connection ended.
    pub fn on_closed(&mut self, peer: PeerId) -> Left {
        if self.peer() != Some(peer) {
            return Left::NotOurs;
        }
        match self.stage {
            Stage::Accepted { .. } => Left::Finished,
            Stage::Approval { .. } => Left::Abandoned,
            Stage::Waiting | Stage::Claimed { .. } => {
                self.stage = Stage::Waiting;
                Left::Released
            }
        }
    }

    /// The relay link went down: every connection on the window is gone.
    pub fn on_link_lost(&mut self) -> Left {
        match self.peer() {
            Some(peer) => self.on_closed(peer),
            None => Left::NotOurs,
        }
    }

    /// When the current stage times out.
    pub fn deadline(&self) -> Instant {
        match &self.stage {
            Stage::Waiting | Stage::Claimed { .. } => self.window.at,
            Stage::Approval { until, .. } | Stage::Accepted { until, .. } => until.at,
        }
    }

    pub fn expired(&self, now: Instant) -> bool {
        now >= self.deadline()
    }

    /// What every GUI shows (`None` once the device is answered: the window is over for them).
    pub fn view(&self) -> Option<PairingView> {
        let (expires_unix_ms, state) = match &self.stage {
            Stage::Waiting | Stage::Claimed { .. } => (self.window.unix_ms, PairingState::Waiting),
            Stage::Approval { candidate, until } => (
                until.unix_ms,
                PairingState::Approval {
                    name: candidate.hello.name.clone(),
                    platform: candidate.hello.platform.clone(),
                    fingerprint: fingerprint(&candidate.public),
                },
            ),
            Stage::Accepted { .. } => return None,
        };
        Some(PairingView {
            pairing: self.id.clone(),
            expires_unix_ms,
            state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(10);

    fn hello() -> DeviceHello {
        DeviceHello {
            name: "Pixel".into(),
            platform: "android".into(),
            ts: 5,
        }
    }

    fn window(now: Instant) -> Window {
        Window::open(TTL, now, 1_000).unwrap()
    }

    #[test]
    fn pair_approve_and_finish_on_close() {
        let t0 = Instant::now();
        let mut w = window(t0);
        assert_eq!(w.view().unwrap().state, PairingState::Waiting);
        assert_eq!(w.view().unwrap().expires_unix_ms, 11_000);
        assert_eq!(w.on_open(1, &[0; 32]), Admit::NotOurs);
        let token = w.token_hash();
        assert_eq!(token, relay::hash(w.token()));
        assert_eq!(w.on_open(1, &token), Admit::Claimed);
        // One connection per invite.
        assert_eq!(w.on_open(2, &token), Admit::Refuse);
        assert!(!w.on_hello(2, [3; 32], hello(), t0, 1_000));
        assert!(w.approve(t0, 1_000).is_none());
        let t1 = t0 + Duration::from_secs(9);
        assert!(w.on_hello(1, [3; 32], hello(), t1, 10_000));
        // The approval wait has its own limit, past the window's.
        assert!(!w.expired(t0 + Duration::from_secs(11)));
        let view = w.view().unwrap();
        assert_eq!(view.expires_unix_ms, 20_000);
        assert_eq!(
            view.state,
            PairingState::Approval {
                name: "Pixel".into(),
                platform: "android".into(),
                fingerprint: fingerprint(&[3; 32]),
            }
        );
        assert_eq!(w.candidate(t1).unwrap().peer, 1);
        let c = w.approve(t1, 10_000).unwrap();
        assert_eq!((c.peer, c.public), (1, [3; 32]));
        assert!(w.accepted() && w.view().is_none());
        // Still one connection only, and other peers' ends do not matter.
        assert_eq!(w.on_open(3, &token), Admit::Refuse);
        assert_eq!(w.on_closed(3), Left::NotOurs);
        assert_eq!(w.on_closed(1), Left::Finished);
    }

    #[test]
    fn an_unverified_claim_that_ends_reopens_the_window() {
        let t0 = Instant::now();
        let mut w = window(t0);
        let token = w.token_hash();
        assert_eq!(w.on_open(1, &token), Admit::Claimed);
        assert_eq!(w.on_closed(1), Left::Released);
        assert_eq!(w.on_open(2, &token), Admit::Claimed);
        assert_eq!(w.on_link_lost(), Left::Released);
        assert_eq!(w.view().unwrap().state, PairingState::Waiting);
        assert_eq!(w.on_link_lost(), Left::NotOurs);
        assert_eq!(w.on_open(3, &token), Admit::Claimed);
        assert!(w.on_hello(3, [4; 32], hello(), t0, 1_000));
    }

    #[test]
    fn a_verified_candidate_that_leaves_ends_the_window() {
        let t0 = Instant::now();
        let mut w = window(t0);
        let token = w.token_hash();
        assert_eq!(w.on_open(1, &token), Admit::Claimed);
        assert!(w.on_hello(1, [4; 32], hello(), t0, 1_000));
        // The approval was for device 1's fingerprint: it must never reach another device.
        assert_eq!(w.on_closed(1), Left::Abandoned);
        assert_eq!(w.on_open(2, &token), Admit::Refuse);
        let mut lost = window(t0);
        let token = lost.token_hash();
        lost.on_open(1, &token);
        assert!(lost.on_hello(1, [4; 32], hello(), t0, 1_000));
        assert_eq!(lost.on_link_lost(), Left::Abandoned);
    }

    #[test]
    fn stages_expire() {
        let t0 = Instant::now();
        let mut w = window(t0);
        assert!(!w.expired(t0 + TTL - Duration::from_millis(1)));
        assert!(w.expired(t0 + TTL));
        let token = w.token_hash();
        w.on_open(1, &token);
        assert!(w.expired(t0 + TTL));
        let t1 = t0 + Duration::from_secs(5);
        assert!(w.on_hello(1, [5; 32], hello(), t1, 6_000));
        assert!(!w.expired(t0 + TTL));
        assert!(w.expired(t1 + TTL));
        assert!(w.approve(t1, 6_000).is_some());
        assert!(w.expired(t1 + TTL));
    }

    #[test]
    fn transitions_refuse_past_their_deadline() {
        let t0 = Instant::now();
        // A claim's proof arriving after the window closed is refused, even before the timer.
        let mut w = window(t0);
        let token = w.token_hash();
        w.on_open(1, &token);
        assert!(!w.on_hello(1, [6; 32], hello(), t0 + TTL, 11_000));
        assert!(w.candidate(t0 + TTL).is_none());
        // An approval after the approval wait timed out stores nothing.
        let mut w = window(t0);
        let token = w.token_hash();
        w.on_open(1, &token);
        assert!(w.on_hello(1, [6; 32], hello(), t0, 1_000));
        assert!(w.candidate(t0 + TTL - Duration::from_millis(1)).is_some());
        assert!(w.candidate(t0 + TTL).is_none());
        assert!(w.approve(t0 + TTL, 11_000).is_none());
        assert!(!w.accepted());
    }

    #[test]
    fn windows_differ_and_debug_hides_secrets() {
        let t0 = Instant::now();
        let a = window(t0);
        let b = window(t0);
        assert_ne!(a.id(), b.id());
        assert_ne!(a.token(), b.token());
        assert_ne!(a.psk().as_ref(), b.psk().as_ref());
        let debug = format!("{a:?}");
        assert!(!debug.contains(&hex(a.token())));
        assert!(!debug.contains(&hex(a.psk().as_ref())));
    }

    #[test]
    fn ttl_defaults_to_two_minutes() {
        // The test process does not set the override.
        if std::env::var_os(TTL_ENV).is_none() {
            assert_eq!(ttl(), Duration::from_secs(120));
        }
    }
}
