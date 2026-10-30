//! Paired devices in the engine (`docs/notes/mobile-relay.md`, "R4 contract"): a device session
//! is a control link whose sink speaks `deck_remote::api`. Every device operation is checked
//! against the device's permissions (read from the registry per request) and translated into
//! the gateway's own queries and journaled mutations; ids (project, session, live session,
//! changed file) are resolved against current state. Feeds are projections recomputed when
//! their source topics change: bounded to one record, `~`-abbreviated, without absolute paths,
//! ctl keys, other clients' request keys or secrets.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use deck_core::agents::{AgentStatus, TranscriptItem};
use deck_core::changes::{ChangedFile, Changes, DiffSide};
use deck_core::fleet::{FleetSession, FleetTarget};
use deck_core::omp::DeleteTarget;
use deck_core::sessions::{ProjectGroup, SessionEntry, abbreviate_home};
use deck_remote::api::{
    self, ApiError, Event, Feed, FeedValue, Op, ReplyResult, Resp, TermEnd, ViewMode,
};
use deck_term::InputMeta;

use super::{ClientState, Conn, Engine, Sink, ViewOp};
use crate::controller::{
    Cancel, Command, ControllerEvent, Delivery, Rejected, RequestId, RequestKey, RequestRecord,
    SessionTarget,
};
use crate::gateway::GATEWAY_VERSION;
use crate::gateway::journal;
use crate::gateway::protocol::{
    Answer, ControllerInfo, Done, GatewayError, LaunchSpec, Mutation, Outcome, Permissions, Query,
    Reply, ServerEvent, SessionRole, Topic, ViewRole,
};
use crate::gateway::remote::device::{DeviceOut, Inbound, NewView, Wake};
use crate::gateway::server::{ViewSide, next_id};
use crate::notifications::inbox::Kind as InboxKind;
use crate::session_actions::{Action, CompactMode};
use crate::status::TabStatus;
use crate::statusfmt::Activity;

/// Deleted sessions remembered (file → id) so a deletion's outcome names sessions, not files.
const DELETED_IDS: usize = 4096;
/// How deep a subagent transcript is searched for below its session's folder.
const AGENT_DEPTH: usize = 4;
/// Directory entries looked at while searching for one.
const AGENT_ENTRIES: usize = 4096;
/// Longest subagent id accepted.
const AGENT_ID_CHARS: usize = 128;

/// Device-protocol state of the engine shared by every device session.
#[derive(Default)]
pub(super) struct Devices {
    /// Files of sessions a device asked to delete, with their ids.
    deleted: HashMap<PathBuf, String>,
}

/// One device session's engine side.
pub(super) struct DeviceConn {
    pub(super) out: Arc<DeviceOut>,
    /// What it was allowed when last looked at (lowering `control` ends its typing views).
    permissions: Permissions,
    /// `Welcome::next_seq` of this session: the lowest seq new work may use.
    next_seq: u64,
    feeds: BTreeMap<Feed, FeedState>,
    /// The link's subscriptions must be rebuilt from `feeds`.
    resubscribe: bool,
    /// Device view id → engine view id.
    views: BTreeMap<u32, u64>,
    next_view: u32,
}

#[derive(Default)]
struct FeedState {
    /// The value may have changed.
    dirty: bool,
    /// Hash of the last value queued.
    last: Option<u64>,
    /// The file (transcript) or folder (changes) it follows.
    path: Option<PathBuf>,
    /// Transcript: what was sent.
    cursor: Option<Cursor>,
}

struct Cursor {
    /// Items `..sent` were sent.
    sent: usize,
    /// Hash of item `sent - 1` (a rewritten file changes it).
    last: u64,
}

impl DeviceConn {
    fn new(out: Arc<DeviceOut>, permissions: Permissions, next_seq: u64) -> DeviceConn {
        DeviceConn {
            out,
            permissions,
            next_seq,
            feeds: BTreeMap::new(),
            resubscribe: false,
            views: BTreeMap::new(),
            next_view: 1,
        }
    }

    /// `topic` has a new revision: the feeds projected from it are recomputed.
    pub(super) fn topic_changed(&mut self, topic: &Topic) {
        let mut follow = false;
        for (feed, st) in &mut self.feeds {
            let hit = match (feed, topic) {
                (Feed::Projects {} | Feed::Sessions { .. }, Topic::Sessions) => true,
                (Feed::Live {}, Topic::Sessions | Topic::Live(_)) => true,
                // The live sessions' control links (busy, model, live control) follow them.
                (Feed::Live {}, Topic::Controllers) => {
                    follow = true;
                    true
                }
                (Feed::Inbox {}, Topic::Notifications) => true,
                (Feed::Cards { .. }, Topic::Fleet) => true,
                // The cards' targets (session files, liveness) follow the sessions.
                (Feed::Cards { .. }, Topic::Sessions | Topic::Running) => {
                    follow = true;
                    false
                }
                (Feed::Transcript { .. }, Topic::Transcript(p))
                | (Feed::Changes { .. }, Topic::Changes(p)) => st.path.as_ref() == Some(p),
                _ => false,
            };
            st.dirty |= hit;
        }
        self.resubscribe |= follow;
    }
}

// ---- projections --------------------------------------------------------------------------------

fn unix_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A folder's project id.
fn project_id(cwd: &Path) -> String {
    api::project_id(cwd.to_string_lossy().as_bytes())
}

fn hash_of(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// `s` cut to `max` bytes (at a character boundary) with [`api::CUT_MARK`].
fn cut(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max.saturating_sub(api::CUT_MARK.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{}", &s[..end], api::CUT_MARK)
}

/// `s` cut to `max` characters.
fn cut_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => s[..i].to_owned(),
        None => s.to_owned(),
    }
}

/// A message for a device: the home folder is never shown in full.
fn scrub(text: &str) -> String {
    match dirs::home_dir() {
        Some(home) if !home.as_os_str().is_empty() => text.replace(&*home.to_string_lossy(), "~"),
        _ => text.to_owned(),
    }
}

fn live_ref(t: &SessionTarget) -> api::LiveRef {
    api::LiveRef {
        key: t.key.to_string(),
        generation: t.generation,
    }
}

/// The encoded [`Event::Feed`] holding as many of `items` as fit one record (`truncated`
/// when some were left out).
fn fit_feed<T: Clone>(
    feed: &Feed,
    items: &[T],
    make: impl Fn(Vec<T>, bool) -> FeedValue,
) -> Option<Vec<u8>> {
    let encode = |n: usize| {
        serde_json::to_vec(&Event::Feed {
            feed: feed.clone(),
            value: make(items[..n].to_vec(), n < items.len()),
        })
        .ok()
    };
    let fits = |b: &Vec<u8>| b.len() <= api::MAX_REPLY;
    let full = encode(items.len())?;
    if fits(&full) {
        return Some(full);
    }
    // `lo` fits, `hi` does not.
    let (mut lo, mut hi) = (0, items.len());
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if encode(mid).is_some_and(|b| fits(&b)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    encode(lo).filter(fits)
}

fn projects(groups: &[ProjectGroup]) -> Vec<api::Project> {
    let mut list: Vec<api::Project> = groups
        .iter()
        .map(|g| api::Project {
            id: project_id(&g.cwd),
            display: abbreviate_home(&g.cwd),
            worktree: g.worktree.is_some(),
            session_count: count(g.sessions.len()),
            modified_unix_ms: g
                .sessions
                .iter()
                .map(|s| unix_ms(s.modified))
                .max()
                .unwrap_or(0),
        })
        .collect();
    list.sort_by_key(|p| std::cmp::Reverse(p.modified_unix_ms));
    list
}

/// A project's newest `limit` sessions, and how many it has.
fn summaries(g: &ProjectGroup, limit: u32) -> (u32, Vec<api::SessionSummary>) {
    let mut newest: Vec<&SessionEntry> = g.sessions.iter().collect();
    newest.sort_by_key(|s| std::cmp::Reverse(s.modified));
    let list = newest
        .into_iter()
        .take(limit as usize)
        .map(|s| api::SessionSummary {
            id: s.id.clone(),
            title: s.title.clone(),
            first_prompt: s
                .first_prompt
                .as_deref()
                .map(|p| cut_chars(p, api::FIRST_PROMPT_CHARS)),
            modified_ms: unix_ms(s.modified),
        })
        .collect();
    (count(g.sessions.len()), list)
}

/// The folder of project `id`.
fn resolve_project<'a>(groups: &'a [ProjectGroup], id: &str) -> Result<&'a Path, ApiError> {
    groups
        .iter()
        .find(|g| project_id(&g.cwd) == id)
        .map(|g| g.cwd.as_path())
        .ok_or_else(|| ApiError::NotFound {
            what: "project".to_owned(),
        })
}

