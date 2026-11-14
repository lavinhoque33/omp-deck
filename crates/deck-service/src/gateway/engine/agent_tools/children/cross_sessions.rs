//! Peer session tools. Observation owns only JSONL readers and optional read-only ctl subscriptions.
//! Deliver frames are sent once, retained through ACK, and never replayed after uncertainty.
use super::*;
use crate::gateway::protocol::{GatewayError, OrchEnd, OrchWatch};
use crate::status::TabStatus;
use deck_core::ctl::{CtlClient, CtlCommand, LiveSnapshot};
use std::io::{BufReader, Read};
use std::sync::Arc;

const CANNOT_MESSAGE: &str = "That session can't receive messages";
const WATCH_OFF: &str = "Watching was turned off";
const MESSAGE_OFF: &str = "Session messaging is turned off";

#[derive(Default)]
pub(in crate::gateway::engine) struct State {
    resolving: HashMap<u64, Resolving>,
    owned_resolving: HashMap<u64, ()>,
    watches: HashMap<u64, Watch>,
    messages: HashMap<u64, Message>,
    rates: HashMap<(String, String), Rate>,
}

impl Drop for State {
    fn drop(&mut self) {
        if self.messages.is_empty()
            && self
                .watches
                .values()
                .all(|watch| watch.client.is_none() && watch.delivery_client.is_none())
        {
            return;
        }
        let watches = std::mem::take(&mut self.watches);
        let messages = std::mem::take(&mut self.messages);
        if let Err(error) = std::thread::Builder::new()
            .name("gateway-cross-session-retire".into())
            .spawn(move || {
                drop(watches);
                drop(messages);
            })
        {
            log::error!("could not retire session control clients: {error}");
        }
    }
}

struct Resolving {
    kind: Kind,
    root: PathBuf,
    started: Instant,
}
enum Kind {
    Watch {
        next: bool,
        background: bool,
        seconds: Option<u64>,
    },
    Message {
        text: String,
        wake: bool,
    },
}
#[derive(Clone, Copy)]
enum PreparePolicy {
    Watch { next: bool },
    Message,
}
#[derive(Clone)]
struct Endpoint {
    end: OrchEnd,
    id: String,
    ctl_key: Option<String>,
}
struct Watch {
    sender: Endpoint,
    watched: Endpoint,
    since_unix_ms: u64,
    background: bool,
    next_turn: bool,
    waiter: Waiter,
    client: Option<CtlClient>,
    subscribed: bool,
    result: Option<WaitResult>,
    delivery_client: Option<CtlClient>,
    delivery_subscribed: bool,
    delivery_sent: Option<(u64, Instant)>,
    attention_until: Option<Instant>,
    arming: bool,
    read_end: u64,
    file_seen: bool,
    initial_busy: bool,
    next_candidate: Option<u64>,
}
impl Watch {
    fn new(
        resolved: Resolved,
        next: bool,
        background: bool,
        seconds: Option<u64>,
        started: Instant,
        client: Option<CtlClient>,
    ) -> Self {
        let file = resolved.other.end.session_path.clone();
        let until = seconds.map(|seconds| started + Duration::from_secs(seconds));
        let mut waiter = Waiter::from_file(
            String::new(),
            Some(file.clone()),
            None,
            seconds,
            until,
            false,
            started,
        );
        let initial_busy = resolved.other.end.status == TabStatus::Busy;
        let next_candidate =
            (next && !resolved.turn_active && resolved.trailing_custom).then_some(resolved.offset);
        waiter.watch = Some(
            if next && (resolved.turn_active || resolved.missing && initial_busy) {
                TurnWatch::next_turn(file, resolved.offset, resolved.missing)
            } else {
                TurnWatch::any_turn(file, resolved.offset)
            },
        );
        waiter.fallback = false;
        waiter.scan = resolved.reply;
        waiter.scanned = !next && !resolved.missing;
        Self {
            sender: resolved.sender,
            watched: resolved.other,
            since_unix_ms: crate::gateway::unix_ms(),
            background,
            next_turn: next,
            waiter,
            client,
            subscribed: false,
            result: None,
            delivery_client: None,
            delivery_subscribed: false,
            delivery_sent: None,
            attention_until: None,
            arming: true,
            read_end: resolved.read_end,
            file_seen: !resolved.missing,
            initial_busy,
            next_candidate,
        }
    }

    fn project(&self, id: u64) -> OrchWatch {
        OrchWatch {
            id: format!("watch-{id:016x}"),
            watcher: self.sender.end.clone(),
            watched: self.watched.end.clone(),
            since_unix_ms: self.since_unix_ms,
            background: self.background,
            next: self.next_turn,
        }
    }

    fn end(&mut self, status: &'static str, text: &str) {
        if self.result.is_none() {
            self.result = Some(WaitResult::new(
                true,
                result_text(self, status, text),
                status,
            ));
        }
    }

    fn accept_scan(&mut self, watch: TurnWatch, result: Result<Option<TurnScan>, String>) {
        self.waiter.in_flight = false;
        self.waiter.freshness.finish();
        self.waiter.exact = watch.matched_exactly();
        self.waiter.watch = Some(watch);
        match result {
            Ok(Some(scan)) => {
                self.file_seen = true;
                self.waiter.scanned = true;
                self.waiter.scan = scan;
            }
            Ok(None) => self.waiter.scanned = self.waiter.exit_seen,
            Err(error) => {
                self.result = Some(WaitResult::new(
                    false,
                    result_text(self, "error", &error),
                    "error",
                ))
            }
        }
    }

    fn arm_next(&mut self, activity: Activity) {
        if activity == Activity::Unknown {
            return;
        }
        // omp session/agent-session.ts:9711–9730 persists idle nextTurn messages
        // without starting a turn; session/session-manager.ts:3626–3650,2088–2105
        // records/appends them immediately. A trailing custom is therefore ambiguous:
        // decide using activity read after the baseline, never its old Busy snapshot.
        if let Some(from) = self.next_candidate.take()
            && activity == Activity::Busy
        {
            self.waiter.watch = Some(TurnWatch::next_turn(
                self.watched.end.session_path.clone(),
                from,
                false,
            ));
        }
    }

    fn observe(&mut self, now: Instant, enabled: bool, snapshot: Option<&LiveSnapshot>) {
        if !enabled {
            self.end("disabled", WATCH_OFF);
        }
        if self.result.is_some() {
            return;
        }
        let activity = match self.watched.end.status {
            TabStatus::Busy => Activity::Busy,
            TabStatus::Attention => Activity::Attention,
            _ => Activity::Idle,
        };
        if self.arming {
            if self.client.is_some()
                && snapshot.is_none()
                && matches!(self.watched.end.status, TabStatus::Idle | TabStatus::Busy)
                && self.waiter.until.is_none_or(|until| now < until)
            {
                return;
            }
            self.arm_next(activity);
            self.arming = false;
            if !self.next_turn {
                if activity == Activity::Busy
                    && !self.initial_busy
                    && self.waiter.scan.end.is_some()
                {
                    self.waiter.watch = Some(TurnWatch::any_turn(
                        self.watched.end.session_path.clone(),
                        self.read_end,
                    ));
                    self.waiter.scan = empty_scan();
                    self.waiter.scanned = false;
                } else if self.watched.end.status == TabStatus::Idle && self.file_seen {
                    self.end("idle", "The session is idle.");
                    return;
                }
            }
        }
        self.waiter.freshness.observe(activity != Activity::Busy);
        if matches!(self.watched.end.status, TabStatus::Exited { .. }) {
            if !self.waiter.exit_seen {
                self.waiter.exit_seen = true;
                self.waiter.scanned = false;
            }
            if self.waiter.scanned && !self.waiter.in_flight {
                self.end("exited", "The watched session exited");
            } else if self.waiter.until.is_some_and(|until| now >= until) {
                self.end(
                    "timeout",
                    &format!(
                        "Timed out after {} s",
                        self.waiter.seconds.unwrap_or_default()
                    ),
                );
            }
        } else if activity == Activity::Attention {
            let pending = snapshot.and_then(|s| s.pending_ui.first());
            if pending.is_none() && self.client.is_some() {
                let until = *self.attention_until.get_or_insert(now + DELIVERY);
                if self.waiter.until.is_some_and(|deadline| now >= deadline) {
                    self.end(
                        "timeout",
                        &format!(
                            "Timed out after {} s",
                            self.waiter.seconds.unwrap_or_default()
                        ),
                    );
                    return;
                }
                if now < until {
                    return;
                }
            }
            let title = pending.map_or("Pending question (title unavailable)", |ui| {
                ui.title.as_str()
            });
            self.end("attention", &format!("Needs the user's attention: {title}"));
        } else if complete(
            &self.waiter.scan,
            activity,
            false,
            self.waiter.exact,
            self.waiter.freshness.after_idle(),
        ) {
            let status = match self.waiter.scan.end {
                Some(TurnOutcome::Error(_)) => "error",
                Some(TurnOutcome::Aborted) => "aborted",
                _ => "completed",
            };
            self.end(status, "The watched turn finished.");
        } else if self.waiter.until.is_some_and(|until| now >= until) {
            self.end(
                "timeout",
                &format!(
                    "Timed out after {} s",
                    self.waiter.seconds.unwrap_or_default()
                ),
            );
        }
    }
}
struct Message {
    sender: Endpoint,
    recipient: Endpoint,
    text: String,
    wake: bool,
    client: CtlClient,
    subscribed: bool,
    stage: MessageStage,
    until: Instant,
}
#[derive(Clone, Copy)]
enum MessageStage {
    Queued,
    Sent(u64),
}

struct Rate {
    credit: Duration,
    updated: Instant,
}
impl Rate {
    fn new(now: Instant) -> Self {
        Self {
            credit: Duration::from_secs(30),
            updated: now,
        }
    }
    fn take(&mut self, now: Instant) -> bool {
        self.credit = self
            .credit
            .saturating_add(now.saturating_duration_since(self.updated))
            .min(Duration::from_secs(30));
        self.updated = now;
        if self.credit < Duration::from_secs(10) {
            return false;
        }
        self.credit -= Duration::from_secs(10);
        true
    }
}

