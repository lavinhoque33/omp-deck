//! Modes, OS supervision and the maintenance barrier of the gateway, plus the synchronous
//! command-line side of `omp-deck-gateway` (`supervise`, `status`, `request`, `watch`, `drain`,
//! `resume`, `uninstall`).
//!
//! - [`set_mode`]: the lifetime transaction a mode change runs in the owner: register OS
//!   supervision → commit `gateway_mode` → start the supervisor → wait for its hello; any failure
//!   rolls back the registration and the mode. Background is never claimed without a supervisor
//!   that answered.
//! - [`supervise`]: the OS-started supervisor. Its endpoint ([`SUPERVISOR_ENDPOINT`]) is its
//!   singleton lock and answers every connection with a [`SupervisorHello`] (pid, version, the
//!   program it runs from). It adopts a running owner over a [`Role::Supervisor`] control link
//!   and starts `serve --startup` when the owner is gone, while the mode is Background and no
//!   barrier is up. It exits 0 when the mode leaves Background or the barrier appears, and
//!   [`exit::RESTART`] (so the OS restarts the registered, current program) once the startup
//!   context names another program or its own executable was replaced. When its own program
//!   stays missing for 30 s (a system package was removed, which cannot reach per-user
//!   registrations), it drains the owner, removes this user's registration and exits 0.
//! - The barrier: a persistent marker ([`super::maintenance_marker`]) written before any owner
//!   is asked to drain, removed only by `resume`.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use deck_core::config::GatewayMode;
use deck_core::platform::gateway::{self as autostart, AutostartError, Registration, Startup};
use deck_host::transport::{Endpoint, Listener, Stream};
use serde::{Deserialize, Serialize};

use super::protocol::{
    Answer, DrainState, Envelope, GatewayError, GatewayStatus, Hello, MaintenanceOp, PROTOCOL,
    Refusal, Reply, Request, Role, ServerEvent, Topic, TopicValue, Welcome, kind,
};
use super::wire::{self, Assembly};
use super::{
    GATEWAY_VERSION, MaintenanceMarker, MaintenancePurpose, OWNER_ENDPOINT, Profile, Program,
    SUPERVISOR_ENDPOINT, StartupContext, gateway_program, gui_program, known_contexts,
    maintenance_active, maintenance_marker, unix_ms, write_private,
};
use crate::settings::{Edit, Setting, SettingsError, SettingsService};

/// Exit codes of the `omp-deck-gateway` subcommands.
pub mod exit {
    pub const OK: i32 = 0;
    /// A real failure (message on stderr).
    pub const FAILED: i32 = 1;
    /// Invalid arguments.
    pub const USAGE: i32 = 2;
    /// No owner is running for this identity.
    pub const NOT_RUNNING: i32 = 3;
    /// `drain`: the deadline passed with accepted work left (the barrier stays up and the owner
    /// keeps running).
    pub const TIMEOUT: i32 = 4;
    /// The maintenance barrier is up.
    pub const BARRIER: i32 = super::super::server::exit::BARRIER;
    /// `drain`: the deadline passed while a direct-PTY program still runs (never killed).
    pub const DIRECT_PROGRAMS: i32 = 6;
    /// Supervisor: a newer installation took over; the OS restarts the registered program.
    pub const RESTART: i32 = 75;
}

/// Environment variable with the log level of `serve`/`supervise` (`error` … `trace`).
pub const LOG_ENV: &str = "OMP_DECK_GATEWAY_LOG";

/// Handshake / single-reply budget of a command-line link.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Single request budget while draining.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Drain/supervisor poll interval.
const POLL: Duration = Duration::from_millis(500);
/// How long a mode change or `resume` waits for the supervisor's hello.
const SUPERVISOR_WAIT: Duration = Duration::from_secs(40);
/// Between two `start` requests while waiting for a supervisor that is not there.
const RESTART_EVERY: Duration = Duration::from_secs(3);
/// Supervisor tick.
const TICK: Duration = Duration::from_secs(1);
/// After launching an owner, the supervisor waits this long for it before launching again.
const RELAUNCH_GRACE: Duration = Duration::from_secs(15);
/// Largest log file of `serve`/`supervise`; truncated when the next line would exceed it.
const MAX_LOG: u64 = 1 << 20;

fn endpoint(profile: &Profile, name: &str) -> deck_host::Result<Endpoint> {
    profile
        .id()
        .namespace()
        .and_then(|ns| Endpoint::new(&ns, name))
}

fn same_path(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(x), Ok(y)) if x == y
        )
}

/// `true` when both run the same file with the same leading arguments.
pub fn same_program(a: &Program, b: &Program) -> bool {
    a.args == b.args && same_path(&a.program, &b.program)
}

fn failed(why: impl Into<String>) -> GatewayError {
    GatewayError::Failed(why.into())
}

// ---- profile resolution -----------------------------------------------------------------------

/// Profile flags common to every subcommand.
#[derive(Clone, Debug, Default)]
pub struct ProfileArgs {
    pub startup: Option<PathBuf>,
    pub ns: Option<String>,
    pub settings: Option<PathBuf>,
    pub sessions_root: Option<PathBuf>,
}

/// The profile a subcommand serves: the startup context's (explicit flags must name the same
/// files) or the flags over the user's defaults.
pub fn resolve_profile(args: &ProfileArgs) -> Result<(Profile, Option<StartupContext>), String> {
    let absolute = |p: &Path| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    if let Some(ns) = &args.ns
        && !deck_host::valid_namespace(ns)
    {
        return Err(format!("--ns {ns:?} is not [A-Za-z0-9_-]{{1,40}}"));
    }
    if let Some(file) = &args.startup {
        let ctx = StartupContext::load(file)
            .map_err(|e| format!("reading the startup context {}: {e}", file.display()))?;
        let p = &ctx.profile;
        if let Some(ns) = &args.ns
            && *ns != p.host_namespace
        {
            return Err(format!(
                "--ns {ns} conflicts with the startup context ({})",
                p.host_namespace
            ));
        }
        if let Some(s) = &args.settings
            && !same_path(&absolute(s), &p.settings)
        {
            return Err(format!(
                "--settings {} conflicts with the startup context ({})",
                s.display(),
                p.settings.display()
            ));
        }
        if let Some(r) = &args.sessions_root
            && !same_path(&absolute(r), &p.sessions_root)
        {
            return Err(format!(
                "--sessions-root {} conflicts with the startup context ({})",
                r.display(),
                p.sessions_root.display()
            ));
        }
        return Ok((ctx.profile.clone(), Some(ctx)));
    }
    let profile = Profile {
        host_namespace: args
            .ns
            .clone()
            .unwrap_or_else(deck_core::mux::host_namespace),
        settings: args.settings.as_deref().map_or_else(
            || deck_core::config::config_dir().join("state.json"),
            absolute,
        ),
        sessions_root: args.sessions_root.as_deref().map_or_else(
            || deck_core::sessions::SessionsRoot::resolve().path,
            absolute,
        ),
    };
    Ok((profile, None))
}

/// Every identity under the profile's application root: the profile itself plus each one with
/// a startup context (the bundle-wide set `drain`, `resume` and `uninstall` act on).
pub fn identities(profile: &Profile) -> Vec<Profile> {
    let mut out = vec![profile.clone()];
    for (_, ctx) in known_contexts(&profile.app_root()) {
        if !out.iter().any(|p| p.id() == ctx.profile.id()) {
            out.push(ctx.profile);
        }
    }
    out
}

// ---- synchronous control link -----------------------------------------------------------------

/// Why a command-line link could not be opened.
#[derive(Debug)]
pub enum LinkError {
    /// No owner holds the endpoint.
    NotRunning,
    /// The owner refused the handshake.
    Refused(Refusal),
    Io(io::Error),
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LinkError::NotRunning => f.write_str("the gateway is not running"),
            LinkError::Refused(r) => write!(f, "the gateway refused the connection: {r}"),
            LinkError::Io(e) => write!(f, "gateway connection failed: {e}"),
        }
    }
}

impl std::error::Error for LinkError {}

/// What arrives on a control link.
pub enum Incoming {
    Reply(u64, Reply),
    Event(ServerEvent),
}

/// A blocking control link (command line and supervisor; native clients use
/// [`super::client`]). One request at a time; events are returned to the caller as they come.
pub struct Link {
    stream: Stream,
    assembly: Assembly,
    next_id: u64,
    /// This link's client id: the `client` of every mutation key it sends.
    pub client: String,
    pub welcome: Welcome,
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "timed out waiting for the gateway")
}

