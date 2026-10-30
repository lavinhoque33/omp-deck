//! Mobile access in the engine: every decision about the relay link's devices (admission,
//! pairing approval, reconnect freshness, revocation), the relay settings and the pairing window
//! (`Topic::Mobile`). The link thread (`remote::link`) does the network and Noise work.
//!
//! Ordering rules (`docs/notes/mobile-relay.md`, "R3 contract"):
//! - a reconnect's `last_ts` is on disk before message 2 is sent;
//! - an approved device is on disk, and its token registered at the relay, before message 2;
//! - the invite token stays registered until the pairing connection ends (or deny, cancel,
//!   expiry), so the relay never cuts the device before it read message 2;
//! - a revocation is on disk, its token withdrawn and its sessions closed before `Done`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deck_core::config::GatewayMode;
use deck_remote::noise::{
    Keypair, MAX_GATEWAY_LABEL_CHARS, MAX_NAME_CHARS, PairingInvite, Welcome, fingerprint,
    is_display_safe, timestamp_fresh,
};
use deck_remote::relay::{self, Hash, MAX_TOKENS, PeerId, close};

use super::{Engine, Msg};
use crate::gateway::protocol::{
    Done, GatewayError, MobileView, Mutation, RelayLink, RelayView, SecretText,
};
use crate::gateway::remote::config::{self, RelayConfig};
use crate::gateway::remote::epoch::{self, Epochs};
use crate::gateway::remote::link::{Cmd, Event, Invite, Link};
use crate::gateway::remote::pairing::{self, Admit, Candidate, Left, Window};
use crate::gateway::remote::registry::{self, Device, Registry};
use crate::gateway::remote::{RemoteError, hex, identity, random};

/// The gateway's key and devices; without them mobile access is unavailable.
struct Parts {
    identity: Keypair,
    registry: Registry,
}

/// One device connection the relay admitted.
struct Peer {
    /// The link's name for this admission ([`Event::Opened`]): commands carry it, so they
    /// never reach a later admission on a reused peer number.
    serial: u64,
    token: Hash,
    /// Its session is established as this device.
    device: Option<String>,
    /// The engine link of its application session (reconnects only).
    conn: Option<u64>,
}

pub(super) struct Mobile {
    dir: PathBuf,
    parts: Result<Parts, String>,
    relay: Option<RelayConfig>,
    link: Option<Link>,
    /// Generation of `link`: events of a stopped link are ignored.
    link_gen: u64,
    link_state: RelayLink,
    pairing: Option<Window>,
    peers: HashMap<PeerId, Peer>,
    ttl: Duration,
    /// What devices are told this gateway is called.
    label: String,
    /// Hello epochs, shared by every link this owner starts (old and new alike).
    epochs: Arc<Mutex<Epochs>>,
}

impl Mobile {
    /// Loads the key, the devices and the relay from the gateway's secrets dir, created
    /// owner-only first. A dir that cannot be made private, or a broken key or device file,
    /// disables mobile access (reported, never overwritten); the engine still starts.
    pub(super) fn open(dir: &Path) -> Mobile {
        let parts = crate::gateway::secret_dir(dir)
            .map_err(|e| format!("protecting {}: {e}", dir.display()))
            .and_then(|()| {
                let identity = identity::load_or_create(&dir.join(identity::FILE))
                    .map_err(|e| e.to_string())?;
                Ok(Parts {
                    identity,
                    registry: Registry::load(&dir.join(registry::FILE))
                        .map_err(|e| e.to_string())?,
                })
            });
        let parts = parts.map_err(|e| {
            log::error!("mobile access is unavailable: {e}");
            format!("mobile access is unavailable: {e}")
        });
        // Nothing more is read from (or written to) a dir that is not private.
        let relay = match &parts {
            Ok(_) => RelayConfig::load(&dir.join(config::FILE)).unwrap_or_else(|e| {
                log::error!("ignoring the relay settings: {e}");
                None
            }),
            Err(_) => None,
        };
        Mobile {
            dir: dir.to_path_buf(),
            parts,
            relay,
            link: None,
            link_gen: 0,
            link_state: RelayLink::Off,
            pairing: None,
            peers: HashMap::new(),
            ttl: pairing::ttl(),
            label: gateway_label(),
            epochs: Arc::new(Mutex::new(Epochs::load(&dir.join(epoch::FILE)))),
        }
    }

