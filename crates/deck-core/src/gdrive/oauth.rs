//! Google's installed-app OAuth flow: PKCE (S256) + random `state`, answered on a loopback
//! redirect (`http://127.0.0.1:<ephemeral port>`) served by a plain `TcpListener`.

use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::{Credentials, Endpoints, Error, Result, google_error, network};

/// Drive app folder (non-sensitive) plus `openid email` to show who is signed in.
pub const SCOPES: &str = "https://www.googleapis.com/auth/drive.appdata openid email";
/// How long the loopback listener waits for the browser.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

const POLL: Duration = Duration::from_millis(100);
/// A connection that sends no request within this is dropped (browsers pre-connect sockets).
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_REQUEST: usize = 16 * 1024;

/// A sign-in in progress: the listener is bound, the consent URL is ready to open.
pub struct LoginFlow {
    listener: TcpListener,
    url: String,
    redirect_uri: String,
    verifier: String,
    state: String,
}

#[derive(Deserialize)]
pub(super) struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub id_token: Option<String>,
}

impl LoginFlow {
    pub(super) fn start(creds: &Credentials, ep: &Endpoints) -> Result<LoginFlow> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|e| Error::Login(format!("cannot listen on 127.0.0.1: {e}")))?;
        let port = listener
            .local_addr()
            .map_err(|e| Error::Login(e.to_string()))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}");
        let verifier = random_token(32)?;
        let state = random_token(16)?;
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let params = [
            ("client_id", creds.client_id.as_str()),
            ("redirect_uri", &redirect_uri),
            ("response_type", "code"),
            ("scope", SCOPES),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("state", &state),
            ("access_type", "offline"),
            // Always show consent so Google issues a refresh token again on re-sign-in.
            ("prompt", "consent"),
        ];
        let query: Vec<String> = params
            .iter()
            .map(|(k, v)| format!("{k}={}", encode(v)))
            .collect();
        let url = format!("{}?{}", ep.auth, query.join("&"));
        Ok(LoginFlow {
            listener,
            url,
            redirect_uri,
            verifier,
            state,
        })
    }

    /// The consent page to open in the browser.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The loopback port the redirect must reach.
    #[cfg(test)]
    pub(super) fn port(&self) -> u16 {
        self.listener.local_addr().map_or(0, |a| a.port())
    }

    /// Serves the loopback until the redirect arrives; returns its authorization code. Requests
    /// for other paths (favicon, …) are answered 404 and ignored; a redirect with a wrong
    /// `state` or an error ends the sign-in.
    pub(super) fn wait_code(&self, cancel: &AtomicBool, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        self.listener
            .set_nonblocking(true)
            .map_err(|e| Error::Login(e.to_string()))?;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(Error::TimedOut);
            }
            match self.listener.accept() {
                Ok((stream, _)) => match self.answer(stream) {
                    Ok(Some(outcome)) => return outcome,
                    Ok(None) => {}
                    Err(e) => log::debug!("loopback connection: {e}"),
                },
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) => return Err(Error::Login(format!("loopback listener: {e}"))),
            }
        }
    }

    /// `Ok(None)` = not the redirect (answered and ignored).
    fn answer(&self, mut stream: TcpStream) -> io::Result<Option<Result<String>>> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(READ_TIMEOUT))?;
        let head = read_head(&mut stream)?;
        let line = head.lines().next().unwrap_or_default();
        let mut parts = line.split(' ');
        let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        if method != "GET" || path != "/" {
            respond(&mut stream, "404 Not Found", "Not found")?;
            return Ok(None);
        }
        let param = |name: &str| {
            query.split('&').find_map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (decode(k) == name).then(|| decode(v))
            })
        };
        let outcome = if param("state").as_deref() != Some(self.state.as_str()) {
            Err(Error::Login(
                "the browser answered for a different sign-in attempt (state mismatch)".to_owned(),
            ))
        } else if let Some(err) = param("error") {
            Err(Error::Login(if err == "access_denied" {
                "access was denied in the browser".to_owned()
            } else {
                format!("Google answered “{err}”")
            }))
        } else {
            param("code")
                .filter(|c| !c.is_empty())
                .ok_or_else(|| Error::Login("the redirect carried no authorization code".into()))
        };
        let page = match &outcome {
            Ok(_) => "Signed in to OmpDeck. You can close this tab.".to_owned(),
            Err(e) => format!("OmpDeck {e}. You can close this tab."),
        };
        respond(&mut stream, "200 OK", &page)?;
        Ok(Some(outcome))
    }

    /// Trades the authorization code (plus the PKCE verifier) for tokens.
    pub(super) fn exchange(
        &self,
        http: &ureq::Agent,
        creds: &Credentials,
        ep: &Endpoints,
        code: &str,
    ) -> Result<Tokens> {
        let mut form = vec![
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", creds.client_id.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("code_verifier", self.verifier.as_str()),
        ];
        if let Some(s) = &creds.client_secret {
            form.push(("client_secret", s));
        }
        token_request(http, ep, &form, "Sign-in")
    }
}

