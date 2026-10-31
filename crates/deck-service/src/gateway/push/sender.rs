//! The one push sender thread: a bounded queue (overflow drops the oldest), encryption and VAPID
//! signing per request, and the push services' answers (`docs/notes/mobile-pwa.md`, "Delivery").
//! Endpoints are capability URLs: logs name the device, never the endpoint.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use ureq::http::{HeaderValue, Uri, header};
use ureq::unversioned::transport::DefaultConnector;
use web_push_native::WebPushBuilder;

use super::ssrf::{PublicResolver, Refused};
use super::vapid::VapidKey;
use super::{Gone, Policy, TTL, lock, parse};

/// Pushes waiting at most (the oldest is dropped beyond).
pub const QUEUE: usize = 256;
/// Pushes waiting for one device at most (that device's oldest is dropped beyond), so a flood
/// for one device cannot evict the others' pushes.
pub const PER_DEVICE: usize = 16;
/// Tries per push (the first one included) for 429 and 5xx answers and transport failures.
pub const MAX_ATTEMPTS: u8 = 3;
/// The longest `Retry-After` honoured.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);
/// A 429 without a usable `Retry-After` waits this long.
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);
/// First backoff after a 5xx or a transport failure (then ×4).
const BACKOFF: Duration = Duration::from_secs(5);
/// A VAPID JWT is reused this long per push service origin (Apple: at most one refresh an hour).
const JWT_REUSE: Duration = Duration::from_secs(12 * 3600);
/// A JWT's lifetime: past [`JWT_REUSE`], never more than the 24 h RFC 8292 allows.
const JWT_VALID: Duration = Duration::from_secs(20 * 3600);
const JWT_CACHE: usize = 64;
/// A push service that does not accept the connection in this long is given up on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// A whole request (connect, send, response) may take this long: the one sender thread must
/// not be held by a slow endpoint.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One push to one device.
#[derive(Clone, Debug)]
pub struct Job {
    pub device: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    /// The plaintext JSON.
    pub payload: Vec<u8>,
    pub urgency: &'static str,
    pub topic: String,
    /// The VAPID `sub`.
    pub contact: String,
}

struct Queued {
    job: Job,
    attempts: u8,
    due: Instant,
    /// The device's invalidation generation when the push was submitted.
    generation: u64,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Queued>,
    /// Per device: how often it was purged. A push older than its device's generation is stale,
    /// including one whose attempt is in flight (it is outside `queue`).
    generations: HashMap<String, u64>,
    closed: bool,
}

impl State {
    fn generation(&self, device: &str) -> u64 {
        self.generations.get(device).copied().unwrap_or(0)
    }

    fn is_stale(&self, item: &Queued) -> bool {
        item.generation != self.generation(&item.job.device)
    }

    fn purge(&mut self, device: &str) {
        *self.generations.entry(device.to_owned()).or_insert(0) += 1;
        purge(&mut self.queue, device);
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

pub type Report = Arc<dyn Fn(Gone) + Send + Sync>;

/// The handle; dropping it stops the thread after its current request.
pub struct Sender {
    shared: Arc<Shared>,
}

impl Sender {
    pub fn start(key: Arc<VapidKey>, policy: Policy, report: Report) -> io::Result<Sender> {
        Sender::start_with(key, policy, report, BACKOFF)
    }

    pub(crate) fn start_with(
        key: Arc<VapidKey>,
        policy: Policy,
        report: Report,
        backoff: Duration,
    ) -> io::Result<Sender> {
        let shared = Arc::new(Shared::default());
        let worker = Worker {
            agent: agent(policy),
            key,
            policy,
            report,
            backoff,
            jwts: HashMap::new(),
        };
        let theirs = shared.clone();
        std::thread::Builder::new()
            .name("gateway-push".to_owned())
            .spawn(move || worker.run(&theirs))?;
        Ok(Sender { shared })
    }

    pub fn send(&self, job: Job) {
        let mut st = lock(&self.shared.state);
        let generation = st.generation(&job.device);
        enqueue(
            &mut st.queue,
            Queued {
                job,
                attempts: 0,
                due: Instant::now(),
                generation,
            },
        );
        drop(st);
        self.shared.ready.notify_one();
    }

    /// Drops everything still queued for `device` (revoked, or unsubscribed) and makes any
    /// attempt in flight for it final: it is not retried.
    pub fn purge(&self, device: &str) {
        lock(&self.shared.state).purge(device);
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        lock(&self.shared.state).closed = true;
        self.shared.ready.notify_one();
    }
}

fn purge(queue: &mut VecDeque<Queued>, device: &str) {
    queue.retain(|q| q.job.device != device);
}

fn enqueue(queue: &mut VecDeque<Queued>, item: Queued) {
    if queue
        .iter()
        .filter(|q| q.job.device == item.job.device)
        .count()
        >= PER_DEVICE
        && let Some(i) = queue.iter().position(|q| q.job.device == item.job.device)
    {
        queue.remove(i);
        log::warn!(
            "push queue full for {}: dropped its oldest push",
            item.job.device
        );
    }
    if queue.len() >= QUEUE
        && let Some(old) = queue.pop_front()
    {
        log::warn!(
            "push queue full: dropped the oldest push (to {})",
            old.job.device
        );
    }
    queue.push_back(item);
}

/// The HTTP client: status codes are answers, no proxy, no redirect, and only allowed
/// addresses are connected to.
fn agent(policy: Policy) -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .proxy(None)
        .max_redirects(0)
        .https_only(!policy.allow_loopback)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(REQUEST_TIMEOUT))
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::new(), PublicResolver { policy })
}

