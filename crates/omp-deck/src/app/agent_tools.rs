//! Agent → OmpDeck tools, this window's side (`docs/notes/contracts.md`, "Agent → OmpDeck
//! tools"): the gateway routes `ompdeck_open_session`, `ompdeck_show_changes` and
//! `ompdeck_request_review` here; this module opens and places sessions, shows the Changes panel,
//! keeps the pending review requests (Changes banner, tab badge, inbox row) and answers the
//! gateway with `Mutation::AgentToolDone` / `Mutation::AgentReviewReply`.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use deck_core::sessions::abbreviate_home;
use deck_service::controller::SessionTarget;
use deck_service::gateway::protocol::{InboxEntry, Mutation, Split};
use deck_service::notifications::inbox::Kind;

use super::*;
use crate::changes_view::{LoadState, ReviewAsk};
use crate::notify_panel::ReviewNote;
use crate::panes::Side;

/// How long "not a git repository" must last before `show_changes` believes it: the gateway
/// says so until its first listing of a folder is read.
const SHOW_LISTING_WAIT: Duration = Duration::from_secs(4);
/// How long `show_changes` waits for the session's tab to exist in this window.
const SHOW_TAB_WAIT: Duration = Duration::from_secs(20);
/// How long `show_changes` waits for a listing that is still loading.
const SHOW_LOAD_WAIT: Duration = Duration::from_secs(20);
/// Summary characters the inbox row of a review request shows.
const NOTE_SUMMARY: usize = 200;

/// A pending `ompdeck_request_review` and the session that asked.
struct Ask {
    target: SessionTarget,
    ask: ReviewAsk,
}

/// A `show_changes` request; the oldest is served, the rest wait their turn.
struct ShowWait {
    request: u64,
    target: SessionTarget,
    path: Option<String>,
    /// When this request's turn came (its tab and the panel were shown).
    started: Option<Instant>,
    /// Its tab was there and activated (else a view of the session is being opened).
    on_tab: bool,
}

#[derive(Default)]
pub(super) struct AgentToolsUi {
    /// Pending review requests, oldest first.
    asks: Vec<Ask>,
    /// The inbox rows of `asks`, same order.
    notes: Vec<ReviewNote>,
    shows: Vec<ShowWait>,
    /// The request the user (or its arrival) last brought up: the panel shows it for its
    /// session, which may wait on several.
    focus: Option<u64>,
}

impl AgentToolsUi {
    /// The review request the Changes panel shows for the session `target` belongs to (any
    /// generation): the focused one when it is this session's, else the session's oldest.
    pub(super) fn ask_for(&self, target: &SessionTarget) -> Option<&ReviewAsk> {
        let mut of_session = self.asks.iter().filter(|a| a.target.key == target.key);
        let focused = self
            .focus
            .and_then(|f| of_session.clone().find(|a| a.ask.request == f));
        focused.or_else(|| of_session.next()).map(|a| &a.ask)
    }

    pub(super) fn notes(&self) -> &[ReviewNote] {
        &self.notes
    }

    /// The session that sent pending review request `request`.
    pub(super) fn asker(&self, request: u64) -> Option<&SessionTarget> {
        self.asks
            .iter()
            .find(|a| a.ask.request == request)
            .map(|a| &a.target)
    }

    /// The answer to `request` is on its way (or failed and may be sent again).
    pub(super) fn set_sending(&mut self, request: u64, sending: bool) {
        if let Some(a) = self.asks.iter_mut().find(|a| a.ask.request == request) {
            a.ask.sending = sending;
        }
    }

    /// Keeps review request `ask` with its inbox row; true when it is new. A request already
    /// kept (replayed after a reconnect or resync) is updated in place: same row, same age,
    /// and an answer on its way stays on its way.
    fn upsert(&mut self, mut ask: Ask, mut note: ReviewNote) -> bool {
        let request = ask.ask.request;
        let Some(i) = self.asks.iter().position(|a| a.ask.request == request) else {
            self.asks.push(ask);
            self.notes.push(note);
            return true;
        };
        ask.ask.sending = self.asks[i].ask.sending;
        self.asks[i] = ask;
        if let Some(old) = self.notes.iter_mut().find(|n| n.request == request) {
            note.entry.at_unix_ms = old.entry.at_unix_ms;
            *old = note;
        }
        false
    }