/// A fresh access token for `refresh_token`. A revoked/expired grant is [`Error::SignedOut`].
pub(super) fn refresh(
    http: &ureq::Agent,
    creds: &Credentials,
    ep: &Endpoints,
    refresh_token: &str,
) -> Result<String> {
    let mut form = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", creds.client_id.as_str()),
    ];
    if let Some(s) = &creds.client_secret {
        form.push(("client_secret", s));
    }
    token_request(http, ep, &form, "Refreshing the Google sign-in").map(|t| t.access_token)
}

fn token_request(
    http: &ureq::Agent,
    ep: &Endpoints,
    form: &[(&str, &str)],
    what: &str,
) -> Result<Tokens> {
    let mut resp = http
        .post(&ep.token)
        .send_form(form.iter().copied())
        .map_err(network)?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().map_err(network)?;
    if !(200..300).contains(&status) {
        let invalid_grant = serde_json::from_str::<serde_json::Value>(&body)
            .is_ok_and(|v| v["error"] == "invalid_grant");
        return Err(
            if invalid_grant && form.contains(&("grant_type", "refresh_token")) {
                Error::SignedOut
            } else {
                google_error(what, status, &body)
            },
        );
    }
    serde_json::from_str(&body).map_err(|e| {
        Error::Google(format!(
            "{what} failed: unexpected answer from Google ({e})"
        ))
    })
}

/// Revokes a token at Google (sign-out).
pub(super) fn revoke(http: &ureq::Agent, ep: &Endpoints, token: &str) -> Result<()> {
    let mut resp = http
        .post(&ep.revoke)
        .send_form([("token", token)])
        .map_err(network)?;
    let status = resp.status().as_u16();
    if (200..300).contains(&status) {
        return Ok(());
    }
    let body = resp.body_mut().read_to_string().unwrap_or_default();
    // Already invalid at Google: the goal (token unusable) is reached.
    if status == 400 && body.contains("invalid_token") {
        return Ok(());
    }
    Err(google_error("Revoking the Google sign-in", status, &body))
}

/// The `email` claim of an id_token (its payload is trusted: it came straight from the token
/// endpoint over TLS).
pub(super) fn email_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(payload))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["email"].as_str().map(str::to_owned)
}

/// `n` random bytes, base64url without padding.
fn random_token(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).map_err(|e| Error::Login(format!("no system randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(buf))
}

/// The request line and headers (up to the blank line).
fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REQUEST {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn respond(stream: &mut TcpStream, status: &str, text: &str) -> io::Result<()> {
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>OmpDeck</title></head>\
         <body style=\"font-family:sans-serif;margin:3em\"><p>{}</p></body></html>",
        escape_html(text)
    );
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Connection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    )?;
    stream.flush()
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` and `+` (form encoding); invalid escapes are kept literally.
pub(super) fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let hex = |c: u8| char::from(c).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (
            b[i],
            b.get(i + 1).copied().and_then(hex),
            b.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(hi), Some(lo)) => {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
            (b'+', ..) => out.push(b' '),
            (c, ..) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
