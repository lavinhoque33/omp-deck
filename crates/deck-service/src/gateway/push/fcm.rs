//! Native Android push: Firebase Cloud Messaging HTTP v1, sent straight from the gateway.
//!
//! The gateway's Google service account (`fcm-service-account.json` in the secrets dir, or the
//! file [`CREDENTIALS_ENV`] names) is read and validated once at startup ([`load`]); nothing on
//! the engine thread touches the disk or the network per notification. The sender thread signs
//! an RS256 OAuth assertion (ring), exchanges it at the fixed Google token endpoint for an access
//! token (cached until shortly before it expires) and posts one message per notification to the
//! fixed `fcm.googleapis.com` endpoint of the validated project. The account's own `token_uri`
//! is ignored, so a doctored file cannot make the gateway call anywhere else; redirects are
//! refused and every response is read with a size bound.
//!
//! Google only relays an opaque envelope: `{v, desktop, nonce, ciphertext}`. The ciphertext is
//! the Web Push JSON (`web_push`, `notification`) plus `expires_unix_ms`, sealed with AES-256-GCM
//! under the key the phone made for this desktop (AAD `ompdeck-android-push-v1:<desktop>`, a
//! fresh random nonce per attempt). Titles, session text and routes never reach Google.
//!
//! Tokens, keys, payloads and request URLs are never logged: logs name the device and Google's
//! error status.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom as _, SystemRandom};
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use ureq::http::{HeaderValue, Request, header};
use zeroize::Zeroizing;

use super::now_ms;
use super::queue::{self, Outcome, Routed, retry_after};

/// The service account file in the gateway's secrets dir.
pub const CREDENTIALS_FILE: &str = "fcm-service-account.json";
/// The path of a service account file elsewhere (takes precedence over [`CREDENTIALS_FILE`]).
pub const CREDENTIALS_ENV: &str = "OMP_DECK_FCM_CREDENTIALS";
/// The one OAuth endpoint the assertion is sent to.
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/firebase.messaging";
/// The envelope version (`data.v`).
pub const ENVELOPE_VERSION: &str = "1";
/// The AAD of every ciphertext, followed by the desktop id.
pub const AAD_PREFIX: &str = "ompdeck-android-push-v1:";
/// How long Google keeps an undelivered message (`android.ttl`) and how long a message is valid
/// (`expires_unix_ms`, inside the ciphertext).
pub const TTL: Duration = Duration::from_secs(3600);
/// The largest service account file read.
const MAX_ACCOUNT_FILE: u64 = 64 * 1024;
/// The largest answer read from Google.
const MAX_RESPONSE: u64 = 64 * 1024;
/// An access token is renewed this long before it expires.
const TOKEN_MARGIN: Duration = Duration::from_secs(120);
/// The lifetime of an OAuth assertion (Google's maximum).
const ASSERTION_VALID: u64 = 3600;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// First backoff after a 5xx or a transport failure (then ×4).
const BACKOFF: Duration = Duration::from_secs(5);
/// A bad-account error is logged at most this often.
const ACCOUNT_LOG_EVERY: Duration = Duration::from_secs(600);

// ---- credentials -----------------------------------------------------------------------------

/// A validated service account: the project to send for and the key that proves it.
pub struct Credentials {
    project: String,
    email: String,
    key: RsaKeyPair,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("project", &self.project)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct AccountFile {
    #[serde(rename = "type")]
    kind: Option<String>,
    project_id: Option<String>,
    client_email: Option<String>,
    private_key: Option<String>,
}

/// A GCP project id: 6–30 lowercase letters, digits and hyphens, starting with a letter and not
/// ending with a hyphen. (Legacy domain-scoped ids are not accepted: the id is part of a URL.)
fn valid_project(id: &str) -> bool {
    let b = id.as_bytes();
    (6..=30).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[b.len() - 1] != b'-'
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
}

fn valid_email(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    email.len() <= 256
        && !local.is_empty()
        && !domain.is_empty()
        && local
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-'))
        && domain
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
}

/// The DER of a PKCS#8 `-----BEGIN PRIVATE KEY-----` block.
fn pem_to_der(pem: &str) -> Option<Zeroizing<Vec<u8>>> {
    let body = pem
        .trim()
        .strip_prefix("-----BEGIN PRIVATE KEY-----")?
        .strip_suffix("-----END PRIVATE KEY-----")?;
    let compact: Zeroizing<String> =
        Zeroizing::new(body.chars().filter(|c| !c.is_whitespace()).collect());
    STANDARD.decode(compact.as_bytes()).ok().map(Zeroizing::new)
}

impl Credentials {
    /// Parses the text of a service account file. Only `project_id`, `client_email` and
    /// `private_key` are used.
    pub fn parse(bytes: &[u8]) -> Result<Credentials, String> {
        let mut file: AccountFile = serde_json::from_slice(bytes)
            .map_err(|_| "it is not a JSON service account file".to_owned())?;
        let key = file.private_key.take().map(Zeroizing::new);
        if file.kind.as_deref() != Some("service_account") {
            return Err("it is not a service account key (\"type\": \"service_account\")".into());
        }
        let project = file
            .project_id
            .filter(|p| valid_project(p))
            .ok_or("its project_id is missing or not a project id")?;
        let email = file
            .client_email
            .filter(|e| valid_email(e))
            .ok_or("its client_email is missing or not a service account address")?;
        let der = key
            .as_deref()
            .and_then(|k| pem_to_der(k))
            .ok_or("its private_key is missing or not a PKCS#8 PEM key")?;
        let key = RsaKeyPair::from_pkcs8(&der)
            .map_err(|_| "its private_key is not a usable RSA key".to_owned())?;
        Ok(Credentials {
            project,
            email,
            key,
        })
    }