enum Outcome {
    Sent,
    Gone,
    Retry(Duration),
    Failed,
}

struct Worker {
    agent: ureq::Agent,
    key: Arc<VapidKey>,
    policy: Policy,
    report: Report,
    backoff: Duration,
    /// (audience, contact) → the signed header and when it was signed.
    jwts: HashMap<(String, String), (HeaderValue, Instant)>,
}

impl Worker {
    fn run(mut self, shared: &Shared) {
        while let Some(mut item) = next(shared) {
            if lock(&shared.state).is_stale(&item) {
                continue;
            }
            match self.deliver(&item.job, item.attempts) {
                Outcome::Sent | Outcome::Failed => {}
                Outcome::Gone => {
                    log::info!(
                        "push to {}: the subscription is gone; deleting it",
                        item.job.device
                    );
                    (self.report)(Gone {
                        device: item.job.device,
                        endpoint: item.job.endpoint,
                    });
                }
                Outcome::Retry(wait) => {
                    item.attempts += 1;
                    if item.attempts >= MAX_ATTEMPTS {
                        log::warn!(
                            "push to {}: dropped after {MAX_ATTEMPTS} attempts",
                            item.job.device
                        );
                        continue;
                    }
                    item.due = Instant::now() + wait;
                    let mut st = lock(&shared.state);
                    if st.is_stale(&item) {
                        continue;
                    }
                    enqueue(&mut st.queue, item);
                }
            }
        }
    }

    /// One try; `attempt` counts the earlier ones (the backoff grows ×4 with each).
    fn deliver(&mut self, job: &Job, attempt: u8) -> Outcome {
        let backoff = self.backoff * 4u32.pow(u32::from(attempt));
        let target = match parse(&job.endpoint, &job.p256dh, &job.auth, self.policy) {
            Ok(t) => t,
            Err(e) => {
                log::warn!("push to {}: {e}", job.device);
                return Outcome::Failed;
            }
        };
        let Some(authorization) = self.vapid(&target.uri, &job.contact) else {
            log::warn!("push to {}: cannot sign the VAPID token", job.device);
            return Outcome::Failed;
        };
        let request = WebPushBuilder::new(target.uri, target.key, target.auth)
            .with_valid_duration(TTL)
            .build(job.payload.as_slice());
        let mut request = match request {
            Ok(r) => r,
            Err(e) => {
                log::warn!("push to {}: cannot encrypt: {e}", job.device);
                return Outcome::Failed;
            }
        };
        let headers = request.headers_mut();
        headers.insert(header::AUTHORIZATION, authorization);
        headers.insert("urgency", HeaderValue::from_static(job.urgency));
        match HeaderValue::from_str(&job.topic) {
            Ok(v) => {
                headers.insert("topic", v);
            }
            Err(_) => log::warn!("push to {}: unusable topic", job.device),
        }
        match self.agent.run(request) {
            Ok(response) => match response.status().as_u16() {
                200..=299 => Outcome::Sent,
                404 | 410 => Outcome::Gone,
                429 => Outcome::Retry(retry_after(response.headers().get("retry-after"))),
                s @ 500..=599 => {
                    log::warn!("push to {}: the push service answered {s}", job.device);
                    Outcome::Retry(backoff)
                }
                s => {
                    log::warn!("push to {}: the push service answered {s}", job.device);
                    Outcome::Failed
                }
            },
            Err(ureq::Error::Other(e)) if e.is::<Refused>() => {
                log::warn!(
                    "push to {}: skipped, the endpoint resolves to no public address",
                    job.device
                );
                Outcome::Failed
            }
            Err(e @ (ureq::Error::TooManyRedirects | ureq::Error::RequireHttpsOnly(_))) => {
                log::warn!("push to {}: {e}", job.device);
                Outcome::Failed
            }
            Err(e) => {
                log::warn!("push to {}: {e}", job.device);
                Outcome::Retry(backoff)
            }
        }
    }

