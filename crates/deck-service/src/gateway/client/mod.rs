//! The native gateway client: [`GatewayClient`].
//!
//! Every network step runs on the client's own threads; the UI thread only reads the
//! latest-state cache ([`StateCache`]), drains [`ClientEvent`]s and polls [`Pending`] results.
//! One control connection per client (shared by clones):
//!
//! - **Connect**: the owner endpoint of the profile's [`super::GatewayId`]. When no owner runs
//!   and `autostart` is set, the owner is launched detached (inheriting this process's
//!   environment) unless the maintenance barrier is up, then connected with a bounded retry.
//! - **Replace**: an autostarting GUI that finds an owner of an older build (an update replaced
//!   the executables under it) asks it to exit once it is drained, then launches its own. Once
//!   per client: an owner that comes back still older is kept.
//! - **Reconnect** forever with backoff. A lost link resolves every outstanding request with
//!   [`GatewayError::Disconnected`]; the next link re-sends the client state, interest
//!   subscriptions and dashboard targets and refetches every cached topic.
//! - **Cache**: a topic is refetched after each `Changed` (coalesced while a fetch is in
//!   flight) and after `Resync`; [`ClientEvent::Changed`] follows the stored value.
//!
//! Terminal views use their own connection each ([`GatewayClient::frontend`]).

mod cache;
mod frontend;
mod pending;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use deck_core::agents::AgentsTarget;
use deck_core::config::GatewayMode;
use deck_core::fleet::FleetTarget;
use deck_host::transport::{self, Endpoint, Stream};

pub use self::cache::{ALWAYS_ON, Slot, StateCache};
pub use self::pending::Pending;
use self::pending::Resolver;
use super::protocol::{
    self, Answer, ClientState, Done, DrainState, Envelope, GatewayStatus, Hello, MaintenanceOp,
    ModelFactsView, Mutation, Outcome, PROTOCOL, Progress, Query, Refusal, Reply, Request, Role,
    ServerEvent, SwitchInfo, Topic, Welcome, kind,
};
pub use super::protocol::{Accepted, ViewRole};
use super::wire::{self, Assembly};
use super::{GATEWAY_VERSION, MaintenanceMarker, OWNER_ENDPOINT, Profile, Program};
use crate::controller::{
    Command, ControllerEvent, RequestId, RequestKey, RequestKeys, ServiceEpoch, SessionTarget,
};
use crate::settings::{Edit, SettingsError, Snapshot};

/// Repaint/notify callback, called after any cache, event or result change.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// Client events kept for the UI before the oldest are dropped for one [`ClientEvent::Resync`].
pub const MAX_EVENTS: usize = 4096;
/// How long a handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a launched owner may take until it accepts connections.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// First reconnect delay; doubles up to [`RETRY_MAX`].
const RETRY_MIN: Duration = Duration::from_millis(100);
const RETRY_MAX: Duration = Duration::from_secs(5);
/// Retry delay while the owner refuses this client (other profile/protocol).
const REFUSED_RETRY: Duration = Duration::from_secs(30);
/// Poll interval while the maintenance barrier keeps the owner down.
const MAINTENANCE_POLL: Duration = Duration::from_secs(2);
/// Poll interval while an outdated owner has work left before it can be replaced.
const REPLACE_RETRY: Duration = Duration::from_secs(15);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- options ----------------------------------------------------------------------------------

/// How a [`GatewayClient`] connects.
#[derive(Clone)]
pub struct GatewayOptions {
    pub profile: Profile,
    pub role: Role,
    /// The `client` of this client's [`RequestKey`]s; unique per client
    /// (`<role>-<pid>-<random hex>` from [`GatewayOptions::new`]).
    pub client_id: String,
    /// Launch the owner (detached, inheriting this environment) when none runs. Never while the
    /// maintenance barrier is up.
    pub autostart: bool,
    /// The gateway executable to launch; `None` = [`super::gateway_program`].
    pub gateway_exe: Option<PathBuf>,
    /// Called after any cache, event or result change (egui: `request_repaint`).
    pub wake: Wake,
}

impl GatewayOptions {
    /// Options for `profile`; `autostart` only for [`Role::Gui`].
    pub fn new(profile: Profile, role: Role, wake: Wake) -> GatewayOptions {
        GatewayOptions {
            profile,
            role,
            client_id: new_client_id(role),
            autostart: role == Role::Gui,
            gateway_exe: None,
            wake,
        }
    }

    /// A GUI client of this user's default profile.
    pub fn gui(wake: Wake) -> GatewayOptions {
        GatewayOptions::new(Profile::default_for_user(), Role::Gui, wake)
    }
}

fn new_client_id(role: Role) -> String {
    let name = match role {
        Role::Gui => "gui",
        Role::Cli => "cli",
        Role::Supervisor => "supervisor",
    };
    let mut bytes = [0u8; 8];
    if let Err(e) = getrandom::fill(&mut bytes) {
        log::warn!("no random source for the client id: {e}");
        let now = super::unix_ms();
        bytes = now.to_le_bytes();
        static NEXT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
        bytes[7] ^= NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{name}-{}-{hex}", std::process::id())
}

// ---- public types -----------------------------------------------------------------------------

/// The control link's state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Link {
    /// Before the first attempt finished, and while a launched owner starts.
    Connecting,
    Up(Arc<Welcome>),
    /// The running owner refuses this client (another profile or protocol). Retried slowly.
    Refused(Refusal),
    /// Not connected; retrying.
    Down {
        error: String,
    },
    /// The maintenance barrier is up and no owner runs: nothing is started meanwhile.
    Maintenance(MaintenanceMarker),
}