    fn token_set(&self) -> Vec<Hash> {
        let Ok(parts) = &self.parts else {
            return Vec::new();
        };
        parts
            .registry
            .token_hashes()
            .chain(self.pairing.as_ref().map(Window::token_hash))
            .collect()
    }

    fn send(&self, cmd: Cmd) {
        if let Some(link) = &self.link {
            link.send(cmd);
        }
    }

    fn publish_tokens(&self) {
        self.send(Cmd::Tokens(self.token_set()));
    }

    /// Closes `peer` and forgets it at once: a closed admission holds no engine state while
    /// the relay's `Closed` is on its way.
    fn close_peer(&mut self, peer: PeerId, code: u16) {
        if let Some(p) = self.peers.remove(&peer) {
            self.send(Cmd::Close {
                peer,
                serial: p.serial,
                code,
            });
        }
    }

    /// The relay closed `peer`: its device was last seen now, and a pairing candidate on it
    /// is gone.
    fn peer_closed(&mut self, peer: PeerId) {
        if let Some(Peer {
            device: Some(id), ..
        }) = self.peers.remove(&peer)
            && let Ok(parts) = &mut self.parts
        {
            let seen = crate::gateway::unix_ms();
            if let Err(e) = parts.registry.update(&id, |d| d.last_seen_unix_ms = seen) {
                log::warn!("mobile access: recording when {id} was last seen: {e}");
            }
        }
        match self.pairing.as_mut().map(|w| w.on_closed(peer)) {
            Some(Left::Finished) => self.end_window(),
            Some(Left::Abandoned) => {
                log::info!("mobile access: pairing ended (the device left)");
                self.end_window();
            }
            Some(Left::Released | Left::NotOurs) | None => {}
        }
    }

    /// The relay admitted a device connection on `peer` (admission `serial`).
    fn peer_opened(&mut self, peer: PeerId, serial: u64, token: Hash) {
        // A reused peer number ends whatever the old admission was doing (a pairing candidate
        // on it is abandoned), before the new one starts.
        if self.peers.contains_key(&peer) {
            self.peer_closed(peer);
        }
        self.peers.insert(
            peer,
            Peer {
                serial,
                token,
                device: None,
                conn: None,
            },
        );
        let known = self
            .parts
            .as_ref()
            .is_ok_and(|p| p.registry.token_hashes().any(|h| h == token));
        match self.pairing.as_mut().map(|w| w.on_open(peer, &token)) {
            Some(Admit::Claimed) => {}
            Some(Admit::Refuse) => {
                log::info!("relay peer {peer}: the pairing window is taken");
                self.close_peer(peer, close::UNAUTHORIZED);
            }
            Some(Admit::NotOurs) | None if !known => {
                // A token withdrawn while the relay admitted it.
                self.close_peer(peer, close::UNAUTHORIZED);
            }
            Some(Admit::NotOurs) | None => {}
        }
    }

    /// Engine link `conn` is a device session the link still carries.
    pub(super) fn serves(&self, conn: u64) -> bool {
        self.peers.values().any(|p| p.conn == Some(conn))
    }

    /// The engine link of `peer`'s application session, while it lasts.
    pub(super) fn peer_conn(&self, peer: PeerId) -> Option<u64> {
        self.peers.get(&peer).and_then(|p| p.conn)
    }

    /// What paired device `id` may do now (`None`: revoked or unknown).
    pub(super) fn permissions(&self, id: &str) -> Option<crate::gateway::protocol::Permissions> {
        let parts = self.parts.as_ref().ok()?;
        parts.registry.get(id).map(|d| d.permissions)
    }

    /// Ends the pairing window: its connection (if any) is closed and its token withdrawn.
    fn end_window(&mut self) {
        let Some(w) = self.pairing.take() else {
            return;
        };
        if let Some(peer) = w.peer()
            && !w.accepted()
        {
            self.close_peer(peer, close::UNAUTHORIZED);
        }
        self.send(Cmd::Invite(None));
        self.publish_tokens();
    }

