//! Transport of the OmpDeck mobile companion (slice 3, `docs/notes/mobile-relay.md`).
//!
//! - [`relay`]: the wire format between `omp-deck-relay` and its clients.
//! - `noise` (feature `noise`): device pairing/reconnect handshakes and encrypted sessions.
//! - `api` (feature `noise`): the device application protocol inside those sessions.

pub mod relay;

#[cfg(feature = "noise")]
pub mod noise;

#[cfg(feature = "noise")]
pub mod api;