/// What happened since the last [`GatewayClient::events`].
#[derive(Clone, Debug)]
pub enum ClientEvent {
    /// Every link change.
    Link(Link),
    /// The cache holds a new value of `topic`; `revision` is its [`StateCache::revision`].
    Changed {
        topic: Topic,
        revision: u64,
    },
    Controller {
        target: SessionTarget,
        event: ControllerEvent,
    },
    /// A mutation of this client reached its final outcome.
    Outcome {
        key: RequestKey,
        outcome: Outcome,
    },
    Progress {
        key: RequestKey,
        progress: Progress,
    },
    /// A finished turn: the tab's bell marker.
    Bell(SessionTarget),
    /// A desktop notification was clicked: raise and show the session (`token`: the desktop's
    /// activation token, when it sent one).
    Activate {
        target: SessionTarget,
        token: Option<String>,
    },
    /// omp reported another model than a switch asked for.
    ModelUnconfirmed {
        target: SessionTarget,
        switch: SwitchInfo,
    },
    /// A tab copy found its new session file, or failed.
    CopyResult {
        target: SessionTarget,
        result: Result<PathBuf, String>,
    },
    /// Events were dropped (the gateway's queue or this one overflowed): every cached topic is
    /// being refetched (`Changed` follows per topic); re-derive UI state and look accepted
    /// requests up again.
    Resync,
}

/// A sent mutation: its key (allocated locally, not an id of the gateway), whether the gateway
/// accepted (journaled) it, and its final outcome.
pub struct Mutating {
    pub key: RequestKey,
    /// `Ok` once journaled by the gateway; an error means it never ran.
    pub accepted: Pending<Result<Accepted, GatewayError>>,
    /// The final [`Outcome`], without consuming [`ClientEvent`]s (for worker threads). Resolves
    /// `Err(Disconnected)` if the link drops first: look it up with [`GatewayClient::outcome`]
    /// after the reconnect. Dropping it costs nothing.
    pub outcome: Pending<Result<Outcome, GatewayError>>,
}

pub use super::protocol::GatewayError;

// ---- client -----------------------------------------------------------------------------------

/// The asynchronous native client. Clones share one control connection; it closes (with a clean
/// `BYE`) when the last clone (or [`RemoteSettings`]) is dropped.
#[derive(Clone)]
pub struct GatewayClient {
    handle: Arc<Handle>,
}

struct Handle {
    shared: Arc<Shared>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.close();
    }
}

impl GatewayClient {
    /// Returns at once; connecting, launching, the handshake and reconnects run on the client's
    /// own thread.
    pub fn start(opts: GatewayOptions) -> GatewayClient {
        let GatewayOptions {
            profile,
            role,
            client_id,
            autostart,
            gateway_exe,
            wake,
        } = opts;
        let shared = Arc::new(Shared {
            keys: Mutex::new(RequestKeys::new(client_id.clone())),
            profile,
            role,
            client_id,
            autostart,
            gateway_exe,
            wake,
            cache: Arc::new(StateCache::default()),
            inner: Mutex::new(Inner {
                link: Link::Connecting,
                conn: None,
                next_id: 1,
                waiting: HashMap::new(),
                watchers: HashMap::new(),
                events: VecDeque::new(),
                client_state: None,
                interests: BTreeSet::new(),
                fleet: None,
                agents: None,
                fetches: HashMap::new(),
                closing: false,
                replaced: false,
            }),
            cv: Condvar::new(),
        });
        let worker = shared.clone();
        let spawned = std::thread::Builder::new()
            .name("gateway-client".into())
            .spawn(move || worker.run());
        if let Err(e) = spawned {
            log::error!("starting the gateway client thread: {e}");
            shared.set_link(Link::Down {
                error: format!("cannot start the gateway client: {e}"),
            });
        }
        GatewayClient {
            handle: Arc::new(Handle { shared }),
        }
    }

    fn shared(&self) -> &Arc<Shared> {
        &self.handle.shared
    }

    pub fn link(&self) -> Link {
        lock(&self.shared().inner).link.clone()
    }

    /// The connected owner's epoch; `None` while not connected.
    pub fn epoch(&self) -> Option<ServiceEpoch> {
        lock(&self.shared().inner)
            .conn
            .as_ref()
            .map(|c| c.welcome.epoch)
    }

    pub fn client_id(&self) -> &str {
        &self.shared().client_id
    }

    pub fn profile(&self) -> &Profile {
        &self.shared().profile
    }

    /// Drains the events since the last call (bounded: overflow leaves one `Resync`).
    pub fn events(&self) -> Vec<ClientEvent> {
        lock(&self.shared().inner).events.drain(..).collect()
    }

    /// The latest-state cache.
    pub fn state(&self) -> Arc<StateCache> {
        self.shared().cache.clone()
    }

    /// Focus/visibility of this GUI; replaces the previous one, re-sent after reconnects.
    pub fn set_client_state(&self, s: ClientState) {
        let sh = self.shared();
        let up = {
            let mut inner = lock(&sh.inner);
            inner.client_state = Some(s.clone());
            inner.conn.is_some()
        };
        if up {
            sh.notice(Request::ClientState(s));
        }
    }

    /// Follows a topic beyond the always-on ones (interest-driven watchers run only while
    /// someone follows them). Restored after reconnects.
    pub fn subscribe(&self, t: Topic) {
        let sh = self.shared();
        let (fresh, up) = {
            let mut inner = lock(&sh.inner);
            (inner.interests.insert(t.clone()), inner.conn.is_some())
        };
        if fresh && up {
            if needs_subscription(&t) {
                sh.notice(Request::Subscribe(t.clone()));
            }
            sh.fetch(t);
        }
    }