    /// The gateway finished replaying its pending review requests (`pending`): the ones this
    /// window keeps beyond them ended while their cancel could not reach it. An answer on its
    /// way settles on its own.
    fn replayed(&mut self, pending: &[u64]) {
        let gone: Vec<u64> = self
            .asks
            .iter()
            .filter(|a| !a.ask.sending && !pending.contains(&a.ask.request))
            .map(|a| a.ask.request)
            .collect();
        for request in gone {
            self.remove(request);
        }
    }

    /// Drops request `request`; whether it was pending.
    fn remove(&mut self, request: u64) -> bool {
        let before = self.asks.len();
        self.asks.retain(|a| a.ask.request != request);
        self.notes.retain(|n| n.request != request);
        if self.focus == Some(request) {
            self.focus = None;
        }
        self.asks.len() != before
    }
}

/// Where an agent's new session goes: beside the opener when it asked for a split and its tab
/// is on screen (and the launch has a backend name the reserved pane can wait for), else a tab.
fn placement_side(placement: Split, opener_visible: bool, backend: bool) -> Option<Side> {
    if !opener_visible || !backend {
        return None;
    }
    match placement {
        Split::None => None,
        Split::Right => Some(Side::Right),
        Split::Down => Some(Side::Down),
    }
}

fn placement_label(side: Option<Side>) -> &'static str {
    match side {
        None => "new tab",
        Some(Side::Right) => "split right",
        Some(Side::Down) => "split down",
    }
}

/// `show_changes`' answer once the listing is known.
fn shown_text(files: usize, path: Option<&str>, found: bool) -> String {
    let noun = if files == 1 { "file" } else { "files" };
    let mut text = format!("Opened the Changes panel ({files} changed {noun})");
    match path {
        Some(p) if found => text.push_str(&format!(" at {p}")),
        Some(p) => text.push_str(&format!("; {p} has no changes")),
        None => {}
    }
    text
}

/// The inbox row of a review request: the session's label and folder, "Review requested" and
/// the start of the agent's summary.
fn review_note(
    request: u64,
    target: SessionTarget,
    label: &str,
    project: &str,
    summary: Option<&str>,
) -> ReviewNote {
    let summary = summary.map(str::trim).filter(|s| !s.is_empty());
    let mut body = "Review requested".to_owned();
    if let Some(s) = summary {
        body.push('\n');
        match s.char_indices().nth(NOTE_SUMMARY) {
            Some((cut, _)) => {
                body.push_str(&s[..cut]);
                body.push('…');
            }
            None => body.push_str(s),
        }
    }
    let at_unix_ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    ReviewNote {
        request,
        entry: InboxEntry {
            target,
            kind: Kind::AgentMessage {
                title: "Review requested".to_owned(),
                body: summary.map(str::to_owned),
                level: "warning".to_owned(),
            },
            title: label.to_owned(),
            project: project.to_owned(),
            body,
            at_unix_ms,
            armed: true,
        },
    }
}

impl App {
    /// Answers tool request `request` of session `target` (`opened`: the session
    /// `open_session` started).
    pub(super) fn agent_tool_done(
        &mut self,
        request: u64,
        target: &SessionTarget,
        ok: bool,
        text: String,
        opened: Option<SessionTarget>,
    ) {
        self.gw.mutate(
            Mutation::AgentToolDone {
                request,
                target: target.clone(),
                ok,
                text,
                opened,
            },
            move |_, result| {
                if let Err(e) = result {
                    log::warn!("answering agent tool request {request} failed: {e}");
                }
            },
        );
    }

    /// The tab of `target`'s session (any generation), if this window shows it.
    fn tab_of_session(&self, target: &SessionTarget) -> Option<usize> {
        self.tabs.iter().position(|t| t.target().key == target.key)
    }

    /// Activates `target`'s tab (its pane when split), or opens a view of the running session.
    fn show_session_tab(&mut self, target: &SessionTarget) {
        match self.tab_of_session(target) {
            Some(i) => self.activate(i),
            None => self.show_session_of(target),
        }
    }

