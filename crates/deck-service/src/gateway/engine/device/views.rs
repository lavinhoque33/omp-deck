//! Slice-4 projections (`docs/notes/mobile-pwa.md`): gateway values in device terms. No path
//! crosses: folders appear only as `~` displays ([`Canon::display`]) or opaque ids, and free
//! text that may quote paths (errors) is scrubbed ([`Canon::scrub`]).

use std::collections::HashMap;
use std::path::Path;

use deck_core::agents::AgentSummary;
use deck_core::ctl::{ContextBreakdown, LiveAgent, LiveStatus, PendingUi, PendingUiKind};
use deck_core::gdrive::{BackupFile, ProjectBackup, ProjectRestorePlan};
use deck_core::inventory::{InventoryEntry, SessionStats};
use deck_core::omp::SettingsCatalog;
use deck_core::usage::{LimitStatus, UsageSnapshot, parse_iso_ms};
use deck_remote::api::{self, ReplyResult, Resp};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::{Canon, cut, unix_ms};
use crate::gateway::protocol;

/// Each list of a restore plan is cut to this many entries (then `truncated`).
const PLAN_LIST: usize = 500;
/// Per-tool context entries listed at most.
const CONTEXT_TOOLS: usize = 200;

/// The first 8 bytes of `digest` as 16 hex digits.
pub(super) fn hex16(digest: &[u8]) -> String {
    let mut s = String::with_capacity(16);
    for b in digest.iter().take(8) {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// A running mux session's id: `r-` + 16 hex of SHA-256 over its name and the gateway epoch
/// (stable while this gateway runs, meaningless to the next one).
pub(super) fn running_id(name: &str, epoch: &str) -> String {
    let mut h = Sha256::new();
    h.update(name.as_bytes());
    h.update([0]);
    h.update(epoch.as_bytes());
    format!("r-{}", hex16(&h.finalize()))
}

/// Milliseconds as the wire's unsigned form (`None` before the epoch).
fn ms(v: i64) -> Option<u64> {
    u64::try_from(v).ok()
}

fn finite(v: f64) -> Option<f64> {
    Some(v).filter(|c| c.is_finite())
}

fn finite_opt(v: Option<f64>) -> Option<f64> {
    v.filter(|c| c.is_finite())
}

/// The reply holding as many of `items` as fit one record (`truncated` when some were left
/// out).
pub(super) fn fit_reply<T: Clone>(items: &[T], make: impl Fn(Vec<T>, bool) -> Resp) -> ReplyResult {
    let reply = |n: usize| ReplyResult::Ok(make(items[..n].to_vec(), n < items.len()));
    // The largest id a reply can carry: whatever fits with it fits with any.
    let size = |n: usize| {
        serde_json::to_vec(&api::Reply {
            id: u64::MAX,
            result: reply(n),
        })
        .map_or(usize::MAX, |b| b.len())
    };
    if size(items.len()) <= api::MAX_REPLY {
        return reply(items.len());
    }
    let (mut lo, mut hi) = (0, items.len());
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

pub(super) fn image(i: &deck_core::agents::ImageRef) -> api::ImageRef {
    api::ImageRef {
        blob: i.blob.clone(),
        mime: i.mime.clone(),
        bytes: i.bytes,
    }
}

/// `v` when it serializes to at most `max` bytes.
pub(super) fn bounded(v: Option<&Value>, max: usize) -> Option<Value> {
    v.filter(|v| serde_json::to_vec(v).is_ok_and(|b| b.len() <= max))
        .cloned()
}

/// A blob id: 16 lowercase hex digits.
pub(super) fn valid_blob(blob: &str) -> bool {
    blob.len() == 16
        && blob
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether a blob may be served as `mime`: a raster image type (an SVG or HTML "image" is
/// active content the phone must never open) whose bytes carry that type's signature — a
/// declared type is never trusted alone.
pub(super) fn raster_image(mime: &str, bytes: &[u8]) -> bool {
    match mime {
        "image/png" => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
        "image/jpeg" => bytes.starts_with(&[0xff, 0xd8, 0xff]),
        "image/gif" => bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"),
        _ => false,
    }
}

/// The file name a downloaded image gets.
pub(super) fn blob_name(blob: &str, mime: &str) -> String {
    let ext = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        _ => "bin",
    };
    format!("{blob}.{ext}")
}

// ---- pending UI ---------------------------------------------------------------------------------

pub(super) fn ui_request(u: &PendingUi) -> api::UiRequestView {
    let t = |s: &str| cut(s, api::MAX_UI_TEXT);
    let options = |list: &[deck_core::ctl::UiOption]| -> Vec<api::UiOption> {
        list.iter()
            .take(api::MAX_UI_OPTIONS)
            .map(|o| api::UiOption {
                label: t(&o.label),
                description: o.description.as_deref().map(t),
            })
            .collect()
    };
    api::UiRequestView {
        request: format!("u-{}", u.id),
        kind: match u.kind {
            PendingUiKind::Select => api::UiKind::Select,
            PendingUiKind::Confirm => api::UiKind::Confirm,
            PendingUiKind::Input => api::UiKind::Input,
            PendingUiKind::Editor => api::UiKind::Editor,
            PendingUiKind::Ask => api::UiKind::Ask,
            PendingUiKind::Approve => api::UiKind::Approve,
        },
        title: t(&u.title),
        message: u.message.as_deref().map(t),
        timeout_ms: u.timeout_ms,
        options: options(&u.options),
        multi: u.multi,
        initial: u.initial,
        prefill: u.prefill.as_deref().map(t),
        questions: u
            .questions
            .iter()
            .take(api::MAX_UI_OPTIONS)
            .map(|q| api::UiQuestion {
                id: q.id.clone(),
                question: t(&q.question),
                header: q.header.as_deref().map(t),
                options: options(&q.options),
                multi: q.multi,
                recommended: q.recommended,
            })
            .collect(),
        approve: u.approve.as_ref().map(|a| api::ApproveView {
            full: t(&a.full),
            compact: t(&a.compact),
            session_label: a.session.as_deref().map(t),
            grants: a
                .grants
                .iter()
                .take(api::MAX_UI_OPTIONS)
                .map(|g| api::ApproveGrant {
                    scope: g.scope.clone(),
                    label: t(&g.label),
                    always_label: t(&g.always_label),
                })
                .collect(),
        }),
    }
}

/// The ctl id of a [`api::UiRequestView::request`] (`u-<decimal>`).
pub(super) fn ui_request_id(request: &str) -> Option<u64> {
    let digits = request.strip_prefix("u-")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

pub(super) fn ui_answer(a: api::UiAnswer) -> crate::ui_answer::UiAnswer {
    use crate::ui_answer::{ApproveDecision as D, AskAnswer, UiAnswer as U};
    match a {
        api::UiAnswer::Choice { index } => U::Choice { index },
        api::UiAnswer::Choices { indices } => U::Choices { indices },
        api::UiAnswer::Text { text } => U::Text { text },
        api::UiAnswer::Confirm { yes } => U::Confirm { yes },
        api::UiAnswer::Ask { answers } => U::Ask {
            answers: answers
                .into_iter()
                .map(|a| AskAnswer {
                    id: a.id,
                    selected: a.selected,
                    custom: a.custom,
                })
                .collect(),
        },
        api::UiAnswer::Approve { decision } => U::Approve {
            decision: match decision {
                api::ApproveDecision::Once {} => D::Once,
                api::ApproveDecision::Session {} => D::Session,
                api::ApproveDecision::Deny {} => D::Deny,
                api::ApproveDecision::Grant { index, always } => D::Grant { index, always },
            },
        },
        api::UiAnswer::Cancel {} => U::Cancel,
    }
}

// ---- context, usage, agents ---------------------------------------------------------------------

pub(super) fn context(c: &ContextBreakdown) -> api::ContextView {
    let mut tools: Vec<api::ToolTokens> = c
        .tools
        .iter()
        .map(|(name, tokens)| api::ToolTokens {
            name: name.clone(),
            tokens: *tokens,
        })
        .collect();
    tools.sort_by_key(|t| std::cmp::Reverse(t.tokens));
    tools.truncate(CONTEXT_TOOLS);
    api::ContextView {
        model: c.model.clone(),
        context_window: c.context_window,
        used_tokens: c.used_tokens,
        anchored: c.anchored,
        categories: c
            .categories
            .iter()
            .map(|k| api::ContextCategory {
                id: k.id.clone(),
                label: k.label.clone(),
                tokens: k.tokens,
            })
            .collect(),
        auto_compact_buffer: c.auto_compact_buffer,
        free_tokens: c.free_tokens,
        threshold_tokens: c.threshold_tokens,
        snapcompact_saved: c.snapcompact_saved,
        tools,
        skills: c.skills,
        computed_unix_ms: ms(c.computed_at_ms).unwrap_or(0),
    }
}

fn limit_status(s: LimitStatus) -> &'static str {
    match s {
        LimitStatus::Ok => "ok",
        LimitStatus::Warning => "warning",
        LimitStatus::Exhausted => "exhausted",
        LimitStatus::Unknown => "unknown",
    }
}

pub(super) fn usage(s: &UsageSnapshot) -> api::UsageView {
    let reports: Vec<api::UsageReportView> = s
        .reports
        .iter()
        .map(|r| api::UsageReportView {
            provider: r.provider.clone(),
            account: r
                .metadata
                .email
                .clone()
                .or_else(|| r.metadata.account_id.clone()),
            plan: r
                .metadata
                .plan_type
                .clone()
                .or_else(|| r.metadata.tier.clone()),
            fetched_unix_ms: ms(r.fetched_at_ms),
            limits: r
                .limits
                .iter()
                .map(|l| api::UsageLimitView {
                    id: l.id.clone(),
                    label: l.label.clone(),
                    window: l.window.as_ref().map(|w| w.label.clone()),
                    resets_unix_ms: l.window.as_ref().and_then(|w| w.resets_at_ms).and_then(ms),
                    used: finite_opt(l.amount.used),
                    limit: finite_opt(l.amount.limit),
                    remaining: finite_opt(l.amount.remaining),
                    used_fraction: finite_opt(l.amount.used_fraction),
                    remaining_fraction: finite_opt(l.amount.remaining_fraction),
                    unit: l.amount.unit.clone(),
                    status: limit_status(l.status).to_owned(),
                })
                .collect(),
            notes: r.notes.clone(),
        })
        .collect();
    let tightest_remaining = reports
        .iter()
        .flat_map(|r| &r.limits)
        .filter_map(|l| l.remaining_fraction)
        .reduce(f64::min);
    api::UsageView {
        reports,
        tightest_remaining,
        unavailable: s
            .accounts_without_usage
            .iter()
            .map(|a| {
                a.label
                    .clone()
                    .or_else(|| a.email.clone())
                    .or_else(|| a.account_id.clone())
                    .unwrap_or_else(|| a.provider.clone())
            })
            .collect(),
    }
}

pub(super) fn days(a: &deck_core::usage::Activity) -> Vec<api::DayCount> {
    a.days
        .iter()
        .map(|d| api::DayCount {
            day: d.day.to_string(),
            cost: finite(d.totals.cost),
            requests: d.totals.requests,
            tokens: d.totals.tokens,
        })
        .collect()
}

/// `agents` parent before child, newest first among siblings (orphans count as roots).
pub(super) fn agent_order(agents: &[AgentSummary]) -> Vec<&AgentSummary> {
    let ids: std::collections::HashSet<&str> = agents.iter().map(|a| a.id.as_str()).collect();
    let mut children: HashMap<Option<&str>, Vec<&AgentSummary>> = HashMap::new();
    for a in agents {
        let parent = a.parent_id.as_deref().filter(|p| ids.contains(p));
        children.entry(parent).or_default().push(a);
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| a.id.cmp(&b.id)));
    }
    let mut out = Vec::with_capacity(agents.len());
    let mut stack: Vec<&AgentSummary> = children
        .get(&None)
        .map(|l| l.iter().rev().copied().collect())
        .unwrap_or_default();
    while let Some(a) = stack.pop() {
        out.push(a);
        if let Some(kids) = children.get(&Some(a.id.as_str())) {
            stack.extend(kids.iter().rev().copied());
        }
    }
    out
}

pub(super) fn agent(a: &AgentSummary, live: Option<&LiveAgent>, canon: &Canon) -> api::AgentView {
    use deck_core::agents::AgentStatus as S;
    let parked = live.is_some_and(|l| l.status == LiveStatus::Parked);
    api::AgentView {
        id: a.id.clone(),
        parent: a.parent_id.clone(),
        depth: u32::try_from(a.depth).unwrap_or(u32::MAX),
        label: cut(&a.task_gist, api::MAX_UI_TEXT),
        agent: a.agent.clone(),
        status: match a.status {
            S::Running => api::AgentStatus::Running,
            _ if parked => api::AgentStatus::Parked,
            S::Done => api::AgentStatus::Done,
            S::Failed => api::AgentStatus::Failed,
            S::Aborted => api::AgentStatus::Aborted,
            S::Interrupted => api::AgentStatus::Interrupted,
        },
        model: a.model.clone(),
        thinking: a.thinking.clone(),
        error: a
            .error
            .as_deref()
            .map(|e| cut(&canon.scrub(e), api::MAX_UI_TEXT)),
        current: a.current.as_deref().map(|c| cut(c, api::MAX_UI_TEXT)),
        created_unix_ms: a.created.map(unix_ms),
        last_activity_unix_ms: a.last_activity.map(unix_ms),
        duration_ms: a
            .duration
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        requests: a.requests,
        tool_calls: a.tool_calls,
        tokens: a.tokens,
        cost: finite(a.cost),
        context_tokens: a.context_tokens,
        has_output: a.output_path.is_some(),
        live: live.is_some_and(|l| l.live),
    }
}

// ---- inventory ----------------------------------------------------------------------------------

pub(super) struct InventoryRow<'a> {
    pub entry: &'a InventoryEntry,
    pub project: String,
    pub parent: Option<String>,
    pub running: bool,
    pub stats: Option<&'a SessionStats>,
}

pub(super) fn inventory(r: InventoryRow<'_>, canon: &Canon) -> api::InventoryView {
    let e = r.entry;
    api::InventoryView {
        id: e.id.clone(),
        project: r.project,
        display: canon.display(&e.cwd),
        title: e.title.clone(),
        first_prompt: e
            .first_prompt
            .as_deref()
            .map(|p| super::cut_chars(p, api::FIRST_PROMPT_CHARS)),
        stub: e.is_stub(),
        created_unix_ms: e.created.map(unix_ms),
        modified_unix_ms: unix_ms(e.modified),
        jsonl_bytes: e.jsonl_bytes,
        artifact_bytes: e.artifact_bytes,
        artifact_files: e.artifact_files,
        subagents: e.subagents,
        parent: r.parent,
        cwd_exists: e.cwd_exists,
        running: r.running,
        stats: r.stats.map(|s| api::InventoryStats {
            user_messages: s.user_messages,
            assistant_turns: s.assistant_turns,
            input_tokens: s.input_tokens,
            output_tokens: s.output_tokens,
            cache_read_tokens: s.cache_read_tokens,
            cache_write_tokens: s.cache_write_tokens,
            cost: finite(s.cost),
            subagent_cost: finite(s.subagent_cost),
            model: s.models.first().cloned(),
            compactions: s.compactions,
            last_activity_unix_ms: s.last_activity.map(unix_ms),
        }),
    }
}

// ---- omp settings -------------------------------------------------------------------------------

/// omp's settings; a secret never carries a value, only whether one is set.
pub(super) fn settings_groups(c: &SettingsCatalog) -> Vec<api::SettingsGroup> {
    use deck_core::omp::SettingKind as K;
    c.tabs
        .iter()
        .map(|tab| api::SettingsGroup {
            id: tab.id.clone(),
            label: tab.label.clone(),
            entries: tab
                .settings
                .iter()
                .map(|s| api::SettingView {
                    key: s.key.clone(),
                    kind: match &s.kind {
                        K::Bool => api::SettingKind::Bool {},
                        K::Number => api::SettingKind::Number {},
                        K::Enum(options) => api::SettingKind::Enum {
                            options: options.clone(),
                        },
                        K::String => api::SettingKind::String {},
                        K::Array => api::SettingKind::Array {},
                        K::Record => api::SettingKind::Record {},
                    },
                    description: s.description.clone(),
                    value: (!s.secret).then(|| s.value.clone()).flatten(),
                    default: (!s.secret).then(|| s.default.clone()).flatten(),
                    modified: s.value.is_some() && s.value != s.default,
                    secret: s.secret,
                    set: s.value.is_some(),
                })
                .collect(),
        })
        .collect()
}

// ---- Google Drive -------------------------------------------------------------------------------

fn created_ms(created: &str) -> Option<u64> {
    parse_iso_ms(created).and_then(ms)
}

/// `files` newest first.
pub(super) fn newest_backups(files: &[BackupFile]) -> Vec<BackupFile> {
    let mut list = files.to_vec();
    list.sort_by_key(|f| std::cmp::Reverse(parse_iso_ms(&f.created)));
    list
}

/// Config backups grouped by machine, keeping `files`' order.
pub(super) fn drive_hosts(files: &[BackupFile]) -> Vec<api::DriveHost> {
    let mut hosts: Vec<api::DriveHost> = Vec::new();
    for f in files {
        let host = f.host().unwrap_or_default();
        let view = api::DriveBackupView {
            id: f.id.clone(),
            name: f.name.clone(),
            size: f.size,
            created_unix_ms: created_ms(&f.created),
        };
        match hosts.iter_mut().find(|h| h.host == host) {
            Some(h) => h.backups.push(view),
            None => hosts.push(api::DriveHost {
                host: host.to_owned(),
                backups: vec![view],
            }),
        }
    }
    hosts
}

pub(super) fn sync_backup(b: &ProjectBackup, canon: &Canon) -> api::SyncBackupView {
    api::SyncBackupView {
        id: b.file.id.clone(),
        name: b.name.clone(),
        host: b.host.clone(),
        display: canon.display(Path::new(&b.path)),
        size: b.file.size,
        created_unix_ms: created_ms(&b.file.created),
        omp_bytes: b.omp_bytes,
        memory_bytes: b.memory_bytes,
        sessions: b.sessions,
        session_bytes: b.session_bytes,
    }
}

fn capped(list: &[String], canon: &Canon, cut_any: &mut bool) -> Vec<String> {
    *cut_any |= list.len() > PLAN_LIST;
    list.iter()
        .take(PLAN_LIST)
        .map(|s| canon.scrub(s))
        .collect()
}

/// A Drive restore plan and whether a list was cut.
pub(super) fn drive_plan(
    p: &protocol::RestorePlanView,
    canon: &Canon,
) -> (api::RestorePlanView, bool) {
    let mut truncated = false;
    let view = api::RestorePlanView::Drive {
        name: p.name.clone(),
        created_unix_ms: created_ms(&p.created),
        host: p.host.clone(),
        replaced: capped(&p.replaced, canon, &mut truncated),
        added: capped(&p.added, canon, &mut truncated),
        dirs: capped(&p.dirs, canon, &mut truncated),
        removed: capped(&p.removed, canon, &mut truncated),
        safety_display: canon.display(&p.safety_dir),
    };
    (view, truncated)
}

/// A project restore plan and whether a list was cut; a local copy's folder becomes its
/// project id (`project_of`) and display.
pub(super) fn project_plan(
    p: &ProjectRestorePlan,
    canon: &Canon,
    project_of: impl Fn(&Path) -> String,
) -> (api::RestorePlanView, bool) {
    let mut truncated = p.sessions.len() > PLAN_LIST;
    let sessions = p
        .sessions
        .iter()
        .take(PLAN_LIST)
        .map(|s| api::SessionPlanView {
            id: s.id.clone(),
            title: s.title.clone(),
            modified_unix_ms: ms(s.modified_unix.saturating_mul(1000)).unwrap_or(0),
            bytes: s.bytes,
            local: s
                .local
                .iter()
                .map(|l| api::LocalCopyView {
                    project: l.cwd.as_deref().map(&project_of),
                    display: l.cwd.as_deref().map(|c| canon.display(c)),
                    modified_unix_ms: ms(l.modified_unix.saturating_mul(1000)).unwrap_or(0),
                    bytes: l.bytes,
                })
                .collect(),
            problem: s.problem.as_deref().map(|m| canon.scrub(m)),
        })
        .collect();
    let view = api::RestorePlanView::Project {
        target_display: canon.display(&p.target),
        create_target: p.create_target,
        omp_exists: p.omp_exists,
        omp_overwritten: capped(&p.omp_overwritten, canon, &mut truncated),
        omp_added: capped(&p.omp_added, canon, &mut truncated),
        omp_skipped: capped(&p.omp_skipped, canon, &mut truncated),
        memory_exists: p.memory_exists,
        memory_overwritten: capped(&p.memory_overwritten, canon, &mut truncated),
        memory_added: capped(&p.memory_added, canon, &mut truncated),
        memory_skipped: capped(&p.memory_skipped, canon, &mut truncated),
        sessions,
    };
    (view, truncated)
}

pub(super) fn sync_interval(i: deck_core::config::SyncInterval) -> api::SyncInterval {
    use deck_core::config::SyncInterval as S;
    match i {
        S::Off => api::SyncInterval::Off,
        S::Every10Minutes => api::SyncInterval::TenMinutes,
        S::Every30Minutes => api::SyncInterval::ThirtyMinutes,
        S::Hourly => api::SyncInterval::Hourly,
        S::Every6Hours => api::SyncInterval::SixHours,
        S::Daily => api::SyncInterval::Daily,
    }
}

pub(super) fn gateway_mode(m: deck_core::config::GatewayMode) -> api::GatewayMode {
    use deck_core::config::GatewayMode as G;
    match m {
        G::Off => api::GatewayMode::Off,
        G::WhileOpen => api::GatewayMode::WhileOpen,
        G::Background => api::GatewayMode::Background,
    }
}

// ---- updates ------------------------------------------------------------------------------------

pub(super) fn omp_update(v: &protocol::OmpUpdateView, canon: &Canon) -> api::OmpUpdateView {
    let (current, latest, error) = match &v.status {
        None => (None, None, None),
        Some(Ok(s)) => (Some(s.current.clone()), s.latest.clone(), None),
        Some(Err(e)) => (None, None, Some(canon.scrub(e))),
    };
    api::OmpUpdateView {
        version: v.version.clone(),
        current,
        latest,
        error,
        checked_unix_ms: v.checked_unix_ms,
        checking: v.checking,
    }
}

/// An update a device may apply: an in-place install (packages are installed at the desktop).
pub(super) fn apply_allowed(install: &deck_core::selfupdate::InstallKind) -> bool {
    use deck_core::selfupdate::InstallKind as K;
    matches!(
        install,
        K::UserDir { .. } | K::AppImage { .. } | K::WindowsDir { .. }
    )
}

pub(super) fn app_update(v: &protocol::AppUpdateView, canon: &Canon) -> api::AppUpdateView {
    use deck_core::selfupdate::InstallKind as K;
    use protocol::UpdatePhase as P;
    api::AppUpdateView {
        install: match &v.install {
            K::UserDir { .. } => api::InstallKind::UserDir,
            K::AppImage { .. } => api::InstallKind::AppImage,
            K::Deb => api::InstallKind::Deb,
            K::Rpm => api::InstallKind::Rpm,
            K::WindowsDir { .. } => api::InstallKind::WindowsDir,
            K::Dev => api::InstallKind::Dev,
            K::Unsupported { .. } => api::InstallKind::Unsupported,
        },
        apply_allowed: apply_allowed(&v.install),
        current: v.current.clone(),
        latest: v.latest.as_ref().map(|r| r.version.clone()),
        page_url: v.latest.as_ref().map(|r| r.page_url.clone()),
        asset: v.asset.clone(),
        checked_unix_ms: v.checked_unix_ms,
        phase: match &v.phase {
            P::Idle => api::UpdatePhase::Idle {},
            P::Checking => api::UpdatePhase::Checking {},
            P::Downloading { done, total } => api::UpdatePhase::Downloading {
                done: *done,
                total: *total,
            },
            P::Applied(_) => api::UpdatePhase::Applied {},
            P::NeedsManual { .. } => api::UpdatePhase::NeedsManual {},
            P::Failed(m) => api::UpdatePhase::Failed {
                message: canon.scrub(m),
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_ids_are_opaque_and_per_epoch() {
        let a = running_id("omp-1234", "e1");
        assert!(a.starts_with("r-") && a.len() == 18);
        assert!(!a.contains("omp"));
        assert_eq!(a, running_id("omp-1234", "e1"));
        assert_ne!(a, running_id("omp-1234", "e2"));
        assert_ne!(a, running_id("omp-1235", "e1"));
    }

    #[test]
    fn ui_request_ids_round_trip_and_reject_anything_else() {
        assert_eq!(ui_request_id("u-17"), Some(17));
        for bad in ["17", "u-", "u-+1", "u-1a", "x-1", "u-99999999999999999999"] {
            assert_eq!(ui_request_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn pending_ui_is_bounded() {
        let long = "x".repeat(api::MAX_UI_TEXT * 2);
        let ui = PendingUi {
            id: 3,
            kind: PendingUiKind::Select,
            title: long.clone(),
            message: None,
            options: (0..80)
                .map(|i| deck_core::ctl::UiOption {
                    label: format!("o{i}"),
                    description: None,
                })
                .collect(),
            multi: false,
            initial: Some(1),
            prefill: None,
            questions: Vec::new(),
            approve: None,
            timeout_ms: Some(5000),
        };
        let v = ui_request(&ui);
        assert_eq!(v.request, "u-3");
        assert_eq!(v.kind, api::UiKind::Select);
        assert!(v.title.len() <= api::MAX_UI_TEXT);
        assert_eq!(v.options.len(), api::MAX_UI_OPTIONS);
        assert_eq!(v.initial, Some(1));
    }

    #[test]
    fn oversized_values_are_dropped() {
        let small = serde_json::json!({"path": "a"});
        assert_eq!(bounded(Some(&small), 64), Some(small.clone()));
        assert_eq!(bounded(Some(&small), 4), None);
        assert_eq!(bounded(None, 64), None);
    }

    #[test]
    fn blob_ids_are_sixteen_lowercase_hex() {
        assert!(valid_blob("0123456789abcdef"));
        for bad in [
            "0123456789ABCDEF",
            "0123",
            "../../etc/passwd",
            "0123456789abcdeg",
        ] {
            assert!(!valid_blob(bad), "{bad}");
        }
    }

    #[test]
    fn a_blob_is_served_only_when_its_bytes_match_a_raster_type() {
        let png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".as_slice();
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0, 0x10].as_slice();
        let gif = b"GIF89a\x01\0\x01\0".as_slice();
        let webp = b"RIFF\x24\0\0\0WEBPVP8 ".as_slice();
        let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script/></svg>".as_slice();
        assert!(raster_image("image/png", png));
        assert!(raster_image("image/jpeg", jpeg));
        assert!(raster_image("image/gif", gif));
        assert!(raster_image("image/gif", b"GIF87a\x01\0"));
        assert!(raster_image("image/webp", webp));
        // A raster label on other bytes, or on another raster type's bytes: refused.
        for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            assert!(!raster_image(mime, svg), "{mime}");
            assert!(!raster_image(mime, b""), "{mime}");
        }
        assert!(!raster_image("image/png", jpeg));
        assert!(!raster_image("image/jpeg", png));
        assert!(!raster_image("image/webp", b"RIFF\x24\0\0\0WAVEfmt "));
        assert!(!raster_image("image/webp", b"RIFF"));
        // Non-raster types never pass, whatever the bytes.
        assert!(!raster_image("image/svg+xml", svg));
        assert!(!raster_image("text/html", png));
        assert!(!raster_image("", png));
    }

    #[test]
    fn agents_list_parents_before_children_newest_first() {
        let at = |s: u64| Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(s));
        let mk = |id: &str, parent: Option<&str>, created: u64| AgentSummary {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            depth: usize::from(parent.is_some()),
            path: std::path::PathBuf::new(),
            advisor: false,
            agent: None,
            model: None,
            thinking: None,
            task_gist: String::new(),
            task: String::new(),
            status: deck_core::agents::AgentStatus::Done,
            error: None,
            current: None,
            created: at(created),
            last_activity: None,
            duration: None,
            requests: 0,
            tool_calls: 0,
            tokens: 0,
            cost: 0.0,
            context_tokens: None,
            output_path: None,
        };
        let list = vec![
            mk("A", None, 1),
            mk("A.x", Some("A"), 5),
            mk("B", None, 2),
            mk("A.y", Some("A"), 6),
        ];
        let order: Vec<&str> = agent_order(&list).iter().map(|a| a.id.as_str()).collect();
        assert_eq!(order, ["B", "A", "A.y", "A.x"]);
    }

    #[test]
    fn replies_are_fit_to_one_record() {
        let items: Vec<String> = (0..64).map(|_| "x".repeat(64 * 1024)).collect();
        match fit_reply(&items, |text, truncated| Resp::DriveBackups {
            hosts: vec![api::DriveHost {
                host: text.concat(),
                backups: Vec::new(),
            }],
            truncated,
        }) {
            ReplyResult::Ok(Resp::DriveBackups { hosts, truncated }) => {
                assert!(truncated);
                assert!(hosts[0].host.len() < api::MAX_REPLY);
            }
            other => panic!("{other:?}"),
        }
    }
}