    pub fn unsubscribe(&self, t: &Topic) {
        let sh = self.shared();
        let (gone, up) = {
            let mut inner = lock(&sh.inner);
            (inner.interests.remove(t), inner.conn.is_some())
        };
        if !gone {
            return;
        }
        if !sh.wants(t) {
            sh.cache.forget(t);
        }
        if up && needs_subscription(t) {
            sh.notice(Request::Unsubscribe(t.clone()));
        }
    }

    /// The sessions this client's agents dashboard follows ([`Topic::Fleet`]); restored after
    /// reconnects.
    pub fn set_fleet_targets(&self, targets: Vec<FleetTarget>) {
        let sh = self.shared();
        let up = {
            let mut inner = lock(&sh.inner);
            inner.fleet = Some(targets.clone());
            inner.conn.is_some()
        };
        if up {
            sh.notice(Request::SetFleetTargets(targets));
        }
    }

    /// The session whose subagents this client shows ([`Topic::Agents`]); restored after
    /// reconnects.
    pub fn set_agents_target(&self, target: Option<AgentsTarget>) {
        let sh = self.shared();
        let up = {
            let mut inner = lock(&sh.inner);
            inner.agents = Some(target.clone());
            inner.conn.is_some()
        };
        if up {
            sh.notice(Request::SetAgentsTarget(target));
        }
    }

    /// What this client's status bar shows about a session's model (not restored: the next
    /// frame reports it again).
    pub fn observe_model(&self, target: SessionTarget, facts: ModelFactsView) {
        self.shared()
            .notice(Request::ObserveModel { target, facts });
    }

    pub fn query(&self, q: Query) -> Pending<Result<Answer, GatewayError>> {
        self.shared().call(Request::Query(q), Ok)
    }

    /// Sends a journaled mutation bound to the connected owner's epoch.
    pub fn mutate(&self, op: Mutation) -> Mutating {
        let sh = self.shared();
        let wake = sh.wake.clone();
        let (accepted, acc_res) = Pending::new(wake.clone());
        let (outcome, out_res) = Pending::new(wake);
        let key = sh.send_mutation(op, Some(acc_res), Box::new(OutcomeWatch(out_res)));
        Mutating {
            key,
            accepted,
            outcome,
        }
    }

    /// The record of an earlier mutation (lookup only: never executes anything).
    pub fn outcome(&self, key: RequestKey) -> Pending<Result<Outcome, GatewayError>> {
        self.shared().call(Request::Outcome(key), |a| match a {
            Answer::Outcome(o) => Ok(o),
            _ => Err(unexpected()),
        })
    }

    /// An injected command through the session's controller ([`Mutation::Submit`]).
    /// `accepted` carries the controller's request id ([`Accepted::submitted`]).
    pub fn submit(&self, target: SessionTarget, command: Command) -> Mutating {
        self.mutate(Mutation::Submit { target, command })
    }

    /// Answers a draft confirmation; `false`: no such outstanding confirmation.
    pub fn answer(
        &self,
        target: SessionTarget,
        id: RequestId,
        clear: bool,
    ) -> Pending<Result<bool, GatewayError>> {
        self.shared()
            .mutate_done(Mutation::Answer { target, id, clear }, |_, r| {
                r.and_then(|d| match d {
                    Done::Answered(b) => Ok(b),
                    _ => Err(unexpected()),
                })
            })
    }

    /// An own view of `target` for [`deck_term::Terminal::frontend`] (own connection).
    pub fn frontend(&self, target: SessionTarget, view: ViewRole) -> Box<dyn deck_term::Frontend> {
        Box::new(frontend::ViewFrontend {
            shared: self.shared().clone(),
            target,
            view,
        })
    }

    pub fn settings(&self) -> RemoteSettings {
        RemoteSettings {
            client: self.clone(),
        }
    }

    /// Every earlier settings edit committed to disk (before an update restart or exit).
    pub fn flush(&self) -> Pending<Result<(), GatewayError>> {
        self.shared().call(Request::Flush, |a| match a {
            Answer::Flushed => Ok(()),
            _ => Err(unexpected()),
        })
    }

    /// Bundle maintenance (CLI): raise the barrier and drain, poll, or let the owner exit.
    pub fn maintenance(&self, op: MaintenanceOp) -> Pending<Result<DrainState, GatewayError>> {
        self.shared().call(Request::Maintenance(op), |a| match a {
            Answer::Maintenance(d) => Ok(d),
            _ => Err(unexpected()),
        })
    }
}

/// The settings façade over the gateway's single settings writer (same shape as
/// [`crate::settings::SettingsService`]).
#[derive(Clone)]
pub struct RemoteSettings {
    client: GatewayClient,
}

impl RemoteSettings {
    /// The latest committed settings; `None` until the first fetch after a Welcome.
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.client
            .shared()
            .cache
            .settings
            .get()
            .map(|s| (*s).clone())
    }

    /// A compare-and-set edit committed by the gateway's settings writer.
    pub fn submit(&self, edit: Edit) -> Pending<Result<Snapshot, SettingsError>> {
        self.client
            .shared()
            .mutate_done(Mutation::Settings(edit), |sh, r| match r {
                Ok(Done::Settings(snap)) => {
                    if let Some(revision) = sh.cache.offer_settings(&snap) {
                        sh.push_event(ClientEvent::Changed {
                            topic: Topic::Settings,
                            revision,
                        });
                    }
                    Ok(snap)
                }
                Ok(_) => Err(SettingsError::Io(unexpected().to_string())),
                Err(GatewayError::Settings(e)) => Err(e),
                Err(other) => Err(SettingsError::Io(other.to_string())),
            })
    }

    /// The gateway lifetime transaction (register supervision → commit the mode → start and
    /// observe the supervisor, rolled back on failure). Resolves with the resulting status.
    pub fn set_gateway_mode(
        &self,
        mode: GatewayMode,
    ) -> Pending<Result<GatewayStatus, GatewayError>> {
        self.client
            .shared()
            .mutate_done(Mutation::SetGatewayMode(mode), |sh, r| match r? {
                Done::Gateway(status) => {
                    let revision = sh.cache.store(
                        &Topic::Gateway,
                        protocol::TopicValue::Gateway(status.clone()),
                    );
                    sh.push_event(ClientEvent::Changed {
                        topic: Topic::Gateway,
                        revision,
                    });
                    Ok(status)
                }
                _ => Err(unexpected()),
            })
    }
}