    /// The `Authorization` header for `uri`'s push service, signed at most every
    /// [`JWT_REUSE`]. The audience is the service's origin.
    fn vapid(&mut self, uri: &Uri, contact: &str) -> Option<HeaderValue> {
        let scheme = uri.scheme_str()?;
        let audience = match uri.port_u16() {
            Some(port) if !matches!((scheme, port), ("https", 443) | ("http", 80)) => {
                format!("{scheme}://{}:{port}", uri.host()?)
            }
            _ => format!("{scheme}://{}", uri.host()?),
        };
        let key = (audience, contact.to_owned());
        let now = Instant::now();
        if let Some((value, at)) = self.jwts.get(&key)
            && now.duration_since(*at) < JWT_REUSE
        {
            return Some(value.clone());
        }
        let value =
            HeaderValue::from_str(&self.key.authorization(&key.0, contact, JWT_VALID)?).ok()?;
        if self.jwts.len() >= JWT_CACHE {
            self.jwts
                .retain(|_, (_, at)| now.duration_since(*at) < JWT_REUSE);
            if self.jwts.len() >= JWT_CACHE {
                self.jwts.clear();
            }
        }
        self.jwts.insert(key, (value.clone(), now));
        Some(value)
    }
}

/// A `Retry-After` in seconds, capped at [`MAX_RETRY_AFTER`].
fn retry_after(value: Option<&HeaderValue>) -> Duration {
    value
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map_or(DEFAULT_RETRY_AFTER, Duration::from_secs)
        .min(MAX_RETRY_AFTER)
}

/// The next due push; `None` once the sender is dropped.
fn next(shared: &Shared) -> Option<Queued> {
    let mut st = lock(&shared.state);
    loop {
        if st.closed {
            return None;
        }
        let now = Instant::now();
        let first = st
            .queue
            .iter()
            .enumerate()
            .min_by_key(|(_, q)| q.due)
            .map(|(i, q)| (i, q.due));
        st = match first {
            Some((i, due)) if due <= now => return st.queue.remove(i),
            Some((_, due)) => {
                shared
                    .ready
                    .wait_timeout(st, due - now)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            }
            None => shared
                .ready
                .wait(st)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queued(device: &str) -> Queued {
        Queued {
            job: Job {
                device: device.to_owned(),
                endpoint: String::new(),
                p256dh: String::new(),
                auth: String::new(),
                payload: Vec::new(),
                urgency: "normal",
                topic: String::new(),
                contact: String::new(),
            },
            attempts: 0,
            due: Instant::now(),
            generation: 0,
        }
    }

    #[test]
    fn a_full_queue_drops_its_oldest_push() {
        let mut q = VecDeque::new();
        for i in 0..QUEUE + 2 {
            enqueue(&mut q, queued(&i.to_string()));
        }
        assert_eq!(q.len(), QUEUE);
        assert_eq!(q.front().unwrap().job.device, "2");
        assert_eq!(q.back().unwrap().job.device, (QUEUE + 1).to_string());
    }

    #[test]
    fn one_devices_flood_keeps_to_its_own_cap() {
        let mut q = VecDeque::new();
        enqueue(&mut q, queued("other"));
        for _ in 0..PER_DEVICE * 3 {
            enqueue(&mut q, queued("flood"));
        }
        assert_eq!(
            q.iter().filter(|x| x.job.device == "flood").count(),
            PER_DEVICE
        );
        assert!(q.iter().any(|x| x.job.device == "other"));
    }

    #[test]
    fn purging_a_device_drops_only_its_pushes() {
        let mut q = VecDeque::new();
        for d in ["a", "b", "a"] {
            enqueue(&mut q, queued(d));
        }
        purge(&mut q, "a");
        assert_eq!(q.len(), 1);
        assert_eq!(q.front().unwrap().job.device, "b");
    }

    #[test]
    fn a_purge_makes_an_in_flight_push_stale() {
        let mut st = State::default();
        let mut item = queued("a");
        item.generation = st.generation("a");
        assert!(!st.is_stale(&item));
        // The item is "in flight": outside the queue when the purge happens.
        st.purge("a");
        assert!(st.is_stale(&item));
        assert!(!st.is_stale(&queued("b")));
        let mut fresh = queued("a");
        fresh.generation = st.generation("a");
        assert!(!st.is_stale(&fresh));
    }

    #[test]
    fn retry_after_is_honoured_up_to_ten_minutes() {
        let v = |s: &'static str| HeaderValue::from_static(s);
        assert_eq!(retry_after(Some(&v("30"))), Duration::from_secs(30));
        assert_eq!(retry_after(Some(&v("86400"))), MAX_RETRY_AFTER);
        assert_eq!(retry_after(Some(&v("soon"))), DEFAULT_RETRY_AFTER);
        assert_eq!(retry_after(None), DEFAULT_RETRY_AFTER);
    }
}