    /// `ompdeck_open_session`: a new omp session in `cwd` through the normal launch path, beside
    /// the opener's pane when asked and the opener is on screen, else in a new tab.
    #[expect(
        clippy::too_many_arguments,
        reason = "the fields of ClientEvent::AgentOpenSession"
    )]
    pub(super) fn on_agent_open_session(
        &mut self,
        request: u64,
        opener: &SessionTarget,
        cwd: PathBuf,
        prompt: Option<String>,
        model: Option<String>,
        placement: Split,
        title: Option<String>,
    ) {
        let opener_uid = self
            .tab_of_session(opener)
            .and_then(|i| self.tabs[i].pane_uid());
        let visible = opener_uid.is_some_and(|u| self.visible_uids().contains(&u));
        let mux_name = self.new_mux_name();
        let side = placement_side(placement, visible, mux_name.is_some());
        let text = format!(
            "Opened a new omp session in {} ({})",
            abbreviate_home(&cwd),
            placement_label(side)
        );
        if let (Some(side), Some(uid), Some(name)) = (side, opener_uid, mux_name.clone()) {
            self.reserve_split_beside(uid, side, name);
        }
        let mut req = SpawnReq::session(
            mux_name,
            cwd,
            None,
            title.unwrap_or_default(),
            self.cfg.session_view,
            Post::Agent {
                request,
                opener: opener.clone(),
                text,
            },
            Focus::Claim,
        );
        req.argv = tabs::agent_omp_argv(model.as_deref(), prompt.as_deref());
        req.agent_child = true;
        self.spawn_tab_post(req);
        self.raise_window(None);
        self.ctx.request_repaint();
    }

    /// `ompdeck_show_changes`: queued; [`Self::drive_agent_tools`] shows the session's tab with
    /// the Changes panel and answers once the listing is known.
    pub(super) fn on_agent_show_changes(
        &mut self,
        request: u64,
        target: SessionTarget,
        path: Option<String>,
    ) {
        self.raise_window(None);
        self.agent_tools.shows.push(ShowWait {
            request,
            target,
            path,
            started: None,
            on_tab: false,
        });
        self.ctx.request_repaint();
    }

    /// `ompdeck_request_review`: keep the request (banner, badge, inbox row); `activate` (true
    /// for exactly one window, on first delivery) also shows it. A replayed request only
    /// refreshes what is kept.
    pub(super) fn on_agent_review_request(
        &mut self,
        request: u64,
        target: SessionTarget,
        summary: Option<String>,
        paths: Vec<String>,
        activate: bool,
    ) {
        let (label, project) = match self.tab_of_session(&target) {
            Some(i) => {
                let t = &self.tabs[i];
                (t.label(&self.labels).to_owned(), folder_name(&t.cwd))
            }
            None => self.gw.controller(&target).map_or_else(
                || (String::new(), String::new()),
                |info| (info.label.clone(), folder_name(&info.cwd)),
            ),
        };
        let note = review_note(
            request,
            target.clone(),
            &label,
            &project,
            summary.as_deref(),
        );
        let ask = Ask {
            target: target.clone(),
            ask: ReviewAsk {
                request,
                summary,
                paths,
                sending: false,
            },
        };
        let new = self.agent_tools.upsert(ask, note);
        if new && activate {
            self.agent_tools.focus = Some(request);
            self.show_session_tab(&target);
            self.open_changes_panel();
            self.raise_window(None);
        }
        self.ctx.request_repaint();
    }

    /// The gateway replayed its pending review requests (`pending`, after a connect or a
    /// resync): the others ended meanwhile.
    pub(super) fn on_agent_reviews_replayed(&mut self, pending: &[u64]) {
        self.agent_tools.replayed(pending);
        self.ctx.request_repaint();
    }

    /// The agent stopped waiting (Stop, the session ended): the request goes away.
    pub(super) fn on_agent_review_cancel(&mut self, request: u64) {
        if self.agent_tools.remove(request) {
            self.toasts
                .info(&self.ctx, "The agent no longer waits for its review");
        }
        self.ctx.request_repaint();
    }

    /// The inbox row of review request `request` was clicked.
    pub(super) fn show_review_ask(&mut self, request: u64) {
        let Some(target) = self
            .agent_tools
            .asks
            .iter()
            .find(|a| a.ask.request == request)
            .map(|a| a.target.clone())
        else {
            return;
        };
        self.agent_tools.focus = Some(request);
        self.show_session_tab(&target);
        self.open_changes_panel();
        self.raise_window(None);
    }

    /// The gateway took the answer to review request `request`.
    pub(super) fn review_answered(&mut self, request: u64) {
        self.agent_tools.remove(request);
    }

    /// Uids of the tabs whose session has a pending review request (the strip's badges).
    pub(super) fn review_uids(&self) -> Vec<u64> {
        if self.agent_tools.asks.is_empty() {
            return Vec::new();
        }
        self.tabs
            .iter()
            .filter(|t| self.agent_tools.ask_for(t.target()).is_some())
            .map(|t| t.uid)
            .collect()
    }

    /// Per frame: serves the oldest `show_changes` request (the panel follows one tab, so
    /// requests take turns): shows its tab with the panel, then answers from the listing.
    pub(super) fn drive_agent_tools(&mut self) {
        let Some(w) = self.agent_tools.shows.first() else {
            return;
        };
        let (request, target, on_tab) = (w.request, w.target.clone(), w.on_tab);
        let tab = self.tab_of_session(&target);
        let Some(started) = w.started else {
            let w = &mut self.agent_tools.shows[0];
            w.started = Some(Instant::now());
            w.on_tab = tab.is_some();
            self.show_session_tab(&target);
            self.open_changes_panel();
            return;
        };
        if let (Some(i), false) = (tab, on_tab) {
            // The view this window opened for the session is here: the panel follows it.
            self.agent_tools.shows[0].on_tab = true;
            self.activate(i);
            self.open_changes_panel();
            return;
        }
        let waited = started.elapsed();
        let state = match tab.map(|i| self.tabs[i].uid) {
            None if waited >= SHOW_TAB_WAIT => ShowState::Done(
                false,
                "OmpDeck could not show this session's tab".to_owned(),
            ),
            None => ShowState::Wait,
            Some(uid) => show_state(self.changes_load_of(uid).as_ref(), waited),
        };
        let (ok, text) = match state {
            ShowState::Wait => {
                self.ctx.request_repaint_after(Duration::from_millis(250));
                return;
            }
            ShowState::Ready => {
                let path = self.agent_tools.shows[0].path.clone();
                let found = path.as_deref().is_some_and(|p| self.changes_select_path(p));
                let files = self.changes_file_count();
                (true, shown_text(files, path.as_deref(), found))
            }
            ShowState::Done(ok, text) => (ok, text),
        };
        self.agent_tools.shows.remove(0);
        self.agent_tool_done(request, &target, ok, text, None);
        // The next request's turn.
        self.ctx.request_repaint();
    }
}