fn client_id(role: Role) -> String {
    let mut nonce = [0u8; 6];
    let _ = getrandom::fill(&mut nonce);
    let hex: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
    let role = match role {
        Role::Gui => "gui",
        Role::Cli => "cli",
        Role::Supervisor => "supervisor",
    };
    format!("{role}-{}-{hex}", std::process::id())
}

impl Link {
    /// Connects to the owner of `profile`'s identity and completes the handshake within
    /// `timeout`.
    pub fn connect(profile: &Profile, role: Role, timeout: Duration) -> Result<Link, LinkError> {
        let ep =
            endpoint(profile, OWNER_ENDPOINT).map_err(|e| LinkError::Io(io::Error::other(e)))?;
        let mut stream = match deck_host::transport::connect(&ep) {
            Ok(s) => s,
            Err(deck_host::Error::NotRunning(_)) => return Err(LinkError::NotRunning),
            Err(deck_host::Error::Io(e)) => return Err(LinkError::Io(e)),
            Err(e) => return Err(LinkError::Io(io::Error::other(e))),
        };
        let client = client_id(role);
        let hello = Hello {
            protocol: PROTOCOL,
            version: GATEWAY_VERSION.to_owned(),
            role,
            client_id: client.clone(),
            pid: std::process::id(),
            profile: profile.clone(),
            attach: None,
        };
        let handshake = (|| -> Result<Welcome, LinkError> {
            let bytes =
                serde_json::to_vec(&hello).map_err(|e| LinkError::Io(io::Error::other(e)))?;
            wire::write_frame(&mut stream, kind::HELLO, &bytes).map_err(LinkError::Io)?;
            stream.flush().map_err(LinkError::Io)?;
            stream
                .set_read_timeout(Some(timeout))
                .map_err(LinkError::Io)?;
            let invalid =
                |e: serde_json::Error| LinkError::Io(io::Error::new(io::ErrorKind::InvalidData, e));
            match wire::read_frame(&mut stream) {
                Ok(Some((kind::WELCOME, p))) => serde_json::from_slice(&p).map_err(invalid),
                Ok(Some((kind::REFUSE, p))) => Err(LinkError::Refused(
                    serde_json::from_slice(&p).map_err(invalid)?,
                )),
                Ok(Some((k, _))) => Err(LinkError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("expected WELCOME, got frame kind {k}"),
                ))),
                Ok(None) => Err(LinkError::Io(io::ErrorKind::UnexpectedEof.into())),
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    Err(LinkError::Io(timed_out()))
                }
                Err(e) => Err(LinkError::Io(e)),
            }
        })();
        match handshake {
            Ok(welcome) => Ok(Link {
                stream,
                assembly: Assembly::default(),
                next_id: 0,
                client,
                welcome,
            }),
            Err(e) => {
                stream.shutdown();
                Err(e)
            }
        }
    }

    /// Sends one request; returns its id.
    pub fn send(&mut self, body: Request) -> io::Result<u64> {
        self.next_id += 1;
        let id = self.next_id;
        let bytes = serde_json::to_vec(&Envelope { id, body }).map_err(io::Error::other)?;
        for chunk in wire::chunks(id, &bytes) {
            wire::write_frame(&mut self.stream, kind::REQUEST, &chunk)?;
        }
        self.stream.flush()?;
        Ok(id)
    }

    /// The next reply or event; `TimedOut` after `deadline`, `UnexpectedEof` once the owner
    /// closed the link. After a timeout the link is unusable (a frame may be half read).
    pub fn next(&mut self, deadline: Option<Instant>) -> io::Result<Incoming> {
        loop {
            let wait = match deadline {
                Some(d) => {
                    let now = Instant::now();
                    if now >= d {
                        return Err(timed_out());
                    }
                    Some((d - now).max(Duration::from_millis(1)))
                }
                None => None,
            };
            self.stream.set_read_timeout(wait)?;
            let frame = wire::read_frame(&mut self.stream).map_err(|e| {
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) {
                    timed_out()
                } else {
                    e
                }
            })?;
            match frame {
                Some((kind::REPLY, chunk)) => {
                    if let Some((id, bytes)) = self.assembly.push(&chunk)? {
                        let reply: Reply = serde_json::from_slice(&bytes)
                            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                        return Ok(Incoming::Reply(id, reply));
                    }
                }
                Some((kind::EVENT, payload)) => match serde_json::from_slice(&payload) {
                    Ok(ev) => return Ok(Incoming::Event(ev)),
                    Err(e) => log::debug!("skipping an unreadable gateway event: {e}"),
                },
                Some((k, _)) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unexpected frame kind {k}"),
                    ));
                }
                None => return Err(io::ErrorKind::UnexpectedEof.into()),
            }
        }
    }

    /// Sends `body` and waits for its reply (events meanwhile are dropped).
    pub fn request(&mut self, body: Request, timeout: Duration) -> io::Result<Reply> {
        let deadline = Instant::now() + timeout;
        let id = self.send(body)?;
        loop {
            if let Incoming::Reply(rid, reply) = self.next(Some(deadline))?
                && rid == id
            {
                return Ok(reply);
            }
        }
    }

    /// Leaves cleanly.
    pub fn bye(mut self) {
        let _ = wire::write_frame(&mut self.stream, kind::BYE, &[]);
        let _ = self.stream.flush();
        self.stream.shutdown();
    }
}

/// What holds the owner endpoint of an identity.
#[derive(Debug)]
pub enum OwnerProbe {
    /// An owner serving this profile.
    Running {
        pid: u32,
    },
    NotRunning,
    /// The identity's owner serves another profile.
    OtherProfile(Profile),
    Maintenance(MaintenanceMarker),
    /// The owner is exiting.
    ShuttingDown,
    /// The endpoint exists but did not complete a handshake.
    Unreachable(String),
}

/// Asks the owner endpoint who is there (one command-line handshake).
pub fn probe_owner(profile: &Profile) -> OwnerProbe {
    match Link::connect(profile, Role::Cli, CONNECT_TIMEOUT) {
        Ok(link) => {
            let pid = link.welcome.pid;
            link.bye();
            OwnerProbe::Running { pid }
        }
        Err(LinkError::NotRunning) => OwnerProbe::NotRunning,
        Err(LinkError::Refused(Refusal::ProfileMismatch { served })) => {
            OwnerProbe::OtherProfile(served)
        }
        Err(LinkError::Refused(Refusal::Maintenance(m))) => OwnerProbe::Maintenance(m),
        Err(LinkError::Refused(Refusal::ShuttingDown)) => OwnerProbe::ShuttingDown,
        Err(e) => OwnerProbe::Unreachable(e.to_string()),
    }
}

// ---- supervisor endpoint ----------------------------------------------------------------------

/// What a supervisor answers on its endpoint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupervisorHello {
    pub pid: u32,
    pub version: String,
    /// The program it runs from ([`gateway_program`] of the supervisor process).
    pub program: Program,
    /// Its startup context file.
    pub startup: PathBuf,
}

/// Takes the supervisor singleton lock of `profile`'s identity.
pub fn bind_supervisor(profile: &Profile) -> deck_host::Result<Listener> {
    Listener::bind(&endpoint(profile, SUPERVISOR_ENDPOINT)?)
}

/// Answers every connection on `listener` with `hello` until the listener is removed.
pub fn answer_hellos(listener: &Listener, hello: &SupervisorHello) {
    let Ok(bytes) = serde_json::to_vec(hello) else {
        return;
    };
    loop {
        let mut s = match listener.accept() {
            Ok(s) => s,
            Err(e) => {
                log::debug!("supervisor endpoint closed: {e}");
                return;
            }
        };
        let _ = wire::write_frame(&mut s, kind::WELCOME, &bytes).and_then(|()| s.flush());
        s.shutdown();
    }
}

/// The hello of the running supervisor of `profile`'s identity, if one answers.
pub fn supervisor_hello(profile: &Profile) -> Option<SupervisorHello> {
    let ep = endpoint(profile, SUPERVISOR_ENDPOINT).ok()?;
    let mut s = deck_host::transport::connect(&ep).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    let frame = wire::read_frame(&mut s);
    s.shutdown();
    match frame {
        Ok(Some((kind::WELCOME, p))) => serde_json::from_slice(&p).ok(),
        _ => None,
    }
}