/// The one session with omp id `id` (ambiguous or unknown → `not_found`).
fn resolve_session<'a>(groups: &'a [ProjectGroup], id: &str) -> Result<&'a SessionEntry, ApiError> {
    let mut found = groups
        .iter()
        .flat_map(|g| &g.sessions)
        .filter(|s| s.id == id);
    match (found.next(), found.next()) {
        (Some(s), None) => Ok(s),
        _ => Err(ApiError::NotFound {
            what: "session".to_owned(),
        }),
    }
}

/// The id of the session stored at `path`.
fn session_at(groups: &[ProjectGroup], path: &Path) -> Option<String> {
    groups
        .iter()
        .flat_map(|g| &g.sessions)
        .find(|s| s.path == path)
        .map(|s| s.id.clone())
}

/// A subagent id is one file stem: no separators, no parent references.
fn valid_agent(agent: &str) -> bool {
    !agent.is_empty()
        && agent.chars().count() <= AGENT_ID_CHARS
        && agent != "."
        && agent != ".."
        && !agent.contains(['/', '\\', '\0'])
}

/// Subagent `agent`'s transcript below `session`'s folder (`<session without .jsonl>/`).
fn agent_transcript(session: &Path, agent: &str) -> Option<PathBuf> {
    let dir = session.with_extension("");
    let want = format!("{agent}.jsonl");
    let mut seen = 0;
    let mut level = vec![dir];
    for _ in 0..AGENT_DEPTH {
        let mut next = Vec::new();
        for d in level {
            let Ok(entries) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in entries.flatten() {
                seen += 1;
                if seen > AGENT_ENTRIES {
                    return None;
                }
                let path = e.path();
                let Ok(kind) = e.file_type() else { continue };
                if kind.is_dir() {
                    next.push(path);
                } else if kind.is_file() && e.file_name().to_string_lossy() == want {
                    return Some(path);
                }
            }
        }
        level = next;
    }
    None
}

fn activity(a: Activity) -> api::Activity {
    match a {
        Activity::Unknown => api::Activity::Unknown,
        Activity::Idle => api::Activity::Idle,
        Activity::Busy => api::Activity::Busy,
        Activity::Attention => api::Activity::Attention,
    }
}

fn status(s: TabStatus) -> api::Status {
    match s {
        TabStatus::Exited { clean } => api::Status::Exited { clean },
        TabStatus::Attention => api::Status::Attention {},
        TabStatus::Busy => api::Status::Busy {},
        TabStatus::Bell | TabStatus::Idle => api::Status::Idle {},
    }
}

fn delivery(d: &Delivery) -> api::DeliveryView {
    use api::DeliveryView as D;
    match d {
        Delivery::Queued => D::Queued {},
        Delivery::AwaitingConfirm { draft } => D::AwaitingConfirm {
            draft: draft.clone(),
        },
        Delivery::Writing => D::Writing {},
        Delivery::Delivered => D::Delivered {},
        Delivery::Refused(r) => D::Refused {
            reason: match r {
                crate::controller::Refusal::Precondition(_) => api::Refusal::Precondition,
                crate::controller::Refusal::DialogOpen => api::Refusal::DialogOpen,
                crate::controller::Refusal::NoBracketedPaste => api::Refusal::NoBracketedPaste,
            },
        },
        Delivery::Declined => D::Declined {},
        Delivery::Aborted => D::Aborted {},
        Delivery::Stalled => D::Stalled {},
        Delivery::Cancelled(c) => D::Cancelled {
            reason: match c {
                Cancel::Exited => api::CancelReason::Exited,
                Cancel::Detached => api::CancelReason::Detached,
                Cancel::Closing => api::CancelReason::Closing,
                Cancel::SessionChanged => api::CancelReason::SessionChanged,
            },
        },
        Delivery::Unobserved => D::Unobserved {},
    }
}

fn rejected(r: &Rejected) -> api::RejectedView {
    use api::RejectedView as R;
    match r {
        Rejected::Stale { current } => R::Stale {
            current: Some(live_ref(current)),
        },
        Rejected::Exited => R::Exited {},
        Rejected::ReadOnly => R::ReadOnly {},
        Rejected::Detached => R::Detached {},
        Rejected::Busy => R::Busy {},
        Rejected::NoBracketedPaste => R::NoBracketedPaste {},
        Rejected::NotInjected => R::NotInjected {},
        Rejected::Empty => R::Empty {},
        Rejected::NoChange => R::NoChange {},
        Rejected::ModelUnknown => R::ModelUnknown {},
        Rejected::KeyReused { .. } => R::KeyReused {},
        Rejected::Expired => R::Expired {},
        Rejected::InputHeld => R::InputHeld {},
        Rejected::BadPath => R::BadPath {},
    }
}

/// A gateway error as a device sees it (messages without the home folder).
fn api_error(e: &GatewayError) -> ApiError {
    match e {
        GatewayError::Maintenance(_) => ApiError::Maintenance {},
        GatewayError::StaleEpoch { .. } => ApiError::Stale { current: None },
        GatewayError::Rejected(Rejected::Stale { current }) => ApiError::Stale {
            current: Some(live_ref(current)),
        },
        GatewayError::Rejected(r) => ApiError::Rejected {
            reason: rejected(r),
        },
        GatewayError::KeyReused => ApiError::KeyReused {},
        GatewayError::Expired => ApiError::Expired {},
        GatewayError::TooLarge => ApiError::TooLarge {},
        GatewayError::NoSuchJob => ApiError::NotFound {
            what: "job".to_owned(),
        },
        GatewayError::Refused(m) | GatewayError::Failed(m) => {
            ApiError::Failed { message: scrub(m) }
        }
        other => ApiError::Failed {
            message: scrub(&other.to_string()),
        },
    }
}

fn submitted(r: &RequestRecord) -> api::DoneView {
    api::DoneView::Submitted {
        request: r.id.0,
        label: r.label.clone(),
        delivery: delivery(&r.delivery),
    }
}

fn own_requests(info: &ControllerInfo, device: &str) -> Vec<api::OwnRequest> {
    info.requests
        .iter()
        .filter(|r| r.key.client == device)
        .map(|r| api::OwnRequest {
            request: r.id.0,
            seq: r.key.seq,
            label: r.label.clone(),
            delivery: delivery(&r.delivery),
        })
        .collect()
}

/// A controlled session as `device` sees it.
fn live_session(
    info: &ControllerInfo,
    device: &str,
    session: Option<String>,
    desktop_controlled: bool,
) -> api::LiveSession {
    let cwd = info.live_cwd.as_deref().unwrap_or(&info.cwd);
    api::LiveSession {
        live: live_ref(&info.target),
        kind: match info.role {
            SessionRole::Session => api::LiveKind::Session,
            SessionRole::Tool => api::LiveKind::Tool,
        },
        label: info.label.clone(),
        title: info.title.clone(),
        session,
        project: Some(project_id(cwd)),
        display: abbreviate_home(cwd),
        activity: activity(info.activity),
        status: status(info.status),
        exit_code: info.exit_code,
        busy: info.busy,
        model: info.current_model.clone(),
        pending_switch: info.pending_switch.as_ref().map(|s| api::SwitchView {
            selector: s.selector.clone(),
            level: s.level.clone(),
        }),
        live_control: info.live_control,
        desktop_controlled,
        views: count(info.views),
        requests: own_requests(info, device),
    }
}

