//! Provider authentication through omp's own storage and login flows. Credentials never cross
//! this API: the helper reports only provider/account metadata and interactive login events.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Omp, headless, strip_error_prefix};
use crate::platform::{self, KillableChild};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CANCEL_GRACE: Duration = Duration::from_secs(2);
const MAX_LINE: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoginProvider {
    pub id: String,
    pub name: String,
    pub available: bool,
    pub store: String,
    pub disabled: bool,
    pub origin: Option<AuthOrigin>,
    pub accounts: Vec<StoredAccount>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthOrigin {
    pub kind: String,
    #[serde(alias = "envVar")]
    pub env_var: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredAccount {
    #[serde(alias = "credentialId")]
    pub credential_id: i64,
    pub label: String,
    pub detail: String,
    #[serde(alias = "type")]
    pub kind: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogoutOutcome {
    pub removed: bool,
    pub remaining: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoginEvent {
    Auth {
        url: String,
        launch_url: Option<String>,
        instructions: Option<String>,
    },
    Prompt {
        seq: u64,
        message: String,
        placeholder: Option<String>,
        allow_empty: bool,
        masked: bool,
    },
    Manual {
        seq: u64,
    },
    Progress(String),
    Done {
        store: String,
        label: Option<String>,
    },
    Failed {
        message: String,
        cancelled: bool,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("failed to run omp: {0}")]
    Io(#[from] io::Error),
    #[error("{0}")]
    Failed(String),
    #[error("omp authentication timed out")]
    Timeout,
}

/// Owns the private answer directory and kill-on-drop omp process tree.
pub struct LoginRun {
    run: AuthRun,
}

impl Omp {
    /// Lists every login provider in omp's order, with stored accounts and credential origin.
    /// Blocks for at most 20 seconds; call off the UI thread.
    pub fn login_providers(&self) -> Result<Vec<LoginProvider>, AuthError> {
        match self.auth_request(json!({"op": "list"}))? {
            WireEvent::Providers { providers } => Ok(providers),
            _ => Err(AuthError::Failed("omp produced no provider list".into())),
        }
    }

    /// Removes one stored credential through omp, leaving env/config keys untouched.
    pub fn logout(&self, provider: &str, credential_id: i64) -> Result<LogoutOutcome, AuthError> {
        match self.auth_request(
            json!({"op": "logout", "provider": provider, "credentialId": credential_id}),
        )? {
            WireEvent::LoggedOut { removed, remaining } => Ok(LogoutOutcome { removed, remaining }),
            _ => Err(AuthError::Failed("omp produced no logout result".into())),
        }
    }

    /// Starts a login immediately; poll and answer its events on a worker thread.
    pub fn login(&self, provider: &str) -> Result<LoginRun, AuthError> {
        Ok(LoginRun {
            run: AuthRun::spawn(
                self,
                json!({"op": "login", "provider": provider}),
                LOGIN_TIMEOUT,
            )?,
        })
    }

    fn auth_request(&self, request: Value) -> Result<WireEvent, AuthError> {
        let mut run = AuthRun::spawn(self, request, REQUEST_TIMEOUT)?;
        loop {
            for event in run.poll() {
                match event {
                    WireEvent::Providers { .. } | WireEvent::LoggedOut { .. } => return Ok(event),
                    WireEvent::Error { message, .. } => {
                        return Err(if run.timed_out {
                            AuthError::Timeout
                        } else {
                            AuthError::Failed(message)
                        });
                    }
                    event if event.terminal() => {
                        return Err(AuthError::Failed(
                            "omp produced an unexpected authentication result".into(),
                        ));
                    }
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl LoginRun {
    /// Reads only newly appended event bytes, retaining an incomplete last line for next time.
    pub fn poll(&mut self) -> Vec<LoginEvent> {
        self.run
            .poll()
            .into_iter()
            .filter_map(WireEvent::into_login)
            .collect()
    }

    /// Atomically publishes an answer. The helper consumes and deletes this file; never log it.
    pub fn answer(&self, seq: u64, value: &str) -> io::Result<()> {
        write_answer(self.run.work.path(), seq, value)
    }

    /// Requests graceful abort; polling enforces a two-second process-tree kill deadline.
    pub fn cancel(&mut self) {
        if self.run.terminal || self.run.cancelled_at.is_some() {
            return;
        }
        self.run.cancelled_at = Some(Instant::now());
        if let Err(error) = fs::write(self.run.work.path().join("cancel"), []) {
            log::warn!("omp auth: cannot publish cancellation: {error}");
            if let Some(child) = &self.run.child {
                child.kill_tree();
            }
        }
    }
}

fn write_answer(dir: &Path, seq: u64, value: &str) -> io::Result<()> {
    let mut file = tempfile::Builder::new()
        .prefix(".answer-")
        .tempfile_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    serde_json::to_writer(&mut file, &json!({"value": value}))?;
    file.flush()?;
    file.persist(dir.join(format!("answer-{seq}.json")))
        .map_err(|e| e.error)?;
    Ok(())
}

/// Prompt masking is deliberately independent of provider-specific flow kinds.
fn masked(secret: bool, message: &str) -> bool {
    secret
        || [
            "api key", "api-key", "apikey", "token", "secret", "password", "passcode", "cookie",
        ]
        .iter()
        .any(|needle| {
            message
                .as_bytes()
                .windows(needle.len())
                .any(|s| s.eq_ignore_ascii_case(needle.as_bytes()))
        })
}

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
enum WireEvent {
    Providers {
        providers: Vec<LoginProvider>,
    },
    Auth {
        url: String,
        #[serde(rename = "launchUrl")]
        launch_url: Option<String>,
        instructions: Option<String>,
    },
    Prompt {
        seq: u64,
        message: String,
        placeholder: Option<String>,
        #[serde(rename = "allowEmpty", default)]
        allow_empty: bool,
        #[serde(default)]
        secret: bool,
    },
    Manual {
        seq: u64,
    },
    Progress {
        message: String,
    },
    Done {
        store: String,
        label: Option<String>,
    },
    LoggedOut {
        removed: bool,
        remaining: Option<String>,
    },
    Error {
        message: String,
        cancelled: bool,
    },
    #[serde(other)]
    Unknown,
}

impl WireEvent {
    fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Providers { .. }
                | Self::Done { .. }
                | Self::LoggedOut { .. }
                | Self::Error { .. }
        )
    }

    fn into_login(self) -> Option<LoginEvent> {
        Some(match self {
            Self::Auth {
                url,
                launch_url,
                instructions,
            } => LoginEvent::Auth {
                url,
                launch_url,
                instructions,
            },
            Self::Prompt {
                seq,
                message,
                placeholder,
                allow_empty,
                secret,
            } => {
                let masked = masked(secret, &message);
                LoginEvent::Prompt {
                    seq,
                    message,
                    placeholder,
                    allow_empty,
                    masked,
                }
            }
            Self::Manual { seq } => LoginEvent::Manual { seq },
            Self::Progress { message } => LoginEvent::Progress(message),
            Self::Done { store, label } => LoginEvent::Done { store, label },
            Self::Error { message, cancelled } => LoginEvent::Failed { message, cancelled },
            _ => return None,
        })
    }
}

#[derive(Default)]
struct EventLines {
    partial: Vec<u8>,
    oversized: bool,
}

impl EventLines {
    fn push(&mut self, bytes: &[u8], events: &mut Vec<WireEvent>) {
        for chunk in bytes.split_inclusive(|b| *b == b'\n') {
            if !self.oversized {
                if self.partial.len() + chunk.len() > MAX_LINE {
                    self.partial.clear();
                    self.oversized = true;
                    log::warn!("omp auth: skipping oversized event line");
                } else {
                    self.partial.extend_from_slice(chunk);
                }
            }
            if chunk.last() == Some(&b'\n') {
                if !self.oversized {
                    match serde_json::from_slice(&self.partial) {
                        Ok(WireEvent::Unknown) => {}
                        Ok(event) => events.push(event),
                        Err(_) => log::warn!("omp auth: skipping malformed event line"),
                    }
                }
                self.partial.clear();
                self.oversized = false;
            }
        }
    }
}

struct AuthRun {
    // Drop the child before deleting its private workspace.
    child: Option<KillableChild>,
    work: tempfile::TempDir,
    output: File,
    lines: EventLines,
    deadline: Instant,
    cancelled_at: Option<Instant>,
    terminal: bool,
    timed_out: bool,
}

impl AuthRun {
    fn spawn(omp: &Omp, request: Value, timeout: Duration) -> Result<Self, AuthError> {
        let work = tempfile::Builder::new().prefix("ompdeck-auth").tempdir()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(work.path(), fs::Permissions::from_mode(0o700))?;
        }
        let extension = crate::ctl::install_auth_extension(work.path())?;
        let out_path = work.path().join("events");
        let output = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&out_path)?;
        let mut command = Command::new(&omp.program);
        command
            .args(["--mode", "rpc", "--no-session", "--no-extensions", "-e"])
            .arg(extension)
            .current_dir(platform::home_or_root())
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(work.path().join("stderr"))?);
        for (key, value) in &omp.env {
            command.env(key, value);
        }
        command
            .env("OMPDECK_AUTH_REQ", request.to_string())
            .env("OMPDECK_AUTH_OUT", out_path)
            .env("OMPDECK_AUTH_DIR", work.path());
        headless(&mut command);
        let child = platform::spawn_killable(command)?;
        Ok(Self {
            child: Some(child),
            work,
            output,
            lines: EventLines::default(),
            deadline: Instant::now() + timeout,
            cancelled_at: None,
            terminal: false,
            timed_out: false,
        })
    }

    fn read_events(&mut self, events: &mut Vec<WireEvent>) -> io::Result<()> {
        let mut bytes = [0; 8192];
        // Bounded work per poll, and no rereading earlier bytes from a growing file.
        for _ in 0..32 {
            let count = self.output.read(&mut bytes)?;
            if count == 0 {
                break;
            }
            self.lines.push(&bytes[..count], events);
        }
        Ok(())
    }

    fn poll(&mut self) -> Vec<WireEvent> {
        if self.terminal {
            return Vec::new();
        }
        let mut events = Vec::new();
        let result = self.read_events(&mut events);
        if !events.iter().any(WireEvent::terminal) {
            let now = Instant::now();
            let cancelled = self.cancelled_at.is_some();
            let cancel_expired = self
                .cancelled_at
                .is_some_and(|at| now.duration_since(at) >= CANCEL_GRACE);
            let exited = match self.child.as_mut().map(|child| child.try_wait()) {
                Some(Ok(Some(_))) => true,
                Some(Err(error)) => {
                    events.push(WireEvent::Error {
                        message: error.to_string(),
                        cancelled,
                    });
                    false
                }
                _ => false,
            };
            if exited {
                // The final append may have raced the first read. Exit status is not trusted:
                // omp can exit 1 after a successful factory when no default model is selected.
                let _ = self.read_events(&mut events);
            }
            if !events.iter().any(WireEvent::terminal) {
                let message = if let Err(error) = result {
                    Some(error.to_string())
                } else if cancel_expired {
                    Some("Login cancelled".into())
                } else if now >= self.deadline {
                    self.timed_out = true;
                    Some("omp authentication timed out".into())
                } else if exited {
                    let text = File::open(self.work.path().join("stderr"))
                        .and_then(|file| {
                            let mut bytes = Vec::new();
                            file.take(64 * 1024).read_to_end(&mut bytes)?;
                            Ok(bytes)
                        })
                        .unwrap_or_default();
                    let text = String::from_utf8_lossy(&text);
                    let text = strip_error_prefix(text.trim());
                    Some(if text.is_empty() {
                        "omp produced no authentication result".into()
                    } else {
                        text.to_owned()
                    })
                } else {
                    None
                };
                if let Some(message) = message {
                    events.push(WireEvent::Error { message, cancelled });
                }
            }
        }
        if events.iter().any(WireEvent::terminal) {
            self.terminal = true;
            self.child.take(); // kills and reaps the entire tree on every terminal path
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masking_matches_all_secret_words_case_insensitively() {
        for word in [
            "API key", "API-KEY", "ApiKey", "TOKEN", "secret", "Password", "passcode", "cookie",
        ] {
            assert!(masked(false, word), "{word}");
            assert!(masked(false, &format!("Paste your {word}:")));
        }
        for text in [
            "",
            "Enter authorization code",
            "GitHub Enterprise URL/domain",
            "api",
            "key",
            "pass",
        ] {
            assert!(!masked(false, text), "{text}");
            assert!(masked(true, text));
        }
        assert!(masked(false, "éTOKEN界"));
        assert!(!masked(false, "🔑"));
    }

    #[test]
    fn event_lines_retain_partial_tail_and_ignore_unknown_events() {
        let mut lines = EventLines::default();
        let mut events = Vec::new();
        lines.push(
            b"{\"t\":\"future\",\"x\":1}\nnot-json\n{\"t\":\"pro",
            &mut events,
        );
        assert!(events.is_empty());
        lines.push(
            b"gress\",\"message\":\"hi\"}\n{\"t\":\"done\",\"store\":\"vllm\"}",
            &mut events,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(
            events.pop().unwrap().into_login(),
            Some(LoginEvent::Progress("hi".into()))
        );
        lines.push(b"\n", &mut events);
        assert_eq!(
            events.pop().unwrap().into_login(),
            Some(LoginEvent::Done {
                store: "vllm".into(),
                label: None
            })
        );
        lines.push(b"{\"t\":\"prompt\",\"seq\":7,\"message\":\"API key\",\"allowEmpty\":true,\"secret\":false}\n", &mut events);
        assert!(matches!(
            events.pop().unwrap().into_login(),
            Some(LoginEvent::Prompt {
                seq: 7,
                allow_empty: true,
                masked: true,
                ..
            })
        ));
        lines.push(&vec![b'x'; MAX_LINE + 1], &mut events);
        assert!(lines.partial.is_empty());
        lines.push(b"\n{\"t\":\"manual\",\"seq\":8}\n", &mut events);
        assert_eq!(
            events.pop().unwrap().into_login(),
            Some(LoginEvent::Manual { seq: 8 })
        );
    }

    #[test]
    fn answers_are_atomically_replaced_at_the_sequence_filename() {
        let dir = tempfile::tempdir().unwrap();
        write_answer(dir.path(), 42, "old").unwrap();
        let path = dir.path().join("answer-42.json");
        let previous = File::open(&path).unwrap();
        write_answer(dir.path(), 42, "\"secret\"\n").unwrap();
        let current: Value = serde_json::from_reader(File::open(&path).unwrap()).unwrap();
        assert_eq!(current, json!({"value": "\"secret\"\n"}));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                serde_json::from_reader::<_, Value>(previous).unwrap(),
                json!({"value":"old"})
            );
        }
        #[cfg(not(unix))]
        drop(previous);
        let names: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["answer-42.json"]);
    }

    #[cfg(unix)]
    fn helper_script(dir: &Path, body: &str) -> Omp {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("omp");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Omp::at(path)
    }

    #[cfg(unix)]
    #[test]
    fn child_exit_without_a_terminal_event_reports_stripped_stderr_once() {
        let dir = tempfile::tempdir().unwrap();
        let omp = helper_script(dir.path(), "printf 'Error: login failed\\n' >&2");
        let mut login = omp.login("test").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let events = login.poll();
            if !events.is_empty() {
                assert_eq!(
                    events,
                    [LoginEvent::Failed {
                        message: "login failed".into(),
                        cancelled: false
                    }]
                );
                assert!(login.poll().is_empty());
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn timeout_and_cancel_grace_kill_uncooperative_children() {
        let dir = tempfile::tempdir().unwrap();
        let omp = helper_script(dir.path(), "exec sleep 60");
        let mut run =
            AuthRun::spawn(&omp, json!({"op":"login","provider":"test"}), LOGIN_TIMEOUT).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(run.work.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        run.deadline = Instant::now();
        assert!(matches!(
            run.poll().as_slice(),
            [WireEvent::Error {
                cancelled: false,
                ..
            }]
        ));
        assert!(run.timed_out && run.child.is_none());

        let mut login = omp.login("test").unwrap();
        login.cancel();
        assert!(login.run.work.path().join("cancel").is_file());
        let start = Instant::now();
        loop {
            let events = login.poll();
            if !events.is_empty() {
                assert!(matches!(
                    events.as_slice(),
                    [LoginEvent::Failed {
                        cancelled: true,
                        ..
                    }]
                ));
                assert!(start.elapsed() >= CANCEL_GRACE);
                assert!(start.elapsed() < Duration::from_secs(5));
                assert!(login.run.child.is_none());
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(5));
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
