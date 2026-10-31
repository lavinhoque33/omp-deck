//! Web Push to the paired phones (`docs/notes/mobile-pwa.md`, "Web Push"): the gateway's VAPID
//! key (`push.key` in the secrets dir, next to `remote.key`), each device's subscription
//! (`Device.push` in `devices.json`), the endpoint guard ([`ssrf`]) and one sender thread
//! ([`sender`]).
//!
//! The notification center offers every canonical event ([`MobileKind`]); [`Push::offer`] fans it
//! out to the subscribed devices whose prefs want it and that are not looking at the app right
//! now. The payload is the declarative Web Push JSON, generic unless the device opted into
//! titles. A subscription the push service reports gone (404/410) is deleted.
//!
//! Test override: `OMP_DECK_PUSH_TEST_LOOPBACK=1` admits `http://` and loopback endpoints, for
//! verification against a local receiver only. `OMP_DECK_PUSH_CONTACT` sets the VAPID `sub`
//! (an e-mail address or `mailto:`/`https:` URL; default: the PWA's origin).

pub mod sender;
pub mod ssrf;
pub mod vapid;

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use deck_remote::api::{PushPrefs, PushStateView};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use ureq::http::Uri;
use web_push_native::{Auth, p256};

use crate::gateway::remote::RemoteError;
use crate::gateway::remote::registry::{Device, Registry};
use crate::notifications::{MobileKind, Wake};
use vapid::VapidKey;

/// The VAPID key file in the gateway's secrets dir.
pub const KEY_FILE: &str = "push.key";
/// Verification only (`test-push-loopback` builds): admit `http://` and loopback endpoints and any port.
pub const TEST_LOOPBACK_ENV: &str = "OMP_DECK_PUSH_TEST_LOOPBACK";
/// The VAPID contact (`sub`).
pub const CONTACT_ENV: &str = "OMP_DECK_PUSH_CONTACT";
/// Largest payload sent (the protocol's limit is 3993 bytes of plaintext).
pub const MAX_PAYLOAD: usize = 1024;
/// A session title in a push is cut to this many characters.
pub const TITLE_CHARS: usize = 80;
/// How long a push service keeps an undelivered message (`TTL`).
pub const TTL: Duration = Duration::from_secs(3600);
const MAX_ENDPOINT: usize = 2048;
const MAX_KEY_TEXT: usize = 256;

/// One device's Web Push subscription, as stored in `devices.json` (keys base64url).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushSub {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub prefs: PushPrefs,
    pub since_unix_ms: u64,
}

/// Why a push operation was refused.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    /// The subscription is malformed (the text says what).
    #[error("{0}")]
    Invalid(String),
    /// The endpoint resolves to no public internet address.
    #[error("the push endpoint does not resolve to a public internet address")]
    Blocked,
    #[error("this device has no push subscription")]
    NotSubscribed,
    #[error("no such device")]
    NoDevice,
    /// `devices.json` could not be written.
    #[error(transparent)]
    Store(#[from] RemoteError),
}

/// Which endpoints may be pushed to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// `http://` and loopback endpoints too ([`TEST_LOOPBACK_ENV`]).
    pub allow_loopback: bool,
}

impl Policy {
    /// [`TEST_LOOPBACK_ENV`] is honoured only in builds with the `test-push-loopback` feature
    /// (tests enable it through a self dev-dependency); release artifacts ignore it.
    pub fn from_env() -> Policy {
        Policy::from_parts(
            cfg!(feature = "test-push-loopback"),
            std::env::var_os(TEST_LOOPBACK_ENV).as_deref(),
        )
    }