    /// Stops the link. Never blocks unless `wait` (process exit): the old thread is cancelled
    /// at once and its late events carry a stale generation.
    fn stop_link(&mut self, wait: bool) {
        // The window cannot work without the link.
        self.end_window();
        if let Some(link) = self.link.take() {
            self.link_gen += 1;
            if wait {
                link.stop_and_wait();
            } else {
                link.stop();
            }
        }
        self.peers.clear();
        self.link_state = RelayLink::Off;
    }

    fn view(&self) -> MobileView {
        let parts = self.parts.as_ref().ok();
        let online = |id: &str| self.peers.values().any(|p| p.device.as_deref() == Some(id));
        MobileView {
            relay: self.relay.as_ref().map(|r| RelayView {
                url: r.url.as_str().to_owned(),
                room_id: relay::room_id(&r.room_key.room),
            }),
            link: match &self.parts {
                Err(reason) => RelayLink::Refused {
                    reason: reason.clone(),
                },
                Ok(_) => self.link_state.clone(),
            },
            fingerprint: parts.map_or_else(String::new, |p| fingerprint(p.identity.public())),
            devices: parts.map_or_else(Vec::new, |p| {
                p.registry
                    .devices()
                    .iter()
                    .map(|d| d.view(online(&d.id)))
                    .collect()
            }),
            pairing: self.pairing.as_ref().and_then(Window::view),
        }
    }

    /// The device the desktop approves with window `pairing`, if that exact window still waits
    /// for this decision at `now`.
    fn approvable(&self, pairing: &str, now: Instant) -> Result<Candidate, GatewayError> {
        let ended = || GatewayError::Refused(PAIRING_ENDED.into());
        let w = self
            .pairing
            .as_ref()
            .filter(|w| w.id() == pairing)
            .ok_or_else(ended)?;
        if w.accepted() {
            return Err(ended());
        }
        match w.candidate(now) {
            Some(c) => Ok(c.clone()),
            None if w.expired(now) => Err(ended()),
            None => Err(GatewayError::Refused(
                "No device is waiting for approval yet.".into(),
            )),
        }
    }

    /// Window `pairing` can still be denied.
    fn deniable(&self, pairing: &str) -> Result<(), GatewayError> {
        match &self.pairing {
            Some(w) if w.id() == pairing && !w.accepted() => Ok(()),
            _ => Err(GatewayError::Refused(PAIRING_ENDED.into())),
        }
    }
}

/// Approve/deny of a window that is gone (cancelled, timed out, its device left or answered).
const PAIRING_ENDED: &str =
    "This pairing has ended (the device left, it timed out or it was cancelled); start a new one.";

/// `OmpDeck on <host>` (at most the protocol's label length), or `OmpDeck`.
fn gateway_label() -> String {
    match deck_core::platform::hostname() {
        Some(host) => format!("OmpDeck on {host}")
            .chars()
            .take(MAX_GATEWAY_LABEL_CHARS)
            .collect(),
        None => "OmpDeck".to_owned(),
    }
}

fn refused(text: impl Into<String>) -> Option<GatewayError> {
    Some(GatewayError::Refused(text.into()))
}

fn failed(e: RemoteError) -> GatewayError {
    GatewayError::Failed(e.to_string())
}

/// The mobile-access mutations.
pub(super) fn is_mobile(op: &Mutation) -> bool {
    matches!(
        op,
        Mutation::RelaySet { .. }
            | Mutation::RelayClear
            | Mutation::RelayReconnect
            | Mutation::PairingBegin
            | Mutation::PairingCancel
            | Mutation::PairingApprove { .. }
            | Mutation::PairingDeny { .. }
            | Mutation::DeviceRevoke { .. }
            | Mutation::DeviceRename { .. }
            | Mutation::DeviceSetPermissions { .. }
    )
}

/// Every paired device's id (and so its client id) starts with this.
const DEVICE_ID_PREFIX: &str = "d-";

/// `id` is (or claims to be) a paired device's: reserved for device sessions.
pub(super) fn is_device_id(id: &str) -> bool {
    id.starts_with(DEVICE_ID_PREFIX)
}

/// A new device's id from 8 random bytes.
fn device_id(random: &[u8; 8]) -> String {
    format!("{DEVICE_ID_PREFIX}{}", hex(random))
}