fn unexpected() -> GatewayError {
    GatewayError::Failed("the gateway sent an unexpected answer".into())
}

/// Topics the gateway follows for a client only after `Subscribe`.
fn needs_subscription(t: &Topic) -> bool {
    !ALWAYS_ON.contains(t)
}

/// A final outcome that is not a success, as an error.
fn outcome_error(o: Outcome) -> GatewayError {
    match o {
        Outcome::Failed(s) => GatewayError::Failed(s),
        Outcome::Rejected(r) => GatewayError::Rejected(r),
        Outcome::Cancelled => GatewayError::Cancelled,
        Outcome::Uncertain => GatewayError::Uncertain,
        Outcome::Recorded { succeeded: true } => GatewayError::Failed(
            "finished under an earlier gateway instance; its result is no longer available".into(),
        ),
        Outcome::Recorded { succeeded: false } => {
            GatewayError::Failed("failed under an earlier gateway instance".into())
        }
        Outcome::Running | Outcome::Done(_) => unexpected(),
    }
}

// ---- shared state -----------------------------------------------------------------------------

type Responder = Box<dyn FnOnce(&Arc<Shared>, Reply) + Send>;

/// Waits for the final outcome of one accepted mutation.
trait Watch: Send {
    /// Someone still waits for it.
    fn alive(&self) -> bool;
    fn finish(self: Box<Self>, shared: &Arc<Shared>, r: Result<Outcome, GatewayError>);
}

struct OutcomeWatch(Resolver<Result<Outcome, GatewayError>>);

impl Watch for OutcomeWatch {
    fn alive(&self) -> bool {
        self.0.alive()
    }
    fn finish(self: Box<Self>, _: &Arc<Shared>, r: Result<Outcome, GatewayError>) {
        self.0.resolve(r);
    }
}

/// Maps the final outcome (its [`Done`], or why there is none) to a typed result.
struct DoneWatch<T, F> {
    res: Resolver<T>,
    map: F,
}

impl<T, F> Watch for DoneWatch<T, F>
where
    T: Send,
    F: FnOnce(&Arc<Shared>, Result<Done, GatewayError>) -> T + Send,
{
    fn alive(&self) -> bool {
        self.res.alive()
    }
    fn finish(self: Box<Self>, shared: &Arc<Shared>, r: Result<Outcome, GatewayError>) {
        let r = match r {
            Ok(Outcome::Done(d)) => Ok(*d),
            Ok(o) => Err(outcome_error(o)),
            Err(e) => Err(e),
        };
        self.res.resolve((self.map)(shared, r));
    }
}

/// One established control connection.
struct Conn {
    welcome: Arc<Welcome>,
    /// Encoded frames for the connection's writer thread.
    frames: Sender<Vec<u8>>,
}

#[derive(Default)]
struct FetchState {
    inflight: bool,
    again: bool,
}

struct Inner {
    link: Link,
    conn: Option<Conn>,
    next_id: u64,
    waiting: HashMap<u64, Responder>,
    watchers: HashMap<RequestKey, Box<dyn Watch>>,
    events: VecDeque<ClientEvent>,
    client_state: Option<ClientState>,
    interests: BTreeSet<Topic>,
    fleet: Option<Vec<FleetTarget>>,
    agents: Option<Option<AgentsTarget>>,
    fetches: HashMap<Topic, FetchState>,
    closing: bool,
    /// This client asked an outdated owner to exit (at most once per client).
    replaced: bool,
}

pub(crate) struct Shared {
    pub(crate) profile: Profile,
    pub(crate) role: Role,
    pub(crate) client_id: String,
    autostart: bool,
    gateway_exe: Option<PathBuf>,
    wake: Wake,
    cache: Arc<StateCache>,
    keys: Mutex<RequestKeys>,
    inner: Mutex<Inner>,
    /// Signalled on close (interrupts retry sleeps).
    cv: Condvar,
}

impl Shared {
    fn wake(&self) {
        (self.wake)();
    }

    fn push_locked(inner: &mut Inner, ev: ClientEvent) {
        if inner.events.len() >= MAX_EVENTS {
            inner.events.clear();
            inner.events.push_back(ClientEvent::Resync);
        }
        inner.events.push_back(ev);
    }

    fn push_event(&self, ev: ClientEvent) {
        Shared::push_locked(&mut lock(&self.inner), ev);
        self.wake();
    }

    fn set_link(&self, link: Link) {
        {
            let mut inner = lock(&self.inner);
            if inner.link == link {
                return;
            }
            inner.link = link.clone();
            Shared::push_locked(&mut inner, ClientEvent::Link(link));
        }
        self.wake();
    }

    /// The topics this client caches.
    fn wants(&self, t: &Topic) -> bool {
        (self.role == Role::Gui && ALWAYS_ON.contains(t)) || lock(&self.inner).interests.contains(t)
    }

    fn wanted(&self) -> Vec<Topic> {
        let inner = lock(&self.inner);
        let mut out: Vec<Topic> = if self.role == Role::Gui {
            ALWAYS_ON.to_vec()
        } else {
            Vec::new()
        };
        let extra: Vec<Topic> = inner
            .interests
            .iter()
            .filter(|t| !out.contains(t))
            .cloned()
            .collect();
        out.extend(extra);
        out
    }

