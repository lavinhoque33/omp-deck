//! OS supervision of the per-user gateway: a user systemd unit (Linux) or an interactive-user
//! logon scheduled task (Windows) that runs the provided program (`omp-deck-gateway supervise …`).
//!
//! The functions only manage OmpDeck's own registration, named after the application namespace, so
//! repeating them is idempotent and nothing else the user owns is ever overwritten or deleted.
//! Registering never starts anything and never stops anything: [`start`] is a separate step and
//! [`remove`] neither stops the supervisor (it leaves on its own when the mode turns off) nor
//! terminates any process tree, because the sessions' hosts and omp children may live in the same
//! cgroup / job as the supervisor.
//!
//! The startup context itself (roots, profile, non-secret omp environment) lives in a private file
//! owned by the service; `args` here must only point at it and carry no secret values.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::usage::{UsageError, run_bounded_bytes};

#[cfg(unix)]
mod systemd;
#[cfg_attr(not(windows), allow(dead_code))]
mod task;

/// How long one `systemctl` / `schtasks` invocation may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Most output kept of one invocation (a full `schtasks /Query` listing is the largest).
const COMMAND_OUTPUT_CAP: u64 = 4 * 1024 * 1024;

/// What the OS must run, and for which application namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Startup {
    /// Application namespace (`omp-deck` in production): names the registration, so each
    /// namespace owns exactly one. `[A-Za-z0-9_-]{1,40}`.
    pub namespace: String,
    /// Absolute path of the executable. AppImage: the stable AppImage path, never its mount dir.
    pub program: PathBuf,
    /// Arguments, e.g. `supervise --startup <private file>`. No secret values.
    pub args: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistrationKind {
    SystemdUser,
    WindowsTask,
}

/// One OmpDeck-owned OS registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Registration {
    pub kind: RegistrationKind,
    /// Unit name (`omp-deck-gateway-<ns>.service`) or task name (`OmpDeck-Gateway-<ns>`).
    pub name: String,
    /// The unit file; `None` for a scheduled task (the OS keeps it in its task store).
    pub path: Option<PathBuf>,
}