fn valid_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("give the device a name".to_owned());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "a device name has at most {MAX_NAME_CHARS} characters"
        ));
    }
    // No control, invisible or direction-changing characters: a name shown in a dialog must
    // read the same as what is stored.
    if !is_display_safe(name) {
        return Err("a device name cannot contain control or invisible characters".to_owned());
    }
    Ok(name.to_owned())
}

impl Engine {
    // ---- link lifecycle -----------------------------------------------------------------------

    /// Runs the link exactly while mobile access is on and a relay is configured.
    pub(super) fn sync_mobile_link(&mut self) {
        let wanted = self.cfg.gateway_mode != GatewayMode::Off
            && !self.stopping
            && self.mobile.relay.is_some()
            && self.mobile.parts.is_ok();
        if wanted == self.mobile.link.is_some() {
            return;
        }
        if !wanted {
            log::info!("mobile access: relay link stopped");
            self.mobile.stop_link(false);
            self.dirty.mobile = true;
            return;
        }
        let (Some(relay), Ok(parts)) = (&self.mobile.relay, &self.mobile.parts) else {
            return;
        };
        self.mobile.link_gen += 1;
        let generation = self.mobile.link_gen;
        let tx = self.tx.clone();
        let invite = self.mobile.pairing.as_ref().map(|w| Invite {
            token_hash: w.token_hash(),
            psk: w.psk().clone(),
        });
        match Link::start(
            relay.clone(),
            parts.identity.clone(),
            self.mobile.epochs.clone(),
            self.mobile.token_set(),
            invite,
            Box::new(move |event| {
                let _ = tx.send(Msg::Relay {
                    link: generation,
                    event,
                });
            }),
        ) {
            Ok(link) => {
                self.mobile.link = Some(link);
                self.mobile.link_state = RelayLink::Connecting {
                    attempt: 1,
                    error: None,
                };
            }
            Err(e) => {
                log::error!("cannot start the relay link: {e}");
                self.mobile.link_state = RelayLink::Refused {
                    reason: format!("cannot start the relay link: {e}"),
                };
            }
        }
        self.dirty.mobile = true;
    }

    /// Restarts the link (a new relay).
    fn restart_mobile_link(&mut self) {
        self.mobile.stop_link(false);
        self.sync_mobile_link();
        self.dirty.mobile = true;
    }

    /// The maintenance barrier closes an open pairing window.
    pub(super) fn cancel_pairing(&mut self) {
        if self.mobile.pairing.is_some() {
            self.mobile.end_window();
            self.dirty.mobile = true;
        }
    }

    /// Owner shutdown: the link closes, waiting a bounded time so the process exits cleanly.
    pub(super) fn stop_mobile(&mut self) {
        self.mobile.stop_link(true);
    }

    /// Times out the pairing window's stages.
    pub(super) fn step_mobile(&mut self, now: Instant) {
        let Some(w) = &self.mobile.pairing else {
            return;
        };
        if w.expired(now) {
            log::info!("mobile access: pairing window {} expired", w.id());
            // An answered device's connection is not cut by `end_window`; a lingering one is.
            if w.accepted()
                && let Some(peer) = w.peer()
            {
                self.mobile.close_peer(peer, close::UNAUTHORIZED);
            }
            self.mobile.end_window();
            self.dirty.mobile = true;
        } else {
            let left = w.deadline().saturating_duration_since(now);
            self.wake_within(left);
        }
    }

    pub(super) fn mobile_view(&self) -> MobileView {
        self.mobile.view()
    }

    // ---- relay events -------------------------------------------------------------------------