    /// The Firebase project messages are sent for.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// A signed OAuth assertion (RS256 JWT) valid from `now_secs` for [`ASSERTION_VALID`].
    fn assertion(&self, audience: &str, now_secs: u64) -> Option<Zeroizing<String>> {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "iss": self.email,
                "scope": SCOPE,
                "aud": audience,
                "iat": now_secs,
                "exp": now_secs + ASSERTION_VALID,
            })
            .to_string(),
        );
        let signed = format!("{header}.{claims}");
        let mut signature = vec![0u8; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .ok()?;
        Some(Zeroizing::new(format!(
            "{signed}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        )))
    }
}

/// Where the sender talks to. Production is only [`Endpoints::google`]; the tests point it at a
/// loopback server.
#[derive(Clone, Debug)]
pub struct Endpoints {
    token: String,
    messages: String,
    https_only: bool,
}

impl Endpoints {
    /// The fixed Google endpoints for `project` (already validated by [`Credentials::parse`]).
    fn google(project: &str) -> Endpoints {
        Endpoints {
            token: TOKEN_URL.to_owned(),
            messages: format!("https://fcm.googleapis.com/v1/projects/{project}/messages:send"),
            https_only: true,
        }
    }

    /// A local test server (`http://127.0.0.1:<port>`).
    #[cfg(test)]
    pub(crate) fn loopback(port: u16, project: &str) -> Endpoints {
        Endpoints {
            token: format!("http://127.0.0.1:{port}/token"),
            messages: format!("http://127.0.0.1:{port}/v1/projects/{project}/messages:send"),
            https_only: false,
        }
    }
}

/// What the sender needs: the validated account and where to send.
pub struct Config {
    credentials: Credentials,
    endpoints: Endpoints,
}

impl Config {
    fn new(credentials: Credentials) -> Config {
        let endpoints = Endpoints::google(credentials.project());
        Config {
            credentials,
            endpoints,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_endpoints(credentials: Credentials, endpoints: Endpoints) -> Config {
        Config {
            credentials,
            endpoints,
        }
    }
}

/// Reads the service account named by [`CREDENTIALS_ENV`], else `dir`'s [`CREDENTIALS_FILE`].
/// `None`: native push is not configured (nothing set; a file that is set but unusable is
/// logged as an error). Blocking disk I/O: call at startup only.
pub fn load(dir: &Path) -> Option<Config> {
    load_from(
        dir,
        std::env::var_os(CREDENTIALS_ENV).filter(|v| !v.is_empty()),
    )
}

/// [`load`] with the value of [`CREDENTIALS_ENV`] given.
fn load_from(dir: &Path, explicit: Option<std::ffi::OsString>) -> Option<Config> {
    let (path, explicit) = match explicit {
        Some(p) => (std::path::PathBuf::from(p), true),
        None => (dir.join(CREDENTIALS_FILE), false),
    };
    let read = std::fs::File::open(&path).and_then(|f| {
        let mut bytes = Zeroizing::new(Vec::new());
        f.take(MAX_ACCOUNT_FILE + 1).read_to_end(&mut bytes)?;
        Ok(bytes)
    });
    let bytes = match read {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
            log::info!(
                "Android push: not configured; put a Firebase service account at {} (or set \
                 {CREDENTIALS_ENV}) to enable it",
                path.display()
            );
            return None;
        }
        Err(e) => {
            log::error!(
                "Android push is unavailable: cannot read {}: {e}",
                path.display()
            );
            return None;
        }
    };
    if bytes.len() as u64 > MAX_ACCOUNT_FILE {
        log::error!(
            "Android push is unavailable: {} is larger than {MAX_ACCOUNT_FILE} bytes",
            path.display()
        );
        return None;
    }
    match Credentials::parse(&bytes) {
        Ok(c) => {
            log::info!(
                "Android push: enabled for Firebase project {} (service account {})",
                c.project(),
                path.display()
            );
            Some(Config::new(c))
        }
        Err(why) => {
            log::error!(
                "Android push is unavailable: {} is not usable: {why}",
                path.display()
            );
            None
        }
    }
}

// ---- messages --------------------------------------------------------------------------------

/// One push to one Android registration.
#[derive(Clone)]
pub struct Job {
    /// The paired device.
    pub device: String,
    /// The FCM registration token.
    pub token: String,
    /// The per-desktop AES-256-GCM key.
    pub key: Zeroizing<[u8; 32]>,
    /// This desktop's id (`data.desktop`, bound into the AAD).
    pub desktop: String,
    /// The bytes to seal: the notification JSON with `expires_unix_ms`.
    pub plaintext: Zeroizing<Vec<u8>>,
    /// The same expiry, for the sender: an expired push is not sent.
    pub expires_unix_ms: u64,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

impl Routed for Job {
    fn device(&self) -> &str {
        &self.device
    }
}

/// The declarative payload with `expires_unix_ms` added at the top level: what the phone
/// decrypts. `None`: `payload` is not a JSON object.
pub fn plaintext(payload: &[u8], expires_unix_ms: u64) -> Option<Zeroizing<Vec<u8>>> {
    let mut value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    value
        .as_object_mut()?
        .insert("expires_unix_ms".to_owned(), expires_unix_ms.into());
    serde_json::to_vec(&value).ok().map(Zeroizing::new)
}

/// `plaintext` sealed under `key`: ciphertext followed by the 16-byte tag.
pub fn seal(
    key: &[u8; 32],
    desktop: &str,
    nonce: [u8; NONCE_LEN],
    plaintext: &[u8],
) -> Option<Vec<u8>> {
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, key).ok()?);
    let aad = format!("{AAD_PREFIX}{desktop}");
    let mut buf = plaintext.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::from(aad.as_bytes()),
        &mut buf,
    )
    .ok()?;
    Some(buf)
}

/// The FCM v1 request body for `job` under `nonce`.
fn message(job: &Job, nonce: [u8; NONCE_LEN]) -> Option<Vec<u8>> {
    let ciphertext = seal(&job.key, &job.desktop, nonce, &job.plaintext)?;
    serde_json::to_vec(&serde_json::json!({
        "message": {
            "token": job.token,
            "data": {
                "v": ENVELOPE_VERSION,
                "desktop": job.desktop,
                "nonce": URL_SAFE_NO_PAD.encode(nonce),
                "ciphertext": URL_SAFE_NO_PAD.encode(ciphertext),
            },
            "android": {
                "priority": "HIGH",
                "ttl": format!("{}s", TTL.as_secs()),
            },
        }
    }))
    .ok()
}

// ---- sender ----------------------------------------------------------------------------------

/// An FCM registration the push service reported unregistered: exactly this token under this
/// key (a phone that registered anew since is a different registration).
#[derive(Clone, PartialEq, Eq)]
pub struct Gone {
    pub device: String,
    pub token: String,
    /// base64url, as stored.
    pub key: String,
}

impl std::fmt::Debug for Gone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gone")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

pub type Report = Arc<dyn Fn(Gone) + Send + Sync>;

/// The handle; dropping it stops the thread after its current request.
pub type Sender = queue::Handle<Job>;

impl Sender {
    pub fn start(config: Arc<Config>, report: Report) -> std::io::Result<Sender> {
        Sender::start_with(config, report, BACKOFF)
    }

    pub(crate) fn start_with(
        config: Arc<Config>,
        report: Report,
        backoff: Duration,
    ) -> std::io::Result<Sender> {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .proxy(None)
                .max_redirects(0)
                .https_only(config.endpoints.https_only)
                .timeout_connect(Some(CONNECT_TIMEOUT))
                .timeout_global(Some(REQUEST_TIMEOUT))
                .build(),
        );
        queue::Handle::spawn(
            "gateway-fcm",
            Android {
                agent,
                config,
                report,
                backoff,
                rng: SystemRandom::new(),
                access: None,
                account_logged: None,
            },
        )
    }
}