    /// Sends a request; `responder` gets its reply, or `Disconnected` when there is no link or
    /// it drops first.
    fn request(self: &Arc<Self>, body: Request, responder: Responder) {
        let mut inner = lock(&self.inner);
        if inner.conn.is_none() {
            drop(inner);
            responder(self, Err(GatewayError::Disconnected));
            return;
        }
        let id = inner.next_id;
        let bytes = match serde_json::to_vec(&Envelope { id, body }) {
            Ok(b) if b.len() > wire::MAX_ASSEMBLED => Err(GatewayError::TooLarge),
            Ok(b) => Ok(b),
            Err(e) => Err(GatewayError::Failed(format!("encoding the request: {e}"))),
        };
        let bytes = match bytes {
            Ok(b) => b,
            Err(e) => {
                drop(inner);
                responder(self, Err(e));
                return;
            }
        };
        let sent = inner.conn.as_ref().is_some_and(|conn| {
            wire::chunks(id, &bytes)
                .all(|chunk| conn.frames.send(frame(kind::REQUEST, &chunk)).is_ok())
        });
        if !sent {
            // The writer is gone: the reader is tearing the link down.
            drop(inner);
            responder(self, Err(GatewayError::Disconnected));
            return;
        }
        inner.next_id += 1;
        inner.waiting.insert(id, responder);
    }