    pub(super) fn on_relay(&mut self, link: u64, event: Event) {
        if link != self.mobile.link_gen || self.mobile.link.is_none() {
            return;
        }
        if let Event::Device { peer, inbound } = event {
            // A session closed meanwhile (revoked, replaced) is not served any more.
            if let Some(conn) = self.mobile.peer_conn(peer) {
                self.on_device_inbound(conn, inbound);
            }
            return;
        }
        let now = Instant::now();
        let m = &mut self.mobile;
        match event {
            Event::Device { .. } => {}
            Event::Link(state) => {
                if state != RelayLink::Up {
                    // Every device connection went with the old socket.
                    m.peers.clear();
                    match m.pairing.as_mut().map(Window::on_link_lost) {
                        Some(Left::Finished) => m.end_window(),
                        Some(Left::Abandoned) => {
                            log::info!("mobile access: pairing ended (the device left)");
                            m.end_window();
                        }
                        Some(Left::Released | Left::NotOurs) | None => {}
                    }
                }
                m.link_state = state;
            }
            Event::Opened {
                peer,
                serial,
                token,
            } => m.peer_opened(peer, serial, token),
            Event::Hello {
                peer,
                serial,
                pairing,
                public,
                hello,
            } => {
                let Some(token) = m.peers.get(&peer).map(|p| p.token) else {
                    // Closed meanwhile.
                    return;
                };
                if m.peers.get(&peer).is_some_and(|p| p.serial != serial) {
                    // An earlier admission's handshake: the link has dropped it.
                    return;
                }
                if pairing {
                    let unix = crate::gateway::unix_ms();
                    let ok = m.pairing.as_mut().is_some_and(|w| {
                        w.token_hash() == token && w.on_hello(peer, public, hello, now, unix)
                    });
                    if ok {
                        log::info!(
                            "mobile access: device {} asks to pair",
                            fingerprint(&public)
                        );
                    } else {
                        m.close_peer(peer, close::UNAUTHORIZED);
                    }
                } else {
                    self.on_reconnect(peer, serial, token, public, hello);
                }
            }
            Event::Closed { peer } => m.peer_closed(peer),
        }
        self.dirty.mobile = true;
        self.sync_devices();
    }

    /// A paired device's message 1: known key, its own token, a fresh timestamp. The timestamp
    /// is on disk before message 2.
    fn on_reconnect(
        &mut self,
        peer: PeerId,
        serial: u64,
        token: Hash,
        public: deck_remote::noise::PublicKey,
        hello: deck_remote::noise::DeviceHello,
    ) {
        let m = &mut self.mobile;
        let Ok(parts) = &mut m.parts else {
            m.close_peer(peer, close::UNAUTHORIZED);
            return;
        };
        let refusal = match parts.registry.by_public(&public) {
            None => Some("unknown device"),
            Some(d) if d.token_hash != token => Some("another device's token"),
            Some(d) if !timestamp_fresh(d.last_ts, hello.ts) => Some("replayed or stale timestamp"),
            Some(_) => None,
        };
        let id = match (refusal, parts.registry.by_public(&public)) {
            (None, Some(d)) => d.id.clone(),
            (why, _) => {
                log::info!(
                    "relay peer {peer}: reconnect of {} refused ({})",
                    fingerprint(&public),
                    why.unwrap_or("unknown device")
                );
                m.close_peer(peer, close::UNAUTHORIZED);
                return;
            }
        };
        let seen = crate::gateway::unix_ms();
        if let Err(e) = parts.registry.update(&id, |d| {
            d.last_ts = hello.ts;
            d.last_seen_unix_ms = seen;
        }) {
            log::error!("mobile access: recording {id}'s handshake failed, refusing it: {e}");
            m.close_peer(peer, close::UNAUTHORIZED);
            return;
        }
        let found = parts
            .registry
            .get(&id)
            .map(|d| (d.name.clone(), d.permissions));
        // One live session per device: a reconnect replaces the previous one.
        let previous: Vec<PeerId> = m
            .peers
            .iter()
            .filter(|(p, x)| **p != peer && x.conn.is_some() && x.device.as_deref() == Some(&id))
            .map(|(p, _)| *p)
            .collect();
        for p in previous {
            m.close_peer(p, close::NORMAL);
        }
        let (Some(wake), Some((name, permissions))) = (m.link.as_ref().map(Link::waker), found)
        else {
            m.close_peer(peer, close::UNAUTHORIZED);
            return;
        };
        let label = m.label.clone();
        let (conn, out) = self.open_device(&id, name, permissions, hello.api, hello.features, wake);
        let m = &mut self.mobile;
        m.send(Cmd::Accept {
            peer,
            serial,
            public,
            welcome: Welcome {
                device: id.clone(),
                gateway: label,
                token: None,
            },
            device: Some(out),
        });
        if let Some(p) = m.peers.get_mut(&peer) {
            p.device = Some(id);
            p.conn = Some(conn);
        }
    }