/// Where a served `show_changes` request stands.
#[derive(Debug, PartialEq)]
enum ShowState {
    Wait,
    /// The listing is there: select the path and answer.
    Ready,
    Done(bool, String),
}

/// What the panel's listing state `load` on the request's tab (`None`: the panel is closed or
/// follows another tab) means `waited` after the panel was shown for it.
fn show_state(load: Option<&LoadState>, waited: Duration) -> ShowState {
    match load {
        None => ShowState::Done(
            false,
            "The Changes panel was closed or moved to another session before it showed this \
             session's changes"
                .to_owned(),
        ),
        Some(LoadState::Ready) => ShowState::Ready,
        // The gateway reports "no repository" until its first listing of a folder is read:
        // only a lasting answer is one.
        Some(LoadState::NotRepo) if waited >= SHOW_LISTING_WAIT => ShowState::Done(
            true,
            "Opened the Changes panel (this folder is not inside a git repository)".to_owned(),
        ),
        Some(LoadState::Failed(e)) => {
            ShowState::Done(false, format!("Could not read the repository: {e}"))
        }
        _ if waited >= SHOW_LOAD_WAIT => ShowState::Done(
            true,
            "Opened the Changes panel; its file list is still loading".to_owned(),
        ),
        _ => ShowState::Wait,
    }
}