struct Access {
    token: Zeroizing<String>,
    valid_until: Instant,
}

struct Android {
    agent: ureq::Agent,
    config: Arc<Config>,
    report: Report,
    backoff: Duration,
    rng: SystemRandom,
    access: Option<Access>,
    /// When a bad-account error was last logged.
    account_logged: Option<Instant>,
}

/// Why no access token was obtained.
enum TokenError {
    /// Google refused the service account (`invalid_grant`, a disabled key, …): the gateway's
    /// configuration is wrong; no registration is touched.
    Account,
    /// Worth another try after the wait.
    Retry(Duration),
    Failed,
}

/// What Google said an error was: the HTTP-level `status` and FCM's own `errorCode`, both kept
/// only when they look like codes (`UNREGISTERED`, `SENDER_ID_MISMATCH`, …).
#[derive(Default)]
struct Reason {
    status: Option<String>,
    code: Option<String>,
}

impl Reason {
    fn parse(body: &[u8]) -> Reason {
        let code = |v: &serde_json::Value| {
            v.as_str()
                .filter(|s| {
                    s.len() <= 48
                        && !s.is_empty()
                        && s.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
                })
                .map(str::to_owned)
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
            return Reason::default();
        };
        let error = &value["error"];
        Reason {
            status: code(&error["status"]),
            code: error["details"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|d| code(&d["errorCode"])),
        }
    }