fn card(id: &str, s: &FleetSession) -> api::Card {
    let mut agents = api::AgentCounts {
        running: 0,
        done: 0,
        failed: 0,
        total: count(s.agents.len()),
    };
    for a in s.agents.iter() {
        match a.status {
            AgentStatus::Running => agents.running += 1,
            AgentStatus::Done => agents.done += 1,
            AgentStatus::Failed | AgentStatus::Aborted | AgentStatus::Interrupted => {
                agents.failed += 1;
            }
        }
    }
    api::Card {
        session: id.to_owned(),
        model: s.model.clone(),
        thinking: s.thinking.clone(),
        context_tokens: s.context_tokens,
        input_tokens: s.input_tokens,
        output_tokens: s.output_tokens,
        cost: s.cost,
        last_reply: s.last_reply.clone(),
        last_turn_failed: s.last_turn_failed,
        git: s.git.as_ref().map(|g| api::GitCounts {
            branch: g.branch.clone(),
            ahead: g.ahead,
            behind: g.behind,
            staged: g.staged,
            unstaged: g.unstaged,
            untracked: g.untracked,
        }),
        agents,
    }
}

fn change_kind(c: deck_core::changes::Change) -> api::ChangeKind {
    use deck_core::changes::Change;
    match c {
        Change::Modified => api::ChangeKind::Modified,
        Change::Added => api::ChangeKind::Added,
        Change::Deleted => api::ChangeKind::Deleted,
        Change::Renamed => api::ChangeKind::Renamed,
        Change::Copied => api::ChangeKind::Copied,
        Change::TypeChanged => api::ChangeKind::TypeChanged,
    }
}

fn changed_file(f: &ChangedFile) -> api::ChangedFile {
    api::ChangedFile {
        path: f.path.clone(),
        orig_path: f.orig_path.clone(),
        staged: f.staged.map(change_kind),
        unstaged: f.unstaged.map(change_kind),
        untracked: f.untracked,
        conflicted: f.conflicted,
        is_dir: f.is_dir,
    }
}

fn changes_feed(feed: &Feed, value: &Result<Option<Changes>, String>) -> Option<Vec<u8>> {
    match value {
        Err(e) => fit_feed::<api::ChangedFile>(feed, &[], |_, truncated| FeedValue::Changes {
            changes: None,
            error: Some(scrub(e)),
            truncated,
        }),
        Ok(None) => fit_feed::<api::ChangedFile>(feed, &[], |_, truncated| FeedValue::Changes {
            changes: None,
            error: None,
            truncated,
        }),
        Ok(Some(c)) => {
            let files: Vec<api::ChangedFile> = c.files.iter().map(changed_file).collect();
            fit_feed(feed, &files, |files, truncated| FeedValue::Changes {
                changes: Some(api::Changes {
                    branch: c.branch.clone(),
                    files,
                }),
                error: None,
                truncated,
            })
        }
    }
}

fn item(t: &TranscriptItem) -> api::Item {
    let c = |s: &str| cut(s, api::MAX_ITEM_TEXT);
    let at = |t: &Option<SystemTime>| t.map(unix_ms);
    match t {
        TranscriptItem::User {
            text,
            from_agent,
            at: when,
        } => api::Item::User {
            text: c(text),
            from_agent: *from_agent,
            at_ms: at(when),
        },
        TranscriptItem::Assistant {
            text,
            model,
            at: when,
        } => api::Item::Assistant {
            text: c(text),
            model: model.clone(),
            at_ms: at(when),
        },
        TranscriptItem::Thinking { text } => api::Item::Thinking { text: c(text) },
        TranscriptItem::ToolCall {
            call_id,
            name,
            intent,
            summary,
            args,
        } => api::Item::ToolCall {
            call_id: call_id.clone(),
            name: name.clone(),
            intent: intent.as_deref().map(c),
            summary: c(summary),
            args: c(args),
        },
        TranscriptItem::ToolResult {
            call_id,
            name,
            text,
            is_error,
            truncated,
        } => api::Item::ToolResult {
            call_id: call_id.clone(),
            name: name.clone(),
            text: c(text),
            is_error: *is_error,
            truncated: *truncated || text.len() > api::MAX_ITEM_TEXT,
        },
        TranscriptItem::Notice { kind, text } => api::Item::Notice {
            kind: kind.clone(),
            text: c(text),
        },
        TranscriptItem::Irc {
            from,
            agent,
            text,
            note,
        } => api::Item::Irc {
            from: from.clone(),
            agent: agent.clone(),
            text: c(text),
            note: note.as_deref().map(c),
        },
        TranscriptItem::Compaction {
            tokens_before,
            tokens_after,
            summary,
        } => api::Item::Compaction {
            tokens_before: *tokens_before,
            tokens_after: *tokens_after,
            summary: c(summary),
        },
        TranscriptItem::ModelChange { model } => api::Item::ModelChange {
            model: model.clone(),
        },
        TranscriptItem::Error { message } => api::Item::Error {
            message: c(message),
        },
        TranscriptItem::Exit { reason } => api::Item::Exit {
            reason: reason.clone(),
        },
    }
}

fn item_hash(t: &TranscriptItem) -> u64 {
    hash_of(&serde_json::to_vec(t).unwrap_or_default())
}

fn models(list: &[deck_core::omp::ModelInfo]) -> Vec<api::Model> {
    list.iter()
        .map(|m| api::Model {
            selector: m.selector.clone(),
            provider: m.provider.clone(),
            id: m.id.clone(),
            name: m.name.clone(),
            context_window: m.context_window,
            max_tokens: m.max_tokens,
            reasoning: m.reasoning,
            thinking: m.thinking.clone(),
            input: m.input.clone(),
            cost_input: m.cost_input,
            cost_output: m.cost_output,
        })
        .collect()
}

