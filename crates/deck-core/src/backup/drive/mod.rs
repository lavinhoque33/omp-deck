//! The Google Drive destination: sign-in is Google's installed-app OAuth flow (PKCE + loopback
//! redirect, [`oauth`]); backups live in the Drive `appDataFolder` ([`client`]), which only this
//! OAuth client can see. Token state lives in `config_dir()/google-drive.json` ([`account`]). See
//! `docs/google-drive-setup.md` for the maintainer's Google Cloud setup.

mod account;
pub(in crate::backup) mod client;
pub(in crate::backup) mod oauth;

use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub use account::{Account, AccountStore};
pub use oauth::{LOGIN_TIMEOUT, LoginFlow, SCOPES};

use super::{Error, Result, Service, http};

const CLIENT_ID_VAR: &str = "OMP_DECK_GOOGLE_CLIENT_ID";
const CLIENT_SECRET_VAR: &str = "OMP_DECK_GOOGLE_CLIENT_SECRET";

/// Google's endpoints; injectable so tests can talk to a local mock server.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub auth: String,
    pub token: String,
    pub revoke: String,
    /// Drive v3 metadata API.
    pub drive: String,
    /// Drive v3 upload API.
    pub upload: String,
}

impl Endpoints {
    pub fn google() -> Endpoints {
        Endpoints {
            auth: "https://accounts.google.com/o/oauth2/v2/auth".to_owned(),
            token: "https://oauth2.googleapis.com/token".to_owned(),
            revoke: "https://oauth2.googleapis.com/revoke".to_owned(),
            drive: "https://www.googleapis.com/drive/v3".to_owned(),
            upload: "https://www.googleapis.com/upload/drive/v3".to_owned(),
        }
    }
}

/// The OAuth client of this build. Google desktop clients need the secret, which is not
/// confidential for installed apps.
#[derive(Clone, Debug)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: Option<String>,
}

impl Credentials {
    /// Runtime env vars win over the values baked in at build time; `None` = this build has no
    /// OAuth client, so the Drive destination is unavailable.
    pub fn detect() -> Option<Credentials> {
        Self::pick(
            std::env::var(CLIENT_ID_VAR).ok(),
            option_env!("OMP_DECK_GOOGLE_CLIENT_ID"),
            std::env::var(CLIENT_SECRET_VAR).ok(),
            option_env!("OMP_DECK_GOOGLE_CLIENT_SECRET"),
        )
    }

    pub(in crate::backup) fn pick(
        id: Option<String>,
        baked_id: Option<&str>,
        secret: Option<String>,
        baked_secret: Option<&str>,
    ) -> Option<Credentials> {
        let choose = |runtime: Option<String>, baked: Option<&str>| {
            runtime
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    baked
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                })
        };
        Some(Credentials {
            client_id: choose(id, baked_id)?,
            client_secret: choose(secret, baked_secret),
        })
    }
}

impl Service {
    pub(in crate::backup) fn creds(&self) -> Result<&Credentials> {
        self.creds.as_ref().ok_or(Error::DriveUnavailable)
    }

    /// Binds the loopback listener and builds the consent URL (no network).
    pub fn begin_login(&self) -> Result<LoginFlow> {
        LoginFlow::start(self.creds()?, &self.endpoints)
    }

    /// Waits for the browser, exchanges the code and saves the account.
    pub fn finish_login(
        &self,
        flow: LoginFlow,
        cancel: &AtomicBool,
        timeout: Duration,
    ) -> Result<Account> {
        let creds = self.creds()?;
        let code = flow.wait_code(cancel, timeout)?;
        let tokens = flow.exchange(&http(), creds, &self.endpoints, &code)?;
        let refresh_token = tokens.refresh_token.ok_or_else(|| {
            Error::Login("Google returned no refresh token; try signing in again".to_owned())
        })?;
        let account = Account {
            refresh_token,
            email: tokens
                .id_token
                .as_deref()
                .and_then(oauth::email_from_id_token),
            ..Account::default()
        };
        self.store.save(&account)?;
        Ok(account)
    }

    /// Revokes the refresh token at Google and deletes the local state. The local state is
    /// removed even when revoking fails; that failure is still reported.
    pub fn sign_out(&self) -> Result<()> {
        let account = self.store.load()?;
        let revoked = match &account {
            Some(a) => oauth::revoke(&http(), &self.endpoints, &a.refresh_token),
            None => Ok(()),
        };
        self.store.delete()?;
        revoked
    }
}

pub(in crate::backup) fn network(e: ureq::Error) -> Error {
    Error::Network(e.to_string())
}

/// A readable error for a non-success answer to `what`. Understands Drive's
/// `{"error":{"message"}}` and OAuth's `{"error","error_description"}` bodies.
pub(in crate::backup) fn google_error(what: &str, status: u16, body: &str) -> Error {
    let json: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let err = &json["error"];
    let detail = err["message"]
        .as_str()
        .or_else(|| json["error_description"].as_str())
        .or_else(|| err.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let msg = match (status, detail) {
        (429 | 500..=599, _) => {
            format!("{what} failed: Google is busy or unavailable (HTTP {status}); try again later")
        }
        (_, Some(d)) => format!("{what} failed: {d} (HTTP {status})"),
        (_, None) => format!("{what} failed (HTTP {status})"),
    };
    Error::Google(msg)
}