    fn describe(&self) -> String {
        match (&self.code, &self.status) {
            (Some(c), _) => c.clone(),
            (None, Some(s)) => s.clone(),
            (None, None) => "no error code".to_owned(),
        }
    }
}

impl Android {
    fn body(response: &mut ureq::http::Response<ureq::Body>) -> Vec<u8> {
        response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE)
            .read_to_vec()
            .unwrap_or_default()
    }

    /// The cached access token, or a fresh one from the token endpoint.
    fn access_token(&mut self) -> Result<Zeroizing<String>, TokenError> {
        let now = Instant::now();
        if let Some(a) = &self.access
            && now + TOKEN_MARGIN < a.valid_until
        {
            return Ok(a.token.clone());
        }
        self.access = None;
        let endpoints = &self.config.endpoints;
        let Some(assertion) = self
            .config
            .credentials
            .assertion(TOKEN_URL, now_ms() / 1000)
        else {
            log::error!("Android push: cannot sign the OAuth assertion");
            return Err(TokenError::Failed);
        };
        let form = Zeroizing::new(format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer&assertion={}",
            assertion.as_str()
        ));
        let request = Request::post(&endpoints.token)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(form.as_bytes().to_vec())
            .map_err(|_| TokenError::Failed)?;
        let mut response = match self.agent.run(request) {
            Ok(r) => r,
            Err(e @ (ureq::Error::TooManyRedirects | ureq::Error::RequireHttpsOnly(_))) => {
                log::error!("Android push: the token request was refused: {e}");
                return Err(TokenError::Failed);
            }
            Err(e) => {
                log::warn!("Android push: the token request failed: {e}");
                return Err(TokenError::Retry(self.backoff));
            }
        };
        let status = response.status().as_u16();
        let wait = retry_after(response.headers().get("retry-after"));
        let body = Zeroizing::new(Android::body(&mut response));
        match status {
            200 => {}
            400 | 401 | 403 => return Err(TokenError::Account),
            429 => return Err(TokenError::Retry(wait)),
            500..=599 => return Err(TokenError::Retry(self.backoff)),
            s => {
                log::error!("Android push: the token endpoint answered {s}");
                return Err(TokenError::Failed);
            }
        }
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            #[serde(default)]
            expires_in: Option<u64>,
        }
        let Ok(parsed) = serde_json::from_slice::<Token>(&body) else {
            log::error!("Android push: the token endpoint's answer is not an access token");
            return Err(TokenError::Failed);
        };
        let token = Zeroizing::new(parsed.access_token);
        // Sent as a header value: visible ASCII only.
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            log::error!("Android push: the token endpoint's access token is unusable");
            return Err(TokenError::Failed);
        }
        let lifetime = Duration::from_secs(parsed.expires_in.unwrap_or(3600).min(3600));
        self.access = Some(Access {
            token: token.clone(),
            valid_until: now + lifetime,
        });
        Ok(token)
    }

    /// A bad-account error, logged at most every [`ACCOUNT_LOG_EVERY`].
    fn account_error(&mut self, what: &str) {
        let now = Instant::now();
        if self
            .account_logged
            .is_none_or(|at| now.duration_since(at) >= ACCOUNT_LOG_EVERY)
        {
            self.account_logged = Some(now);
            log::error!(
                "Android push: Google refused the service account ({what}); check \
                 the service account file and that the Firebase Cloud Messaging API is enabled"
            );
        }
    }

    fn try_once(&mut self, job: &Job, attempt: u8) -> Outcome {
        let backoff = self.backoff * 4u32.pow(u32::from(attempt));
        if now_ms() >= job.expires_unix_ms {
            log::info!("push to {}: expired before it could be sent", job.device);
            return Outcome::Failed;
        }
        let access = match self.access_token() {
            Ok(a) => a,
            Err(TokenError::Account) => {
                self.account_error("the OAuth token request was rejected");
                return Outcome::Failed;
            }
            Err(TokenError::Retry(wait)) => return Outcome::Retry(wait),
            Err(TokenError::Failed) => return Outcome::Failed,
        };
        let mut nonce = [0u8; NONCE_LEN];
        if self.rng.fill(&mut nonce).is_err() {
            log::warn!("push to {}: no random nonce", job.device);
            return Outcome::Failed;
        }
        let Some(body) = message(job, nonce) else {
            log::warn!("push to {}: cannot encrypt", job.device);
            return Outcome::Failed;
        };
        let Ok(mut authorization) = HeaderValue::from_str(&format!("Bearer {}", access.as_str()))
        else {
            return Outcome::Failed;
        };
        authorization.set_sensitive(true);
        let request = Request::post(&self.config.endpoints.messages)
            .header(header::AUTHORIZATION, authorization)
            .header(header::CONTENT_TYPE, "application/json; charset=UTF-8")
            .body(body);
        let Ok(request) = request else {
            return Outcome::Failed;
        };
        let mut response = match self.agent.run(request) {
            Ok(r) => r,
            Err(e @ (ureq::Error::TooManyRedirects | ureq::Error::RequireHttpsOnly(_))) => {
                log::warn!("push to {}: {e}", job.device);
                return Outcome::Failed;
            }
            Err(e) => {
                log::warn!("push to {}: {e}", job.device);
                return Outcome::Retry(backoff);
            }
        };
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            log::debug!("push to {}: FCM accepted it", job.device);
            return Outcome::Sent;
        }
        let wait = retry_after(response.headers().get("retry-after"));
        let reason = Reason::parse(&Android::body(&mut response));
        // Only FCM's own UNREGISTERED says the registration is dead; no other answer (a 400
        // included) clears it.
        if reason.code.as_deref() == Some("UNREGISTERED") {
            return Outcome::Gone;
        }
        let why = reason.describe();
        match status {
            401 => {
                // The access token was refused (revoked, clock): get a new one; the attempts
                // are capped, so this cannot loop.
                self.access = None;
                log::warn!(
                    "push to {}: Google refused the access token ({why})",
                    job.device
                );
                Outcome::Retry(self.backoff)
            }
            403 => {
                // SENDER_ID_MISMATCH: the phone's Firebase project is not the account's;
                // PERMISSION_DENIED: the API is off or the account lacks the role. Both are
                // configuration, not a dead registration.
                self.account_error(&why);
                Outcome::Failed
            }
            429 => {
                log::warn!("push to {}: FCM rate limit ({why})", job.device);
                Outcome::Retry(wait)
            }
            500..=599 => {
                log::warn!("push to {}: FCM answered {status} ({why})", job.device);
                Outcome::Retry(backoff)
            }
            s => {
                log::warn!("push to {}: FCM answered {s} ({why})", job.device);
                Outcome::Failed
            }
        }
    }
}

impl queue::Worker<Job> for Android {
    fn deliver(&mut self, job: &Job, attempt: u8) -> Outcome {
        self.try_once(job, attempt)
    }

    fn gone(&mut self, job: Job) {
        log::info!(
            "push to {}: the Android registration is unregistered; deleting it",
            job.device
        );
        (self.report)(Gone {
            device: job.device,
            token: job.token,
            key: URL_SAFE_NO_PAD.encode(*job.key),
        });
    }
}

#[cfg(test)]
pub(super) mod tests;
