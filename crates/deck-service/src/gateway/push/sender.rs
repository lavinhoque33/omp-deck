//! The Web Push sender thread: encryption and VAPID signing per request, and the push services'
//! answers (`docs/notes/mobile-pwa.md`, "Delivery"); the bounded queue is [`super::queue`].
//! Endpoints are capability URLs: logs name the device, never the endpoint.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ureq::http::{HeaderValue, Uri, header};
use ureq::unversioned::transport::DefaultConnector;
use web_push_native::WebPushBuilder;

use super::queue::{self, Outcome, Routed, retry_after};
use super::ssrf::{PublicResolver, Refused};
use super::vapid::VapidKey;
use super::{Gone, Policy, TTL, parse};

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

impl Routed for Job {
    fn device(&self) -> &str {
        &self.device
    }
}

pub type Report = Arc<dyn Fn(Gone) + Send + Sync>;

/// The handle; dropping it stops the thread after its current request.
pub type Sender = queue::Handle<Job>;

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
        queue::Handle::spawn(
            "gateway-push",
            Web {
                agent: agent(policy),
                key,
                policy,
                report,
                backoff,
                jwts: HashMap::new(),
            },
        )
    }
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

struct Web {
    agent: ureq::Agent,
    key: Arc<VapidKey>,
    policy: Policy,
    report: Report,
    backoff: Duration,
    /// (audience, contact) → the signed header and when it was signed.
    jwts: HashMap<(String, String), (HeaderValue, Instant)>,
}

impl queue::Worker<Job> for Web {
    fn deliver(&mut self, job: &Job, attempt: u8) -> Outcome {
        self.try_once(job, attempt)
    }

    fn gone(&mut self, job: Job) {
        log::info!(
            "push to {}: the subscription is gone; deleting it",
            job.device
        );
        (self.report)(Gone {
            device: job.device,
            endpoint: job.endpoint,
        });
    }
}

impl Web {
    /// One try; `attempt` counts the earlier ones (the backoff grows ×4 with each).
    fn try_once(&mut self, job: &Job, attempt: u8) -> Outcome {
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