    // ---- mutations ----------------------------------------------------------------------------

    /// Refusals of mobile-access mutations, before anything is journaled.
    pub(super) fn mobile_precheck(&self, op: &Mutation) -> Option<GatewayError> {
        let m = &self.mobile;
        let parts = match (&m.parts, op) {
            (_, Mutation::RelayClear) => return None,
            (Err(reason), _) => return refused(reason.clone()),
            (Ok(p), _) => p,
        };
        let device = |id: &str| parts.registry.get(id).is_some();
        match op {
            Mutation::RelaySet { url, room_key } => RelayConfig::new(url, room_key.expose())
                .err()
                .map(|e| GatewayError::Refused(e.to_string())),
            Mutation::RelayReconnect => {
                if self.cfg.gateway_mode == GatewayMode::Off {
                    refused("Mobile access is off: choose when the gateway runs first.")
                } else if m.relay.is_none() {
                    refused("No relay is set up: set the relay URL and room key first.")
                } else {
                    None
                }
            }
            Mutation::PairingBegin => {
                if self.cfg.gateway_mode == GatewayMode::Off {
                    refused("Mobile access is off: choose when the gateway runs first.")
                } else if m.relay.is_none() {
                    refused("No relay is set up: set the relay URL and room key first.")
                } else if m.link_state != RelayLink::Up {
                    refused("The relay is not connected yet; try again once it is.")
                } else if m.pairing.is_some() {
                    refused("A pairing window is already open.")
                } else if parts.registry.devices().len() + 1 >= MAX_TOKENS {
                    refused("Too many paired devices: revoke one first.")
                } else {
                    None
                }
            }
            Mutation::PairingCancel => match &m.pairing {
                Some(w) if !w.accepted() => None,
                _ => refused("No pairing window is open."),
            },
            Mutation::PairingApprove { pairing, .. } => {
                self.mobile.approvable(pairing, Instant::now()).err()
            }
            Mutation::PairingDeny { pairing } => self.mobile.deniable(pairing).err(),
            Mutation::DeviceRevoke { id } | Mutation::DeviceSetPermissions { id, .. } => {
                (!device(id)).then(|| GatewayError::Refused("No such device.".to_owned()))
            }
            Mutation::DeviceRename { id, name } => {
                if !device(id) {
                    refused("No such device.")
                } else {
                    valid_name(name).err().map(GatewayError::Refused)
                }
            }
            _ => None,
        }
    }