/// Basename of a session folder (the inbox row's project).
fn folder_name(cwd: &std::path::Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_is_used_only_beside_a_visible_opener_with_a_backend() {
        assert_eq!(placement_side(Split::Right, true, true), Some(Side::Right));
        assert_eq!(placement_side(Split::Down, true, true), Some(Side::Down));
        // The opener's tab is not on screen: a new tab instead.
        assert_eq!(placement_side(Split::Right, false, true), None);
        // No backend name: a reserved pane could never be matched to the launch.
        assert_eq!(placement_side(Split::Down, true, false), None);
        // "tab" is always a new tab.
        assert_eq!(placement_side(Split::None, true, true), None);
    }

    fn ask(request: u64, summary: &str) -> (Ask, ReviewNote) {
        let target = SessionTarget {
            epoch: deck_service::controller::ServiceEpoch([0; 16]),
            key: deck_service::controller::SessionKey::tmux("test", "s"),
            generation: 1,
        };
        let note = review_note(request, target.clone(), "s", "proj", Some(summary));
        let ask = Ask {
            target,
            ask: ReviewAsk {
                request,
                summary: Some(summary.to_owned()),
                paths: Vec::new(),
                sending: false,
            },
        };
        (ask, note)
    }

    #[test]
    fn a_replay_drops_only_the_requests_the_gateway_no_longer_has() {
        let mut ui = AgentToolsUi::default();
        for (r, s) in [(1, "kept"), (2, "lost cancel"), (3, "answering")] {
            let (a, n) = ask(r, s);
            ui.upsert(a, n);
        }
        ui.set_sending(3, true);
        ui.focus = Some(2);
        ui.replayed(&[1]);
        let left: Vec<u64> = ui.asks.iter().map(|a| a.ask.request).collect();
        assert_eq!(
            left,
            [1, 3],
            "the cancelled one went; an answer in flight stays"
        );
        assert_eq!(ui.notes.len(), 2);
        assert_eq!(ui.focus, None, "the focus went with its request");
    }

    #[test]
    fn the_panel_shows_the_focused_request_of_a_session_waiting_on_several() {
        let mut ui = AgentToolsUi::default();
        for (r, s) in [(1, "older"), (2, "newer")] {
            let (a, n) = ask(r, s);
            ui.upsert(a, n);
        }
        let target = ui.asks[0].target.clone();
        assert_eq!(ui.ask_for(&target).unwrap().request, 1, "oldest by default");
        ui.focus = Some(2);
        assert_eq!(ui.ask_for(&target).unwrap().request, 2);
        ui.remove(2);
        assert_eq!(ui.ask_for(&target).unwrap().request, 1);
    }

    #[test]
    fn show_changes_waits_only_while_its_own_panel_loads() {
        let early = Duration::from_millis(500);
        // The panel is closed or follows another tab: a clear failure, never a fake success.
        assert!(matches!(show_state(None, early), ShowState::Done(false, _)));
        assert!(matches!(
            show_state(None, SHOW_LOAD_WAIT),
            ShowState::Done(false, _)
        ));
        assert_eq!(show_state(Some(&LoadState::Ready), early), ShowState::Ready);
        assert_eq!(
            show_state(Some(&LoadState::Loading), early),
            ShowState::Wait
        );
        // "No repository" before the first listing may be the gateway not having read it.
        assert_eq!(
            show_state(Some(&LoadState::NotRepo), early),
            ShowState::Wait
        );
        assert!(matches!(
            show_state(Some(&LoadState::NotRepo), SHOW_LISTING_WAIT),
            ShowState::Done(true, t) if t.contains("not inside a git repository")
        ));
        assert!(matches!(
            show_state(Some(&LoadState::Failed("x".into())), early),
            ShowState::Done(false, _)
        ));
        assert!(matches!(
            show_state(Some(&LoadState::Loading), SHOW_LOAD_WAIT),
            ShowState::Done(true, t) if t.contains("still loading")
        ));
    }

    #[test]
    fn a_resent_review_request_updates_its_row_instead_of_adding_one() {
        let mut ui = AgentToolsUi::default();
        let (a, mut n) = ask(7, "first");
        n.entry.at_unix_ms = 1;
        assert!(ui.upsert(a, n));
        let (a, n) = ask(8, "other");
        assert!(ui.upsert(a, n));
        ui.set_sending(7, true);
        // After a reconnect/resync the gateway sends request 7 again.
        let (a, n) = ask(7, "again");
        assert!(!ui.upsert(a, n), "a known request is not new");
        assert_eq!(ui.asks.len(), 2);
        assert_eq!(ui.notes.len(), 2);
        let kept = ui.ask_for(&ui.asks[0].target.clone()).unwrap();
        assert_eq!(kept.request, 7, "the row keeps its place");
        assert_eq!(kept.summary.as_deref(), Some("again"));
        assert!(kept.sending, "an answer on its way stays on its way");
        assert_eq!(ui.notes[0].entry.at_unix_ms, 1, "the row keeps its age");
        assert!(ui.remove(7) && !ui.remove(7));
        assert_eq!(ui.notes.len(), 1);
    }

    #[test]
    fn the_show_changes_answer_counts_files_and_reports_the_path() {
        assert_eq!(
            shown_text(1, None, false),
            "Opened the Changes panel (1 changed file)"
        );
        assert_eq!(
            shown_text(3, Some("src/a.rs"), true),
            "Opened the Changes panel (3 changed files) at src/a.rs"
        );
        assert_eq!(
            shown_text(0, Some("x"), false),
            "Opened the Changes panel (0 changed files); x has no changes"
        );
    }
}