    /// A request whose answer is mapped into a [`Pending`].
    fn call<T: Send + 'static>(
        self: &Arc<Self>,
        body: Request,
        map: impl FnOnce(Answer) -> Result<T, GatewayError> + Send + 'static,
    ) -> Pending<Result<T, GatewayError>> {
        let (pending, res) = Pending::new(self.wake.clone());
        self.request(body, Box::new(move |_, r| res.resolve(r.and_then(map))));
        pending
    }

    /// A request whose reply only matters when it fails (logged).
    fn notice(self: &Arc<Self>, body: Request) {
        let what = request_label(&body);
        self.request(
            body,
            Box::new(move |_, r| match r {
                Ok(_) | Err(GatewayError::Disconnected) => {}
                Err(e) => log::warn!("gateway {what}: {e}"),
            }),
        );
    }

    /// Sends a mutation: `accepted` (if wanted) resolves with the acceptance, `watch` with its
    /// final outcome (or the reason it will have none).
    fn send_mutation(
        self: &Arc<Self>,
        op: Mutation,
        accepted: Option<Resolver<Result<Accepted, GatewayError>>>,
        watch: Box<dyn Watch>,
    ) -> RequestKey {
        let key = lock(&self.keys).next_key();
        let epoch = {
            let mut inner = lock(&self.inner);
            let epoch = inner.conn.as_ref().map(|c| c.welcome.epoch);
            if epoch.is_some() {
                inner.watchers.retain(|_, w| w.alive());
                inner.watchers.insert(key.clone(), watch);
            } else {
                drop(inner);
                if let Some(a) = accepted {
                    a.resolve(Err(GatewayError::Disconnected));
                }
                watch.finish(self, Err(GatewayError::Disconnected));
                return key;
            }
            epoch
        };
        let Some(epoch) = epoch else {
            return key;
        };
        let k = key.clone();
        self.request(
            Request::Mutate {
                key: key.clone(),
                epoch,
                op: Box::new(op),
            },
            Box::new(move |sh, r| {
                let r = match r {
                    Ok(Answer::Accepted(a)) => Ok(a),
                    Ok(_) => Err(unexpected()),
                    Err(e) => Err(e),
                };
                if let Err(e) = &r {
                    // Never accepted: no outcome will follow.
                    let w = lock(&sh.inner).watchers.remove(&k);
                    if let Some(w) = w {
                        w.finish(sh, Err(e.clone()));
                    }
                }
                if let Some(a) = accepted {
                    a.resolve(r);
                }
            }),
        );
        key
    }

    /// A mutation whose result is derived from its final [`Done`] (or the error that stopped
    /// it: not accepted, failed, rejected, link lost).
    fn mutate_done<T: Send + 'static>(
        self: &Arc<Self>,
        op: Mutation,
        map: impl FnOnce(&Arc<Shared>, Result<Done, GatewayError>) -> T + Send + 'static,
    ) -> Pending<T> {
        let (pending, res) = Pending::new(self.wake.clone());
        self.send_mutation(op, None, Box::new(DoneWatch { res, map }));
        pending
    }

    // ---- cache ----

    /// Fetches `topic` into the cache; coalesced while a fetch of it is in flight.
    fn fetch(self: &Arc<Self>, topic: Topic) {
        {
            let mut inner = lock(&self.inner);
            if inner.conn.is_none() {
                return;
            }
            let st = inner.fetches.entry(topic.clone()).or_default();
            if st.inflight {
                st.again = true;
                return;
            }
            st.inflight = true;
        }
        let t = topic.clone();
        self.request(
            Request::Query(Query::Topic(topic)),
            Box::new(move |sh, r| {
                match r {
                    Ok(Answer::Topic(value)) => {
                        if sh.wants(&t) {
                            let revision = sh.cache.store(&t, *value);
                            sh.push_event(ClientEvent::Changed {
                                topic: t.clone(),
                                revision,
                            });
                        }
                    }
                    Ok(_) => log::warn!("gateway topic {t:?}: unexpected answer"),
                    Err(GatewayError::Disconnected) => {}
                    Err(e) => log::warn!("gateway topic {t:?}: {e}"),
                }
                let again = {
                    let mut inner = lock(&sh.inner);
                    match inner.fetches.get_mut(&t) {
                        Some(st) => {
                            st.inflight = false;
                            std::mem::take(&mut st.again)
                        }
                        None => false,
                    }
                };
                if again {
                    sh.fetch(t);
                }
            }),
        );
    }

    fn fetch_all(self: &Arc<Self>) {
        for t in self.wanted() {
            self.fetch(t);
        }
    }

    // ---- events ----

    fn on_event(self: &Arc<Self>, ev: ServerEvent) {
        match ev {
            ServerEvent::Changed { topic, .. } => {
                if self.wants(&topic) {
                    self.fetch(topic);
                }
            }
            ServerEvent::Controller { target, event } => {
                self.push_event(ClientEvent::Controller { target, event })
            }
            ServerEvent::Outcome { key, outcome } => {
                if outcome.is_final() {
                    let w = lock(&self.inner).watchers.remove(&key);
                    if let Some(w) = w {
                        w.finish(self, Ok(outcome.clone()));
                    }
                }
                self.push_event(ClientEvent::Outcome { key, outcome });
            }
            ServerEvent::Progress { key, progress } => {
                self.push_event(ClientEvent::Progress { key, progress })
            }
            ServerEvent::Bell(t) => self.push_event(ClientEvent::Bell(t)),
            ServerEvent::Activate { target, token } => {
                self.push_event(ClientEvent::Activate { target, token })
            }
            ServerEvent::ModelUnconfirmed { target, switch } => {
                self.push_event(ClientEvent::ModelUnconfirmed { target, switch })
            }
            ServerEvent::CopyResult { target, result } => {
                self.push_event(ClientEvent::CopyResult { target, result })
            }
            ServerEvent::Resync => {
                self.fetch_all();
                self.push_event(ClientEvent::Resync);
            }
        }
    }

    // ---- lifetime ----

    fn closing(&self) -> bool {
        lock(&self.inner).closing
    }

    /// Sleeps up to `d` unless the client closes; `true` when closing.
    fn sleep(&self, d: Duration) -> bool {
        let inner = lock(&self.inner);
        let (inner, _) = self
            .cv
            .wait_timeout_while(inner, d, |i| !i.closing)
            .unwrap_or_else(PoisonError::into_inner);
        inner.closing
    }

    /// The last handle is gone: say `BYE` and end the link.
    fn close(&self) {
        let conn = {
            let mut inner = lock(&self.inner);
            inner.closing = true;
            inner.conn.take()
        };
        if let Some(conn) = conn {
            let _ = conn.frames.send(frame(kind::BYE, &[]));
            // Dropping the sender ends the writer after the BYE; it shuts the stream down and
            // the reader ends.
        }
        self.cv.notify_all();
    }

    /// Asks the outdated owner of `epoch` whether it is drained ([`MaintenanceOp::Poll`] raises
    /// no barrier). Drained, it is asked to exit; else asked again after [`REPLACE_RETRY`] while
    /// the link still serves it.
    fn poll_outdated(self: &Arc<Self>, epoch: ServiceEpoch) {
        self.request(
            Request::Maintenance(MaintenanceOp::Poll),
            Box::new(move |sh, r| match r {
                Ok(Answer::Maintenance(DrainState::Drained)) => sh.retire_outdated(epoch),
                Ok(Answer::Maintenance(DrainState::Draining { .. })) => {
                    let sh = sh.clone();
                    let spawned = std::thread::Builder::new()
                        .name("gateway-replace".into())
                        .spawn(move || {
                            if !sh.sleep(REPLACE_RETRY) && serves(&lock(&sh.inner), epoch) {
                                sh.poll_outdated(epoch);
                            }
                        });
                    if let Err(e) = spawned {
                        log::warn!("replacing the outdated gateway: {e}");
                    }
                }
                Ok(_) => log::warn!("replacing the outdated gateway: {}", unexpected()),
                // The next link decides again.
                Err(GatewayError::Disconnected) => {}
                Err(e) => log::warn!("replacing the outdated gateway: {e}"),
            }),
        );
    }

    /// [`MaintenanceOp::Exit`] to the drained outdated owner of `epoch`. It raises its barrier in
    /// memory only (no marker file, so nothing blocks a launch), exits once idle, and the
    /// reconnect launches this client's owner.
    fn retire_outdated(self: &Arc<Self>, epoch: ServiceEpoch) {
        {
            let mut inner = lock(&self.inner);
            if inner.replaced || !serves(&inner, epoch) {
                return;
            }
            inner.replaced = true;
        }
        log::info!("asking the outdated gateway to exit");
        self.notice(Request::Maintenance(MaintenanceOp::Exit));
    }

    /// The client thread: connect, serve, reconnect until closed.
    fn run(self: Arc<Self>) {
        let mut delay = RETRY_MIN;
        loop {
            if self.closing() {
                return;
            }
            let wait = match self.establish() {
                Attempt::Up(stream, welcome) => {
                    delay = RETRY_MIN;
                    let error = self.serve(stream, welcome);
                    if self.closing() {
                        return;
                    }
                    self.set_link(Link::Down { error });
                    RETRY_MIN
                }
                Attempt::Refused(r) => {
                    self.set_link(Link::Refused(r));
                    REFUSED_RETRY
                }
                Attempt::Maintenance(m) => {
                    self.set_link(Link::Maintenance(m));
                    MAINTENANCE_POLL
                }
                Attempt::Down(error) => {
                    self.set_link(Link::Down { error });
                    let d = delay;
                    delay = (delay * 2).min(RETRY_MAX);
                    d
                }
            };
            if self.sleep(wait) {
                return;
            }
        }
    }

    fn endpoint(&self) -> Result<Endpoint, String> {
        self.profile
            .id()
            .namespace()
            .and_then(|ns| Endpoint::new(&ns, OWNER_ENDPOINT))
            .map_err(|e| e.to_string())
    }

    fn hello(&self) -> Hello {
        Hello {
            protocol: PROTOCOL,
            version: GATEWAY_VERSION.to_owned(),
            role: self.role,
            client_id: self.client_id.clone(),
            pid: std::process::id(),
            profile: self.profile.clone(),
            attach: None,
        }
    }

    /// One connection attempt, launching the owner when allowed.
    fn establish(&self) -> Attempt {
        let endpoint = match self.endpoint() {
            Ok(e) => e,
            Err(e) => return Attempt::Down(e),
        };
        match transport::connect(&endpoint) {
            Ok(stream) => self.greet(stream),
            Err(deck_host::Error::NotRunning(_)) => {
                if let Some(m) = super::maintenance_active(&self.profile.app_root()) {
                    return Attempt::Maintenance(m);
                }
                if !self.autostart {
                    return Attempt::Down("the OmpDeck gateway is not running".into());
                }
                self.start_owner(&endpoint)
            }
            Err(e) => Attempt::Down(e.to_string()),
        }
    }

    fn greet(&self, mut stream: Stream) -> Attempt {
        match handshake(&mut stream, &self.hello(), &self.profile) {
            Handshake::Welcome(w) => Attempt::Up(stream, w),
            Handshake::Refused(Refusal::Maintenance(m)) => Attempt::Maintenance(m),
            Handshake::Refused(Refusal::ShuttingDown) => {
                Attempt::Down(Refusal::ShuttingDown.to_string())
            }
            Handshake::Refused(r) => Attempt::Refused(r),
            Handshake::Failed(e) => Attempt::Down(e),
        }
    }

    /// Launches the owner detached and connects to it within [`START_TIMEOUT`].
    fn start_owner(&self, endpoint: &Endpoint) -> Attempt {
        let program = match &self.gateway_exe {
            Some(p) => Program {
                program: p.clone(),
                args: Vec::new(),
            },
            None => match super::gateway_program() {
                Some(p) => p,
                None => {
                    return Attempt::Down("the omp-deck-gateway executable was not found".into());
                }
            },
        };
        let (exe, args) = program.command(super::serve_args(&self.profile));
        self.set_link(Link::Connecting);
        log::info!("starting the OmpDeck gateway: {}", exe.display());
        let mut launcher = match deck_host::launch_detached(&exe, &args) {
            Ok(l) => l,
            Err(e) => return Attempt::Down(format!("starting {}: {e}", exe.display())),
        };
        let deadline = Instant::now() + START_TIMEOUT;
        let mut pause = Duration::from_millis(25);
        let mut exited = None;
        let attempt = loop {
            match transport::connect(endpoint) {
                Ok(stream) => break self.greet(stream),
                Err(deck_host::Error::NotRunning(_)) => {}
                Err(e) => break Attempt::Down(e.to_string()),
            }
            match exited {
                // Still starting (or its launcher not yet reaped).
                None => match launcher.try_wait() {
                    Ok(status) => exited = status,
                    Err(e) => log::debug!("gateway launcher: {e}"),
                },
                // It failed and the endpoint still is not there (one look after the exit).
                Some(status) if !status.success() => {
                    if let Some(m) = super::maintenance_active(&self.profile.app_root()) {
                        break Attempt::Maintenance(m);
                    }
                    break Attempt::Down(format!(
                        "the OmpDeck gateway exited during startup ({status})"
                    ));
                }
                Some(_) => {}
            }
            if Instant::now() >= deadline {
                break Attempt::Down("the OmpDeck gateway did not start in time".into());
            }
            if self.sleep(pause) {
                break Attempt::Down("closed".into());
            }
            pause = (pause * 2).min(Duration::from_millis(250));
        };
        reap(launcher, exited.is_some());
        attempt
    }

    /// Serves an established link until it drops; returns why.
    fn serve(self: &Arc<Self>, stream: Stream, welcome: Welcome) -> String {
        let (reader, writer) = match (stream.try_clone(), stream.try_clone()) {
            (Ok(r), Ok(w)) => (r, w),
            (Err(e), _) | (_, Err(e)) => {
                stream.shutdown();
                return e.to_string();
            }
        };
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("gateway-client-out".into())
            .spawn(move || write_loop(writer, &rx));
        if let Err(e) = spawned {
            stream.shutdown();
            return e.to_string();
        }
        let welcome = Arc::new(welcome);
        let epoch = welcome.epoch;
        let older = (self.role == Role::Gui
            && self.autostart
            && deck_core::selfupdate::is_newer(GATEWAY_VERSION, &welcome.version))
        .then(|| welcome.version.clone());
        let restore = {
            let mut inner = lock(&self.inner);
            if inner.closing {
                drop(tx);
                return "closed".into();
            }
            inner.conn = Some(Conn {
                welcome: welcome.clone(),
                frames: tx,
            });
            inner.fetches.clear();
            inner.link = Link::Up(welcome.clone());
            Shared::push_locked(&mut inner, ClientEvent::Link(Link::Up(welcome)));
            (
                older.filter(|_| !inner.replaced),
                inner.client_state.clone(),
                inner.interests.clone(),
                inner.fleet.clone(),
                inner.agents.clone(),
            )
        };
        self.wake();
        let (replace, client_state, interests, fleet, agents) = restore;
        if let Some(theirs) = replace {
            log::info!(
                "the running gateway is {theirs}, this client {GATEWAY_VERSION}: replacing it once it is drained"
            );
            self.poll_outdated(epoch);
        }
        if let Some(s) = client_state {
            self.notice(Request::ClientState(s));
        }
        for t in interests.into_iter().filter(needs_subscription) {
            self.notice(Request::Subscribe(t));
        }
        if let Some(f) = fleet {
            self.notice(Request::SetFleetTargets(f));
        }
        if let Some(a) = agents {
            self.notice(Request::SetAgentsTarget(a));
        }
        self.fetch_all();

        let error = self.read_loop(reader);

        stream.shutdown();
        let (waiting, watchers) = {
            let mut inner = lock(&self.inner);
            // Dropping the sender ends the writer thread.
            inner.conn = None;
            inner.fetches.clear();
            (
                std::mem::take(&mut inner.waiting),
                std::mem::take(&mut inner.watchers),
            )
        };
        for (_, r) in waiting {
            r(self, Err(GatewayError::Disconnected));
        }
        for (_, w) in watchers {
            w.finish(self, Err(GatewayError::Disconnected));
        }
        self.wake();
        error
    }

    fn read_loop(self: &Arc<Self>, mut reader: Stream) -> String {
        let mut assembly = Assembly::default();
        loop {
            let (k, payload) = match wire::read_frame(&mut reader) {
                Ok(Some(f)) => f,
                Ok(None) => return "the gateway closed the connection".into(),
                Err(e) => return format!("gateway connection lost: {e}"),
            };
            match k {
                kind::REPLY => match assembly.push(&payload) {
                    Ok(Some((id, bytes))) => {
                        let reply: Reply = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                            Err(GatewayError::Failed(format!(
                                "undecodable gateway reply: {e}"
                            )))
                        });
                        let r = lock(&self.inner).waiting.remove(&id);
                        match r {
                            Some(r) => r(self, reply),
                            None => log::debug!("gateway reply {id} without a request"),
                        }
                    }
                    Ok(None) => {}
                    Err(e) => return format!("gateway reply: {e}"),
                },
                kind::EVENT => match serde_json::from_slice::<ServerEvent>(&payload) {
                    Ok(ev) => self.on_event(ev),
                    // A newer gateway's event this build does not know: skip it.
                    Err(e) => log::debug!("undecodable gateway event: {e}"),
                },
                other => return format!("unexpected gateway frame kind {other}"),
            }
        }
    }
}