/// The models reply holding as many models as fit one record.
fn models_reply(list: &[deck_core::omp::ModelInfo]) -> ReplyResult {
    let all = models(list);
    let reply = |n: usize| {
        ReplyResult::Ok(Resp::Models {
            models: all[..n].to_vec(),
            truncated: n < all.len(),
        })
    };
    // The largest id a reply can carry: whatever fits with it fits with any.
    let size = |n: usize| {
        serde_json::to_vec(&api::Reply {
            id: u64::MAX,
            result: reply(n),
        })
        .map_or(usize::MAX, |b| b.len())
    };
    if size(all.len()) <= api::MAX_REPLY {
        return reply(all.len());
    }
    let (mut lo, mut hi) = (0, all.len());
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if size(mid) <= api::MAX_REPLY {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    reply(lo)
}

fn diff(d: &deck_core::changes::FileDiff) -> api::Diff {
    let cut_text = d.text.len() > api::MAX_DIFF_TEXT;
    api::Diff {
        text: if cut_text {
            cut(&d.text, api::MAX_DIFF_TEXT)
        } else {
            d.text.clone()
        },
        binary: d.binary,
        truncated: d.truncated || cut_text,
        added: d.added,
        removed: d.removed,
    }
}

fn not_found(what: &str) -> ApiError {
    ApiError::NotFound {
        what: what.to_owned(),
    }
}

fn bad(message: &str) -> ApiError {
    ApiError::BadRequest {
        message: message.to_owned(),
    }
}

fn api_permissions(p: Permissions) -> api::Permissions {
    api::Permissions {
        control: p.control,
        manage: p.manage,
    }
}

// ---- engine -------------------------------------------------------------------------------------

impl Engine {
    /// Opens the application session of device `id`: its link and queue, `WELCOME` first.
    pub(super) fn open_device(
        &mut self,
        id: &str,
        name: String,
        permissions: Permissions,
        wake: Wake,
    ) -> (u64, Arc<DeviceOut>) {
        let out = DeviceOut::new(wake, self.waker.callback());
        let next_seq = self.journal.next_seq(id);
        out.welcome(&api::Welcome {
            protocol: api::PROTOCOL,
            gateway_version: GATEWAY_VERSION.to_owned(),
            epoch: self.epoch.to_hex(),
            device: id.to_owned(),
            name,
            permissions: api_permissions(permissions),
            next_seq,
            limits: api::Limits::default(),
            features: Vec::new(),
        });
        let conn = next_id();
        self.next_seq += 1;
        self.conns.insert(
            conn,
            Conn {
                sink: Sink::Device(Box::new(DeviceConn::new(
                    out.clone(),
                    permissions,
                    next_seq,
                ))),
                role: None,
                client_id: id.to_owned(),
                seq: self.next_seq,
                state: ClientState::default(),
                subs: BTreeSet::new(),
                fleet: Vec::new(),
                agents: None,
                agents_seq: 0,
            },
        );
        (conn, out)
    }

    /// Ends the device sessions the relay link no longer carries (closed, replaced, revoked,
    /// link lost).
    pub(super) fn sync_devices(&mut self) {
        let gone: Vec<u64> = self
            .conns
            .iter()
            .filter(|(id, c)| matches!(c.sink, Sink::Device(_)) && !self.mobile.serves(**id))
            .map(|(id, _)| *id)
            .collect();
        for conn in gone {
            self.close_device(conn);
        }
    }

    fn close_device(&mut self, conn: u64) {
        let Some(Conn {
            sink: Sink::Device(d),
            client_id,
            ..
        }) = self.conns.remove(&conn)
        else {
            return;
        };
        d.out.close();
        for engine in d.views.values() {
            if self.owns(conn, *engine) {
                self.on_view_gone(*engine);
            }
        }
        // A vanished phone never holds a session's injector.
        if !self.conns.values().any(|o| o.client_id == client_id) {
            self.decline_confirmations(&client_id);
        }
    }

    /// Per step: closed sessions go, permissions are re-read, feeds are recomputed.
    pub(super) fn step_devices(&mut self) {
        self.sync_devices();
        let conns: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| matches!(c.sink, Sink::Device(_)))
            .map(|(id, _)| *id)
            .collect();
        for conn in conns {
            self.pump_device(conn);
        }
    }

    fn device_mut(&mut self, conn: u64) -> Option<&mut DeviceConn> {
        match &mut self.conns.get_mut(&conn)?.sink {
            Sink::Device(d) => Some(d),
            Sink::Local(_) => None,
        }
    }

    fn owns(&self, conn: u64, view: u64) -> bool {
        self.views.get(&view).is_some_and(|v| v.owner == Some(conn))
    }

    /// The device may type into (or resize) its view `view` now: it owns it, the view is not
    /// an observer, and the registry still grants `control`. Re-read per record: a lowered
    /// permission ends the typing views only at the next step, and input already on its way
    /// must not be written meanwhile.
    fn may_type(&self, conn: u64, view: u64) -> bool {
        self.views
            .get(&view)
            .is_some_and(|v| v.owner == Some(conn) && v.role != ViewRole::Observer)
            && self
                .conns
                .get(&conn)
                .and_then(|c| self.mobile.permissions(&c.client_id))
                .is_some_and(|p| p.control)
    }

    fn pump_device(&mut self, conn: u64) {
        let Some(perms) = self
            .conns
            .get(&conn)
            .and_then(|c| self.mobile.permissions(&c.client_id))
        else {
            // Revoked: the link closes it.
            return;
        };
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        let device = c.client_id.clone();
        let Sink::Device(d) = &mut c.sink else {
            return;
        };
        let lowered = d.permissions.control && !perms.control;
        d.permissions = perms;
        let mut feeds = std::mem::take(&mut d.feeds);
        let resubscribe = std::mem::take(&mut d.resubscribe);
        let out = d.out.clone();
        if lowered {
            self.end_typing_views(conn);
        }
        let interest = resubscribe.then(|| self.device_interest(&feeds));
        for (feed, st) in &mut feeds {
            if st.dirty {
                self.refresh_feed(&device, feed, st, &out);
            }
        }
        let mut resubscribed = false;
        if let Some(c) = self.conns.get_mut(&conn) {
            if let Some((subs, fleet)) = interest
                && (subs != c.subs || fleet != c.fleet)
            {
                c.subs = subs;
                c.fleet = fleet;
                resubscribed = true;
            }
            if let Sink::Device(d) = &mut c.sink {
                d.feeds = feeds;
            }
        }
        if resubscribed {
            // The watchers and control links start (or stop) on the next step.
            self.waker.now();
        }
    }

    /// The device lost `control`: its input and control views end.
    fn end_typing_views(&mut self, conn: u64) {
        let Some(d) = self.device_mut(conn) else {
            return;
        };
        let views: Vec<(u32, u64)> = d.views.iter().map(|(k, v)| (*k, *v)).collect();
        let out = d.out.clone();
        for (id, engine) in views {
            let typing = self
                .views
                .get(&engine)
                .is_some_and(|v| v.role != ViewRole::Observer);
            if typing {
                self.on_view_gone(engine);
                if let Some(d) = self.device_mut(conn) {
                    d.views.remove(&id);
                }
                out.end_view(id, TermEnd::NotAllowed {});
            }
        }
    }

    fn groups(&self) -> Arc<Vec<ProjectGroup>> {
        self.sessions_watch.latest().unwrap_or_default()
    }

    /// The topics and fleet targets a device's feeds follow.
    fn device_interest(
        &self,
        feeds: &BTreeMap<Feed, FeedState>,
    ) -> (BTreeSet<Topic>, Vec<FleetTarget>) {
        let groups = self.groups();
        let running = self.running.latest();
        let mut subs = BTreeSet::new();
        let mut fleet: Vec<FleetTarget> = Vec::new();
        for (feed, st) in feeds {
            match feed {
                Feed::Cards { sessions } => {
                    subs.insert(Topic::Fleet);
                    for id in sessions {
                        let Ok(s) = resolve_session(&groups, id) else {
                            continue;
                        };
                        if fleet.iter().any(|t| t.session == s.path) {
                            continue;
                        }
                        fleet.push(FleetTarget {
                            session: s.path.clone(),
                            cwd: s.cwd.clone(),
                            owner_alive: running
                                .as_deref()
                                .is_some_and(|r| r.holder(&s.path).is_some()),
                        });
                    }
                }
                Feed::Transcript { .. } => {
                    if let Some(p) = &st.path {
                        subs.insert(Topic::Transcript(p.clone()));
                    }
                }
                Feed::Changes { .. } => {
                    if let Some(p) = &st.path {
                        subs.insert(Topic::Changes(p.clone()));
                    }
                }
                Feed::Live {} => {
                    // Like a desktop tab, the feed keeps each session's control link open.
                    subs.extend(
                        self.sess
                            .managed
                            .iter()
                            .map(|m| Topic::Live(m.ctl.target().key.clone())),
                    );
                }
                Feed::Projects {} | Feed::Sessions { .. } | Feed::Inbox {} => {}
            }
        }
        (subs, fleet)
    }

    fn live_sessions(&self, device: &str) -> Vec<api::LiveSession> {
        let groups = self.groups();
        self.controller_infos()
            .iter()
            .zip(&self.sess.managed)
            .map(|(info, m)| {
                let session = info
                    .session_path
                    .as_deref()
                    .and_then(|p| session_at(&groups, p));
                let desktop = m
                    .controlling()
                    .is_some_and(|v| self.views.get(&v).is_some_and(|v| v.owner.is_none()));
                live_session(info, device, session, desktop)
            })
            .collect()
    }

    /// Recomputes one feed and queues it when it changed.
    fn refresh_feed(&self, device: &str, feed: &Feed, st: &mut FeedState, out: &DeviceOut) {
        let body = match feed {
            Feed::Projects {} => {
                let list = projects(&self.groups());
                fit_feed(feed, &list, |projects, truncated| FeedValue::Projects {
                    projects,
                    truncated,
                })
            }
            Feed::Sessions { project, limit } => {
                let groups = self.groups();
                let (total, list) = groups
                    .iter()
                    .find(|g| project_id(&g.cwd) == *project)
                    .map_or((0, Vec::new()), |g| summaries(g, *limit));
                fit_feed(feed, &list, |sessions, truncated| FeedValue::Sessions {
                    project: project.clone(),
                    total,
                    sessions,
                    truncated,
                })
            }
            Feed::Live {} => {
                let list = self.live_sessions(device);
                fit_feed(feed, &list, |sessions, truncated| FeedValue::Live {
                    sessions,
                    truncated,
                })
            }
            Feed::Inbox {} => {
                let view = self.inbox_view();
                let entries: Vec<api::InboxItem> = view
                    .entries
                    .iter()
                    .map(|e| api::InboxItem {
                        live: live_ref(&e.target),
                        kind: match &e.kind {
                            InboxKind::Finished => api::InboxKind::Finished {},
                            InboxKind::Error(message) => api::InboxKind::Error {
                                message: message.as_deref().map(scrub),
                            },
                            InboxKind::Attention => api::InboxKind::Attention {},
                            InboxKind::Exited { code } => api::InboxKind::Exited { code: *code },
                        },
                        title: e.title.clone(),
                        project: e.project.clone(),
                        body: e.body.clone(),
                        at_ms: e.at_unix_ms,
                        armed: e.armed,
                    })
                    .collect();
                fit_feed(feed, &entries, |entries, truncated| FeedValue::Inbox {
                    inbox: api::Inbox {
                        entries,
                        warns: view.warns,
                        badge: view.badge.clone(),
                    },
                    truncated,
                })
            }
            Feed::Cards { sessions } => {
                let Some(snap) = self.fleet_latest() else {
                    // Its watcher starts on the next step and reports then.
                    st.dirty = false;
                    return;
                };
                let groups = self.groups();
                let cards: Vec<api::Card> = sessions
                    .iter()
                    .filter_map(|id| {
                        let s = resolve_session(&groups, id).ok()?;
                        let f = snap.sessions.iter().find(|f| f.session == s.path)?;
                        Some(card(id, f))
                    })
                    .collect();
                fit_feed(feed, &cards, |cards, truncated| FeedValue::Cards {
                    cards,
                    truncated,
                })
            }
            Feed::Transcript { tail, .. } => {
                self.refresh_transcript(feed, *tail, st, out);
                return;
            }
            Feed::Changes { .. } => {
                let Some(value) = st.path.as_deref().and_then(|p| self.changes_of(p)) else {
                    // Listed on a worker; its result marks the feed again.
                    st.dirty = false;
                    return;
                };
                changes_feed(feed, value)
            }
        };
        st.dirty = false;
        let Some(body) = body else {
            return;
        };
        let h = hash_of(&body);
        if st.last != Some(h) {
            st.last = Some(h);
            out.feed(feed, body);
        }
    }

    /// The next transcript value: the last `tail` items after a (re)start, then appends, one
    /// bounded chunk per value (the next one once the link took the previous one).
    fn refresh_transcript(&self, feed: &Feed, tail: u32, st: &mut FeedState, out: &DeviceOut) {
        if out.feed_pending(feed) {
            return;
        }
        let Some(items) = st.path.as_deref().and_then(|p| self.transcript_of(p)) else {
            return;
        };
        let continues = st.cursor.as_ref().is_some_and(|c| {
            c.sent > 0 && c.sent <= items.len() && item_hash(&items[c.sent - 1]) == c.last
        });
        let (start, reset) = match &st.cursor {
            Some(c) if continues => (c.sent, false),
            // Nothing was sent yet and there is still nothing: no new empty value.
            Some(c) if c.sent == 0 && items.is_empty() => {
                st.dirty = false;
                return;
            }
            _ => (items.len().saturating_sub(tail as usize), true),
        };
        if !reset && start == items.len() {
            st.dirty = false;
            return;
        }
        let mut chunk = Vec::new();
        let mut bytes = 0;
        for t in &items[start..] {
            let it = item(t);
            let n = serde_json::to_vec(&it).map_or(0, |b| b.len());
            if !chunk.is_empty() && bytes + n > api::MAX_TRANSCRIPT_BYTES {
                break;
            }
            bytes += n;
            chunk.push(it);
        }
        let sent = start + chunk.len();
        let body = serde_json::to_vec(&Event::Feed {
            feed: feed.clone(),
            value: FeedValue::Transcript {
                start: start as u64,
                items: chunk,
                reset,
            },
        });
        st.cursor = Some(Cursor {
            sent,
            last: sent.checked_sub(1).map_or(0, |i| item_hash(&items[i])),
        });
        st.dirty = sent < items.len();
        match body {
            Ok(body) => out.feed(feed, body),
            Err(e) => log::error!("device protocol: a transcript value did not encode: {e}"),
        }
    }

    // ---- requests -----------------------------------------------------------------------------

    /// What a device session sent.
    pub(super) fn on_device_inbound(&mut self, conn: u64, inbound: Inbound) {
        match inbound {
            Inbound::Request(req) => self.on_device_request(conn, *req),
            Inbound::Input {
                view,
                bytes,
                ctrl_c,
                credit: _credit,
            } => {
                if self.may_type(conn, view) {
                    let meta = InputMeta {
                        ctrl_c,
                        win32_modifiers: None,
                    };
                    self.on_view_op(view, ViewOp::Input(bytes, meta));
                }
            }
            Inbound::Resize {
                view,
                cols,
                rows,
                cell_width,
                cell_height,
                credit: _credit,
            } => {
                if self.may_type(conn, view) {
                    self.on_view_op(view, ViewOp::Resize(cols, rows, cell_width, cell_height));
                }
            }
            Inbound::ViewGone(view) => {
                if self.owns(conn, view) {
                    self.on_view_gone(view);
                }
                if let Some(d) = self.device_mut(conn) {
                    d.views.retain(|_, v| *v != view);
                }
            }
        }
    }

    fn device_answer(&self, conn: u64, id: u64, result: Result<Resp, ApiError>) {
        if let Some(Sink::Device(d)) = self.conns.get(&conn).map(|c| &c.sink) {
            d.out.reply(
                id,
                match result {
                    Ok(r) => ReplyResult::Ok(r),
                    Err(e) => ReplyResult::Err(e),
                },
            );
        }
    }

    fn on_device_request(&mut self, conn: u64, req: api::Request) {
        let Some(device) = self.conns.get(&conn).map(|c| c.client_id.clone()) else {
            return;
        };
        let Some(perms) = self.mobile.permissions(&device) else {
            return;
        };
        let id = req.id;
        if let Err(need) = req.op.allowed(api_permissions(perms)) {
            return self.device_answer(conn, id, Err(ApiError::NotAllowed { need }));
        }
        if req.op.journaled() {
            let Some(seq) = req.seq else {
                return self.device_answer(conn, id, Err(bad("seq is required")));
            };
            let key = RequestKey {
                client: device,
                seq,
            };
            // The work is the device's operation: a retry is the same work even when the
            // mutation it becomes would differ now (names, running holders).
            let work = journal::hash_of(&req.op);
            if let Some(known) = self.journal.recall(&key, &work) {
                return self.answer_known(conn, id, key, known, None);
            }
            // New work needs a seq at or above the session's `next_seq`: a lower unknown seq
            // was forgotten (or never used) before this session and must not run now.
            let floor = match self.conns.get(&conn).map(|c| &c.sink) {
                Some(Sink::Device(d)) => d.next_seq,
                _ => return,
            };
            if seq < floor {
                return self.device_answer(conn, id, Err(ApiError::Expired {}));
            }
            match self.device_mutation(&key, req.op) {
                Ok(op) => self.on_mutate(conn, id, key, self.epoch, op, Some(work)),
                Err(e) => self.device_answer(conn, id, Err(e)),
            }
            return;
        }
        match req.op {
            Op::Subscribe { feed } => {
                let r = self.device_subscribe(conn, feed);
                self.device_answer(conn, id, r);
            }
            Op::Unsubscribe { feed } => {
                if let Some(d) = self.device_mut(conn)
                    && d.feeds.remove(&feed).is_some()
                {
                    d.out.drop_feed(&feed);
                    d.resubscribe = true;
                }
                self.device_answer(conn, id, Ok(Resp::Unit {}));
            }
            Op::Outcome { seq } => {
                let key = RequestKey {
                    client: device,
                    seq,
                };
                let outcome = match self.journal.lookup(&key) {
                    Ok(Some(o)) => self.outcome_view(&o),
                    Ok(None) => api::OutcomeView::Unknown {},
                    Err(_) => api::OutcomeView::Expired {},
                };
                self.device_answer(conn, id, Ok(Resp::Outcome { outcome }));
            }
            Op::Models {} => self.on_query(conn, id, Query::Models),
            Op::Diff {
                project,
                file,
                side,
            } => match self.changed_files(&project, std::slice::from_ref(&file)) {
                Ok((root, mut files)) => {
                    let Some(file) = files.pop() else {
                        return self.device_answer(conn, id, Err(not_found("changed file")));
                    };
                    let side = match side {
                        api::DiffSide::Staged => DiffSide::Staged,
                        api::DiffSide::Unstaged => DiffSide::Unstaged,
                        api::DiffSide::Untracked => DiffSide::Untracked,
                    };
                    self.on_query(conn, id, Query::Diff { root, file, side });
                }
                Err(e) => self.device_answer(conn, id, Err(e)),
            },
            Op::Attach { live, mode } => self.device_attach(conn, id, &live, mode),
            Op::Detach { view } => {
                let r = self.device_detach(conn, view);
                self.device_answer(conn, id, r);
            }
            Op::Ping {} => self.device_answer(conn, id, Ok(Resp::Pong {})),
            // Journaled ops were handled above.
            _ => self.device_answer(conn, id, Err(bad("seq is required"))),
        }
    }

    fn device_subscribe(&mut self, conn: u64, feed: Feed) -> Result<Resp, ApiError> {
        let groups = self.groups();
        let path = match &feed {
            Feed::Projects {} | Feed::Live {} | Feed::Inbox {} => None,
            Feed::Sessions { project, .. } => {
                resolve_project(&groups, project)?;
                None
            }
            Feed::Cards { sessions } => {
                for s in sessions {
                    resolve_session(&groups, s)?;
                }
                None
            }
            Feed::Transcript { session, agent, .. } => {
                let s = resolve_session(&groups, session)?;
                match agent {
                    None => Some(s.path.clone()),
                    Some(a) if !valid_agent(a) => return Err(bad("not a subagent id")),
                    Some(a) => {
                        Some(agent_transcript(&s.path, a).ok_or_else(|| not_found("subagent"))?)
                    }
                }
            }
            Feed::Changes { project } => Some(resolve_project(&groups, project)?.to_path_buf()),
        };
        let Some(d) = self.device_mut(conn) else {
            return Err(bad("the session ended"));
        };
        if !d.feeds.contains_key(&feed) && d.feeds.len() >= api::MAX_FEEDS as usize {
            return Err(bad("too many feeds: unsubscribe one first"));
        }
        // A repeated subscription starts the feed afresh.
        d.out.drop_feed(&feed);
        d.feeds.insert(
            feed,
            FeedState {
                dirty: true,
                last: None,
                path,
                cursor: None,
            },
        );
        d.resubscribe = true;
        Ok(Resp::Unit {})
    }

    fn device_attach(&mut self, conn: u64, id: u64, live: &api::LiveRef, mode: ViewMode) {
        let r = self.attach_for_device(conn, id, live, mode);
        if let Err(e) = r {
            self.device_answer(conn, id, Err(e));
        }
    }

    fn attach_for_device(
        &mut self,
        conn: u64,
        id: u64,
        live: &api::LiveRef,
        mode: ViewMode,
    ) -> Result<(), ApiError> {
        if self.barrier.is_some() {
            return Err(ApiError::Maintenance {});
        }
        let Some(d) = self.device_mut(conn) else {
            return Err(bad("the session ended"));
        };
        if d.views.len() >= api::MAX_VIEWS as usize {
            return Err(bad("too many terminal views: detach one first"));
        }
        if !d.out.view_room() {
            // Detached views still wait for their `T_END` on the link.
            return Err(ApiError::TooManyRequests {});
        }
        let out = d.out.clone();
        let target = self.live_target(live)?;
        let role = match mode {
            ViewMode::Observe => ViewRole::Observer,
            ViewMode::Input => ViewRole::Input,
            ViewMode::Control => ViewRole::Controlling,
        };
        if role == ViewRole::Controlling
            && self
                .sess
                .find(&target.key)
                .is_some_and(|i| self.sess.managed[i].controlling().is_some())
        {
            return Err(ApiError::GeometryOwned {});
        }
        let engine = next_id();
        let side = Arc::new(ViewSide::polled(out.link_waker()));
        self.attach_view(engine, target, role, side.clone(), Some(conn))
            .map_err(|current| ApiError::Stale {
                current: current.as_ref().map(live_ref),
            })?;
        let Some(d) = self.device_mut(conn) else {
            self.on_view_gone(engine);
            return Err(bad("the session ended"));
        };
        let view = d.next_view;
        d.next_view = d.next_view.wrapping_add(1).max(1);
        d.views.insert(view, engine);
        // The reply goes before the view's first record.
        out.reply(id, ReplyResult::Ok(Resp::Attached { view, mode }));
        out.attach(NewView {
            id: view,
            engine,
            side,
            mode,
        });
        Ok(())
    }

    fn device_detach(&mut self, conn: u64, view: u32) -> Result<Resp, ApiError> {
        let Some(d) = self.device_mut(conn) else {
            return Err(bad("the session ended"));
        };
        let Some(engine) = d.views.remove(&view) else {
            return Err(not_found("view"));
        };
        let out = d.out.clone();
        if self.owns(conn, engine) {
            self.on_view_gone(engine);
        }
        out.end_view(view, TermEnd::Detached {});
        Ok(Resp::Unit {})
    }

    /// The live session `live` names, at the generation the device saw.
    fn live_target(&self, live: &api::LiveRef) -> Result<SessionTarget, ApiError> {
        let key = self
            .sess
            .managed
            .iter()
            .map(|m| &m.ctl.target().key)
            .find(|k| k.as_str() == live.key)
            .cloned()
            .ok_or_else(|| not_found("live session"))?;
        Ok(SessionTarget {
            epoch: self.epoch,
            key,
            generation: live.generation,
        })
    }

    /// `files` of project `project` as the current changes listing has them, and its root.
    fn changed_files(
        &self,
        project: &str,
        files: &[String],
    ) -> Result<(PathBuf, Vec<ChangedFile>), ApiError> {
        let cwd = resolve_project(&self.groups(), project)?.to_path_buf();
        let changes = match self.changes_of(&cwd) {
            None => return Err(bad("subscribe to the project's changes first")),
            Some(Err(e)) => {
                return Err(ApiError::Failed { message: scrub(e) });
            }
            Some(Ok(None)) => return Err(bad("the project is not a git work tree")),
            Some(Ok(Some(c))) => c,
        };
        if files.is_empty() {
            return Err(bad("no files"));
        }
        let list = files
            .iter()
            .map(|f| {
                changes
                    .files
                    .iter()
                    .find(|c| c.path == *f)
                    .cloned()
                    .ok_or_else(|| not_found("changed file"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok((changes.root.clone(), list))
    }

    /// Backend sessions holding `path` open.
    fn holders(&self, paths: &[&Path]) -> Vec<String> {
        let Some(snap) = self.running.latest() else {
            return Vec::new();
        };
        let mut names: Vec<String> = Vec::new();
        for p in paths {
            if let Some(n) = snap.holder(p)
                && !names.iter().any(|x| x == n)
            {
                names.push(n.to_owned());
            }
        }
        names
    }

    /// The gateway mutation a journaled device operation becomes.
    fn device_mutation(&mut self, key: &RequestKey, op: Op) -> Result<Mutation, ApiError> {
        let submit = |this: &Self, live: &api::LiveRef, command: Command| {
            Ok(Mutation::Submit {
                target: this.live_target(live)?,
                command,
            })
        };
        let groups = self.groups();
        let backend = !self.backends.is_empty();
        Ok(match op {
            Op::NewSession { project } => {
                let cwd = resolve_project(&groups, &project)?.to_path_buf();
                Mutation::Launch(LaunchSpec {
                    mux_name: backend.then(|| deck_core::mux::new_session_name(SystemTime::now())),
                    cwd,
                    session_path: None,
                    label: String::new(),
                    argv: super::sessions::omp_argv(None),
                    role: SessionRole::Session,
                    copy_of: None,
                })
            }
            Op::Resume { session } => {
                let s = resolve_session(&groups, &session)?;
                Mutation::Launch(LaunchSpec {
                    mux_name: backend.then(|| deck_core::mux::session_name_for(&s.id)),
                    cwd: s.cwd.clone(),
                    session_path: Some(s.path.clone()),
                    label: s.title.clone().unwrap_or_default(),
                    argv: super::sessions::omp_argv(Some(&s.path)),
                    role: SessionRole::Session,
                    copy_of: None,
                })
            }
            Op::Restart { live } => Mutation::Restart(self.live_target(&live)?),
            Op::Stop { live } => Mutation::Stop(self.live_target(&live)?),
            Op::Kill { live } => Mutation::Kill(self.live_target(&live)?),
            Op::Prompt { live, text } => submit(self, &live, Command::Prompt { text })?,
            Op::Interrupt { live } => submit(self, &live, Command::Action(Action::Stop))?,
            Op::SwitchModel {
                live,
                selector,
                level,
            } => submit(self, &live, Command::SwitchModel { selector, level })?,
            Op::SetEffort { live, level } => submit(self, &live, Command::SetEffort { level })?,
            Op::Action { live, action } => {
                let action = match action {
                    api::SessionAction::Handoff { focus } => Action::Handoff(focus),
                    api::SessionAction::Compact { mode, focus } => Action::Compact(match mode {
                        api::CompactMode::Default => CompactMode::Default,
                        api::CompactMode::Soft => CompactMode::Soft,
                        api::CompactMode::Snapcompact => CompactMode::Snapcompact,
                        api::CompactMode::Focus => CompactMode::Focus(
                            focus
                                .filter(|f| !f.trim().is_empty())
                                .ok_or_else(|| bad("a focused compaction needs its focus"))?,
                        ),
                    }),
                    api::SessionAction::Rename { name } => Action::Rename(name),
                    api::SessionAction::AutoName {} => Action::AutoName,
                    api::SessionAction::Plan {} => Action::PlanMode,
                    api::SessionAction::Retry {} => Action::Retry,
                };
                submit(self, &live, Command::Action(action))?
            }
            Op::Answer {
                live,
                request,
                proceed,
            } => Mutation::Answer {
                target: self.live_target(&live)?,
                id: RequestId(request),
                clear: proceed,
            },
            Op::Cancel { seq } => Mutation::CancelJob(RequestKey {
                client: key.client.clone(),
                seq,
            }),
            Op::InboxOpen { live } => Mutation::InboxOpen(self.live_target(&live)?),
            Op::InboxDismiss { live } => Mutation::InboxDismiss(self.live_target(&live)?),
            Op::InboxClear {} => Mutation::InboxClear,
            Op::Stage { project, files } => {
                let (root, files) = self.changed_files(&project, &files)?;
                Mutation::Stage { root, files }
            }
            Op::Unstage { project, files } => {
                let (root, files) = self.changed_files(&project, &files)?;
                Mutation::Unstage { root, files }
            }
            Op::Discard { project, files } => {
                let (root, files) = self.changed_files(&project, &files)?;
                Mutation::Discard { root, files }
            }
            Op::DeleteUntracked { project, files } => {
                let (root, files) = self.changed_files(&project, &files)?;
                Mutation::DeleteUntracked { root, files }
            }
            Op::DeleteSessions { sessions } => {
                if sessions.is_empty() {
                    return Err(bad("no sessions"));
                }
                let targets = sessions
                    .iter()
                    .map(|id| {
                        resolve_session(&groups, id).map(|s| DeleteTarget {
                            path: s.path.clone(),
                            id: s.id.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                let paths: Vec<&Path> = targets.iter().map(|t| t.path.as_path()).collect();
                let stop = self.holders(&paths);
                if self.devices.deleted.len() + targets.len() > DELETED_IDS {
                    self.devices.deleted.clear();
                }
                for t in &targets {
                    self.devices.deleted.insert(t.path.clone(), t.id.clone());
                }
                Mutation::Delete { targets, stop }
            }
            Op::CopySession { session, project } => {
                let s = resolve_session(&groups, &session)?;
                Mutation::Copy {
                    file: s.path.clone(),
                    dest_cwd: resolve_project(&groups, &project)?.to_path_buf(),
                }
            }
            Op::MoveSession { session, project } => {
                let s = resolve_session(&groups, &session)?;
                Mutation::Move {
                    file: s.path.clone(),
                    dest_cwd: resolve_project(&groups, &project)?.to_path_buf(),
                    stop: self.holders(&[s.path.as_path()]),
                }
            }
            Op::Subscribe { .. }
            | Op::Unsubscribe { .. }
            | Op::Outcome { .. }
            | Op::Models {}
            | Op::Diff { .. }
            | Op::Attach { .. }
            | Op::Detach { .. }
            | Op::Ping {} => return Err(bad("not a journaled operation")),
        })
    }

    // ---- translation of replies and events ----------------------------------------------------

    fn session_of(&self, path: &Path) -> Option<String> {
        session_at(&self.groups(), path).or_else(|| self.devices.deleted.get(path).cloned())
    }

    fn outcome_view(&self, o: &Outcome) -> api::OutcomeView {
        use api::OutcomeView as O;
        match o {
            Outcome::Running => O::Running {},
            Outcome::Done(d) => O::Done {
                result: self.done_view(d),
            },
            Outcome::Failed(m) => O::Failed { message: scrub(m) },
            Outcome::Rejected(r) => O::Rejected {
                reason: rejected(r),
            },
            Outcome::Cancelled => O::Cancelled {},
            Outcome::Uncertain => O::Uncertain {},
            Outcome::Recorded { succeeded } => O::Recorded {
                succeeded: *succeeded,
            },
        }
    }

    fn done_view(&self, d: &Done) -> api::DoneView {
        use api::DoneView as D;
        match d {
            Done::Session(t) => D::Session { live: live_ref(t) },
            Done::Submitted(r) => submitted(r),
            Done::Answered(applied) => D::Answered { applied: *applied },
            Done::Deleted(list) => D::Deleted {
                results: list
                    .iter()
                    .map(|(path, error)| api::SessionResult {
                        session: self.session_of(path).unwrap_or_default(),
                        error: error.as_deref().map(scrub),
                    })
                    .collect(),
            },
            Done::Path(p) => D::File {
                session: self.session_of(p),
            },
            Done::Moved(m) => D::File {
                session: self.session_of(&m.new_path),
            },
            _ => D::Unit {},
        }
    }

    /// A reply to a device request in device terms.
    pub(super) fn device_reply(&self, reply: Reply) -> ReplyResult {
        match reply {
            Ok(Answer::Accepted(a)) => ReplyResult::Ok(Resp::Accepted {
                seq: a.key.seq,
                duplicate: a.duplicate,
            }),
            Ok(Answer::Outcome(o)) => ReplyResult::Ok(Resp::Outcome {
                outcome: self.outcome_view(&o),
            }),
            Ok(Answer::Models(list)) => models_reply(&list),
            Ok(Answer::Diff(d)) => ReplyResult::Ok(Resp::Diff { diff: diff(&d) }),
            Ok(_) => ReplyResult::Ok(Resp::Unit {}),
            Err(e) => ReplyResult::Err(api_error(&e)),
        }
    }

    /// What a device is told of an event for its client (`None`: nothing).
    pub(super) fn device_event(&self, ev: &ServerEvent) -> Option<Event> {
        match ev {
            ServerEvent::Outcome { key, outcome } => Some(Event::Outcome {
                seq: key.seq,
                outcome: self.outcome_view(outcome),
            }),
            ServerEvent::Progress { key, progress } => Some(Event::Progress {
                seq: key.seq,
                stage: progress.stage.clone(),
                done: progress.done,
                total: progress.total,
            }),
            ServerEvent::Controller {
                target,
                event: ControllerEvent::Confirm { id, label, draft },
            } => Some(Event::Confirm {
                live: live_ref(target),
                request: id.0,
                label: label.clone(),
                draft: draft.clone(),
            }),
            ServerEvent::ModelUnconfirmed { target, switch } => Some(Event::ModelUnconfirmed {
                live: live_ref(target),
                selector: switch.selector.clone(),
                level: switch.level.clone(),
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, cwd: &str, ms: u64) -> SessionEntry {
        SessionEntry {
            id: id.to_owned(),
            path: PathBuf::from(format!("{cwd}/.sessions/{id}.jsonl")),
            cwd: PathBuf::from(cwd),
            title: Some(format!("title {id}")),
            first_prompt: Some("p".repeat(1000)),
            modified: UNIX_EPOCH + std::time::Duration::from_millis(ms),
        }
    }

    fn groups() -> Vec<ProjectGroup> {
        vec![
            ProjectGroup {
                cwd: PathBuf::from("/srv/alpha"),
                display: "/srv/alpha".to_owned(),
                sessions: vec![entry("a1", "/srv/alpha", 10), entry("a2", "/srv/alpha", 30)],
                worktree: None,
            },
            ProjectGroup {
                cwd: PathBuf::from("/srv/beta"),
                display: "/srv/beta".to_owned(),
                sessions: vec![entry("b1", "/srv/beta", 20), entry("dup", "/srv/beta", 5)],
                worktree: None,
            },
            ProjectGroup {
                cwd: PathBuf::from("/srv/gamma"),
                display: "/srv/gamma".to_owned(),
                sessions: vec![entry("dup", "/srv/gamma", 6)],
                worktree: None,
            },
        ]
    }

    #[test]
    fn ids_resolve_against_the_inventory_and_ambiguity_is_not_found() {
        let g = groups();
        let alpha = project_id(Path::new("/srv/alpha"));
        assert!(alpha.starts_with("p-") && alpha.len() == 18);
        assert_eq!(
            resolve_project(&g, &alpha).ok(),
            Some(Path::new("/srv/alpha"))
        );
        assert!(matches!(
            resolve_project(&g, "p-0000000000000000"),
            Err(ApiError::NotFound { .. })
        ));
        assert_eq!(
            resolve_session(&g, "b1").map(|s| s.id.as_str()).ok(),
            Some("b1")
        );
        assert!(matches!(
            resolve_session(&g, "dup"),
            Err(ApiError::NotFound { .. })
        ));
        assert!(matches!(
            resolve_session(&g, "nope"),
            Err(ApiError::NotFound { .. })
        ));
        assert_eq!(
            session_at(&g, Path::new("/srv/beta/.sessions/b1.jsonl")).as_deref(),
            Some("b1")
        );
    }

    #[test]
    fn subagent_ids_are_single_file_stems() {
        assert!(valid_agent("Scout"));
        assert!(valid_agent("M2cCore.M2cSaveService"));
        for bad in ["", ".", "..", "a/b", "a\\b", "../x"] {
            assert!(!valid_agent(bad), "{bad:?}");
        }
        assert!(!valid_agent(&"x".repeat(AGENT_ID_CHARS + 1)));
    }

    #[test]
    fn class_table_refuses_without_the_permission() {
        let view = api_permissions(Permissions {
            control: false,
            manage: false,
        });
        let live = api::LiveRef {
            key: "k".to_owned(),
            generation: 1,
        };
        let prompt = Op::Prompt {
            live: live.clone(),
            text: "hi".to_owned(),
        };
        assert_eq!(prompt.allowed(view), Err(api::Class::Control));
        let delete = Op::DeleteSessions {
            sessions: vec!["a".to_owned()],
        };
        let control = api_permissions(Permissions::default());
        assert_eq!(delete.allowed(control), Err(api::Class::Manage));
        assert_eq!(Op::Ping {}.allowed(view), Ok(()));
        let observe = Op::Attach {
            live,
            mode: ViewMode::Observe,
        };
        assert_eq!(observe.allowed(view), Ok(()));
    }

    #[test]
    fn project_and_session_feeds_are_bounded_and_hold_no_paths() {
        let g = groups();
        let list = projects(&g);
        assert_eq!(list.len(), 3);
        assert_eq!(
            list[0].id,
            project_id(Path::new("/srv/alpha")),
            "newest first"
        );
        let (total, sessions) = summaries(&g[0], 1);
        assert_eq!(total, 2);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "a2");
        assert_eq!(
            sessions[0].first_prompt.as_ref().map(|p| p.chars().count()),
            Some(api::FIRST_PROMPT_CHARS)
        );
        let json = serde_json::to_string(&sessions).expect("json");
        assert!(!json.contains("/srv/"), "{json}");
        assert!(!json.contains(".jsonl"), "{json}");
    }

    #[test]
    fn oversized_feeds_are_cut_to_one_record() {
        let big: Vec<api::Project> = (0..20_000)
            .map(|i| api::Project {
                id: format!("p-{i:016x}"),
                display: "~/".to_owned() + &"d".repeat(60),
                worktree: false,
                session_count: 1,
                modified_unix_ms: i,
            })
            .collect();
        let feed = Feed::Projects {};
        let body = fit_feed(&feed, &big, |projects, truncated| FeedValue::Projects {
            projects,
            truncated,
        })
        .expect("fits");
        assert!(body.len() <= api::MAX_REPLY);
        let ev: Event = serde_json::from_slice(&body).expect("event");
        let Event::Feed {
            value:
                FeedValue::Projects {
                    projects,
                    truncated,
                },
            ..
        } = ev
        else {
            panic!("not a projects value");
        };
        assert!(truncated);
        assert!(!projects.is_empty() && projects.len() < big.len());
    }

    #[test]
    fn live_projection_shows_only_the_devices_own_requests_and_no_ctl_key() {
        use crate::controller::{CommandKind, RequestRecord, ServiceEpoch, SessionKey};
        let target = SessionTarget {
            epoch: ServiceEpoch([7; 16]),
            key: SessionKey::tmux("omp-deck-test-1", "omp-x"),
            generation: 3,
        };
        let record = |client: &str, seq| {
            let mut r: RequestRecord = serde_json::from_value(serde_json::json!({
                "id": seq,
                "key": { "client": client, "seq": seq },
                "target": target,
                "kind": "Prompt",
                "label": "Send prompt",
                "delivery": "Queued",
                "runtime": "Untracked",
            }))
            .expect("record");
            r.kind = CommandKind::Prompt;
            r
        };
        let info = ControllerInfo {
            target: target.clone(),
            role: SessionRole::Session,
            label: "x".to_owned(),
            cwd: PathBuf::from("/srv/alpha"),
            session_path: Some(PathBuf::from("/srv/alpha/.sessions/a1.jsonl")),
            backend: crate::gateway::protocol::BackendRef::Direct,
            title: None,
            activity: Activity::Idle,
            exit_code: None,
            status: TabStatus::Bell,
            busy: false,
            current_model: None,
            pending_switch: None,
            assumed_switch: None,
            ctl_expect: true,
            live_control: true,
            spawned_unix_ms: 1,
            ctl_key: Some("secret-ctl-key".to_owned()),
            live_cwd: None,
            requests: vec![record("d-1", 1), record("gui-other", 2)],
            views: 1,
            controlled: false,
        };
        let s = live_session(&info, "d-1", Some("a1".to_owned()), false);
        assert_eq!(s.requests.len(), 1);
        assert_eq!(s.requests[0].seq, 1);
        assert_eq!(s.status, api::Status::Idle {});
        let json = serde_json::to_string(&s).expect("json");
        assert!(!json.contains("secret-ctl-key"), "{json}");
        assert!(!json.contains("gui-other"), "{json}");
        assert!(!json.contains("/srv/alpha/"), "{json}");
    }

    #[test]
    fn transcript_items_and_diffs_are_cut() {
        let long = "é".repeat(api::MAX_ITEM_TEXT);
        let it = item(&TranscriptItem::Thinking { text: long });
        let api::Item::Thinking { text } = it else {
            panic!("kind");
        };
        assert!(text.len() <= api::MAX_ITEM_TEXT);
        assert!(text.ends_with(api::CUT_MARK));
        let d = diff(&deck_core::changes::FileDiff {
            text: "x".repeat(api::MAX_DIFF_TEXT + 10),
            binary: false,
            truncated: false,
            added: 1,
            removed: 0,
        });
        assert!(d.truncated && d.text.len() <= api::MAX_DIFF_TEXT);
    }
}