    /// Runs an admitted mobile-access mutation (inline: only small private-file writes).
    pub(super) fn mobile_execute(&mut self, op: Mutation) -> Result<Done, GatewayError> {
        self.dirty.mobile = true;
        match op {
            Mutation::RelaySet { url, room_key } => {
                // The room secret goes only into a dir that was made private.
                if let Err(e) = &self.mobile.parts {
                    return Err(GatewayError::Refused(e.clone()));
                }
                let cfg = RelayConfig::new(&url, room_key.expose())
                    .map_err(|e| GatewayError::Refused(e.to_string()))?;
                cfg.save(&self.mobile.dir.join(config::FILE))
                    .map_err(failed)?;
                log::info!(
                    "mobile access: relay set to {} room {}",
                    cfg.url.as_str(),
                    relay::room_id(&cfg.room_key.room)
                );
                self.mobile.relay = Some(cfg);
                self.restart_mobile_link();
                Ok(Done::Unit)
            }
            Mutation::RelayClear => {
                RelayConfig::clear(&self.mobile.dir.join(config::FILE)).map_err(failed)?;
                self.mobile.relay = None;
                self.sync_mobile_link();
                log::info!("mobile access: relay cleared");
                Ok(Done::Unit)
            }
            Mutation::RelayReconnect => {
                log::info!("mobile access: reconnecting to the relay");
                self.restart_mobile_link();
                Ok(Done::Unit)
            }
            Mutation::PairingBegin => self.begin_pairing(),
            Mutation::PairingCancel => {
                self.mobile.end_window();
                Ok(Done::Unit)
            }
            Mutation::PairingDeny { pairing } => {
                self.mobile.deniable(&pairing)?;
                log::info!("mobile access: pairing denied");
                self.mobile.end_window();
                Ok(Done::Unit)
            }
            Mutation::PairingApprove {
                pairing,
                permissions,
            } => {
                // One clock reading for the check and the transition: a candidate whose wait
                // timed out is never stored.
                let at = Instant::now();
                let candidate = self.mobile.approvable(&pairing, at)?;
                let token: relay::Token = random().map_err(failed)?;
                let id = device_id(&random::<8>().map_err(failed)?);
                let now = crate::gateway::unix_ms();
                let name = valid_name(&candidate.hello.name).unwrap_or_else(|_| "Phone".into());
                let m = &mut self.mobile;
                let (Ok(parts), Some(w)) = (&mut m.parts, &mut m.pairing) else {
                    return Err(GatewayError::Refused(PAIRING_ENDED.into()));
                };
                let replaced = parts
                    .registry
                    .add(Device {
                        id: id.clone(),
                        name,
                        platform: candidate.hello.platform.clone(),
                        public: candidate.public,
                        token_hash: relay::hash(&token),
                        permissions,
                        last_ts: candidate.hello.ts,
                        created_unix_ms: now,
                        last_seen_unix_ms: now,
                    })
                    .map_err(failed)?;
                w.approve(at, now);
                log::info!(
                    "mobile access: paired {id} ({})",
                    fingerprint(&candidate.public)
                );
                // The pairing connection is this device's session from now on: it shows online
                // and a revocation closes it.
                let serial = m.peers.get_mut(&candidate.peer).map(|p| {
                    p.device = Some(id.clone());
                    p.serial
                });
                // Its token is registered before message 2 reaches the device.
                m.publish_tokens();
                if let Some(serial) = serial {
                    m.send(Cmd::Accept {
                        peer: candidate.peer,
                        serial,
                        public: candidate.public,
                        welcome: Welcome {
                            device: id.clone(),
                            gateway: m.label.clone(),
                            token: Some(token),
                        },
                        device: None,
                    });
                }
                // A re-paired key's old record is gone: nothing it asked for runs any more
                // (the new pairing may well have fewer rights).
                for old in &replaced {
                    log::info!("mobile access: {id} replaces {old}");
                    self.cancel_client_work(old);
                }
                Ok(Done::Unit)
            }
            Mutation::DeviceRevoke { id } => {
                let m = &mut self.mobile;
                let Ok(parts) = &mut m.parts else {
                    return Err(GatewayError::Refused("No such device.".into()));
                };
                let Some(gone) = parts.registry.remove(&id).map_err(failed)? else {
                    return Err(GatewayError::Refused("No such device.".into()));
                };
                // The relay closes its live connection when the token leaves the set; any
                // session of the key is closed here too.
                m.publish_tokens();
                let live: Vec<PeerId> = m
                    .peers
                    .iter()
                    .filter(|(_, p)| {
                        p.device.as_deref() == Some(id.as_str()) || p.token == gone.token_hash
                    })
                    .map(|(peer, _)| *peer)
                    .collect();
                for peer in &live {
                    m.close_peer(*peer, close::UNAUTHORIZED);
                }
                // Revoked while still on its pairing connection: the invite token goes too.
                if m.pairing
                    .as_ref()
                    .is_some_and(|w| w.accepted() && w.peer().is_some_and(|p| live.contains(&p)))
                {
                    m.end_window();
                }
                log::info!(
                    "mobile access: revoked {id} ({})",
                    fingerprint(&gone.public)
                );
                // Nothing it asked for runs after this: unwritten commands and queued
                // launches are cancelled.
                self.cancel_client_work(&id);
                Ok(Done::Unit)
            }
            Mutation::DeviceRename { id, name } => {
                let name = valid_name(&name).map_err(GatewayError::Refused)?;
                self.update_device(&id, |d| d.name = name)
            }
            Mutation::DeviceSetPermissions { id, permissions } => {
                let had_control = self.mobile.permissions(&id).is_some_and(|p| p.control);
                let done = self.update_device(&id, |d| d.permissions = permissions)?;
                // Lowered while offline too: its commands that have not started are cancelled
                // (its typing views end at the next step).
                if had_control && !permissions.control {
                    self.cancel_client_work(&id);
                }
                Ok(done)
            }
            other => Err(GatewayError::Failed(format!(
                "{} is not a mobile-access operation",
                other.label()
            ))),
        }
    }