impl State {
    pub(super) fn needs_controller(&self, key: &SessionKey) -> bool {
        self.watches
            .values()
            .any(|w| w.sender.end.target.as_ref().is_some_and(|t| &t.key == key))
            || self.messages.values().any(|m| {
                [&m.sender, &m.recipient]
                    .into_iter()
                    .any(|e| e.end.target.as_ref().is_some_and(|t| &t.key == key))
            })
    }
    pub(super) fn background_count(&self, root: &Path) -> usize {
        self.watches
            .values()
            .filter(|w| {
                w.background && w.delivery_sent.is_none() && w.sender.end.session_path == root
            })
            .count()
            + self
                .resolving
                .values()
                .filter(|r| {
                    r.root == root
                        && matches!(
                            r.kind,
                            Kind::Watch {
                                background: true,
                                ..
                            }
                        )
                })
                .count()
    }
    pub(super) fn contains(&self, request: u64) -> bool {
        self.resolving.contains_key(&request)
            || self.owned_resolving.contains_key(&request)
            || self.watches.contains_key(&request)
            || self.messages.contains_key(&request)
    }
    fn delivery_pending(&self, root: &Path, ack_only: bool) -> bool {
        self.messages.values().any(|m| {
            m.recipient.end.session_path == root
                && (!ack_only || matches!(m.stage, MessageStage::Sent(_)))
        }) || self.watches.values().any(|w| {
            w.sender.end.session_path == root
                && (w.delivery_sent.is_some() || (!ack_only && w.background && w.result.is_some()))
        })
    }
    fn projection(&self) -> Vec<OrchWatch> {
        let mut watches: Vec<_> = self
            .watches
            .iter()
            .filter(|(_, w)| w.result.is_none() && w.delivery_sent.is_none())
            .map(|(id, w)| w.project(*id))
            .collect();
        watches.sort_by(|a, b| (a.since_unix_ms, &a.id).cmp(&(b.since_unix_ms, &b.id)));
        watches
    }

    fn stop_watch(&mut self, id: &str) -> Result<(), GatewayError> {
        let request = id
            .strip_prefix("watch-")
            .and_then(|s| u64::from_str_radix(s, 16).ok())
            .ok_or_else(|| GatewayError::Failed("Unknown watch".to_owned()))?;
        let watch = self
            .watches
            .get_mut(&request)
            .filter(|w| w.result.is_none() && w.delivery_sent.is_none())
            .ok_or_else(|| GatewayError::Failed("Unknown watch".to_owned()))?;
        watch.end("stopped", "Stopped by the user");
        Ok(())
    }
}

#[derive(Deserialize)]
struct WatchArgs {
    session: String,
    #[serde(default)]
    next: bool,
    #[serde(default)]
    background: bool,
    timeout_seconds: Option<u64>,
}
#[derive(Deserialize)]
struct MessageArgs {
    session: String,
    text: String,
    #[serde(default = "wake_default")]
    wake: bool,
}
fn wake_default() -> bool {
    true
}

fn parse_request(
    name: &str,
    args: serde_json::Value,
    watching: bool,
    messaging: bool,
) -> Result<(String, Kind), String> {
    if name == "watch_session" {
        if !watching {
            return Err(WATCH_OFF.to_owned());
        }
        let args: WatchArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
        let seconds = if args.background {
            args.timeout_seconds
        } else {
            Some(args.timeout_seconds.unwrap_or(600))
        };
        let max = if args.background { 86400 } else { 3600 };
        if seconds.is_some_and(|n| !(1..=max).contains(&n)) {
            return Err(format!("timeout_seconds must be between 1 and {max}"));
        }
        Ok((
            args.session,
            Kind::Watch {
                next: args.next,
                background: args.background,
                seconds,
            },
        ))
    } else {
        if !messaging {
            return Err(MESSAGE_OFF.to_owned());
        }
        let args: MessageArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
        if args.text.is_empty() || args.text.len() > 8 * 1024 {
            return Err("text must contain 1 to 8192 bytes".to_owned());
        }
        Ok((
            args.session,
            Kind::Message {
                text: args.text,
                wake: args.wake,
            },
        ))
    }
}

fn delivery_title(prefix: &str, title: &str, reference: &str, status: Option<&str>) -> String {
    let fixed = prefix.chars().count()
        + 3
        + reference.chars().count()
        + status.map_or(0, |value| 1 + value.chars().count());
    let available = 200usize.saturating_sub(fixed);
    let end = title
        .char_indices()
        .nth(available)
        .map_or(title.len(), |(at, _)| at);
    let title = &title[..end];
    match status {
        Some(status) => format!("{prefix}{title} ({reference}) {status}"),
        None => format!("{prefix}{title} ({reference})"),
    }
}

fn message_command(sender: &Endpoint, recipient: &Endpoint, text: &str, wake: bool) -> CtlCommand {
    CtlCommand::Deliver {
        kind: deck_core::ctl::DeliverKind::Message,
        root: recipient.end.session_path.clone(),
        title: delivery_title("Message from ", &sender.end.title, &sender.end.ref_, None),
        text: format!(
            "{text}\nReply with ompdeck_message_session to {}.",
            sender.end.ref_
        ),
        details: None,
        trigger_turn: wake,
    }
}

pub(in crate::gateway) enum Update {
    OwnedRef {
        request: u64,
        name: String,
        args: serde_json::Value,
        result: Result<String, String>,
    },
    Resolved {
        request: u64,
        result: Result<Box<Resolved>, String>,
    },
    Scan {
        request: u64,
        path: PathBuf,
        id: String,
        watch: TurnWatch,
        result: Result<Option<TurnScan>, String>,
    },
}
pub(in crate::gateway) struct Resolved {
    sender: Endpoint,
    other: Endpoint,
    offset: u64,
    read_end: u64,
    missing: bool,
    turn_active: bool,
    trailing_custom: bool,
    reply: TurnScan,
}

pub(in crate::gateway::engine) fn session_ref(id: &str) -> String {
    let start = id
        .char_indices()
        .rev()
        .nth(7)
        .map_or(0, |(offset, _)| offset);
    id[start..].to_owned()
}

/// A bounded header read, not a filename guess. It must be an actual omp session header.
pub(in crate::gateway::engine) fn session_header(path: &Path) -> Result<(PathBuf, String), String> {
    #[derive(Deserialize)]
    struct Header {
        #[serde(rename = "type")]
        kind: String,
        id: Option<String>,
        cwd: Option<PathBuf>,
    }
    let file = std::fs::File::open(path)
        .map_err(|e| format!("Cannot read session {}: {e}", path.display()))?;
    let stream = serde_json::Deserializer::from_reader(BufReader::new(file.take(32 * 1024)))
        .into_iter::<Header>();
    for header in stream {
        let Ok(h) = header else {
            break;
        };
        if h.kind == "session"
            && let (Some(cwd), Some(id)) = (h.cwd, h.id)
            && cwd.is_absolute()
            && !id.is_empty()
        {
            return Ok((cwd, id));
        }
    }
    Err(format!(
        "{} is not an OmpDeck/omp session file",
        path.display()
    ))
}