    /// The policy for `feature_enabled` and the override variable's value.
    fn from_parts(feature_enabled: bool, var: Option<&std::ffi::OsStr>) -> Policy {
        Policy {
            allow_loopback: feature_enabled && var.is_some_and(|v| v == "1"),
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::Policy;
    use std::ffi::OsStr;

    #[test]
    fn loopback_override_needs_the_feature_and_the_variable() {
        let on = Some(OsStr::new("1"));
        // A build without the feature (every release artifact) ignores the variable.
        assert!(!Policy::from_parts(false, on).allow_loopback);
        assert!(!Policy::from_parts(false, None).allow_loopback);
        assert!(!Policy::from_parts(true, None).allow_loopback);
        assert!(!Policy::from_parts(true, Some(OsStr::new("0"))).allow_loopback);
        assert!(Policy::from_parts(true, on).allow_loopback);
    }
}

/// A subscription parsed for sending.
pub(crate) struct Target {
    pub(crate) uri: Uri,
    pub(crate) key: p256::PublicKey,
    pub(crate) auth: Auth,
}

/// base64url with or without padding (browsers send it unpadded).
const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::URL_SAFE,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

fn invalid(why: &str) -> PushError {
    PushError::Invalid(format!("push subscription: {why}"))
}

/// Parses a subscription by hand (never through the crate's serde, which panics on a bad
/// `auth` length): an `https` endpoint with a host, a SEC1 P-256 point, an `auth` of exactly
/// 16 bytes. No network access.
pub(crate) fn parse(
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    policy: Policy,
) -> Result<Target, PushError> {
    if endpoint.len() > MAX_ENDPOINT {
        return Err(invalid("the endpoint is too long"));
    }
    let uri: Uri = endpoint
        .parse()
        .map_err(|_| invalid("the endpoint is not a URL"))?;
    match uri.scheme_str() {
        Some("https") => {}
        Some("http") if policy.allow_loopback => {}
        _ => return Err(invalid("the endpoint must be an https:// URL")),
    }
    let authority = uri
        .authority()
        .ok_or_else(|| invalid("the endpoint has no host"))?;
    if !policy.allow_loopback && uri.port_u16().is_some_and(|p| p != 443) {
        return Err(invalid("the endpoint must use port 443"));
    }
    if authority.as_str().contains('@') || authority.host().is_empty() {
        return Err(invalid("the endpoint has no valid host"));
    }
    let decode = |text: &str, what: &str| {
        if text.len() > MAX_KEY_TEXT {
            return Err(invalid(&format!("{what} is too long")));
        }
        B64.decode(text.as_bytes())
            .map_err(|_| invalid(&format!("{what} is not base64url")))
    };
    let key = p256::PublicKey::from_sec1_bytes(&decode(p256dh, "p256dh")?)
        .map_err(|_| invalid("p256dh is not a P-256 public key"))?;
    let auth = decode(auth, "auth")?;
    if auth.len() != 16 {
        return Err(invalid("auth must be 16 bytes"));
    }
    Ok(Target {
        uri,
        key,
        auth: Auth::clone_from_slice(&auth),
    })
}

/// Validates a subscription a device sent, including that its endpoint resolves to a public
/// address. **Blocks** (DNS, up to 5 s): run it on a worker, then store the result with
/// [`subscribe`] on the engine thread. Its prefs are the defaults; `subscribe` keeps a
/// resubscribing device's own.
pub fn check_subscription(endpoint: &str, p256dh: &str, auth: &str) -> Result<PushSub, PushError> {
    check_with(endpoint, p256dh, auth, Policy::from_env())
}

fn check_with(
    endpoint: &str,
    p256dh: &str,
    auth: &str,
    policy: Policy,
) -> Result<PushSub, PushError> {
    let target = parse(endpoint, p256dh, auth, policy)?;
    ssrf::check_host(&target.uri, policy)?;
    Ok(PushSub {
        endpoint: endpoint.to_owned(),
        p256dh: URL_SAFE_NO_PAD.encode(target.key.to_sec1_bytes()),
        auth: URL_SAFE_NO_PAD.encode(target.auth),
        prefs: PushPrefs::default(),
        since_unix_ms: 0,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Stores `sub` (from [`check_subscription`]) as device `id`'s subscription. A device that
/// resubscribes keeps its prefs; its `since` stays while the endpoint is the same.
pub fn subscribe(registry: &mut Registry, id: &str, sub: PushSub) -> Result<(), PushError> {
    let old = registry.get(id).ok_or(PushError::NoDevice)?.push.clone();
    let mut sub = sub;
    if let Some(old) = old {
        sub.prefs = old.prefs;
        sub.since_unix_ms = if old.endpoint == sub.endpoint {
            old.since_unix_ms
        } else {
            now_ms()
        };
    } else {
        sub.since_unix_ms = now_ms();
    }
    registry.update(id, |d| d.push = Some(sub))?;
    Ok(())
}

/// Deletes device `id`'s subscription (none: nothing to do).
pub fn unsubscribe(registry: &mut Registry, id: &str) -> Result<(), PushError> {
    let device = registry.get(id).ok_or(PushError::NoDevice)?;
    if device.push.is_some() {
        registry.update(id, |d| d.push = None)?;
    }
    Ok(())
}

/// Sets which events device `id` is pushed.
pub fn set_prefs(registry: &mut Registry, id: &str, prefs: PushPrefs) -> Result<(), PushError> {
    let device = registry.get(id).ok_or(PushError::NoDevice)?;
    match &device.push {
        None => Err(PushError::NotSubscribed),
        Some(s) if s.prefs == prefs => Ok(()),
        Some(_) => {
            registry.update(id, |d| {
                if let Some(s) = &mut d.push {
                    s.prefs = prefs;
                }
            })?;
            Ok(())
        }
    }
}

/// The device feed's push state.
pub fn state_view(device: &Device) -> PushStateView {
    match &device.push {
        Some(s) => PushStateView {
            subscribed: true,
            prefs: s.prefs,
            since_unix_ms: Some(s.since_unix_ms),
        },
        None => PushStateView {
            subscribed: false,
            prefs: PushPrefs::default(),
            since_unix_ms: None,
        },
    }
}

/// The push service reported a subscription gone (404/410).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gone {
    pub device: String,
    pub endpoint: String,
}

/// Deletes the subscription [`Gone`] names, unless the device subscribed anew since. Returns
/// whether one was deleted.
pub fn forget(registry: &mut Registry, gone: &Gone) -> Result<bool, PushError> {
    let current = registry
        .get(&gone.device)
        .and_then(|d| d.push.as_ref())
        .is_some_and(|s| s.endpoint == gone.endpoint);
    if current {
        registry.update(&gone.device, |d| d.push = None)?;
    }
    Ok(current)
}

// ---- payload ------------------------------------------------------------------------------------

/// `Topic`: 22 base64url characters of SHA-256(`seed`), so pending pushes coalesce per session.
pub fn topic(seed: &str) -> String {
    let mut t = URL_SAFE_NO_PAD.encode(Sha256::digest(seed.as_bytes()));
    t.truncate(22);
    t
}

fn wants(prefs: &PushPrefs, kind: MobileKind) -> bool {
    match kind {
        MobileKind::TurnFinished => prefs.turn_finished,
        MobileKind::Attention => prefs.attention,
        MobileKind::Ended => prefs.ended,
    }
}

fn cut(s: &str, max: usize) -> &str {
    s.char_indices().nth(max).map_or(s, |(i, _)| &s[..i])
}

/// The notification body: generic, or naming the session when `show_titles`.
fn body(kind: MobileKind, title: Option<&str>) -> String {
    match (kind, title.map(str::trim).filter(|t| !t.is_empty())) {
        (MobileKind::TurnFinished, None) => "A session finished".to_owned(),
        (MobileKind::Attention, None) => "A session needs your attention".to_owned(),
        (MobileKind::Ended, None) => "A session ended".to_owned(),
        (MobileKind::TurnFinished, Some(t)) => format!("{} finished", cut(t, TITLE_CHARS)),
        (MobileKind::Attention, Some(t)) => {
            format!("{} needs your attention", cut(t, TITLE_CHARS))
        }
        (MobileKind::Ended, Some(t)) => format!("{} ended", cut(t, TITLE_CHARS)),
    }
}

/// The declarative Web Push JSON (at most [`MAX_PAYLOAD`] bytes; a titled body that does not
/// fit falls back to the generic one). `navigate` opens the session in the PWA.
pub fn payload(
    kind: MobileKind,
    title: Option<&str>,
    origin: &str,
    session: Option<&str>,
    tag: &str,
) -> Option<Vec<u8>> {
    let origin = origin.trim_end_matches('/');
    let navigate = match session {
        Some(id) => format!("{origin}/#s={id}"),
        None => format!("{origin}/"),
    };
    let encode = |body: String| {
        serde_json::to_vec(&serde_json::json!({
            "web_push": 8030,
            "notification": {
                "title": "OmpDeck",
                "body": body,
                "navigate": navigate,
                "tag": tag,
                "silent": false,
            }
        }))
        .ok()
        .filter(|p| p.len() <= MAX_PAYLOAD)
    };
    encode(body(kind, title)).or_else(|| encode(body(kind, None)))
}

/// One canonical event, as the engine offers it.
#[derive(Clone, Copy, Debug)]
pub struct Offer<'a> {
    pub kind: MobileKind,
    /// The session's label.
    pub title: &'a str,
    /// The id the PWA routes by (`None`: the app opens at its start page).
    pub session: Option<&'a str>,
    /// Coalescing seed when there is no session id (the live session's key).
    pub fallback: &'a str,
    /// The PWA's origin (the relay's).
    pub origin: &'a str,
}

// ---- engine handle ------------------------------------------------------------------------------

/// The engine's push state: the VAPID key and the sender (started at the first push).
pub struct Push {
    key: Option<Arc<VapidKey>>,
    policy: Policy,
    contact: Option<String>,
    sender: Option<sender::Sender>,
    gone: Arc<Mutex<Vec<Gone>>>,
    wake: Wake,
}

/// `OMP_DECK_PUSH_CONTACT` as a VAPID subject: `mailto:`/`https:` kept, a bare address made
/// `mailto:`.
fn contact_from(text: &str) -> Option<String> {
    let text = text.trim();
    if text.starts_with("mailto:") || text.starts_with("https://") {
        Some(text.to_owned())
    } else if text.contains('@') && !text.contains(char::is_whitespace) {
        Some(format!("mailto:{text}"))
    } else {
        None
    }
}

impl Push {
    /// Loads (or creates) `push.key` in the secrets `dir`. Without it push is off (logged); the
    /// gateway runs on.
    pub fn open(dir: &Path, wake: Wake) -> Push {
        let key = crate::gateway::secret_dir(dir)
            .map_err(|e| format!("protecting {}: {e}", dir.display()))
            .and_then(|()| vapid::load_or_create(&dir.join(KEY_FILE)).map_err(|e| e.to_string()));
        let key = key
            .map_err(|e| log::error!("Web Push is unavailable: {e}"))
            .ok();
        let contact = std::env::var(CONTACT_ENV).ok().and_then(|c| {
            let parsed = contact_from(&c);
            if parsed.is_none() {
                log::warn!("ignoring {CONTACT_ENV}: not an e-mail address or https URL");
            }
            parsed
        });
        Push::with_key(key, Policy::from_env(), contact, wake)
    }

    pub fn with_key(
        key: Option<VapidKey>,
        policy: Policy,
        contact: Option<String>,
        wake: Wake,
    ) -> Push {
        Push {
            key: key.map(Arc::new),
            policy,
            contact,
            sender: None,
            gone: Arc::new(Mutex::new(Vec::new())),
            wake,
        }
    }

    /// `Welcome.push_key` (empty: push is unavailable).
    pub fn public_key(&self) -> String {
        self.key
            .as_ref()
            .map_or_else(String::new, |k| k.public_key().to_owned())
    }

    /// Queues the push of `offer` to every subscribed device that wants its kind and is not
    /// `present` (a live connection reports the app visible).
    pub fn offer(&mut self, devices: &[Device], present: impl Fn(&str) -> bool, offer: &Offer<'_>) {
        let Some(key) = &self.key else {
            return;
        };
        let topic = topic(offer.session.unwrap_or(offer.fallback));
        let contact = self
            .contact
            .clone()
            .unwrap_or_else(|| offer.origin.to_owned());
        for d in devices {
            let Some(sub) = &d.push else {
                continue;
            };
            if !wants(&sub.prefs, offer.kind) || present(&d.id) {
                continue;
            }
            let title = sub.prefs.show_titles.then_some(offer.title);
            let Some(payload) = payload(offer.kind, title, offer.origin, offer.session, &topic)
            else {
                log::warn!("push to {}: the payload does not fit", d.id);
                continue;
            };
            if self.sender.is_none() {
                let gone = self.gone.clone();
                let wake = self.wake.clone();
                let report = move |g: Gone| {
                    lock(&gone).push(g);
                    wake();
                };
                match sender::Sender::start(key.clone(), self.policy, Arc::new(report)) {
                    Ok(s) => self.sender = Some(s),
                    Err(e) => {
                        log::error!("cannot start the push sender: {e}");
                        return;
                    }
                }
            }
            if let Some(s) = &self.sender {
                s.send(sender::Job {
                    device: d.id.clone(),
                    endpoint: sub.endpoint.clone(),
                    p256dh: sub.p256dh.clone(),
                    auth: sub.auth.clone(),
                    payload,
                    urgency: if offer.kind == MobileKind::Attention {
                        "high"
                    } else {
                        "normal"
                    },
                    topic: topic.clone(),
                    contact: contact.clone(),
                });
            }
        }
    }

    /// Drops the pushes still queued for device `id` (revoked or unsubscribed).
    pub fn purge(&self, id: &str) {
        if let Some(s) = &self.sender {
            s.purge(id);
        }
    }

    /// Subscriptions the push services reported gone since the last call.
    pub fn take_gone(&self) -> Vec<Gone> {
        std::mem::take(&mut *lock(&self.gone))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