/// State of an existing, OmpDeck-owned registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Inspection {
    pub registration: Registration,
    /// The OS will start it (unit enabled / task enabled).
    pub enabled: bool,
    /// The supervisor is running right now; `None` where the OS gives no locale-independent
    /// answer (Windows `schtasks` prints the state as localized text).
    pub active: Option<bool>,
    /// It would be rewritten identically by [`register`] of the `Startup` given to [`inspect`]
    /// (false after the executable or arguments moved: re-register to repair it).
    pub current: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AutostartError {
    /// The startup description cannot be registered as given.
    #[error("invalid gateway startup: {0}")]
    Invalid(String),
    /// The OS has no usable supervisor (no user systemd manager, no task scheduler, other OS).
    #[error("gateway autostart is unavailable: {0}")]
    Unavailable(String),
    /// A registration of that name exists and is not OmpDeck's; it was left untouched.
    #[error("{name} already exists and is not an OmpDeck gateway registration")]
    Conflict { name: String },
    /// [`start`] of a registration that is not there.
    #[error("no gateway registration for namespace {0}")]
    NotRegistered(String),
    /// The OS command failed.
    #[error("{command} failed: {detail}")]
    Command { command: String, detail: String },
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Startup {
    /// Checks everything a registration depends on; the OS-specific encoders check the rest.
    /// `is_absolute` decides what an absolute program path is in the registration format being
    /// written (the encoders are pure and tested on every host).
    fn validate(&self, is_absolute: fn(&str) -> bool) -> Result<&str, AutostartError> {
        if !deck_host::valid_namespace(&self.namespace) {
            return Err(invalid(format!(
                "namespace {:?} is not [A-Za-z0-9_-]{{1,40}}",
                self.namespace
            )));
        }
        let program = self
            .program
            .to_str()
            .ok_or_else(|| invalid("program path is not valid UTF-8".into()))?;
        if !is_absolute(program) {
            return Err(invalid(format!(
                "program {program} is not an absolute path"
            )));
        }
        for text in std::iter::once(program).chain(self.args.iter().map(String::as_str)) {
            if text.chars().any(char::is_control) {
                return Err(invalid(
                    "program and arguments must not contain control characters".into(),
                ));
            }
        }
        Ok(program)
    }
}

fn host_absolute(path: &str) -> bool {
    std::path::Path::new(path).is_absolute()
}

/// Windows: drive-letter or UNC path.
pub(crate) fn windows_absolute(path: &str) -> bool {
    let b = path.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/'))
        || path.starts_with(r"\\")
}

/// Unix: rooted path.
#[cfg(unix)]
pub(crate) fn unix_absolute(path: &str) -> bool {
    path.starts_with('/')
}

fn invalid(why: String) -> AutostartError {
    AutostartError::Invalid(why)
}

/// Registers (or repairs) the supervisor for `startup.namespace`: writes the unit / task and
/// enables it for the next login. Does not start it. An existing registration that is not
/// OmpDeck's is [`AutostartError::Conflict`]; unavailability is [`AutostartError::Unavailable`];
/// a failed registration leaves nothing newly enabled behind.
pub fn register(startup: &Startup) -> Result<Registration, AutostartError> {
    startup.validate(host_absolute)?;
    os::register(startup)
}

/// Starts the registered supervisor now (without waiting for the next login).
pub fn start(registration: &Registration) -> Result<(), AutostartError> {
    os::start(registration)
}

/// The OmpDeck-owned registration for `startup.namespace` and its state; `None` when there is none.
pub fn inspect(startup: &Startup) -> Result<Option<Inspection>, AutostartError> {
    startup.validate(host_absolute)?;
    os::inspect(startup)
}

/// Removes OmpDeck's registration for `namespace` (disables autostart, deletes the unit / task).
/// `Ok(false)` = there was none. A running supervisor is neither stopped nor are any processes
/// terminated. A same-named registration that is not OmpDeck's is left alone:
/// [`AutostartError::Conflict`].
pub fn remove(namespace: &str) -> Result<bool, AutostartError> {
    if !deck_host::valid_namespace(namespace) {
        return Err(invalid(format!("namespace {namespace:?} is not valid")));
    }
    os::remove(namespace)
}

// ---- command runner ----------------------------------------------------------------------------

/// Result of one finished OS command.
pub(super) struct Out {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// The most useful line(s) of a failure.
    pub fn detail(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let text = text.trim();
        if text.is_empty() {
            format!(
                "exit {}",
                self.code.map_or("by signal".into(), |c| c.to_string())
            )
        } else {
            text.chars().take(600).collect()
        }
    }
}

/// Runs an OS command; injected so the registration logic is testable without touching the host.
pub(super) type Runner<'a> = &'a dyn Fn(&str, &[&str]) -> Result<Out, AutostartError>;

/// The real runner: bounded, no console window, never inherits stdin.
pub(super) fn run(program: &str, args: &[&str]) -> Result<Out, AutostartError> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    super::no_window(&mut cmd);
    let (code, stdout, stderr) = run_bounded_bytes(cmd, COMMAND_TIMEOUT, COMMAND_OUTPUT_CAP)
        .map_err(|e| match e {
            UsageError::Spawn(io) if io.kind() == std::io::ErrorKind::NotFound => {
                AutostartError::Unavailable(format!("{program} was not found"))
            }
            other => AutostartError::Command {
                command: program.to_owned(),
                detail: other.to_string(),
            },
        })?;
    Ok(Out {
        code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

pub(super) fn command_error(program: &str, args: &[&str], out: &Out) -> AutostartError {
    AutostartError::Command {
        command: format!("{program} {}", args.join(" ")),
        detail: out.detail(),
    }
}

pub(super) fn io_error(path: &Path, source: std::io::Error) -> AutostartError {
    AutostartError::Io {
        path: path.to_owned(),
        source,
    }
}

// ---- per-OS dispatch ---------------------------------------------------------------------------

#[cfg(unix)]
mod os {
    use super::*;

    pub fn register(startup: &Startup) -> Result<Registration, AutostartError> {
        systemd::register(&systemd::user_unit_dir()?, startup, &run)
    }

    pub fn start(registration: &Registration) -> Result<(), AutostartError> {
        systemd::start(&systemd::user_unit_dir()?, registration, &run)
    }

    pub fn inspect(startup: &Startup) -> Result<Option<Inspection>, AutostartError> {
        systemd::inspect(&systemd::user_unit_dir()?, startup, &run)
    }

    pub fn remove(namespace: &str) -> Result<bool, AutostartError> {
        systemd::remove(&systemd::user_unit_dir()?, namespace, &run)
    }
}

#[cfg(windows)]
mod os {
    use super::*;

    pub fn register(startup: &Startup) -> Result<Registration, AutostartError> {
        task::register(startup, &task::current_sid()?, &run)
    }

    pub fn start(registration: &Registration) -> Result<(), AutostartError> {
        task::start(registration, &run)
    }

    pub fn inspect(startup: &Startup) -> Result<Option<Inspection>, AutostartError> {
        task::inspect(startup, &task::current_sid()?, &run)
    }

    pub fn remove(namespace: &str) -> Result<bool, AutostartError> {
        task::remove(namespace, &run)
    }
}