/// Whether a supervisor holds `profile`'s supervisor endpoint (a local connect, no handshake).
pub fn supervisor_running(profile: &Profile) -> bool {
    endpoint(profile, SUPERVISOR_ENDPOINT)
        .and_then(|ep| deck_host::transport::connect(&ep))
        .map(|s| s.shutdown())
        .is_ok()
}

// ---- maintenance barrier ----------------------------------------------------------------------

/// Raises the bundle-wide barrier (persistent; overwrites an earlier marker's purpose).
pub fn raise_barrier(
    app_root: &Path,
    purpose: MaintenancePurpose,
) -> io::Result<MaintenanceMarker> {
    let marker = MaintenanceMarker {
        purpose,
        since_unix_ms: unix_ms(),
        by_version: GATEWAY_VERSION.to_owned(),
    };
    let bytes = serde_json::to_vec_pretty(&marker).map_err(io::Error::other)?;
    write_private(&maintenance_marker(app_root), &bytes)?;
    Ok(marker)
}

/// Lowers the barrier; `Ok(false)` when it was not up.
pub fn clear_barrier(app_root: &Path) -> io::Result<bool> {
    match std::fs::remove_file(maintenance_marker(app_root)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

// ---- mode and OS supervision ------------------------------------------------------------------

/// The committed mode in a settings file: `Off` when the file does not exist; an unreadable or
/// corrupt file is an error (the caller keeps what it knew).
pub fn read_mode(settings: &Path) -> io::Result<GatewayMode> {
    match std::fs::read(settings) {
        Ok(bytes) => deck_core::config::parse_config(&bytes)
            .map(|c| c.gateway_mode)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(GatewayMode::Off),
        Err(e) => Err(e),
    }
}

/// The OS registration of `context` with the startup file at `file`.
fn startup_for(context: &StartupContext, file: &Path) -> Startup {
    let (program, args) = context.supervise_command(file);
    Startup {
        namespace: context.profile.host_namespace.clone(),
        program,
        args,
    }
}

/// Commits `mode` through the settings writer (compare-and-set on `gateway_mode`; a concurrent
/// change of the same field is rebased onto and retried).
fn commit_mode(settings: &SettingsService, mode: GatewayMode) -> Result<(), SettingsError> {
    let mut base = settings.snapshot().config;
    let mut tries = 0;
    loop {
        let edit = Edit {
            base,
            changes: vec![Setting::GatewayMode(mode)],
        };
        match settings.submit(edit).wait() {
            Ok(_) => return Ok(()),
            Err(SettingsError::Conflict { current, .. }) if tries < 3 => {
                tries += 1;
                base = current.config;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Waits until a supervisor of `expect`'s program and this version answers on `profile`'s
/// endpoint; re-requests the start while none is there (an older supervisor leaves on its own
/// once it sees the new startup context).
fn await_supervisor(
    profile: &Profile,
    registration: &Registration,
    expect: &Program,
    timeout: Duration,
) -> Result<SupervisorHello, String> {
    let deadline = Instant::now() + timeout;
    let mut last_start = Instant::now();
    let mut start_error: Option<String> = None;
    let mut other: Option<SupervisorHello> = None;
    loop {
        match supervisor_hello(profile) {
            Some(h) if h.version == GATEWAY_VERSION && same_program(&h.program, expect) => {
                return Ok(h);
            }
            Some(h) => other = Some(h),
            None if last_start.elapsed() >= RESTART_EVERY => {
                last_start = Instant::now();
                start_error = autostart::start(registration).err().map(|e| e.to_string());
            }
            None => {}
        }
        if Instant::now() >= deadline {
            let mut why = format!(
                "the gateway supervisor ({}) did not start within {} s",
                registration.name,
                timeout.as_secs()
            );
            if let Some(h) = other {
                why.push_str(&format!(
                    "; an older supervisor (pid {}, version {}, {}) is still running",
                    h.pid,
                    h.version,
                    h.program.program.display()
                ));
            }
            if let Some(e) = start_error {
                why.push_str(&format!("; {e}"));
            }
            return Err(why);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Registers, starts and waits for the supervisor of the context saved at `file`.
fn start_supervision(context: &StartupContext, file: &Path) -> Result<SupervisorHello, String> {
    let registration = autostart::register(&startup_for(context, file))
        .map_err(|e| format!("registering the gateway autostart: {e}"))?;
    autostart::start(&registration).map_err(|e| format!("starting the gateway supervisor: {e}"))?;
    await_supervisor(
        &context.profile,
        &registration,
        &context.gateway,
        SUPERVISOR_WAIT,
    )
}

/// The lifetime transaction of a mode change (blocking: OS commands and the supervisor's
/// hello; run it off the engine loop).
///
/// Background: saves the startup context (private) → registers OS supervision → commits the
/// mode → starts the supervisor → waits for its hello. On failure the mode, a newly made
/// registration and the previous startup context are restored and the real error returned.
/// Off/WhileOpen: commits the mode, then removes the registration (nothing is killed; the
/// supervisor leaves on its own when it sees the mode).
pub fn set_mode(
    context: &StartupContext,
    settings: &SettingsService,
    desired: GatewayMode,
) -> Result<(), GatewayError> {
    let profile = &context.profile;
    if let Some(m) = maintenance_active(&profile.app_root()) {
        return Err(GatewayError::Maintenance(m));
    }
    if desired != GatewayMode::Background {
        commit_mode(settings, desired).map_err(GatewayError::Settings)?;
        return match autostart::remove(&profile.host_namespace) {
            Ok(_) | Err(AutostartError::Unavailable(_)) => Ok(()),
            Err(e) => Err(failed(format!(
                "the gateway mode is now {desired:?}, but removing its autostart failed: {e}"
            ))),
        };
    }
    let previous = settings.snapshot().config.gateway_mode;
    let file = StartupContext::path_in(&profile.state_dir());
    let old_context = std::fs::read(&file).ok();
    let startup = startup_for(context, &file);
    // Only a registration this call creates is removed again on failure; an existing one was
    // repaired for the context saved below and stays consistent with it.
    let prior = autostart::inspect(&startup).ok().flatten();
    context.save(&file).map_err(|e| {
        failed(format!(
            "saving the startup context {}: {e}",
            file.display()
        ))
    })?;
    let rollback = |mode: bool| {
        if mode
            && previous != GatewayMode::Background
            && let Err(e) = commit_mode(settings, previous)
        {
            log::error!("restoring gateway mode {previous:?}: {e}");
        }
        if prior.is_none() {
            if let Err(e) = autostart::remove(&profile.host_namespace) {
                log::error!("removing the failed gateway autostart: {e}");
            }
            let restored = match &old_context {
                Some(bytes) => write_private(&file, bytes),
                None => std::fs::remove_file(&file).or_else(|e| {
                    if e.kind() == io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                }),
            };
            if let Err(e) = restored {
                log::error!("restoring {}: {e}", file.display());
            }
        }
    };
    let registration = match autostart::register(&startup) {
        Ok(r) => r,
        Err(e) => {
            rollback(false);
            return Err(failed(format!("registering the gateway autostart: {e}")));
        }
    };
    if let Err(e) = commit_mode(settings, GatewayMode::Background) {
        rollback(false);
        return Err(GatewayError::Settings(e));
    }
    if let Err(e) = autostart::start(&registration) {
        rollback(true);
        return Err(failed(format!("starting the gateway supervisor: {e}")));
    }
    match await_supervisor(profile, &registration, &context.gateway, SUPERVISOR_WAIT) {
        Ok(h) => {
            log::info!(
                "gateway supervisor pid {} answered ({})",
                h.pid,
                registration.name
            );
            Ok(())
        }
        Err(why) => {
            rollback(true);
            Err(failed(why))
        }
    }
}

/// The OS registration of `context`'s identity, for status display; `None` when there is none
/// or it cannot be inspected (blocking: runs the OS tool).
pub fn registration_status(context: &StartupContext) -> Option<String> {
    let file = StartupContext::path_in(&context.profile.state_dir());
    match autostart::inspect(&startup_for(context, &file)) {
        Ok(Some(i)) => {
            let mut s = i.registration.name;
            if !i.enabled {
                s.push_str(" (disabled)");
            }
            if !i.current {
                s.push_str(" (outdated)");
            }
            Some(s)
        }
        Ok(None) => None,
        Err(e) => {
            log::debug!("inspecting the gateway autostart: {e}");
            None
        }
    }
}

// ---- supervisor -------------------------------------------------------------------------------

/// Size and modification time: changes when the file is replaced.
fn stamp(path: &Path) -> Option<(u64, SystemTime)> {
    let m = std::fs::metadata(path).ok()?;
    Some((m.len(), m.modified().ok()?))
}

/// How long the supervisor's own program must stay missing before it treats OmpDeck as removed
/// (a package upgrade removes and re-creates it within moments).
const MISSING_GRACE: Duration = Duration::from_secs(30);
/// One drain attempt while retiring; repeated (and logged) while work remains.
const RETIRE_DRAIN: Duration = Duration::from_secs(120);
/// Pause after a failed drain attempt while retiring.
const RETIRE_RETRY: Duration = Duration::from_secs(10);

/// Decides when a program that went missing is gone for good: absent continuously for
/// `grace`. Any observation of it present resets the decision.
#[derive(Debug)]
pub(crate) struct MissingWatch {
    grace: Duration,
    since: Option<Instant>,
}

impl MissingWatch {
    pub(crate) fn new(grace: Duration) -> MissingWatch {
        MissingWatch { grace, since: None }
    }

    /// Records one check at `now`; `true` once it has been missing for at least `grace`.
    pub(crate) fn observe(&mut self, present: bool, now: Instant) -> bool {
        if present {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        now.saturating_duration_since(since) >= self.grace
    }

    /// When the current absence began.
    pub(crate) fn missing_since(&self) -> Option<Instant> {
        self.since
    }
}

/// The files whose disappearance means OmpDeck was uninstalled under this supervisor: the
/// program it was started as (the stable image for an AppImage) and its own executable.
fn watched_files(own: &Program) -> Vec<PathBuf> {
    let mut out = vec![own.program.clone()];
    if let Ok(exe) = std::env::current_exe()
        && exe != own.program
    {
        out.push(exe);
    }
    out
}

/// OmpDeck was removed (a system package uninstall cannot reach per-user registrations): drains
/// the owner (as an uninstall drain, without a persistent barrier, so a reinstall just works),
/// removes this user's autostart registration and exits 0. The settings mode is left as it is.
/// If the program reappears meanwhile, the supervisor restarts into it instead.
fn retire(profile: &Profile, watched: &[PathBuf]) -> i32 {
    log::warn!(
        "{} missing for {} s: OmpDeck was removed; draining the gateway and removing this \
         user's autostart registration",
        watched
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        MISSING_GRACE.as_secs()
    );
    loop {
        match drain_owner(
            profile,
            MaintenancePurpose::Uninstall,
            Instant::now() + RETIRE_DRAIN,
        ) {
            OwnerDrain::Gone => {
                log::info!("the gateway owner is gone");
                break;
            }
            OwnerDrain::Timeout(state) => {
                log::warn!("the gateway owner still has accepted work ({state:?}); waiting for it");
            }
            OwnerDrain::Failed(e) => {
                log::error!("draining the gateway owner: {e}; retrying");
                std::thread::sleep(RETIRE_RETRY);
            }
        }
    }
    if watched.iter().all(|p| p.exists()) {
        log::info!("OmpDeck was reinstalled meanwhile: supervisor restarts");
        return exit::RESTART;
    }
    match autostart::remove(&profile.host_namespace) {
        Ok(true) => log::info!(
            "removed the gateway autostart for {}",
            profile.host_namespace
        ),
        Ok(false) => log::info!(
            "no gateway autostart registered for {}",
            profile.host_namespace
        ),
        Err(e) => log::error!(
            "removing the gateway autostart for {}: {e}",
            profile.host_namespace
        ),
    }
    log::info!("supervisor exits after the removal of OmpDeck");
    exit::OK
}

/// Starts the owner of `context` detached.
fn launch_owner(context: &StartupContext, file: &Path) -> io::Result<()> {
    let (program, args) = context.serve_command(file);
    let launcher = deck_host::launch_detached(&program, &args)?;
    #[cfg(unix)]
    {
        let mut launcher = launcher;
        // Reaps the launcher (it exits once the owner is ready; in an AppImage it is the owner).
        std::thread::Builder::new()
            .name("gateway-launcher".into())
            .spawn(move || {
                let _ = launcher.wait();
            })?;
    }
    #[cfg(windows)]
    drop(launcher);
    Ok(())
}

/// Holds a supervisor control link open on its own thread; `alive` turns false when it closes.
fn watch_owner(mut link: Link) -> io::Result<Arc<AtomicBool>> {
    let alive = Arc::new(AtomicBool::new(true));
    let flag = alive.clone();
    std::thread::Builder::new()
        .name("gateway-owner-link".into())
        .spawn(move || {
            while link.next(None).is_ok() {}
            flag.store(false, Ordering::SeqCst);
        })?;
    Ok(alive)
}

/// `omp-deck-gateway supervise`: see the module documentation. Returns the exit code.
pub fn supervise(startup_file: &Path, context: StartupContext) -> i32 {
    let mut context = context;
    let profile = context.profile.clone();
    let app_root = profile.app_root();
    if let Some(m) = maintenance_active(&app_root) {
        log::info!("not supervising: maintenance ({:?}) in progress", m.purpose);
        return exit::OK;
    }
    match read_mode(&profile.settings) {
        Ok(GatewayMode::Background) => {}
        Ok(mode) => {
            log::info!("not supervising: gateway mode is {mode:?}");
            return exit::OK;
        }
        Err(e) => {
            log::error!("reading {}: {e}", profile.settings.display());
            return exit::FAILED;
        }
    }
    let Some(own) = gateway_program() else {
        log::error!("cannot determine this gateway's own program");
        return exit::FAILED;
    };
    let listener = match bind_supervisor(&profile) {
        Ok(l) => Arc::new(l),
        Err(deck_host::Error::AlreadyRunning(_)) => {
            log::info!(
                "another supervisor already runs for {}",
                profile.id().as_str()
            );
            return exit::OK;
        }
        Err(e) => {
            log::error!("binding the supervisor endpoint: {e}");
            return exit::FAILED;
        }
    };
    let hello = SupervisorHello {
        pid: std::process::id(),
        version: GATEWAY_VERSION.to_owned(),
        program: own.clone(),
        startup: startup_file.to_path_buf(),
    };
    {
        let for_thread = listener.clone();
        let spawned = std::thread::Builder::new()
            .name("gateway-supervisor-hello".into())
            .spawn(move || answer_hellos(&for_thread, &hello));
        if let Err(e) = spawned {
            log::error!("starting the supervisor endpoint: {e}");
            listener.remove();
            return exit::FAILED;
        }
    }
    log::info!(
        "supervising {} ({}) from {}",
        profile.id().as_str(),
        profile.settings.display(),
        own.program.display()
    );
    let code = supervise_loop(startup_file, &mut context, &own);
    listener.remove();
    code
}

fn supervise_loop(startup_file: &Path, context: &mut StartupContext, own: &Program) -> i32 {
    let profile = context.profile.clone();
    let app_root = profile.app_root();
    let own_stamp = stamp(&own.program);
    let watched = watched_files(own);
    let mut missing = MissingWatch::new(MISSING_GRACE);
    let mut owner: Option<Arc<AtomicBool>> = None;
    let mut launched: Option<Instant> = None;
    let mut warned_profile = false;
    loop {
        if let Some(m) = maintenance_active(&app_root) {
            log::info!("maintenance ({:?}) started: supervisor exits", m.purpose);
            return exit::OK;
        }
        match read_mode(&profile.settings) {
            Ok(GatewayMode::Background) => {}
            Ok(mode) => {
                log::info!("gateway mode is now {mode:?}: supervisor exits");
                return exit::OK;
            }
            Err(e) => log::warn!("reading {}: {e}", profile.settings.display()),
        }
        match StartupContext::load(startup_file) {
            Ok(c) if !same_program(&c.gateway, own) => {
                log::info!(
                    "the startup context now runs {}: supervisor restarts",
                    c.gateway.program.display()
                );
                return exit::RESTART;
            }
            Ok(c) => *context = c,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                log::info!(
                    "startup context {} removed: supervisor exits",
                    startup_file.display()
                );
                return exit::OK;
            }
            Err(e) => log::warn!("reading {}: {e}", startup_file.display()),
        }
        let present = watched.iter().all(|p| p.exists());
        if present && stamp(&own.program) != own_stamp {
            log::info!(
                "{} was replaced: supervisor restarts",
                own.program.display()
            );
            return exit::RESTART;
        }
        let was_missing = missing.missing_since().is_some();
        if missing.observe(present, Instant::now()) {
            return retire(&context.profile, &watched);
        }
        if !present {
            if !was_missing {
                log::warn!(
                    "{} is missing; the gateway is retired unless it returns within {} s",
                    own.program.display(),
                    MISSING_GRACE.as_secs()
                );
            }
            // Nothing to launch from meanwhile.
            std::thread::sleep(TICK);
            continue;
        }
        if was_missing {
            log::info!("{} is back", own.program.display());
        }
        if !owner.as_ref().is_some_and(|a| a.load(Ordering::SeqCst)) {
            if owner.take().is_some() {
                log::warn!("lost the gateway owner");
            }
            match Link::connect(&context.profile, Role::Supervisor, CONNECT_TIMEOUT) {
                Ok(link) => {
                    log::info!("supervising gateway owner pid {}", link.welcome.pid);
                    warned_profile = false;
                    launched = None;
                    match watch_owner(link) {
                        Ok(alive) => owner = Some(alive),
                        Err(e) => log::error!("watching the owner: {e}"),
                    }
                }
                Err(LinkError::NotRunning) => {
                    if launched.is_none_or(|t| t.elapsed() >= RELAUNCH_GRACE) {
                        launched = Some(Instant::now());
                        match launch_owner(context, startup_file) {
                            Ok(()) => log::info!("started a gateway owner"),
                            Err(e) => log::error!("starting the gateway owner: {e}"),
                        }
                    }
                }
                Err(LinkError::Refused(Refusal::Maintenance(m))) => {
                    log::info!("maintenance ({:?}) started: supervisor exits", m.purpose);
                    return exit::OK;
                }
                Err(LinkError::Refused(Refusal::ProfileMismatch { served })) => {
                    if !warned_profile {
                        warned_profile = true;
                        log::warn!(
                            "the running gateway serves another profile ({}); not replacing it",
                            served.settings.display()
                        );
                    }
                }
                Err(e) => log::debug!("owner not reachable yet: {e}"),
            }
        }
        std::thread::sleep(TICK);
    }
}

// ---- command-line subcommands -----------------------------------------------------------------

/// `status [--json]`: 0 reachable, 3 not running.
pub fn status(profile: &Profile, json: bool) -> i32 {
    let barrier = maintenance_active(&profile.app_root());
    let not_running = |barrier: Option<MaintenanceMarker>| {
        if json {
            let v = serde_json::json!({ "running": false, "maintenance": barrier });
            println!("{v}");
        } else {
            println!("not running ({})", profile.id().as_str());
            if let Some(m) = barrier {
                println!("maintenance: {:?} since {} ms", m.purpose, m.since_unix_ms);
            }
        }
        exit::NOT_RUNNING
    };
    let mut link = match Link::connect(profile, Role::Cli, CONNECT_TIMEOUT) {
        Ok(l) => l,
        Err(LinkError::NotRunning) => return not_running(barrier),
        Err(LinkError::Refused(Refusal::Maintenance(m))) => {
            eprintln!(
                "the gateway refused: maintenance ({:?}) in progress",
                m.purpose
            );
            return exit::BARRIER;
        }
        Err(e) => {
            eprintln!("{e}");
            return exit::FAILED;
        }
    };
    let reply = link.request(
        Request::Query(super::protocol::Query::Topic(Topic::Gateway)),
        REQUEST_TIMEOUT,
    );
    let st: GatewayStatus = match reply {
        Ok(Ok(Answer::Topic(v))) => match *v {
            TopicValue::Gateway(s) => s,
            other => {
                eprintln!("unexpected answer: {other:?}");
                return exit::FAILED;
            }
        },
        Ok(Ok(other)) => {
            eprintln!("unexpected answer: {other:?}");
            return exit::FAILED;
        }
        Ok(Err(e)) => {
            eprintln!("{e}");
            return exit::FAILED;
        }
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return not_running(barrier),
        Err(e) => {
            eprintln!("gateway status: {e}");
            return exit::FAILED;
        }
    };
    let maintenance = link.welcome.maintenance.clone();
    link.bye();
    if json {
        let v = serde_json::json!({ "running": true, "status": st, "maintenance": maintenance });
        println!("{v}");
        return exit::OK;
    }
    println!(
        "running pid={} version={} ({})",
        st.pid,
        st.version,
        profile.id().as_str()
    );
    println!("settings: {}", st.profile.settings.display());
    println!("sessions: {}", st.profile.sessions_root.display());
    println!("mode: {:?} (effective {:?})", st.mode, st.effective);
    println!("supervision: {:?}", st.supervision);
    println!(
        "clients: {} gui, {} cli, supervisor {}",
        st.gui_clients,
        st.cli_clients,
        if st.supervisor { "connected" } else { "absent" }
    );
    println!(
        "jobs: {}, direct programs: {}",
        st.jobs.len(),
        st.direct_programs
    );
    for j in &st.jobs {
        println!("  {} {}#{}", j.label, j.key.client, j.key.seq);
    }
    if let Some(m) = maintenance {
        println!("maintenance: {:?} since {} ms", m.purpose, m.since_unix_ms);
    }
    exit::OK
}

/// `request '<json Request>'`: prints the reply JSON; 0 on `Ok`, 1 on an error reply.
pub fn request(profile: &Profile, json: &str, timeout: Duration) -> i32 {
    let mut body: Request = match serde_json::from_str(json) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("invalid request JSON: {e}");
            return exit::USAGE;
        }
    };
    let mut link = match Link::connect(profile, Role::Cli, CONNECT_TIMEOUT) {
        Ok(l) => l,
        Err(LinkError::NotRunning) => {
            eprintln!("the gateway is not running");
            return exit::NOT_RUNNING;
        }
        Err(e) => {
            eprintln!("{e}");
            return exit::FAILED;
        }
    };
    // A mutation key belongs to the link that sends it; the caller cannot know this link's id.
    if let Request::Mutate { key, .. } = &mut body {
        key.client.clone_from(&link.client);
    }
    let reply = match link.request(body, timeout) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("gateway request: {e}");
            return exit::FAILED;
        }
    };
    link.bye();
    match serde_json::to_string_pretty(&reply) {
        Ok(s) => println!("{s}"),
        Err(e) => {
            eprintln!("encoding the reply: {e}");
            return exit::FAILED;
        }
    }
    if reply.is_ok() {
        exit::OK
    } else {
        exit::FAILED
    }
}

/// Topics `watch` follows: every always-on topic.
const WATCH_TOPICS: &[Topic] = &[
    Topic::Gateway,
    Topic::Settings,
    Topic::Sessions,
    Topic::Running,
    Topic::Controllers,
    Topic::Notifications,
    Topic::Status,
    Topic::Usage,
    Topic::AppUpdate,
    Topic::Backup,
    Topic::OmpUpdate,
    Topic::Mobile,
];

/// `watch`: prints every event as one JSON line until the owner closes the link (0), or 3 when
/// none is running.
pub fn watch(profile: &Profile) -> i32 {
    let mut link = match Link::connect(profile, Role::Cli, CONNECT_TIMEOUT) {
        Ok(l) => l,
        Err(LinkError::NotRunning) => {
            eprintln!("the gateway is not running");
            return exit::NOT_RUNNING;
        }
        Err(e) => {
            eprintln!("{e}");
            return exit::FAILED;
        }
    };
    for t in WATCH_TOPICS {
        if let Err(e) = link.send(Request::Subscribe(t.clone())) {
            eprintln!("subscribing: {e}");
            return exit::FAILED;
        }
    }
    let stdout = io::stdout();
    loop {
        match link.next(None) {
            Ok(Incoming::Event(ev)) => {
                let Ok(line) = serde_json::to_string(&ev) else {
                    continue;
                };
                let mut out = stdout.lock();
                if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                    return exit::OK;
                }
            }
            Ok(Incoming::Reply(_, Err(e))) => eprintln!("{e}"),
            Ok(Incoming::Reply(_, Ok(_))) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                eprintln!("the gateway closed the connection");
                return exit::OK;
            }
            Err(e) => {
                eprintln!("gateway connection: {e}");
                return exit::FAILED;
            }
        }
    }
}

enum OwnerDrain {
    Gone,
    Timeout(Option<DrainState>),
    Failed(String),
}

enum LinkDrain {
    ExitSent,
    Lost,
    Deadline,
    Failed(String),
}

fn report(state: &DrainState, last: Option<&DrainState>) {
    let text = |s: &DrainState| match s {
        DrainState::Draining {
            jobs,
            controllers_busy,
            direct_programs,
        } => format!(
            "draining: {jobs} jobs, {controllers_busy} busy sessions, {direct_programs} direct programs"
        ),
        DrainState::Drained => "drained".to_owned(),
    };
    let now = text(state);
    if last.map(text).as_deref() != Some(now.as_str()) {
        eprintln!("{now}");
    }
}

fn drain_link(
    link: &mut Link,
    purpose: MaintenancePurpose,
    deadline: Instant,
    last: &mut Option<DrainState>,
) -> LinkDrain {
    let mut op = MaintenanceOp::Begin(purpose);
    loop {
        let now = Instant::now();
        if now >= deadline {
            return LinkDrain::Deadline;
        }
        let wait = (deadline - now).min(REQUEST_TIMEOUT);
        match link.request(Request::Maintenance(op), wait) {
            Ok(Ok(Answer::Maintenance(state))) => {
                report(&state, last.as_ref());
                let drained = matches!(state, DrainState::Drained);
                *last = Some(state);
                if drained {
                    let wait = deadline
                        .saturating_duration_since(Instant::now())
                        .min(REQUEST_TIMEOUT);
                    return match link.request(Request::Maintenance(MaintenanceOp::Exit), wait) {
                        Ok(Ok(_)) => LinkDrain::ExitSent,
                        Ok(Err(e)) => LinkDrain::Failed(e.to_string()),
                        Err(_) => LinkDrain::Lost,
                    };
                }
            }
            Ok(Ok(other)) => return LinkDrain::Failed(format!("unexpected answer: {other:?}")),
            Ok(Err(e)) => return LinkDrain::Failed(e.to_string()),
            Err(_) => return LinkDrain::Lost,
        }
        op = MaintenanceOp::Poll;
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

fn drain_owner(profile: &Profile, purpose: MaintenancePurpose, deadline: Instant) -> OwnerDrain {
    let mut profile = profile.clone();
    let mut last: Option<DrainState> = None;
    loop {
        let now = Instant::now();
        if now >= deadline {
            return OwnerDrain::Timeout(last);
        }
        match Link::connect(&profile, Role::Cli, (deadline - now).min(CONNECT_TIMEOUT)) {
            Err(LinkError::NotRunning) => return OwnerDrain::Gone,
            Err(LinkError::Refused(Refusal::ProfileMismatch { served })) if served != profile => {
                // Maintenance acts on whoever holds the identity.
                profile = served;
                continue;
            }
            Err(e) => log::debug!("draining {}: {e}", profile.id().as_str()),
            Ok(mut link) => match drain_link(&mut link, purpose, deadline, &mut last) {
                LinkDrain::ExitSent | LinkDrain::Lost => {}
                LinkDrain::Deadline => return OwnerDrain::Timeout(last),
                LinkDrain::Failed(e) => return OwnerDrain::Failed(e),
            },
        }
        std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
    }
}

/// `drain --purpose …`: raises the barrier, drains and stops every owner of the bundle and waits
/// for their supervisors to leave. 0 drained (or nothing ran), 4 timeout, 6 a direct program
/// still runs, 1 failure. The barrier stays up in every case.
pub fn drain(profile: &Profile, purpose: MaintenancePurpose, timeout: Duration) -> i32 {
    let app_root = profile.app_root();
    if let Err(e) = raise_barrier(&app_root, purpose) {
        eprintln!(
            "raising the maintenance barrier {}: {e}",
            maintenance_marker(&app_root).display()
        );
        return exit::FAILED;
    }
    let deadline = Instant::now() + timeout;
    let profiles = identities(profile);
    let mut timed_out = false;
    let mut direct = 0usize;
    for p in &profiles {
        match drain_owner(p, purpose, deadline) {
            OwnerDrain::Gone => {}
            OwnerDrain::Timeout(state) => {
                timed_out = true;
                if let Some(DrainState::Draining {
                    direct_programs, ..
                }) = state
                {
                    direct += direct_programs;
                }
                eprintln!("the gateway for {} did not drain in time", p.id().as_str());
            }
            OwnerDrain::Failed(e) => {
                eprintln!("draining the gateway for {}: {e}", p.id().as_str());
                return exit::FAILED;
            }
        }
    }
    for p in &profiles {
        while supervisor_running(p) {
            if Instant::now() >= deadline {
                timed_out = true;
                eprintln!(
                    "the gateway supervisor for {} is still running",
                    p.id().as_str()
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    if !timed_out {
        exit::OK
    } else if direct > 0 {
        exit::DIRECT_PROGRAMS
    } else {
        exit::TIMEOUT
    }
}

/// `uninstall`: drains for removal, then removes every OmpDeck autostart registration of the
/// bundle (never kills anything). The barrier stays up.
pub fn uninstall(profile: &Profile, timeout: Duration) -> i32 {
    let mut code = drain(profile, MaintenancePurpose::Uninstall, timeout);
    for p in identities(profile) {
        match autostart::remove(&p.host_namespace) {
            Ok(true) => eprintln!("removed the gateway autostart for {}", p.host_namespace),
            Ok(false) | Err(AutostartError::Unavailable(_)) => {}
            Err(e) => {
                eprintln!(
                    "removing the gateway autostart for {}: {e}",
                    p.host_namespace
                );
                if code == exit::OK {
                    code = exit::FAILED;
                }
            }
        }
    }
    code
}

/// `resume`: lowers the barrier, repairs every startup context for this installation and
/// restarts supervision where the mode is Background, waiting for the new supervisor.
pub fn resume(profile: &Profile) -> i32 {
    let Some(gateway) = gateway_program() else {
        eprintln!("cannot determine this gateway's program");
        return exit::FAILED;
    };
    let gui = gui_program();
    match resume_in(&profile.app_root(), &gateway, gui.as_ref(), |ctx, file| {
        start_supervision(ctx, file).map(|h| format!("supervisor pid {}", h.pid))
    }) {
        Ok(notes) => {
            for n in notes {
                eprintln!("{n}");
            }
            exit::OK
        }
        Err(e) => {
            eprintln!("{e}");
            exit::FAILED
        }
    }
}

/// [`resume`] over `app_root` with the launchers of this installation; `supervise` restarts
/// supervision of one Background context saved at the given file.
pub(crate) fn resume_in(
    app_root: &Path,
    gateway: &Program,
    gui: Option<&Program>,
    supervise: impl Fn(&StartupContext, &Path) -> Result<String, String>,
) -> Result<Vec<String>, String> {
    clear_barrier(app_root).map_err(|e| {
        format!(
            "removing the maintenance barrier {}: {e}",
            maintenance_marker(app_root).display()
        )
    })?;
    let mut notes = Vec::new();
    let mut errors = Vec::new();
    for (file, mut ctx) in known_contexts(app_root) {
        let id = ctx.profile.id();
        ctx.repair_for_install(gateway.clone(), gui.cloned());
        if let Err(e) = ctx.save(&file) {
            errors.push(format!("{}: saving {}: {e}", id.as_str(), file.display()));
            continue;
        }
        match read_mode(&ctx.profile.settings) {
            Ok(GatewayMode::Background) => match supervise(&ctx, &file) {
                Ok(n) => notes.push(format!("{}: {n}", id.as_str())),
                Err(e) => errors.push(format!("{}: {e}", id.as_str())),
            },
            Ok(mode) => notes.push(format!("{}: repaired (mode {mode:?})", id.as_str())),
            Err(e) => errors.push(format!(
                "{}: reading {}: {e}",
                id.as_str(),
                ctx.profile.settings.display()
            )),
        }
    }
    if errors.is_empty() {
        Ok(notes)
    } else {
        Err(errors.join("\n"))
    }
}

// ---- logging ----------------------------------------------------------------------------------

struct FileLog {
    file: Mutex<(std::fs::File, u64)>,
    level: log::LevelFilter,
}

impl log::Log for FileLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!(
            "{} {} {} {}\n",
            unix_ms(),
            std::process::id(),
            record.level(),
            record.args()
        );
        let mut guard = self.file.lock().unwrap_or_else(PoisonError::into_inner);
        let (file, len) = &mut *guard;
        if *len + line.len() as u64 > MAX_LOG {
            if file.set_len(0).is_err() {
                return;
            }
            *len = 0;
        }
        if file.write_all(line.as_bytes()).is_ok() {
            *len += line.len() as u64;
        }
    }

    fn flush(&self) {
        let _ = self
            .file
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .flush();
    }
}

fn log_level(default: log::LevelFilter) -> log::LevelFilter {
    std::env::var(LOG_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Logs of a background process to an owner-only file kept ≤ 1 MiB.
pub fn init_file_log(path: &Path) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        super::private_dir(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(path)?;
    let len = file.metadata()?.len();
    let level = log_level(log::LevelFilter::Info);
    static LOGGER: std::sync::OnceLock<WireCap<FileLog>> = std::sync::OnceLock::new();
    let mut fresh = Some(WireCap(FileLog {
        file: Mutex::new((file, len)),
        level,
    }));
    let logger = LOGGER.get_or_init(|| fresh.take().expect("initialized once"));
    if fresh.is_some() {
        return Err(io::Error::other("the gateway log is already initialized"));
    }
    log::set_logger(logger).map_err(io::Error::other)?;
    log::set_max_level(level);
    Ok(())
}

/// Logs of a foreground command to stderr (`warn` unless [`LOG_ENV`] says otherwise).
pub fn init_stderr_log() {
    let logger =
        env_logger::Builder::from_env(env_logger::Env::default().filter_or(LOG_ENV, "warn"))
            .build();
    let max = logger.filter();
    if log::set_boxed_logger(Box::new(WireCap(logger))).is_ok() {
        log::set_max_level(max);
    }
}

/// Crates whose debug/trace output dumps raw wire bytes (the relay hello carries the room
/// secret, device payloads the tokens): never below `info`, whatever [`LOG_ENV`] asks.
const WIRE_TARGETS: [&str; 4] = ["tungstenite", "tokio_tungstenite", "rustls", "tokio_rustls"];

/// The most verbose level `target` may log at: `info` for a [`WIRE_TARGETS`] crate (the crate
/// name itself or any `crate::module` path below it; `rustlsfoo` is another crate and is not
/// clamped), else unlimited.
fn target_cap(target: &str) -> log::LevelFilter {
    let wire = WIRE_TARGETS.iter().any(|w| {
        target
            .strip_prefix(w)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
    });
    if wire {
        log::LevelFilter::Info
    } else {
        log::LevelFilter::Trace
    }
}

/// Applies [`target_cap`] before the wrapped logger sees a record. A filter inside the logger
/// is not enough: env_logger lets the most specific directive win, so
/// `RUST_LOG=tungstenite::protocol=trace` would get past a `tungstenite=info` directive.
struct WireCap<L>(L);

impl<L: log::Log> log::Log for WireCap<L> {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= target_cap(metadata.target()) && self.0.enabled(metadata)
    }

    fn log(&self, record: &log::Record) {
        if record.level() <= target_cap(record.target()) {
            self.0.log(record);
        }
    }

    fn flush(&self) {
        self.0.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    use crate::controller::ServiceEpoch;

    fn profile(dir: &Path, tag: &str) -> Profile {
        Profile {
            host_namespace: format!("omp-deck-test-{}-{tag}", std::process::id()),
            settings: dir.join("state.json"),
            sessions_root: dir.join("sessions"),
        }
    }

    fn write_mode(settings: &Path, mode: GatewayMode) {
        let cfg = deck_core::config::AppConfig {
            gateway_mode: mode,
            ..Default::default()
        };
        std::fs::write(settings, serde_json::to_vec(&cfg).unwrap()).unwrap();
    }

    /// A minimal owner on `profile`'s endpoint: welcomes every link and answers each
    /// maintenance request with `state` (and `Done` to `Exit`).
    fn fake_owner(profile: &Profile, state: DrainState) -> Arc<Listener> {
        let listener =
            Arc::new(Listener::bind(&endpoint(profile, OWNER_ENDPOINT).unwrap()).unwrap());
        let (l, served) = (listener.clone(), profile.clone());
        std::thread::spawn(move || {
            while let Ok(mut s) = l.accept() {
                let (served, state) = (served.clone(), state.clone());
                std::thread::spawn(move || {
                    let Ok(Some((kind::HELLO, _))) = wire::read_frame(&mut s) else {
                        return;
                    };
                    let welcome = Welcome {
                        protocol: PROTOCOL,
                        version: GATEWAY_VERSION.into(),
                        epoch: ServiceEpoch::current(),
                        pid: 4242,
                        profile: served,
                        mode: GatewayMode::Off,
                        maintenance: None,
                    };
                    let bytes = serde_json::to_vec(&welcome).unwrap();
                    wire::write_frame(&mut s, kind::WELCOME, &bytes).unwrap();
                    let mut asm = Assembly::default();
                    while let Ok(Some((kind::REQUEST, chunk))) = wire::read_frame(&mut s) {
                        let Ok(Some((id, bytes))) = asm.push(&chunk) else {
                            continue;
                        };
                        let env: Envelope = serde_json::from_slice(&bytes).unwrap();
                        let reply: Reply = match env.body {
                            Request::Maintenance(MaintenanceOp::Exit) => Ok(Answer::Done),
                            Request::Maintenance(_) => Ok(Answer::Maintenance(state.clone())),
                            _ => Err(GatewayError::Refused("test owner".into())),
                        };
                        let bytes = serde_json::to_vec(&reply).unwrap();
                        for c in wire::chunks(id, &bytes) {
                            if wire::write_frame(&mut s, kind::REPLY, &c).is_err() {
                                return;
                            }
                        }
                    }
                });
            }
        });
        listener
    }

    #[test]
    fn a_program_counts_as_removed_only_after_missing_continuously_for_the_grace() {
        let grace = Duration::from_secs(30);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = MissingWatch::new(grace);
        assert!(!w.observe(true, at(0)));
        assert_eq!(w.missing_since(), None);
        // A package upgrade: gone for a while, then back; the absence is forgotten.
        assert!(!w.observe(false, at(1)));
        assert_eq!(w.missing_since(), Some(at(1)));
        assert!(!w.observe(false, at(30)));
        assert!(!w.observe(true, at(31)));
        assert_eq!(w.missing_since(), None);
        // Missing again: the grace counts from the new absence, not the first one.
        assert!(!w.observe(false, at(40)));
        assert!(!w.observe(false, at(69)));
        assert!(w.observe(false, at(70)));
        assert!(w.observe(false, at(75)));
        // A clock that stepped back never fires early.
        let mut w = MissingWatch::new(grace);
        assert!(!w.observe(false, at(100)));
        assert!(!w.observe(false, at(50)));
    }

    #[test]
    fn the_barrier_is_persistent_until_cleared_and_blocks_serve() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(maintenance_active(root).is_none());
        assert!(!clear_barrier(root).unwrap());
        let m = raise_barrier(root, MaintenancePurpose::Update).unwrap();
        assert_eq!(maintenance_active(root), Some(m));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(maintenance_marker(root))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0, "the marker is private");
        }
        // An owner refuses to start under it, before taking any lock.
        let code = super::super::server::serve(super::super::server::ServeOptions {
            profile: profile(root, "bar"),
            startup_file: None,
            ready: None,
        });
        assert_eq!(code, exit::BARRIER);
        assert!(clear_barrier(root).unwrap());
        assert!(maintenance_active(root).is_none());
    }

    #[test]
    fn drain_without_an_owner_raises_the_barrier_and_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), "none");
        assert_eq!(status(&p, true), exit::NOT_RUNNING);
        assert_eq!(
            drain(&p, MaintenancePurpose::Uninstall, Duration::from_secs(5)),
            exit::OK
        );
        let m = maintenance_active(dir.path()).expect("barrier stays up");
        assert_eq!(m.purpose, MaintenancePurpose::Uninstall);
        assert_eq!(m.by_version, GATEWAY_VERSION);
    }

    #[test]
    fn drain_timeouts_distinguish_direct_programs_and_keep_the_barrier() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), "dir");
        let owner = fake_owner(
            &p,
            DrainState::Draining {
                jobs: 0,
                controllers_busy: 0,
                direct_programs: 1,
            },
        );
        assert!(matches!(probe_owner(&p), OwnerProbe::Running { pid: 4242 }));
        assert_eq!(
            drain(&p, MaintenancePurpose::Update, Duration::from_millis(1500)),
            exit::DIRECT_PROGRAMS
        );
        assert!(maintenance_active(dir.path()).is_some());
        owner.remove();

        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), "job");
        let owner = fake_owner(
            &p,
            DrainState::Draining {
                jobs: 1,
                controllers_busy: 0,
                direct_programs: 0,
            },
        );
        assert_eq!(
            drain(&p, MaintenancePurpose::Update, Duration::from_millis(1500)),
            exit::TIMEOUT
        );
        assert!(maintenance_active(dir.path()).is_some());
        owner.remove();
    }

    #[test]
    fn a_drained_owner_is_told_to_exit_and_drain_waits_until_it_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), "ok");
        let owner = fake_owner(&p, DrainState::Drained);
        // The fake owner "exits" (stops listening) a moment after it was asked to.
        let o = owner.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            o.remove();
        });
        assert_eq!(
            drain(&p, MaintenancePurpose::Update, Duration::from_secs(10)),
            exit::OK
        );
    }

    #[test]
    fn the_supervisor_endpoint_is_a_singleton_lock_that_says_hello() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), "sup");
        assert!(supervisor_hello(&p).is_none());
        assert!(!supervisor_running(&p));
        let first = Arc::new(bind_supervisor(&p).unwrap());
        assert!(matches!(
            bind_supervisor(&p),
            Err(deck_host::Error::AlreadyRunning(_))
        ));
        let hello = SupervisorHello {
            pid: 7,
            version: GATEWAY_VERSION.into(),
            program: Program {
                program: PathBuf::from("/opt/omp-deck/omp-deck-gateway"),
                args: Vec::new(),
            },
            startup: dir.path().join("startup.json"),
        };
        let (l, h) = (first.clone(), hello.clone());
        let t = std::thread::spawn(move || answer_hellos(&l, &h));
        assert_eq!(supervisor_hello(&p), Some(hello));
        assert!(supervisor_running(&p));
        first.remove();
        t.join().unwrap();
        drop(first);
        // Released with the listener: the next supervisor may take over.
        assert!(bind_supervisor(&p).is_ok());
    }

    #[test]
    fn explicit_flags_must_agree_with_the_startup_context() {
        let dir = tempfile::tempdir().unwrap();
        let p = profile(dir.path(), "args");
        let ctx = StartupContext {
            profile: p.clone(),
            env: Vec::new(),
            gateway: Program {
                program: PathBuf::from("/opt/omp-deck/omp-deck-gateway"),
                args: Vec::new(),
            },
            gui: None,
            host_override: None,
            omp: None,
        };
        let file = dir.path().join("startup.json");
        ctx.save(&file).unwrap();
        let args = |ns: Option<&str>, settings: Option<PathBuf>| ProfileArgs {
            startup: Some(file.clone()),
            ns: ns.map(str::to_owned),
            settings,
            sessions_root: None,
        };
        let (got, loaded) = resolve_profile(&args(None, None)).unwrap();
        assert_eq!(got, p);
        assert_eq!(loaded, Some(ctx));
        assert!(resolve_profile(&args(Some(&p.host_namespace), Some(p.settings.clone()))).is_ok());
        assert!(resolve_profile(&args(Some("other-ns"), None)).is_err());
        assert!(resolve_profile(&args(None, Some(dir.path().join("other.json")))).is_err());
        assert!(
            resolve_profile(&ProfileArgs {
                ns: Some("bad ns".into()),
                ..Default::default()
            })
            .is_err()
        );
    }

    #[test]
    fn resume_clears_the_barrier_and_repairs_contexts_keeping_custom_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let make = |tag: &str, mode: GatewayMode| {
            // Every identity's settings live in the shared app root, so its state dir does too.
            let p = Profile {
                host_namespace: format!("omp-deck-test-{}-{tag}", std::process::id()),
                settings: root.join(format!("{tag}-state.json")),
                sessions_root: root.join(tag),
            };
            write_mode(&p.settings, mode);
            let ctx = StartupContext {
                profile: p.clone(),
                env: vec![("PI_CONFIG_DIR".into(), "/custom/pi".into())],
                gateway: Program {
                    program: PathBuf::from("/old/omp-deck.AppImage"),
                    args: vec!["--gateway".into()],
                },
                gui: None,
                host_override: Some(PathBuf::from("/custom/host")),
                omp: Some(PathBuf::from("/custom/omp")),
            };
            let file = StartupContext::path_in(&p.state_dir());
            ctx.save(&file).unwrap();
            (file, ctx)
        };
        let (bg_file, bg) = make("bg", GatewayMode::Background);
        let (off_file, off) = make("off", GatewayMode::Off);
        raise_barrier(root, MaintenancePurpose::Update).unwrap();
        let new_gateway = Program {
            program: PathBuf::from("/new/omp-deck-gateway"),
            args: Vec::new(),
        };
        let calls = AtomicUsize::new(0);
        let notes = resume_in(root, &new_gateway, None, |ctx, file| {
            calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(file, bg_file.as_path());
            assert_eq!(ctx.gateway, new_gateway);
            Ok("supervisor pid 1".into())
        })
        .unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "only the Background identity is supervised"
        );
        assert_eq!(notes.len(), 2);
        assert!(maintenance_active(root).is_none());
        for (file, before) in [(bg_file, bg), (off_file, off)] {
            let after = StartupContext::load(&file).unwrap();
            assert_eq!(after.gateway, new_gateway);
            assert_eq!(after.profile, before.profile);
            assert_eq!(after.env, before.env);
            assert_eq!(after.host_override, before.host_override);
            assert!(after.omp.is_some());
        }
        // A failed supervisor restart is a real error.
        let err = resume_in(root, &new_gateway, None, |_, _| Err("no systemd".into())).unwrap_err();
        assert!(err.contains("no systemd"));
    }

    #[test]
    fn wire_crates_never_log_below_info() {
        use log::LevelFilter::{Info, Trace};
        assert_eq!(target_cap("tungstenite"), Info);
        assert_eq!(target_cap("tungstenite::protocol::frame"), Info);
        assert_eq!(target_cap("tokio_tungstenite::compat"), Info);
        assert_eq!(target_cap("rustls"), Info);
        assert_eq!(target_cap("rustls::client::hs"), Info);
        assert_eq!(target_cap("tokio_rustls::common"), Info);
        assert_eq!(target_cap("rustlsish"), Trace);
        assert_eq!(target_cap("rustlsfoo::x"), Trace);
        assert_eq!(target_cap("tungstenite_extra"), Trace);
        assert_eq!(target_cap("deck_service::gateway::remote::link"), Trace);
    }

    /// Accepts everything (like `RUST_LOG=trace` or `tungstenite::protocol=trace`) and keeps
    /// what reached it.
    #[derive(Default)]
    struct Capture(Mutex<Vec<(String, log::Level)>>);

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }

        fn log(&self, record: &log::Record) {
            self.0
                .lock()
                .unwrap()
                .push((record.target().to_owned(), record.level()));
        }

        fn flush(&self) {}
    }

    #[test]
    fn wire_cap_drops_wire_debug_whatever_the_inner_logger_allows() {
        use log::Log as _;
        let cap = WireCap(Capture::default());
        let cases = [
            ("tungstenite::protocol", log::Level::Trace, false),
            ("tungstenite::protocol", log::Level::Debug, false),
            ("tungstenite::protocol", log::Level::Info, true),
            ("tokio_tungstenite", log::Level::Trace, false),
            ("rustls::conn", log::Level::Debug, false),
            ("rustls", log::Level::Warn, true),
            ("tokio_rustls::client", log::Level::Trace, false),
            ("rustlsfoo", log::Level::Trace, true),
            ("deck_service::gateway", log::Level::Trace, true),
        ];
        for (target, level, passes) in cases {
            let meta = log::Metadata::builder().target(target).level(level).build();
            assert_eq!(cap.enabled(&meta), passes, "{target} {level}");
            cap.log(
                &log::Record::builder()
                    .target(target)
                    .level(level)
                    .args(format_args!("payload"))
                    .build(),
            );
        }
        let expected: Vec<(String, log::Level)> = cases
            .iter()
            .filter(|c| c.2)
            .map(|c| (c.0.to_owned(), c.1))
            .collect();
        assert_eq!(*cap.0.0.lock().unwrap(), expected);
    }
}