fn native_session_id(path: &Path) -> Option<String> {
    if path.extension()?.to_str()? != "jsonl" {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let (_, id) = stem.rsplit_once('_')?;
    if id.len() != 36
        || !id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    Some(id.to_owned())
}

fn endpoint_id(endpoint: &Endpoint) -> Result<String, String> {
    session_header(&endpoint.end.session_path)
        .map(|(_, id)| id)
        .or_else(|error| {
            // omp announces a native path before persisting its first tool-bearing turn.
            // Only a managed omp process may supply this identity, and only while absent.
            if endpoint.end.target.is_some()
                && std::fs::metadata(&endpoint.end.session_path)
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
            {
                native_session_id(&endpoint.end.session_path).ok_or(error)
            } else {
                Err(error)
            }
        })
}

fn message_capability(features: &[String], wake: bool) -> Result<(), String> {
    if !features.iter().any(|f| f == "deliver") {
        return Err(CANNOT_MESSAGE.to_owned());
    }
    if !wake && !features.iter().any(|f| f == "deliver_no_wake") {
        return Err("That session can't hold a message without waking; restart it to update OmpDeck's extension".to_owned());
    }
    Ok(())
}

fn id_matches(id: &str, reference: &str) -> bool {
    (reference.len() == 8
        && id
            .get(id.len().saturating_sub(8)..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(reference)))
        || id
            .get(..reference.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(reference))
}

fn resolve(reference: &str, mut candidates: Vec<Endpoint>) -> Result<Endpoint, String> {
    if reference.len() < 8 {
        return Err(format!(
            "Unknown session ref {reference:?}; use at least 8 characters of its id"
        ));
    }
    let mut found: Vec<Endpoint> = Vec::new();
    let mut failed = Vec::new();
    for candidate in &mut candidates {
        if !candidate.id.is_empty() && !id_matches(&candidate.id, reference) {
            continue;
        }
        match endpoint_id(candidate) {
            Ok(id) => {
                if !candidate.id.is_empty() && candidate.id != id {
                    continue;
                }
                candidate.id = id;
                if !id_matches(&candidate.id, reference) {
                    continue;
                }
                candidate.end.ref_ = session_ref(&candidate.id);
                if !found.iter().any(|known| known.id == candidate.id) {
                    found.push(candidate.clone());
                }
            }
            Err(error) if !candidate.id.is_empty() && id_matches(&candidate.id, reference) => {
                failed.push(error)
            }
            Err(_) => {}
        }
    }
    if found.len() == 1 {
        return Ok(found.remove(0));
    }
    if found.len() > 1 {
        return Err(format!(
            "Ambiguous session ref {reference:?}: {}. Use a longer id prefix or full id",
            found
                .iter()
                .map(|e| format!("{} ({})", e.end.title, e.id))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !failed.is_empty() {
        return Err(failed.join("; "));
    }
    let available = candidates
        .iter()
        .filter(|candidate| !candidate.id.is_empty())
        .map(|candidate| {
            format!(
                "{} ({}, ref {})",
                candidate.end.title,
                candidate.id,
                session_ref(&candidate.id)
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Unknown session ref {reference:?}. Candidates: {available}"
    ))
}

struct TurnBaseline {
    current: u64,
    end: u64,
    custom: Option<u64>,
    active: bool,
    missing: bool,
}

/// Find the last real user/custom turn without allocating message content.
fn current_turn(path: &Path) -> Result<TurnBaseline, String> {
    #[derive(Deserialize)]
    struct Entry {
        #[serde(rename = "type")]
        kind: Option<String>,
        message: Option<RoleOnly>,
    }
    #[derive(Deserialize)]
    struct RoleOnly {
        role: Option<String>,
        #[serde(rename = "stopReason")]
        stop: Option<String>,
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(TurnBaseline {
                current: 0,
                end: 0,
                custom: None,
                active: false,
                missing: true,
            });
        }
        Err(error) => return Err(error.to_string()),
    };
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    let mut entries = serde_json::Deserializer::from_reader(BufReader::new(file.take(length)))
        .into_iter::<Entry>();
    let mut last = 0;
    let mut custom = None;
    let mut active = false;
    loop {
        let offset = entries.byte_offset() as u64;
        match entries.next() {
            Some(Ok(entry)) => {
                let message = entry.message;
                match message.as_ref().and_then(|m| m.role.as_deref()) {
                    Some("user") => {
                        last = offset;
                        active = true;
                        custom = None;
                    }
                    Some("assistant") => {
                        if !active && let Some(start) = custom.take() {
                            last = start;
                        }
                        active =
                            message.as_ref().and_then(|m| m.stop.as_deref()) == Some("toolUse");
                    }
                    role if !active
                        && (role == Some("custom")
                            || entry.kind.as_deref() == Some("custom_message")) =>
                    {
                        custom = Some(offset)
                    }
                    _ => {}
                }
            }
            Some(Err(error)) if error.is_eof() => break,
            Some(Err(error)) => return Err(format!("Could not read session turn: {error}")),
            None => break,
        }
    }
    Ok(TurnBaseline {
        current: last,
        end: length,
        custom,
        active,
        missing: false,
    })
}

fn scan_watch(
    path: &Path,
    id: &str,
    watch: &mut TurnWatch,
    allow_missing: bool,
) -> Result<Option<TurnScan>, String> {
    match std::fs::metadata(path) {
        Ok(_) => {}
        Err(error) if allow_missing && error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    }
    let (_, current) = session_header(path)?;
    if current != id {
        return Err("The watched session file changed identity".to_owned());
    }
    watch
        .poll()
        .map(Some)
        .map_err(|e| format!("Could not read the watched session: {e}"))
}

fn prepare(
    sender_path: PathBuf,
    reference: String,
    candidates: Vec<Endpoint>,
    policy: PreparePolicy,
) -> Result<Resolved, String> {
    let mut sender = candidates
        .iter()
        .find(|c| c.end.session_path == sender_path)
        .cloned()
        .ok_or_else(|| "The sender's session changed".to_owned())?;
    sender.id = endpoint_id(&sender)?;
    sender.end.ref_ = session_ref(&sender.id);
    let other = resolve(&reference, candidates)?;
    if sender.id == other.id {
        return Err(match policy {
            PreparePolicy::Watch { .. } => "You cannot watch your own session",
            PreparePolicy::Message => "You cannot message your own session",
        }
        .to_owned());
    }
    let resolved = Resolved {
        sender,
        other,
        offset: 0,
        read_end: 0,
        missing: false,
        turn_active: false,
        trailing_custom: false,
        reply: empty_scan(),
    };
    match policy {
        PreparePolicy::Watch { next } => {
            let baseline = current_turn(&resolved.other.end.session_path)?;
            prepare_watch(resolved, next, baseline)
        }
        PreparePolicy::Message => Ok(resolved),
    }
}

fn prepare_watch(
    mut resolved: Resolved,
    next: bool,
    baseline: TurnBaseline,
) -> Result<Resolved, String> {
    let busy = resolved.other.end.status == TabStatus::Busy;
    resolved.read_end = if next {
        baseline.end
    } else {
        baseline.custom.unwrap_or(baseline.end)
    };
    resolved.missing = baseline.missing;
    resolved.turn_active = baseline.active;
    resolved.trailing_custom = baseline.custom.is_some();
    resolved.offset = if next {
        baseline.end
    } else if busy {
        baseline.custom.unwrap_or(baseline.current)
    } else {
        baseline.current
    };
    if !next && !baseline.missing {
        resolved.reply =
            TurnWatch::any_turn(resolved.other.end.session_path.clone(), resolved.offset)
                .poll()
                .map_err(|e| e.to_string())?;
    }
    Ok(resolved)
}

fn result_text(watch: &Watch, status: &str, text: &str) -> String {
    with_reply(
        format!(
            "{} ({}) | status: {status}\n{text}",
            watch.watched.end.title, watch.watched.end.ref_
        ),
        &watch.waiter.scan,
        Some(&watch.watched.end.session_path),
    )
}

impl super::super::AgentTools {
    fn background_wait_count(&self, root: &Path) -> usize {
        self.cross_sessions.background_count(root)
            + self
                .children
                .entries
                .values()
                .filter(|c| c.opener_root == root && c.background.is_some())
                .count()
            + self
                .operations
                .iter()
                .filter(|(_, op)| {
                    matches!(op, Operation::Wait(w) if w.background)
                        || matches!(
                            op,
                            Operation::Attach {
                                action: Action::Wait {
                                    background: true,
                                    ..
                                },
                                ..
                            }
                        )
                })
                .filter(|(request, _)| {
                    self.pending
                        .get(request)
                        .is_some_and(|p| p.root.as_deref() == Some(root))
                })
                .count()
    }
}

impl Engine {
    fn cross_candidates(&self) -> Vec<Endpoint> {
        let mut result = Vec::new();
        let running = self.running.latest();
        for m in &self.sess.managed {
            if m.role != SessionRole::Session {
                continue;
            }
            let Some(path) = m.ctl.session_path() else {
                continue;
            };
            let snapshot = m
                .live
                .as_ref()
                .filter(|l| l.client.connected())
                .and_then(|l| l.client.latest());
            let model = snapshot.as_ref().and_then(|s| s.model.as_ref());
            result.push(Endpoint {
                end: OrchEnd {
                    session_path: path.to_owned(),
                    title: m.display_label(),
                    target: Some(m.ctl.target().clone()),
                    model: model
                        .map(|m| m.selector.clone())
                        .or_else(|| m.ctl.current_model()),
                    thinking: model.map(|m| m.thinking.clone()),
                    status: if m.exited() {
                        TabStatus::Exited {
                            clean: m.ctl.terminal().exit_code() == Some(0),
                        }
                    } else {
                        TabStatus::of(None, m.ctl.activity(), false)
                    },
                    live: !m.exited(),
                    ref_: String::new(),
                },
                id: String::new(),
                ctl_key: m.ctl_key.clone(),
            });
        }
        if let Some(groups) = self.sessions_watch.latest() {
            for session in groups.iter().flat_map(|g| &g.sessions) {
                if result.iter().any(|e| e.end.session_path == session.path) {
                    continue;
                }
                let name = running.as_ref().and_then(|r| {
                    r.holder(&session.path).map(ToOwned::to_owned).or_else(|| {
                        r.is_running(&session.id, &session.path)
                            .then(|| deck_core::mux::session_name_for(&session.id))
                    })
                });
                let live = name.is_some();
                let activity = name
                    .as_deref()
                    .and_then(|n| running.as_ref().map(|r| r.activity_of(n)))
                    .unwrap_or(Activity::Unknown);
                let ctl_key = name.filter(|n| running.as_ref().is_some_and(|r| r.has_ctl(n)));
                result.push(Endpoint {
                    end: OrchEnd {
                        session_path: session.path.clone(),
                        title: session.label().to_owned(),
                        target: None,
                        model: None,
                        thinking: None,
                        status: if live {
                            TabStatus::of(None, activity, false)
                        } else {
                            TabStatus::Exited { clean: true }
                        },
                        live,
                        ref_: session_ref(&session.id),
                    },
                    id: session.id.clone(),
                    ctl_key,
                });
            }
        }
        result
    }

    /// Resolve an id on a worker, then retain the original tools' opener ownership check.
    pub(in crate::gateway::engine) fn begin_owned_session_ref(
        &mut self,
        request: u64,
        name: &str,
        args: &serde_json::Value,
    ) -> Result<bool, String> {
        let Some(reference) = args
            .get("session")
            .and_then(|s| s.as_str())
            .filter(|s| !s.starts_with("s-"))
        else {
            return Ok(false);
        };
        let root = self.agent_tools.pending[&request]
            .root
            .clone()
            .ok_or_else(|| UNKNOWN.to_owned())?;
        let candidates = self.cross_candidates();
        let owned: Vec<_> = self
            .agent_tools
            .children
            .entries
            .iter()
            .filter(|(_, child)| !child.invalidated)
            .filter_map(|(handle, child)| {
                Some((
                    handle.clone(),
                    child.opener_root.clone(),
                    child.file.clone()?,
                ))
            })
            .collect();
        let reference = reference.to_owned();
        let name = name.to_owned();
        let args = args.clone();
        self.agent_tools
            .cross_sessions
            .owned_resolving
            .insert(request, ());
        self.spawn_worker("owned-session-ref", move || {
            let result = resolve(&reference, candidates).and_then(|endpoint| {
                let root = canonical(&root);
                owned.into_iter().find_map(|(handle, opener, file)| (opener == root && file == endpoint.end.session_path).then_some(handle))
                    .ok_or_else(|| "That session is not a child opened by this session; use ompdeck_watch_session or ompdeck_message_session for independent sessions".to_owned())
            });
            Fin::CrossSession(Update::OwnedRef { request, name, args, result })
        });
        Ok(true)
    }

    pub(in crate::gateway::engine) fn begin_cross_session_tool(
        &mut self,
        request: u64,
        name: &str,
        args: serde_json::Value,
        now: Instant,
    ) -> Result<(), String> {
        let (reference, kind) = parse_request(
            name,
            args,
            self.cfg.agent_tools.watch_session,
            self.cfg.agent_tools.message_session,
        )?;
        let root = self.agent_tools.pending[&request]
            .root
            .clone()
            .ok_or_else(|| "The sender has no omp session file".to_owned())?;
        if matches!(
            kind,
            Kind::Watch {
                background: true,
                ..
            }
        ) && self.cross_background_count(&root) >= BACKGROUND_CAP
        {
            return Err("Too many background waits".to_owned());
        }
        let candidates = self.cross_candidates();
        let handle_path = if reference.starts_with("s-") {
            let child = self.agent_tools.children.owned(&reference, &root)?;
            Some(child.file.clone().ok_or_else(|| STALE.to_owned())?)
        } else {
            None
        };
        let policy = match &kind {
            Kind::Watch { next, .. } => PreparePolicy::Watch { next: *next },
            Kind::Message { .. } => PreparePolicy::Message,
        };
        self.agent_tools.cross_sessions.resolving.insert(
            request,
            Resolving {
                kind,
                root: root.clone(),
                started: now,
            },
        );
        self.spawn_worker("cross-session-resolve", move || {
            let reference = if let Some(path) = handle_path {
                match session_header(&path) {
                    Ok((_, id)) => id,
                    Err(error) => {
                        return Fin::CrossSession(Update::Resolved {
                            request,
                            result: Err(error),
                        });
                    }
                }
            } else {
                reference
            };
            Fin::CrossSession(Update::Resolved {
                request,
                result: prepare(root, reference, candidates, policy).map(Box::new),
            })
        });
        self.wake_within(POLL);
        Ok(())
    }

    pub(in crate::gateway::engine) fn on_cross_session_update(&mut self, update: Update) {
        match update {
            Update::OwnedRef {
                request,
                name,
                mut args,
                result,
            } => {
                if self
                    .agent_tools
                    .cross_sessions
                    .owned_resolving
                    .remove(&request)
                    .is_none()
                    || !self.agent_tool_current(request)
                {
                    return;
                }
                let result = result.and_then(|handle| {
                    args["session"] = serde_json::Value::String(handle);
                    let now = Instant::now();
                    match name.as_str() {
                        "send_prompt" | "wait_session" => {
                            self.begin_child_tool(request, &name, args, now)
                        }
                        "answer_question" => self.begin_answer_question(request, args, now),
                        "finish_worktree" => self.begin_child_finish(request, args, now),
                        _ => Err("Unknown child tool".to_owned()),
                    }
                });
                if let Err(error) = result {
                    self.finish_agent_tool(request, false, error);
                }
            }
            Update::Resolved { request, result } => {
                let Some(resolving) = self.agent_tools.cross_sessions.resolving.remove(&request)
                else {
                    return;
                };
                if !self.agent_tool_current(request) {
                    return;
                }
                let outcome = (|| {
                    let resolved = result?;
                    if !self.cross_endpoint_current(&resolved.sender)
                        || !self.cross_endpoint_current(&resolved.other)
                    {
                        return Err(
                            "The session changed while resolving its ref; try again".to_owned()
                        );
                    }
                    match resolving.kind {
                        Kind::Watch {
                            next,
                            background,
                            seconds,
                        } => {
                            if !self.cfg.agent_tools.watch_session {
                                return Err(WATCH_OFF.to_owned());
                            }
                            if background
                                && self.cross_background_count(&resolving.root) >= BACKGROUND_CAP
                            {
                                return Err("Too many background waits".to_owned());
                            }
                            let client = if self.cross_snapshot(&resolved.other, None).is_none() {
                                self.cross_observer(&resolved.other)
                            } else {
                                None
                            };
                            let watch = Watch::new(
                                *resolved,
                                next,
                                background,
                                seconds,
                                resolving.started,
                                client,
                            );
                            let label = format!(
                                "Watching {} ({}) in the background; its result will arrive as a message.",
                                watch.watched.end.title, watch.watched.end.ref_
                            );
                            self.agent_tools
                                .cross_sessions
                                .watches
                                .insert(request, watch);
                            if background {
                                self.finish_agent_tool(request, true, label);
                            }
                        }
                        Kind::Message { text, wake } => {
                            if !self.cfg.agent_tools.message_session {
                                return Err(MESSAGE_OFF.to_owned());
                            }
                            if !resolved.other.end.live {
                                return Err(CANNOT_MESSAGE.to_owned());
                            }
                            let client = self
                                .cross_observer(&resolved.other)
                                .ok_or_else(|| CANNOT_MESSAGE.to_owned())?;
                            self.agent_tools.cross_sessions.messages.insert(
                                request,
                                Message {
                                    sender: resolved.sender,
                                    recipient: resolved.other,
                                    text,
                                    wake,
                                    client,
                                    subscribed: false,
                                    stage: MessageStage::Queued,
                                    until: Instant::now() + DELIVERY,
                                },
                            );
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = outcome {
                    self.finish_agent_tool(request, false, error);
                }
            }
            Update::Scan {
                request,
                path,
                id,
                watch,
                result,
            } => {
                let valid = self
                    .agent_tools
                    .cross_sessions
                    .watches
                    .get(&request)
                    .is_some_and(|w| {
                        w.result.is_none()
                            && w.watched.id == id
                            && w.watched.end.session_path == path
                            && self.cross_endpoint_current(&w.watched)
                    });
                if !valid {
                    return;
                }
                let w = self
                    .agent_tools
                    .cross_sessions
                    .watches
                    .get_mut(&request)
                    .expect("watch");
                w.accept_scan(watch, result);
            }
        }
        self.waker.now();
    }

    fn cross_observer(&self, endpoint: &Endpoint) -> Option<CtlClient> {
        let key = endpoint.ctl_key.as_deref()?;
        let waker = self.waker.clone();
        Some(CtlClient::spawn(
            deck_core::ctl::ctl_dir(&self.ctl_ns, key),
            move || waker.now(),
        ))
    }

    fn cross_endpoint_current(&self, endpoint: &Endpoint) -> bool {
        if let Some(target) = &endpoint.end.target
            && let Some(i) = self.sess.find(&target.key)
        {
            let m = &self.sess.managed[i];
            return m.ctl.target() == target
                && m.ctl.session_path() == Some(endpoint.end.session_path.as_path())
                && m.live
                    .as_ref()
                    .and_then(|l| l.client.latest())
                    .is_none_or(|s| {
                        s.session_file.as_deref() == Some(endpoint.end.session_path.as_path())
                    });
        }
        self.sessions_watch.latest().is_some_and(|groups| {
            groups
                .iter()
                .flat_map(|g| &g.sessions)
                .any(|s| s.path == endpoint.end.session_path && s.id == endpoint.id)
        }) || self
            .running
            .latest()
            .is_some_and(|r| r.is_running(&endpoint.id, &endpoint.end.session_path))
    }

    fn cross_sender_alive(&self, sender: &Endpoint) -> bool {
        self.cross_endpoint_current(sender)
            && sender.end.target.as_ref().is_some_and(|target| {
                self.sess.find(&target.key).is_some_and(|i| {
                    let m = &self.sess.managed[i];
                    !m.exited() && m.live.as_ref().is_some_and(|l| l.client.connected())
                })
            })
    }

    fn cross_watcher_alive(&self, sender: &Endpoint) -> bool {
        if !self.cross_endpoint_current(sender) {
            return false;
        }
        if let Some(i) = sender
            .end
            .target
            .as_ref()
            .and_then(|t| self.sess.find(&t.key))
        {
            return !self.sess.managed[i].exited();
        }
        // Closing a tab/controller does not end a persistent native session.
        self.running
            .latest()
            .is_none_or(|r| r.blind || r.is_running(&sender.id, &sender.end.session_path))
    }

    fn cross_snapshot(
        &self,
        endpoint: &Endpoint,
        client: Option<&CtlClient>,
    ) -> Option<Arc<LiveSnapshot>> {
        endpoint
            .end
            .target
            .as_ref()
            .and_then(|t| self.sess.find(&t.key))
            .and_then(|i| self.sess.managed[i].live.as_ref())
            .filter(|l| l.client.connected())
            .and_then(|l| l.client.latest())
            .or_else(|| client.filter(|c| c.connected()).and_then(CtlClient::latest))
    }

    fn refresh_cross_endpoint(&self, endpoint: &mut Endpoint, snapshot: Option<&LiveSnapshot>) {
        if let Some(target) = &endpoint.end.target
            && let Some(i) = self.sess.find(&target.key)
        {
            let m = &self.sess.managed[i];
            endpoint.end.title = m.display_label();
            endpoint.end.live = !m.exited();
            endpoint.end.status = if m.exited() {
                TabStatus::Exited {
                    clean: m.ctl.terminal().exit_code() == Some(0),
                }
            } else {
                TabStatus::of(None, m.ctl.activity(), false)
            };
        } else if let Some(running) = self.running.latest().filter(|r| !r.blind) {
            endpoint.end.live = running.is_running(&endpoint.id, &endpoint.end.session_path);
            endpoint.end.status = if endpoint.end.live {
                TabStatus::of(
                    None,
                    running
                        .holder(&endpoint.end.session_path)
                        .map_or(Activity::Unknown, |n| running.activity_of(n)),
                    false,
                )
            } else {
                TabStatus::Exited { clean: true }
            };
        }
        let managed_exited = endpoint
            .end
            .target
            .as_ref()
            .and_then(|t| self.sess.find(&t.key))
            .is_some_and(|i| self.sess.managed[i].exited());
        if !managed_exited && let Some(snapshot) = snapshot {
            endpoint.end.live = true;
            if let Some(model) = &snapshot.model {
                endpoint.end.model = Some(model.selector.clone());
                endpoint.end.thinking = Some(model.thinking.clone());
            }
            if !snapshot.pending_ui.is_empty() {
                endpoint.end.status = TabStatus::Attention;
            } else if let Some(idle) = snapshot.main_idle {
                endpoint.end.status = if idle {
                    TabStatus::Idle
                } else {
                    TabStatus::Busy
                };
            }
        }
    }

    fn retire_cross_clients(&self, clients: [Option<CtlClient>; 2]) {
        if clients.iter().any(Option::is_some) {
            self.spawn_worker("cross-session-retire", move || {
                drop(clients);
                Fin::Logged {
                    what: "retiring session control clients",
                    result: Ok(()),
                }
            });
        }
    }

    fn retire_cross_watch(&self, mut watch: Watch) {
        self.retire_cross_clients([watch.client.take(), watch.delivery_client.take()]);
    }

    pub(in crate::gateway::engine) fn cross_background_count(&self, root: &Path) -> usize {
        self.agent_tools.background_wait_count(root)
    }

    pub(in crate::gateway::engine) fn cancel_cross_session_request(&mut self, request: u64) {
        self.agent_tools.cross_sessions.resolving.remove(&request);
        self.agent_tools
            .cross_sessions
            .owned_resolving
            .remove(&request);
        if self
            .agent_tools
            .cross_sessions
            .messages
            .get(&request)
            .is_some_and(|m| matches!(m.stage, MessageStage::Queued))
            && let Some(message) = self.agent_tools.cross_sessions.messages.remove(&request)
        {
            self.retire_cross_clients([Some(message.client), None]);
        }
        if self
            .agent_tools
            .cross_sessions
            .watches
            .get(&request)
            .is_some_and(|w| !w.background)
            && let Some(watch) = self.agent_tools.cross_sessions.watches.remove(&request)
        {
            self.retire_cross_watch(watch);
        }
    }

    pub(in crate::gateway::engine) fn cross_session_watches(&self) -> Vec<OrchWatch> {
        self.agent_tools.cross_sessions.projection()
    }

    pub(in crate::gateway::engine) fn stop_cross_watch(
        &mut self,
        id: &str,
    ) -> Result<(), GatewayError> {
        self.agent_tools.cross_sessions.stop_watch(id)?;
        self.waker.now();
        Ok(())
    }

    pub(in crate::gateway::engine) fn cross_session_delivery_pending(
        &self,
        target: &SessionTarget,
        ack_only: bool,
    ) -> bool {
        self.sess
            .find(&target.key)
            .filter(|i| self.sess.managed[*i].ctl.target() == target)
            .and_then(|i| self.sess.managed[i].ctl.session_path())
            .is_some_and(|root| {
                self.agent_tools
                    .cross_sessions
                    .delivery_pending(root, ack_only)
            })
    }

    pub(in crate::gateway::engine) fn cross_controller_needed(&self, key: &SessionKey) -> bool {
        self.sess
            .find(key)
            .and_then(|i| self.sess.managed[i].ctl.session_path())
            .is_some_and(|path| {
                self.agent_tools
                    .cross_sessions
                    .delivery_pending(path, false)
            })
    }

    fn cross_restore_busy(&self, endpoint: &Endpoint) -> bool {
        endpoint
            .end
            .target
            .as_ref()
            .is_some_and(|t| self.checkpoint_restore_busy(t))
            || self.sess.managed.iter().any(|m| {
                m.ctl.session_path() == Some(endpoint.end.session_path.as_path())
                    && self.checkpoint_restore_busy(m.ctl.target())
            })
    }

    pub(in crate::gateway::engine) fn step_cross_sessions(&mut self, now: Instant) {
        let ids: Vec<_> = self
            .agent_tools
            .cross_sessions
            .watches
            .keys()
            .copied()
            .collect();
        for request in ids {
            let Some(mut w) = self.agent_tools.cross_sessions.watches.remove(&request) else {
                continue;
            };
            if let Some((rid, until)) = w.delivery_sent {
                let Some(client) = &w.delivery_client else {
                    unreachable!("sent delivery owns its ctl client")
                };
                let acknowledged = client.take_replies().into_iter().any(|(id, _)| id == rid);
                if acknowledged || !client.connected() || now >= until {
                    self.retire_cross_watch(w);
                    continue;
                }
                self.agent_tools.cross_sessions.watches.insert(request, w);
                continue;
            }
            if !self.cross_watcher_alive(&w.sender) {
                self.retire_cross_watch(w);
                continue;
            }
            if !self.cfg.agent_tools.watch_session {
                w.end("disabled", WATCH_OFF);
            }
            if w.result.is_none() && !self.cross_endpoint_current(&w.watched) {
                w.result = Some(WaitResult::new(
                    false,
                    result_text(
                        &w,
                        "exited",
                        "The watched session exited or switched conversations",
                    ),
                    "exited",
                ));
            }
            if w.client.is_none() && self.cross_snapshot(&w.watched, None).is_none() {
                w.client = self.cross_observer(&w.watched);
            }
            if let Some(client) = &w.client {
                if !client.connected() {
                    w.subscribed = false;
                } else if !w.subscribed {
                    client.send(CtlCommand::Subscribe {
                        snapshots: true,
                        tools: false,
                    });
                    w.subscribed = true;
                }
                client.take_replies();
            }
            let snapshot = self.cross_snapshot(&w.watched, w.client.as_ref());
            if snapshot.as_ref().is_some_and(|s| {
                s.session_file.as_deref() != Some(w.watched.end.session_path.as_path())
            }) && w.result.is_none()
            {
                w.result = Some(WaitResult::new(
                    false,
                    result_text(&w, "exited", "The watched session switched conversations"),
                    "exited",
                ));
            }
            let sender_snapshot = self.cross_snapshot(&w.sender, None);
            self.refresh_cross_endpoint(&mut w.sender, sender_snapshot.as_deref());
            self.refresh_cross_endpoint(&mut w.watched, snapshot.as_deref());
            if let Some(activity) = w
                .watched
                .end
                .target
                .as_ref()
                .and_then(|target| self.sess.find(&target.key))
                .map(|index| self.sess.managed[index].ctl.activity())
            {
                w.arm_next(activity);
            }
            w.observe(now, self.cfg.agent_tools.watch_session, snapshot.as_deref());
            if let Some(result) = w.result.take() {
                if !w.background {
                    self.finish_agent_tool(request, result.ok, result.text);
                    self.retire_cross_watch(w);
                    continue;
                }
                if let Some(index) = self.sess.managed.iter().position(|m| {
                    m.ctl.session_path() == Some(w.sender.end.session_path.as_path())
                        && !m.exited()
                        && m.live.as_ref().is_some_and(|l| {
                            l.client.connected()
                                && l.client.features().iter().any(|f| f == "deliver")
                        })
                }) && !self.cross_restore_busy(&w.sender)
                {
                    self.deliver_child_message(index, CtlCommand::Deliver { root: w.sender.end.session_path.clone(),
                        kind: deck_core::ctl::DeliverKind::Watch,
                        title: delivery_title("Watch: ", &w.watched.end.title, &w.watched.end.ref_, Some(result.status)), text: result.text,
                        details: Some(serde_json::json!({"ref": w.watched.end.ref_, "status": result.status})), trigger_turn: true });
                    self.retire_cross_watch(w);
                    continue;
                }
                if w.delivery_client.is_none() {
                    w.delivery_client = self.cross_observer(&w.sender);
                }
                if let Some(client) = &w.delivery_client {
                    if !client.connected() {
                        w.delivery_subscribed = false;
                    } else if !w.delivery_subscribed {
                        client.send(CtlCommand::Subscribe {
                            snapshots: true,
                            tools: false,
                        });
                        w.delivery_subscribed = true;
                    }
                    if client.connected()
                        && client.features().iter().any(|f| f == "deliver")
                        && !self.cross_restore_busy(&w.sender)
                        && let Some(snapshot) = client.latest()
                        && snapshot.session_file.as_deref()
                            == Some(w.sender.end.session_path.as_path())
                    {
                        let rid = client.send(CtlCommand::Deliver { root: w.sender.end.session_path.clone(),
                            kind: deck_core::ctl::DeliverKind::Watch,
                            title: delivery_title("Watch: ", &w.watched.end.title, &w.watched.end.ref_, Some(result.status)), text: result.text,
                            details: Some(serde_json::json!({"ref": w.watched.end.ref_, "status": result.status})), trigger_turn: true });
                        w.delivery_sent = Some((rid, now + DELIVERY));
                        self.agent_tools.cross_sessions.watches.insert(request, w);
                        continue;
                    }
                }
                w.result = Some(result);
            } else if !w.waiter.in_flight
                && !w.arming
                && now >= w.waiter.next
                && let Some(mut watch) = w.waiter.watch.take()
            {
                w.waiter.in_flight = true;
                w.waiter.freshness.start();
                w.waiter.next = now + POLL;
                let path = w.watched.end.session_path.clone();
                let id = w.watched.id.clone();
                let allow_missing = !w.file_seen;
                self.spawn_worker("cross-session-scan", move || {
                    let result = scan_watch(&path, &id, &mut watch, allow_missing);
                    Fin::CrossSession(Update::Scan {
                        request,
                        path,
                        id,
                        watch,
                        result,
                    })
                });
            }
            self.agent_tools.cross_sessions.watches.insert(request, w);
        }
        let ids: Vec<_> = self
            .agent_tools
            .cross_sessions
            .messages
            .keys()
            .copied()
            .collect();
        for request in ids {
            let Some(mut message) = self.agent_tools.cross_sessions.messages.remove(&request)
            else {
                continue;
            };
            let mut result = None;
            if (!self.agent_tool_current(request) || !self.cross_sender_alive(&message.sender))
                && matches!(message.stage, MessageStage::Queued)
            {
                self.retire_cross_clients([Some(message.client), None]);
                continue;
            }
            self.refresh_cross_endpoint(&mut message.sender, None);
            if !self.cross_endpoint_current(&message.recipient) {
                result = Some(Err("The recipient's session changed".to_owned()));
            }
            if message.client.connected() && !message.subscribed {
                message.client.send(CtlCommand::Subscribe {
                    snapshots: true,
                    tools: false,
                });
                message.subscribed = true;
            }
            for (rid, reply) in message.client.take_replies() {
                if matches!(message.stage, MessageStage::Sent(sent) if sent == rid) {
                    result = Some(if reply.ok {
                        Ok(format!(
                            "Message delivered to {} ({})",
                            message.recipient.end.title, message.recipient.end.ref_
                        ))
                    } else {
                        Err(reply
                            .error
                            .unwrap_or_else(|| "The message was rejected".to_owned()))
                    });
                }
            }
            if result.is_none() && matches!(message.stage, MessageStage::Queued) {
                if !self.cfg.agent_tools.message_session {
                    result = Some(Err(MESSAGE_OFF.to_owned()));
                } else if message.client.connected()
                    && let Err(error) = message_capability(&message.client.features(), message.wake)
                {
                    result = Some(Err(error));
                } else if message.client.connected()
                    && !self.cross_restore_busy(&message.recipient)
                    && let Some(snapshot) = message.client.latest()
                {
                    if snapshot.session_file.as_deref()
                        != Some(message.recipient.end.session_path.as_path())
                    {
                        result = Some(Err("The recipient's session changed".to_owned()));
                    } else {
                        let pair = (message.sender.id.clone(), message.recipient.id.clone());
                        let rate = self
                            .agent_tools
                            .cross_sessions
                            .rates
                            .entry(pair)
                            .or_insert_with(|| Rate::new(now));
                        if !rate.take(now) {
                            result = Some(Err(
                                "Rate limited; send at most one message per 10 seconds (burst 3)"
                                    .to_owned(),
                            ));
                        } else {
                            let rid = message.client.send(message_command(
                                &message.sender,
                                &message.recipient,
                                &message.text,
                                message.wake,
                            ));
                            message.stage = MessageStage::Sent(rid);
                            message.until = now + DELIVERY;
                        }
                    }
                }
            }
            if result.is_none()
                && now >= message.until
                && !(matches!(message.stage, MessageStage::Queued)
                    && self.cross_restore_busy(&message.recipient))
            {
                result = Some(Err(match message.stage {
                MessageStage::Queued => CANNOT_MESSAGE.to_owned(), MessageStage::Sent(_) => "Message delivery was not acknowledged; it may have arrived. It will not be retried.".to_owned(),
            }));
            }
            if let Some(result) = result {
                match result {
                    Ok(text) => self.finish_agent_tool(request, true, text),
                    Err(error) => self.finish_agent_tool(request, false, error),
                }
                self.retire_cross_clients([Some(message.client), None]);
            } else {
                self.agent_tools
                    .cross_sessions
                    .messages
                    .insert(request, message);
            }
        }
        self.agent_tools.cross_sessions.rates.retain(|_, rate| {
            now.saturating_duration_since(rate.updated) < Duration::from_secs(30)
        });
        if !self.agent_tools.cross_sessions.watches.is_empty()
            || !self.agent_tools.cross_sessions.messages.is_empty()
            || !self.agent_tools.cross_sessions.resolving.is_empty()
        {
            self.wake_within(POLL);
        }
    }
}

/// Workers may render an announced native path before its first header is persisted.
pub(in crate::gateway::engine) fn session_row_ref(path: &Path) -> Option<String> {
    let id = session_header(path).map(|(_, id)| id).ok().or_else(|| {
        std::fs::metadata(path)
            .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
            .then(|| native_session_id(path))
            .flatten()
    })?;
    Some(session_ref(&id))
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    const A: &str = "aaaaaaaa-1111-4111-8111-1111aaaaaaaa";
    const B: &str = "bbbbbbbb-2222-4222-8222-2222bbbbbbbb";

    fn endpoint(file: PathBuf, id: &str, title: &str) -> Endpoint {
        Endpoint {
            end: OrchEnd {
                session_path: file,
                title: title.to_owned(),
                target: None,
                model: Some("provider/model".to_owned()),
                thinking: Some("high".to_owned()),
                status: TabStatus::Idle,
                live: true,
                ref_: session_ref(id),
            },
            id: id.to_owned(),
            ctl_key: None,
        }
    }

    fn session(dir: &Path, id: &str, title: &str) -> Endpoint {
        let file = dir.join(format!("2026-11-14T12-00-00Z_{id}.jsonl"));
        std::fs::write(&file, format!("{}\n{}\n{}\n{}\n",
            json!({"type":"session_info","name":title}),
            json!({"type":"session","id":id,"cwd":dir}),
            r#"{"type":"message","message":{"role":"user","content":"old question"}}"#,
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"old answer"}],"stopReason":"stop"}}"#)).unwrap();
        endpoint(file, id, title)
    }

    fn pair(dir: &Path) -> Vec<Endpoint> {
        vec![session(dir, A, "Alpha"), session(dir, B, "Beta")]
    }

    fn prepared(dir: &Path, next: bool, busy: bool) -> Resolved {
        let mut candidates = pair(dir);
        if busy {
            candidates[1].end.status = TabStatus::Busy;
        }
        prepare(
            candidates[0].end.session_path.clone(),
            session_ref(B),
            candidates,
            PreparePolicy::Watch { next },
        )
        .unwrap()
    }

    fn append(file: &Path, value: serde_json::Value) {
        writeln!(
            std::fs::OpenOptions::new().append(true).open(file).unwrap(),
            "{value}"
        )
        .unwrap();
    }

    fn missing_prepared(dir: &Path, next: bool) -> Resolved {
        let mut endpoints = pair(dir);
        endpoints[1].end.status = TabStatus::Busy;
        endpoints[1].end.target = Some(SessionTarget {
            key: SessionKey::direct(),
            epoch: crate::controller::ServiceEpoch([1; 16]),
            generation: 1,
        });
        std::fs::remove_file(&endpoints[1].end.session_path).unwrap();
        prepare(
            endpoints[0].end.session_path.clone(),
            session_ref(B),
            endpoints,
            PreparePolicy::Watch { next },
        )
        .unwrap()
    }

    fn poll(w: &mut Watch) {
        w.waiter.freshness.start();
        let mut watch = w.waiter.watch.take().unwrap();
        let result = scan_watch(
            &w.watched.end.session_path,
            &w.watched.id,
            &mut watch,
            !w.file_seen,
        );
        w.accept_scan(watch, result);
    }

    #[test]
    fn next_watch_uses_transcript_completion_after_an_earlier_busy_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut endpoints = pair(dir.path());
        let path = endpoints[1].end.session_path.clone();
        append(
            &path,
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"current question"}]}}),
        );
        endpoints[1].end.status = TabStatus::Busy;
        // The captured snapshot precedes completion; the baseline read follows it.
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"current answer"}],"stopReason":"stop"}}),
        );
        let resolved = prepare(
            endpoints[0].end.session_path.clone(),
            session_ref(B),
            endpoints,
            PreparePolicy::Watch { next: true },
        )
        .unwrap();
        assert!(!resolved.turn_active && !resolved.missing);
        let mut watch = Watch::new(resolved, true, false, Some(600), now, None);
        watch.observe(now, true, None);
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"next delivery","display":true,"attribution":"agent"}),
        );
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"delivery answer"}],"stopReason":"stop"}}),
        );
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        let result = watch.result.as_ref().unwrap();
        assert!(result.text.contains("delivery answer") && !result.text.contains("current answer"));
    }

    #[test]
    fn stale_busy_snapshot_and_trailing_passive_custom_do_not_skip_next_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut endpoints = pair(dir.path());
        let path = endpoints[1].end.session_path.clone();
        append(
            &path,
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"current question"}]}}),
        );
        endpoints[1].end.status = TabStatus::Busy;
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"current answer"}],"stopReason":"stop"}}),
        );
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"passive nextTurn message","display":true,"attribution":"agent"}),
        );
        let baseline = current_turn(&path).unwrap();
        assert!(!baseline.active && baseline.custom.is_some());
        let resolved = prepare(
            endpoints[0].end.session_path.clone(),
            session_ref(B),
            endpoints,
            PreparePolicy::Watch { next: true },
        )
        .unwrap();
        assert!(!resolved.turn_active);
        let mut watch = Watch::new(resolved, true, false, Some(600), now, None);
        watch.arm_next(Activity::Idle);
        watch.observe(now, true, None);
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"next waking delivery","display":true,"attribution":"agent"}),
        );
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"waking delivery answer"}],"stopReason":"stop"}}),
        );
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        let result = watch.result.as_ref().unwrap();
        assert_eq!(result.status, "completed");
        assert!(
            result.text.contains("waking delivery answer")
                && !result.text.contains("current answer")
        );
    }

    #[test]
    fn fresh_busy_activity_skips_a_delivery_started_turn_before_its_first_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let endpoints = pair(dir.path());
        let path = endpoints[1].end.session_path.clone();
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"current waking delivery","display":true,"attribution":"agent"}),
        );
        let resolved = prepare(
            endpoints[0].end.session_path.clone(),
            session_ref(B),
            endpoints,
            PreparePolicy::Watch { next: true },
        )
        .unwrap();
        assert!(!resolved.turn_active && resolved.trailing_custom);
        let mut watch = Watch::new(resolved, true, false, Some(600), now, None);
        // Controller activity is read after the baseline, not from the captured Idle.
        watch.arm_next(Activity::Busy);
        watch.observe(now, true, None);
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"aside during current delivery","display":true,"attribution":"agent"}),
        );
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"current delivery answer"}],"stopReason":"stop"}}),
        );
        poll(&mut watch);
        watch.observe(now, true, None);
        assert!(watch.result.is_none() && !watch.waiter.scan.prompt_seen);
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"next waking delivery","display":true,"attribution":"agent"}),
        );
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"next delivery answer"}],"stopReason":"stop"}}),
        );
        poll(&mut watch);
        watch.observe(now, true, None);
        let result = watch.result.as_ref().unwrap();
        assert!(
            result.text.contains("next delivery answer")
                && !result.text.contains("current delivery answer")
        );
    }

    #[test]
    fn missing_baseline_survives_materialization_before_watch_preparation_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let resolved = missing_prepared(dir.path(), true);
        let path = resolved.other.end.session_path.clone();
        let baseline = current_turn(&path).unwrap();
        assert!(baseline.missing);
        // Materialize between the baseline operation and applying its result.
        session(dir.path(), B, "Beta");
        let resolved = prepare_watch(resolved, true, baseline).unwrap();
        assert!(resolved.missing);
        let mut watch = Watch::new(resolved, true, false, Some(600), now, None);
        watch.observe(now, true, None);
        poll(&mut watch);
        assert!(!watch.waiter.scan.prompt_seen && watch.result.is_none());
        append(
            &path,
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"next question"}]}}),
        );
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"next answer"}],"stopReason":"stop"}}),
        );
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        let result = watch.result.as_ref().unwrap();
        assert!(result.text.contains("next answer") && !result.text.contains("old answer"));
    }

    #[test]
    fn absent_first_file_after_confirmed_exit_finishes_foreground_and_background() {
        for background in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let now = Instant::now();
            let resolved = missing_prepared(dir.path(), false);
            let client = CtlClient::spawn_with_key(
                dir.path().join("absent-ctl"),
                dir.path().join("ctl.key"),
                || {},
            );
            let mut watch = Watch::new(
                resolved,
                false,
                background,
                (!background).then_some(600),
                now,
                Some(client),
            );
            watch.watched.end.status = TabStatus::Exited { clean: true };
            watch.observe(now, true, None);
            assert!(watch.waiter.exit_seen && watch.result.is_none());
            poll(&mut watch);
            watch.observe(now, true, None);
            let result = watch.result.as_ref().unwrap();
            assert_eq!(result.status, "exited");
            assert!(watch.waiter.scan.reply.is_none());
        }
    }

    #[test]
    fn late_first_file_keeps_the_next_prompt_in_the_same_scan() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut watch = Watch::new(
            missing_prepared(dir.path(), true),
            true,
            false,
            Some(600),
            now,
            None,
        );
        watch.observe(now, true, None);
        poll(&mut watch);
        let path = watch.watched.end.session_path.clone();
        session(dir.path(), B, "Beta");
        append(
            &path,
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"next question"}]}}),
        );
        poll(&mut watch);
        watch.observe(now, true, None);
        assert!(watch.result.is_none() && watch.waiter.scan.prompt_seen);
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"next answer"}],"stopReason":"stop"}}),
        );
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        let result = watch.result.as_ref().unwrap();
        assert!(result.text.contains("next answer") && !result.text.contains("old answer"));
    }

    #[test]
    fn next_watch_ignores_custom_asides_in_the_already_running_user_turn() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut endpoints = pair(dir.path());
        let path = endpoints[1].end.session_path.clone();
        append(
            &path,
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"current question"}]}}),
        );
        endpoints[1].end.status = TabStatus::Busy;
        let resolved = prepare(
            endpoints[0].end.session_path.clone(),
            session_ref(B),
            endpoints,
            PreparePolicy::Watch { next: true },
        )
        .unwrap();
        let mut watch = Watch::new(resolved, true, false, Some(600), now, None);
        watch.observe(now, true, None);
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"aside","display":true,"attribution":"agent"}),
        );
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"current answer"}],"stopReason":"stop"}}),
        );
        poll(&mut watch);
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        assert!(watch.result.is_none() && !watch.waiter.scan.prompt_seen);
        append(
            &path,
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"next question"}]}}),
        );
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"next answer"}],"stopReason":"stop"}}),
        );
        poll(&mut watch);
        watch.observe(now, true, None);
        let result = watch.result.as_ref().unwrap();
        assert!(result.text.contains("next answer") && !result.text.contains("current answer"));
    }

    #[test]
    fn busy_delivery_started_turn_binds_the_pending_custom_candidate() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut endpoints = pair(dir.path());
        let path = endpoints[1].end.session_path.clone();
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"wake","display":true,"attribution":"agent"}),
        );
        endpoints[1].end.status = TabStatus::Busy;
        let resolved = prepare(
            endpoints[0].end.session_path.clone(),
            session_ref(B),
            endpoints,
            PreparePolicy::Watch { next: false },
        )
        .unwrap();
        let mut watch = Watch::new(resolved, false, false, Some(600), now, None);
        watch.observe(now, true, None);
        poll(&mut watch);
        assert!(watch.result.is_none() && watch.waiter.scan.reply.is_none());
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"delivery answer"}],"stopReason":"stop"}}),
        );
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        let result = watch.result.as_ref().unwrap();
        assert!(result.text.contains("delivery answer") && !result.text.contains("old answer"));
    }

    #[test]
    fn next_watch_observes_delivery_started_custom_turn() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut watch = Watch::new(
            prepared(dir.path(), true, false),
            true,
            false,
            Some(600),
            now,
            None,
        );
        let path = watch.watched.end.session_path.clone();
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"peer message","display":true}),
        );
        poll(&mut watch);
        watch.observe(now, true, None);
        assert!(
            watch.result.is_none() && !watch.waiter.scan.prompt_seen,
            "a passive no-wake entry is not a turn"
        );
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":"delivery reply","stopReason":"stop"}}),
        );
        poll(&mut watch);
        watch.observe(now, true, None);
        assert_eq!(watch.result.as_ref().unwrap().status, "completed");
        assert!(
            watch
                .result
                .as_ref()
                .unwrap()
                .text
                .contains("delivery reply")
        );
    }

    #[test]
    fn resolve_idle_activity_is_rechecked_before_immediate_completion() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let resolved = prepared(dir.path(), false, false);
        let path = resolved.other.end.session_path.clone();
        append(
            &path,
            json!({"type":"message","message":{"role":"user","content":"new turn during resolution"}}),
        );
        let mut watch = Watch::new(resolved, false, false, Some(600), now, None);
        assert!(watch.result.is_none());
        watch.watched.end.status = TabStatus::Busy;
        watch.observe(now, true, None);
        poll(&mut watch);
        assert!(watch.result.is_none() && watch.waiter.scan.end.is_none());
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":"fresh answer","stopReason":"stop"}}),
        );
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        assert!(watch.result.as_ref().unwrap().text.contains("fresh answer"));
        assert!(!watch.result.as_ref().unwrap().text.contains("old answer"));
    }

    #[test]
    fn idle_to_busy_rearm_keeps_a_custom_candidate_before_the_first_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let endpoints = pair(dir.path());
        let path = endpoints[1].end.session_path.clone();
        append(
            &path,
            json!({"type":"custom_message","customType":"ompdeck-message","content":"wake","display":true}),
        );
        let resolved = prepare(
            endpoints[0].end.session_path.clone(),
            session_ref(B),
            endpoints,
            PreparePolicy::Watch { next: false },
        )
        .unwrap();
        let mut watch = Watch::new(resolved, false, false, Some(600), now, None);
        watch.watched.end.status = TabStatus::Busy;
        watch.observe(now, true, None);
        poll(&mut watch);
        assert!(watch.result.is_none());
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":"new custom reply","stopReason":"stop"}}),
        );
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        assert!(
            watch
                .result
                .as_ref()
                .unwrap()
                .text
                .contains("new custom reply")
        );
    }

    #[test]
    fn missing_first_turn_file_waits_until_the_reply_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut endpoints = pair(dir.path());
        endpoints[1].end.status = TabStatus::Busy;
        endpoints[1].end.target = Some(SessionTarget {
            key: SessionKey::direct(),
            epoch: crate::controller::ServiceEpoch([1; 16]),
            generation: 1,
        });
        std::fs::remove_file(&endpoints[1].end.session_path).unwrap();
        let resolved = prepare(
            endpoints[0].end.session_path.clone(),
            session_ref(B),
            endpoints,
            PreparePolicy::Watch { next: false },
        )
        .unwrap();
        let mut watch = Watch::new(resolved, false, false, Some(600), now, None);
        watch.observe(now, true, None);
        poll(&mut watch);
        assert!(watch.result.is_none() && !watch.file_seen);
        session(dir.path(), B, "Beta");
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        assert_eq!(watch.result.as_ref().unwrap().status, "completed");
        assert!(watch.result.as_ref().unwrap().text.contains("old answer"));
    }

    #[test]
    fn delivery_titles_cap_unicode_dynamic_text_and_keep_routing_suffixes() {
        let long = "🦀界".repeat(300);
        for (prefix, status) in [
            ("Message from ", None),
            ("Watch: ", Some("completed")),
            ("Watch: ", Some("attention")),
        ] {
            let title = delivery_title(prefix, &long, "deadbeef", status);
            assert_eq!(title.chars().count(), 200);
            assert!(title.starts_with(prefix));
            assert!(title.contains("(deadbeef)"));
            if let Some(status) = status {
                assert!(title.ends_with(status));
            }
        }
    }

    #[test]
    fn refs_prefixes_full_ids_and_moves_use_header_identity() {
        let dir = tempfile::tempdir().unwrap();
        let candidates = pair(dir.path());
        for reference in [session_ref(B), B[..15].to_owned(), B.to_owned()] {
            let resolved = resolve(&reference, candidates.clone()).unwrap();
            assert_eq!(resolved.id, B);
            assert_eq!(resolved.end.ref_, "bbbbbbbb");
        }
        let mut moved = candidates[1].clone();
        moved.end.session_path = dir.path().join("moved.jsonl");
        std::fs::rename(&candidates[1].end.session_path, &moved.end.session_path).unwrap();
        moved.end.title = "Renamed".to_owned();
        assert_eq!(
            resolve("bbbbbbbb", vec![moved]).unwrap().end.ref_,
            "bbbbbbbb"
        );
    }

    #[test]
    fn uuid_v7_timestamp_collisions_have_unique_tail_refs() {
        let dir = tempfile::tempdir().unwrap();
        let a = "01a1d914-1111-7111-8111-1111a1b2c3d4";
        let b = "01a1d914-2222-7222-8222-2222e5f6ab78";
        let candidates = vec![
            session(dir.path(), a, "Alpha"),
            session(dir.path(), b, "Beta"),
        ];
        assert_eq!(&a[..8], &b[..8]);
        assert_eq!(session_ref(a), "a1b2c3d4");
        assert_eq!(session_ref(b), "e5f6ab78");
        assert_eq!(resolve("A1B2C3D4", candidates.clone()).unwrap().id, a);
        assert_eq!(resolve("e5f6ab78", candidates.clone()).unwrap().id, b);
        assert_eq!(resolve("01a1d914-1111", candidates.clone()).unwrap().id, a);
        let unknown = resolve("ffffffff", candidates.clone()).err().unwrap();
        assert!(unknown.contains("Unknown") && unknown.contains(a) && unknown.contains(b));
        let error = resolve("01a1d914", candidates).err().unwrap();
        assert!(error.contains("Ambiguous") && error.contains(a) && error.contains(b));
    }

    #[test]
    fn duplicate_tail_refs_are_ambiguous_until_full_id_is_used() {
        let dir = tempfile::tempdir().unwrap();
        let other = "cccccccc-9999-7999-8999-9999aaaaaaaa";
        let candidates = vec![
            session(dir.path(), A, "Alpha"),
            session(dir.path(), other, "Other"),
        ];
        let error = resolve("aaaaaaaa", candidates.clone()).err().unwrap();
        assert!(error.contains("Ambiguous") && error.contains(A) && error.contains(other));
        assert_eq!(resolve(other, candidates).unwrap().id, other);
    }

    #[test]
    fn ambiguous_unknown_and_non_session_refs_fail_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let first = session(dir.path(), A, "Alpha");
        let other_id = "aaaaaaaa-9999-4999-8999-999999999999";
        let second = session(dir.path(), other_id, "Other");
        let error = resolve("aaaaaaaa", vec![first.clone(), second.clone()])
            .err()
            .unwrap();
        assert!(error.contains("Ambiguous") && error.contains(A) && error.contains(other_id));
        assert_eq!(resolve("aaaaaaaa-1111", vec![first, second]).unwrap().id, A);
        assert!(
            resolve("deadbeef", Vec::new())
                .err()
                .unwrap()
                .contains("Unknown")
        );
        assert!(
            resolve("aaaa", Vec::new())
                .err()
                .unwrap()
                .contains("at least 8")
        );
        let path = dir.path().join("not-omp.jsonl");
        std::fs::write(
            &path,
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\"}}\n",
        )
        .unwrap();
        assert!(
            resolve("bbbbbbbb", vec![endpoint(path, B, "Foreign")])
                .err()
                .unwrap()
                .contains("not an OmpDeck/omp session")
        );
    }

    #[test]
    fn idle_without_next_returns_last_reply_but_next_waits_start_and_end() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut immediate = Watch::new(
            prepared(dir.path(), false, false),
            false,
            false,
            Some(600),
            now,
            None,
        );
        immediate.observe(now, true, None);
        let result = immediate.result.unwrap();
        assert_eq!(result.status, "idle");
        assert!(result.text.contains("Beta (bbbbbbbb)") && result.text.contains("old answer"));
        let mut next = Watch::new(
            prepared(dir.path(), true, false),
            true,
            false,
            Some(600),
            now,
            None,
        );
        poll(&mut next);
        next.observe(now, true, None);
        assert!(next.result.is_none() && !next.waiter.scan.prompt_seen);
        let path = next.watched.end.session_path.clone();
        append(
            &path,
            json!({"type":"message","message":{"role":"user","content":"new question"}}),
        );
        next.watched.end.status = TabStatus::Busy;
        poll(&mut next);
        next.observe(now, true, None);
        assert!(next.result.is_none() && next.waiter.scan.prompt_seen);
        append(
            &path,
            json!({"type":"message","message":{"role":"assistant","content":"new answer","stopReason":"stop"}}),
        );
        poll(&mut next);
        next.watched.end.status = TabStatus::Idle;
        next.observe(now, true, None);
        assert!(
            next.result.is_none(),
            "requires a post-idle scan, like wait_session"
        );
        poll(&mut next);
        next.observe(now, true, None);
        let result = next.result.unwrap();
        assert_eq!(result.status, "completed");
        assert!(result.text.contains("new answer") && !result.text.contains("old answer"));
    }

    #[test]
    fn busy_wait_binds_current_turn_and_uses_shared_capped_renderer() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut watch = Watch::new(
            prepared(dir.path(), false, true),
            false,
            false,
            Some(600),
            now,
            None,
        );
        assert!(watch.result.is_none());
        poll(&mut watch);
        watch.observe(now, true, None);
        assert!(
            watch.result.is_none(),
            "busy does not return a stale idle reply"
        );
        watch.watched.end.status = TabStatus::Idle;
        watch.observe(now, true, None);
        poll(&mut watch);
        watch.observe(now, true, None);
        assert_eq!(watch.result.as_ref().unwrap().status, "completed");
        watch.waiter.scan.reply = Some("é".repeat(REPLY_CAP));
        watch.waiter.scan.reply_more = 9;
        let text = result_text(&watch, "completed", "Done");
        assert!(text.contains("more characters; full text in"));
        assert!(text.len() < REPLY_CAP + 1024);
    }

    #[test]
    fn toggles_limits_defaults_and_self_refusals_are_enforced() {
        assert!(
            parse_request("watch_session", json!({"session":"bbbbbbbb"}), false, false)
                .err()
                .unwrap()
                .contains(WATCH_OFF)
        );
        assert!(
            parse_request(
                "message_session",
                json!({"session":"bbbbbbbb","text":"hello"}),
                true,
                false
            )
            .err()
            .unwrap()
            .contains(MESSAGE_OFF)
        );
        assert!(matches!(
            parse_request("watch_session", json!({"session":"bbbbbbbb"}), true, false)
                .unwrap()
                .1,
            Kind::Watch {
                seconds: Some(600),
                background: false,
                next: false
            }
        ));
        assert!(matches!(
            parse_request(
                "watch_session",
                json!({"session":"bbbbbbbb","background":true}),
                true,
                false
            )
            .unwrap()
            .1,
            Kind::Watch { seconds: None, .. }
        ));
        for (background, seconds) in [(false, 0), (false, 3601), (true, 86401)] {
            assert!(
                parse_request(
                    "watch_session",
                    json!({"session":"bbbbbbbb","background":background,"timeout_seconds":seconds}),
                    true,
                    false
                )
                .is_err()
            );
        }
        for text in ["".to_owned(), "é".repeat(4097)] {
            assert!(
                parse_request(
                    "message_session",
                    json!({"session":"bbbbbbbb","text":text}),
                    true,
                    true
                )
                .is_err()
            );
        }
        assert!(matches!(
            parse_request(
                "message_session",
                json!({"session":"bbbbbbbb","text":"hello"}),
                true,
                true
            )
            .unwrap()
            .1,
            Kind::Message { wake: true, .. }
        ));
        let dir = tempfile::tempdir().unwrap();
        let candidates = pair(dir.path());
        for policy in [PreparePolicy::Watch { next: false }, PreparePolicy::Message] {
            assert!(
                prepare(
                    candidates[0].end.session_path.clone(),
                    session_ref(A),
                    candidates.clone(),
                    policy
                )
                .err()
                .unwrap()
                .contains("your own session")
            );
        }
        let mut watch = Watch::new(
            prepared(dir.path(), true, false),
            true,
            true,
            None,
            Instant::now(),
            None,
        );
        watch.observe(Instant::now(), false, None);
        assert!(watch.result.as_ref().unwrap().text.contains(WATCH_OFF));
        assert_eq!(watch.result.as_ref().unwrap().status, "disabled");
    }

    #[test]
    fn projection_stop_and_background_delivery_reservations_share_state() {
        let dir = tempfile::tempdir().unwrap();
        let watch = Watch::new(
            prepared(dir.path(), true, false),
            true,
            true,
            None,
            Instant::now(),
            None,
        );
        let root = watch.sender.end.session_path.clone();
        let mut state = State::default();
        state.watches.insert(17, watch);
        let projected = state.projection();
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].watcher.ref_, "aaaaaaaa");
        assert_eq!(projected[0].watched.ref_, "bbbbbbbb");
        assert_eq!(projected[0].watched.thinking.as_deref(), Some("high"));
        assert!(projected[0].background && projected[0].next);
        state.stop_watch(&projected[0].id).unwrap();
        assert!(
            state.watches[&17]
                .result
                .as_ref()
                .unwrap()
                .text
                .contains("Stopped by the user")
        );
        assert!(state.projection().is_empty());
        assert!(state.stop_watch(&projected[0].id).is_err());
        assert!(state.stop_watch("not-a-watch").is_err());
        assert!(state.delivery_pending(&root, false));
        assert!(!state.delivery_pending(&root, true));
        state.watches.get_mut(&17).unwrap().delivery_sent = Some((91, Instant::now() + DELIVERY));
        assert!(
            state.delivery_pending(&root, true),
            "scanned→managed attachment must see outstanding ACKs by file"
        );
    }

    #[test]
    fn message_envelope_wake_and_pair_rate_limit_are_exact() {
        let dir = tempfile::tempdir().unwrap();
        let candidates = pair(dir.path());
        let command = message_command(&candidates[0], &candidates[1], "hello\nworld", false);
        let CtlCommand::Deliver {
            root,
            title,
            text,
            trigger_turn,
            ..
        } = command
        else {
            panic!("Deliver expected");
        };
        assert_eq!(root, candidates[1].end.session_path);
        assert_eq!(title, "Message from Alpha (aaaaaaaa)");
        assert_eq!(
            text,
            "hello\nworld\nReply with ompdeck_message_session to aaaaaaaa."
        );
        assert!(!trigger_turn);
        let now = Instant::now();
        let mut rates = HashMap::new();
        let pair = (A.to_owned(), B.to_owned());
        let rate = rates.entry(pair.clone()).or_insert_with(|| Rate::new(now));
        for _ in 0..3 {
            assert!(rate.take(now));
        }
        assert!(!rate.take(now));
        assert!(!rate.take(now + Duration::from_secs(9)));
        assert!(rate.take(now + Duration::from_secs(10)));
        assert!(!rate.take(now + Duration::from_secs(10)));
        assert!(
            rates
                .entry((B.to_owned(), A.to_owned()))
                .or_insert_with(|| Rate::new(now))
                .take(now),
            "directional per-pair buckets"
        );
    }

    #[test]
    fn managed_first_turn_sender_may_use_only_an_absent_native_filename() {
        let dir = tempfile::tempdir().unwrap();
        let mut candidates = pair(dir.path());
        let source = candidates[0].end.session_path.clone();
        candidates[0].end.target = Some(crate::controller::SessionTarget {
            epoch: crate::controller::ServiceEpoch([1; 16]),
            key: SessionKey::direct(),
            generation: 1,
        });
        std::fs::remove_file(&source).unwrap();
        assert_eq!(
            prepare(
                source.clone(),
                session_ref(B),
                candidates.clone(),
                PreparePolicy::Message
            )
            .unwrap()
            .sender
            .id,
            A
        );
        std::fs::write(&source, "{\"type\":\"not-a-session\"}\n").unwrap();
        assert!(
            prepare(source, session_ref(B), candidates, PreparePolicy::Message).is_err(),
            "existing foreign files never gain the native-filename exception"
        );
        assert!(native_session_id(Path::new("arbitrary.jsonl")).is_none());
    }

    #[test]
    fn background_cap_counts_cross_watches_and_existing_child_waits_together() {
        let dir = tempfile::tempdir().unwrap();
        let seed = prepared(dir.path(), true, false);
        let root = seed.sender.end.session_path.clone();
        let mut tools = super::super::super::AgentTools::default();
        for id in 0..7 {
            let watch = Watch::new(
                prepared(dir.path(), true, false),
                true,
                true,
                None,
                Instant::now(),
                None,
            );
            tools.cross_sessions.watches.insert(id, watch);
        }
        let mut child = super::super::tests::child();
        child.opener_root = root.clone();
        let waiter = Waiter::new(
            "s-existing".to_owned(),
            &child,
            None,
            None,
            None,
            false,
            Instant::now(),
        );
        child.background = Some(BackgroundWatch::new(
            88,
            "Child".to_owned(),
            Box::new(waiter),
        ));
        tools
            .children
            .entries
            .insert("s-existing".to_owned(), child);
        assert_eq!(tools.background_wait_count(&root), BACKGROUND_CAP);
        assert_eq!(
            tools.background_wait_count(&dir.path().join("other.jsonl")),
            0
        );
    }

    #[test]
    fn attention_exit_and_timeout_end_watches_without_mutating_the_peer() {
        let dir = tempfile::tempdir().unwrap();
        let now = Instant::now();
        let mut attention = Watch::new(
            prepared(dir.path(), true, false),
            true,
            false,
            Some(600),
            now,
            None,
        );
        attention.watched.end.status = TabStatus::Attention;
        let mut snapshot = LiveSnapshot::default();
        snapshot.pending_ui.push(
            serde_json::from_value(json!({"id":1,"kind":"input","title":"Choose a deployment"}))
                .unwrap(),
        );
        attention.observe(now, true, Some(&snapshot));
        assert!(
            attention
                .result
                .unwrap()
                .text
                .contains("Choose a deployment")
        );
        let mut exit = Watch::new(
            prepared(dir.path(), false, true),
            false,
            false,
            Some(600),
            now,
            None,
        );
        exit.watched.end.status = TabStatus::Exited { clean: true };
        exit.observe(now, true, None);
        assert!(exit.result.is_none(), "exit inspects one final scan");
        poll(&mut exit);
        exit.observe(now, true, None);
        assert_eq!(exit.result.as_ref().unwrap().status, "exited");
        assert!(exit.result.unwrap().text.contains("old answer"));
        let mut timeout = Watch::new(
            prepared(dir.path(), true, false),
            true,
            false,
            Some(1),
            now,
            None,
        );
        timeout.observe(now + Duration::from_secs(1), true, None);
        assert_eq!(timeout.result.unwrap().status, "timeout");
    }

    #[test]
    fn watching_retains_only_watcher_and_scanned_message_acks_fence_current_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut watch = Watch::new(
            prepared(dir.path(), true, false),
            true,
            true,
            None,
            Instant::now(),
            None,
        );
        let sender = crate::controller::SessionTarget {
            epoch: crate::controller::ServiceEpoch([4; 16]),
            key: SessionKey::direct(),
            generation: 1,
        };
        let recipient = crate::controller::SessionTarget {
            epoch: sender.epoch,
            key: SessionKey::direct(),
            generation: 1,
        };
        watch.sender.end.target = Some(sender.clone());
        watch.watched.end.target = Some(recipient.clone());
        let mut state = State::default();
        state.watches.insert(5, watch);
        assert!(state.needs_controller(&sender.key));
        assert!(
            !state.needs_controller(&recipient.key),
            "watching never retains/attaches the watched controller"
        );
        let endpoints = pair(dir.path());
        let path = endpoints[1].end.session_path.clone();
        let client = CtlClient::spawn_with_key(
            dir.path().join("absent-ctl"),
            dir.path().join("ctl.key"),
            || {},
        );
        state.messages.insert(
            6,
            Message {
                sender: endpoints[0].clone(),
                recipient: endpoints[1].clone(),
                text: "hello".to_owned(),
                wake: false,
                client,
                subscribed: false,
                stage: MessageStage::Queued,
                until: Instant::now() + DELIVERY,
            },
        );
        assert!(state.delivery_pending(&path, false));
        assert!(!state.delivery_pending(&path, true));
        state.messages.get_mut(&6).unwrap().stage = MessageStage::Sent(72);
        assert!(
            state.delivery_pending(&path, true),
            "an independently connected recipient ACK survives later GUI attachment"
        );
    }

    #[test]
    fn legacy_deliver_never_silently_wakes_a_no_wake_message() {
        let legacy = vec!["deliver".to_owned()];
        assert!(message_capability(&legacy, true).is_ok());
        assert!(
            message_capability(&legacy, false)
                .unwrap_err()
                .contains("without waking")
        );
        assert!(
            message_capability(&["deliver".to_owned(), "deliver_no_wake".to_owned()], false)
                .is_ok()
        );
        assert_eq!(message_capability(&[], true).unwrap_err(), CANNOT_MESSAGE);
    }
}