/// The link is up to the owner of `epoch`.
fn serves(inner: &Inner, epoch: ServiceEpoch) -> bool {
    inner
        .conn
        .as_ref()
        .is_some_and(|c| c.welcome.epoch == epoch)
}

fn request_label(r: &Request) -> &'static str {
    match r {
        Request::ClientState(_) => "client state",
        Request::Subscribe(_) => "subscribe",
        Request::Unsubscribe(_) => "unsubscribe",
        Request::Query(_) => "query",
        Request::Mutate { .. } => "mutation",
        Request::Outcome(_) => "outcome lookup",
        Request::Flush => "flush",
        Request::Maintenance(_) => "maintenance",
        Request::SetFleetTargets(_) => "fleet targets",
        Request::SetAgentsTarget(_) => "agents target",
        Request::ObserveModel { .. } => "model observation",
    }
}

enum Attempt {
    Up(Stream, Welcome),
    Refused(Refusal),
    Maintenance(MaintenanceMarker),
    Down(String),
}

/// A finished handshake.
pub(crate) enum Handshake {
    Welcome(Welcome),
    Refused(Refusal),
    Failed(String),
}

/// Says hello on a fresh connection and reads the verdict.
pub(crate) fn handshake(stream: &mut Stream, hello: &Hello, profile: &Profile) -> Handshake {
    let result = (|| -> io::Result<Handshake> {
        let bytes = serde_json::to_vec(hello).map_err(io::Error::other)?;
        wire::write_frame(stream, kind::HELLO, &bytes)?;
        stream.flush()?;
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let verdict = match wire::read_frame(stream)? {
            Some((kind::WELCOME, payload)) => {
                let w: Welcome = serde_json::from_slice(&payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                if w.protocol != PROTOCOL {
                    Handshake::Refused(Refusal::Unsupported {
                        ours: PROTOCOL,
                        theirs: w.protocol,
                    })
                } else if !w.profile.same_as(profile) {
                    Handshake::Refused(Refusal::ProfileMismatch { served: w.profile })
                } else {
                    Handshake::Welcome(w)
                }
            }
            Some((kind::REFUSE, payload)) => Handshake::Refused(
                serde_json::from_slice(&payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            ),
            Some((k, _)) => Handshake::Failed(format!("unexpected handshake frame kind {k}")),
            None => Handshake::Failed("the gateway closed the connection".into()),
        };
        stream.set_read_timeout(None)?;
        Ok(verdict)
    })();
    let verdict = result.unwrap_or_else(|e| Handshake::Failed(format!("gateway handshake: {e}")));
    if !matches!(verdict, Handshake::Welcome(_)) {
        stream.shutdown();
    }
    verdict
}

/// One encoded frame.
fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    // Writing into a Vec cannot fail; the size is bounded by the chunking.
    let _ = wire::write_frame(&mut out, kind, payload);
    out
}

fn write_loop(mut stream: Stream, frames: &Receiver<Vec<u8>>) {
    while let Ok(f) = frames.recv() {
        if let Err(e) = stream.write_all(&f).and_then(|()| stream.flush()) {
            log::debug!("gateway client write: {e}");
            break;
        }
    }
    stream.shutdown();
}

/// Lets the launcher go without leaving a zombie (Unix: the AppImage-hosted owner stays in the
/// foreground of the launched process, so it may still run).
#[cfg(unix)]
fn reap(mut launcher: deck_host::Launcher, reaped: bool) {
    if reaped {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("gateway-launcher-reap".into())
        .spawn(move || {
            let _ = launcher.wait();
        });
    if let Err(e) = spawned {
        log::debug!("gateway launcher reaper: {e}");
    }
}

/// Windows: the launcher is the owner's process handle; closing it does not affect the owner.
#[cfg(windows)]
fn reap(launcher: deck_host::Launcher, _reaped: bool) {
    drop(launcher);
}