    fn update_device(
        &mut self,
        id: &str,
        f: impl FnOnce(&mut Device),
    ) -> Result<Done, GatewayError> {
        let Ok(parts) = &mut self.mobile.parts else {
            return Err(GatewayError::Refused("No such device.".into()));
        };
        if parts.registry.update(id, f).map_err(failed)? {
            Ok(Done::Unit)
        } else {
            Err(GatewayError::Refused("No such device.".into()))
        }
    }

    fn begin_pairing(&mut self) -> Result<Done, GatewayError> {
        let now = crate::gateway::unix_ms();
        let m = &mut self.mobile;
        let (Ok(parts), Some(relay)) = (&m.parts, &m.relay) else {
            return Err(GatewayError::Refused(
                "No relay is set up: set the relay URL and room key first.".into(),
            ));
        };
        let w = Window::open(m.ttl, Instant::now(), now).map_err(failed)?;
        let invite = PairingInvite {
            url: relay.url.as_str().to_owned(),
            room: relay.room_key.room,
            token: *w.token(),
            gateway_public: *parts.identity.public(),
            psk: w.psk().clone(),
        }
        .encode()
        .map_err(|e| GatewayError::Failed(format!("building the pairing invite: {e}")))?;
        let link = SecretText::new(format!("{}/#pair={invite}", relay.url.pwa_origin()));
        let done = Done::PairingStarted {
            pairing: w.id().to_owned(),
            invite: SecretText::new(invite),
            link,
            expires_unix_ms: w.expires_unix_ms(),
        };
        // The PSK reaches the link before the token is admitted.
        m.send(Cmd::Invite(Some(Invite {
            token_hash: w.token_hash(),
            psk: w.psk().clone(),
        })));
        log::info!("mobile access: pairing window {} opened", w.id());
        m.pairing = Some(w);
        m.publish_tokens();
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secrets_dir_that_cannot_be_made_private_only_disables_mobile_access() {
        let tmp = tempfile::tempdir().unwrap();
        // Its parent is a file: the dir can be neither created nor protected.
        std::fs::write(tmp.path().join("blocker"), b"").unwrap();
        let dir = tmp.path().join("blocker").join("secrets");
        let m = Mobile::open(&dir);
        let Err(e) = &m.parts else {
            panic!("mobile access stayed available");
        };
        assert!(e.starts_with("mobile access is unavailable"), "{e}");
        assert!(m.relay.is_none());
        assert!(m.token_set().is_empty());

        // A healthy dir is created and holds the key.
        let ok = Mobile::open(&tmp.path().join("secrets"));
        assert!(ok.parts.is_ok());
        assert!(tmp.path().join("secrets").join(identity::FILE).is_file());
    }

    #[test]
    fn device_ids_are_reserved_and_local_ones_are_not() {
        let id = device_id(&[0xab; 8]);
        assert_eq!(id, "d-abababababababab");
        assert!(is_device_id(&id));
        for local in ["gui-1a2b", "cli-1a2b", "supervisor-1a2b", "D-1", "xd-1"] {
            assert!(!is_device_id(local), "{local}");
        }
    }

    #[test]
    fn a_device_name_is_trimmed_bounded_and_displays_as_stored() {
        assert_eq!(valid_name("  Pixel 8 ").as_deref(), Ok("Pixel 8"));
        assert_eq!(valid_name("Zoë’s phone").as_deref(), Ok("Zoë’s phone"));
        assert!(valid_name("   ").is_err());
        assert!(valid_name(&"x".repeat(MAX_NAME_CHARS + 1)).is_err());
        assert!(valid_name(&"é".repeat(MAX_NAME_CHARS)).is_ok());
        for bad in [
            "Pixel\u{1b}[2J",
            "Pixel\nApprove",
            "Pixel\u{202e}enohp",
            "Pi\u{200b}xel",
            "Pixel\u{2066}x\u{2069}",
            "Pixel\u{feff}",
        ] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
    }
}
