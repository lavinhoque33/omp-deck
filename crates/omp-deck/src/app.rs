//! The eframe application: wires sessions, tmux, tabs, persistence and the terminal widget.

use std::collections::{HashMap, HashSet};
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

use crate::icons::{
    ICON_CLIPBOARD_LIST, ICON_COPY, ICON_EXTERNAL_LINK, ICON_TERMINAL, ICON_TRIANGLE_ALERT,
};
use deck_core::agents::{AgentsTarget, AgentsWatcher};
use deck_core::config::{AppConfig, Density, FolderTabs, TabState, Theme, UiStyle, WindowGeometry};
use deck_core::omp::{
    MoveKind, MoveOutcome, Omp, Setting, UpdateKind, UpdateStatus, fork_argv, update_argv,
};
use deck_core::sessions::{
    ProjectGroup, SessionEntry, SessionWatcher, SessionsRoot, abbreviate_home, find_fork,
};
use deck_core::status::{StatusTarget, StatusWatcher};
use deck_core::tmux::{Tmux, new_session_name, session_name_for};
use deck_term::{Clipboard, TermTheme, TerminalView};
use egui::{Frame, Key, Margin, Modifiers, RichText};
use serde_json::Value;

use crate::agent_ctl::Req;
use crate::agent_hub::HubState;
use crate::agent_model::AgentsView;
use crate::agent_popup::PopupState;
use crate::cli::Cli;
use crate::clipboard_bridge;
use crate::config_writer::ConfigWriter;
use crate::confirm::{self, Confirm};
use crate::folder_scope;
use crate::inject::Injector;
use crate::model_picker::{ModelsCache, PickerState};
use crate::omp_settings::{config_dir_display, defaults_from_catalog};
use crate::omp_worker::{self, MovedTab, OmpEvent};
use crate::running::{RunningMonitor, RunningSnapshot};
use crate::settings_page::{self, SettingsAction, SettingsInput, SettingsState};
use crate::sidebar::{self, MenuEnv, SidebarAction, SidebarCache, SidebarInput};
use crate::statusbar;
use crate::statusfmt::Activity;
use crate::tabs::{
    self, Anchor, ClosedTab, CopyPending, ReopenStack, Tab, TabAction, TabBarInput, TabRole,
    TabStatus, TermTab,
};
use crate::theme::{self, Palette};
use crate::titlebar::{self, BarAction, BarInput, OmpBar};
use crate::toast::{ToastAction, ToastClick, ToastKind, Toasts};
use crate::transfer::{self, FolderDialog, FolderOutcome, TransferKind};
use crate::ui_fonts;
use crate::update::UpdateTracker;
use crate::usage_state::{Usage, UsageEvent};
use crate::usage_view;
use crate::widgets::{empty_state, icon_text, menu_item};

mod agents;
mod bar;
mod ctl;
mod spawn;
mod tree;
mod usage;

use spawn::{Focus, Post, SpawnReq};

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const SAVE_DEBOUNCE: Duration = Duration::from_secs(2);
const REFRESH_SPIN: Duration = Duration::from_millis(800);
const MIN_FONT: f32 = 8.0;
const MAX_FONT: f32 = 40.0;
const DEFAULT_FONT: f32 = 14.0;
const NO_TMUX_BANNER: &str = "tmux not found — sessions are not persistent; closing a tab ends omp";
/// Toast action id: quit the app (after changing the title-bar style).
const TOAST_QUIT: u64 = 1;
/// How often the sidebar's age chips are refreshed (they only change by minutes).
const AGE_REFRESH: Duration = Duration::from_secs(15);
/// How often a pending copy looks for its new session file, and for how long.
const COPY_POLL: Duration = Duration::from_secs(1);
const COPY_TIMEOUT: Duration = Duration::from_secs(120);

pub struct AppInit {
    pub cli: Cli,
    pub cfg: AppConfig,
    pub tmux: Option<Tmux>,
    /// UI font bytes, read before the window opens.
    pub ui_fonts: ui_fonts::UiFonts,
    /// The command-line folder, already resolved (`None` = none given).
    pub folder: Option<Result<PathBuf, String>>,
    /// Process working directory at startup: base of relative typed scope paths.
    pub start_dir: PathBuf,
    /// The installed control extension; `None` = live control unavailable (read-only hub).
    pub ctl_ext: Option<PathBuf>,
}

enum ShotState {
    Waiting,
    Requested(Arc<AtomicBool>),
}

struct ShotPlan {
    path: PathBuf,
    after: Duration,
    state: ShotState,
}

/// Results from worker threads, delivered through a channel and drained each frame.
enum AppEvent {
    Toast(ToastKind, String),
    Omp(OmpEvent),
    /// A poll for the copy's new session file finished (`None` = not there yet).
    ForkFound {
        copy_id: u64,
        path: Option<PathBuf>,
    },
    /// The old tmux session of a restart-with-control is gone (or refused to go): relaunch.
    RestartReady {
        /// Where the restarted tab goes in the strip.
        anchor: Option<Anchor>,
        tab: ClosedTab,
        stopped: Result<(), String>,
    },
    /// A session tab's control directory is ready: spawn its terminal.
    SpawnPrepared(spawn::Prepared),
    /// A headless fork from the session tree finished: the new session file.
    TreeForked(Result<PathBuf, String>),
    /// A usage fetch / `stats.db` read finished.
    Usage(UsageEvent),
}

/// A modal question awaiting the user's answer, with what to do on "yes".
enum PendingConfirm {
    StopSession {
        tmux_name: String,
        label: String,
    },
    SwitchChannel(UpdateKind),
    MoveSession {
        entry: SessionEntry,
        dest: PathBuf,
    },
    /// An unsent draft in a tab's composer that a status-bar action would clear.
    ClearDraft {
        uid: u64,
    },
    /// Kill this subagent through the control link.
    KillAgent {
        id: String,
        /// The tab the question was asked in, its control key and its session file: the kill is
        /// sent only if all three still match when the user confirms.
        uid: u64,
        key: String,
        session: Option<PathBuf>,
    },
    /// Restart a read-only tab's omp with the control extension (tab by `uid`).
    RestartLive {
        uid: u64,
        /// The session file the question was asked about.
        session: PathBuf,
    },
    /// Restart a running session from the sidebar (with or without a tab).
    RestartSession(RestartTarget),
    /// Delete a session file (and its artifacts) through omp.
    DeleteSession(SessionEntry),
}

/// What a sidebar restart was asked about.
pub(super) struct RestartTarget {
    /// The tab showing the session when asked (`None`: only tmux is running it).
    pub uid: Option<u64>,
    pub tmux_name: String,
    pub session: PathBuf,
    pub cwd: PathBuf,
    pub title: String,
}

pub struct App {
    ctx: egui::Context,
    cfg: AppConfig,
    tmux: Option<Tmux>,
    root: SessionsRoot,
    root_exists: bool,
    watcher: SessionWatcher,
    running: RunningMonitor,
    clipboard: Clipboard,
    writer: ConfigWriter,

    tabs: Vec<Tab>,
    active: usize,
    closed: ReopenStack<ClosedTab>,
    labels: HashMap<PathBuf, String>,
    labels_gen: Option<u64>,
    filter: String,
    /// Per-folder mode: the sidebar lists only sessions inside this folder.
    scope: Option<PathBuf>,
    /// Folder given on the command line: this run's tabs are saved under it, not globally.
    launch_folder: Option<PathBuf>,
    /// The native folder dialog for the scope is open.
    picking_scope: bool,
    /// Id of the latest scope validation request; older answers are dropped.
    scope_gen: u64,
    /// Process working directory at startup (base of relative typed paths).
    start_dir: PathBuf,

    pending_restore: Option<(Vec<TabState>, Option<usize>)>,
    focus_pending: bool,
    terminal_focused: bool,
    scroll_to_active: bool,
    refresh_until: Option<Instant>,

    toasts: Toasts,
    events_tx: Sender<AppEvent>,
    events_rx: Receiver<AppEvent>,
    confirm: Option<(Confirm, PendingConfirm)>,
    settings: SettingsState,
    /// `None` = omp not found: every omp-CLI feature is hidden.
    omp: Option<Omp>,
    update: UpdateTracker,
    /// The running check was requested by the user (report its outcome with a toast).
    check_manual: bool,
    /// Session files with a move in flight.
    moving: HashSet<PathBuf>,
    /// Id source for pending copies (see `CopyPending`).
    next_copy_id: u64,
    sidebar_cache: SidebarCache,
    folder_dialog: Option<FolderDialog>,
    home: Option<PathBuf>,
    /// kdialog or zenity exists, so the folder dialog can offer "Browse…".
    picker_available: bool,

    // Wave 3: status bar, model picker, session actions.
    status: StatusWatcher,
    /// Tab the status watcher currently follows.
    status_uid: Option<u64>,
    /// Last target handed to the status watcher (compared per frame to avoid rebuilding it).
    status_target: Option<StatusTarget>,
    bar_view: statusbar::BarView,
    /// omp's default model/thinking from its settings (read once at startup, refreshed with the catalog).
    omp_defaults: statusbar::Defaults,
    picker: PickerState,
    models: ModelsCache,
    dialog: Option<bar::ActionDialog>,
    next_tab_uid: u64,
    /// Tabs (uid) whose close waits for their injector to reach a job boundary and its last byte to settle.
    closing: Vec<u64>,
    /// Opens (sidebar, reopen) requested before the first tmux snapshot: attaching a canonical name
    /// blindly could land in a tmux session that now runs another file. Processed on arrival.
    deferred_opens: Vec<ClosedTab>,
    /// The Neutral font family was rebuilt from the final definitions (first frame).
    fonts_finalized: bool,
    /// Frames drawn since `frame_mark` (debug log `omp_deck::frames`, RUST_LOG=omp_deck::frames=debug).
    frame_count: u32,
    frame_mark: Instant,
    repaint_causes: HashMap<String, u32>,
    // Wave 4: native Agent Hub.
    agents: AgentsWatcher,
    /// Last target handed to the agents watcher.
    agents_target: Option<AgentsTarget>,
    agents_view: AgentsView,
    hub_open: bool,
    hub: HubState,
    popup: Option<PopupState>,
    // Wave 6: session tree.
    tree: Option<tree::TreeUi>,
    // Wave 9: Usage view.
    usage: Usage,
    usage_open: bool,
    usage_ui: usage_view::ViewState,
    tree_busy: Option<tree::TreeBusy>,
    // Wave 5: live subagent control.
    /// The installed control extension every omp launch loads.
    ctl_ext: Option<PathBuf>,
    /// The control client of the active session tab.
    ctl: Option<ctl::CtlLink>,
    /// Commands sent and not answered yet, by request id.
    ctl_reqs: HashMap<u64, Req>,
    /// The last chat error per agent id (shown under the composer).
    ctl_errors: HashMap<String, String>,
    /// Launches waiting for their control directory (processed one at a time, in order).
    spawn: spawn::Pump,
    /// Bumped whenever the active tab changes by the user's hand; a queued launch may take focus
    /// only while this still equals its own claim.
    activation_seq: u64,
    /// Namespace of this instance's control dirs: the tmux socket name, or `direct`.
    ctl_ns: String,
    /// The restored tab that was active (focused once all restored tabs exist).
    restore_focus: Option<u64>,
    /// Counter for the control keys of tabs without tmux.
    direct_seq: u64,

    dirty: bool,
    last_save: Instant,
    last_title: String,
    last_title_at: Instant,

    started: Instant,
    shot: Option<ShotPlan>,
}

/// Minimum time between two window-title updates.
const WINDOW_TITLE_EVERY: Duration = Duration::from_secs(1);

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn term_theme(t: Theme, style: UiStyle) -> TermTheme {
    match (style, t) {
        (UiStyle::Neutral, Theme::Dark) => TermTheme::NeutralDark,
        (UiStyle::Neutral, Theme::Light) => TermTheme::NeutralLight,
        (UiStyle::Neon, Theme::Dark) => TermTheme::Dark,
        (UiStyle::Neon, Theme::Light) => TermTheme::Light,
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, init: AppInit) -> App {
        let ctx = cc.egui_ctx.clone();
        // Order matters: install_fonts replaces the whole font set; icons and the UI face are added after.
        let report = deck_term::install_fonts(&ctx);
        log::info!("terminal fonts: {report:?}");
        let AppInit {
            cli,
            mut cfg,
            tmux,
            ui_fonts,
            folder,
            start_dir,
            ctl_ext,
        } = init;
        let ctl_ns = tmux.as_ref().map_or("direct", |t| t.socket()).to_owned();
        ui_fonts::install(&ctx, ui_fonts);
        ctx.options_mut(|o| o.zoom_with_keyboard = false);

        if !cfg.font_size.is_finite() {
            cfg.font_size = DEFAULT_FONT;
        }
        cfg.font_size = cfg.font_size.clamp(MIN_FONT, MAX_FONT);
        theme::apply(&ctx, cfg.style, cfg.density, cfg.theme == Theme::Dark);

        let root = match &cli.sessions_root {
            Some(p) => SessionsRoot::at(p.clone()),
            None => SessionsRoot::resolve(),
        };
        let repaint = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        let watcher = SessionWatcher::spawn(root.clone(), POLL_INTERVAL, repaint.clone());
        let running =
            RunningMonitor::spawn(tmux.clone(), root.path.clone(), POLL_INTERVAL, repaint);

        let mut startup_error = None;
        let launch_folder = match folder {
            Some(Ok(dir)) => Some(dir),
            Some(Err(e)) => {
                startup_error = Some(e);
                None
            }
            None => None,
        };
        // The command-line folder overrides the saved scope for this run.
        let scope = launch_folder.clone().or_else(|| cfg.folder_scope.clone());
        if let Some(dir) = &launch_folder {
            cfg.push_recent_folder(dir.clone());
        }
        // Tabs are only restored under tmux (without it nothing outlives the app). A folder
        // launch restores that folder's own tabs, a plain launch the global ones.
        let (restore_tabs, restore_active) = match &launch_folder {
            Some(dir) => cfg
                .folder_tabs
                .get(dir)
                .map_or((Vec::new(), None), |f| (f.open_tabs.clone(), f.active_tab)),
            None => (cfg.open_tabs.clone(), cfg.active_tab),
        };
        let pending_restore =
            (tmux.is_some() && !restore_tabs.is_empty()).then_some((restore_tabs, restore_active));
        let shot = cli.screenshot.map(|path| ShotPlan {
            path,
            after: Duration::from_secs_f64(cli.screenshot_after),
            state: ShotState::Waiting,
        });
        let (events_tx, events_rx) = mpsc::channel();
        let omp = Omp::detect();
        if let Some(o) = omp.clone() {
            omp_worker::spawn(&ctx, &events_tx, move || {
                AppEvent::Omp(OmpEvent::Version(o.version().map_err(|e| e.to_string())))
            });
        }
        let status = StatusWatcher::spawn(root.agent_dir(), {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        });
        let mut models = ModelsCache::default();
        if let Some(o) = omp.clone() {
            models.begin();
            omp_worker::spawn(&ctx, &events_tx, move || {
                AppEvent::Omp(OmpEvent::Models(o.models().map_err(|e| e.to_string())))
            });
        }

        let usage = Usage::new(omp.is_some(), Instant::now());
        let mut app = App {
            ctx,
            root_exists: root.exists(),
            cfg,
            tmux,
            root,
            watcher,
            running,
            clipboard: Clipboard::new(),
            writer: ConfigWriter::spawn(),
            tabs: Vec::new(),
            active: 0,
            closed: ReopenStack::new(20),
            labels: HashMap::new(),
            labels_gen: None,
            filter: String::new(),
            scope,
            launch_folder,
            picking_scope: false,
            scope_gen: 0,
            start_dir,
            pending_restore,
            focus_pending: true,
            terminal_focused: false,
            scroll_to_active: false,
            refresh_until: None,
            toasts: Toasts::default(),
            ctl_ext,
            ctl: None,
            ctl_reqs: HashMap::new(),
            ctl_errors: HashMap::new(),
            spawn: spawn::Pump::default(),
            activation_seq: 0,
            ctl_ns,
            restore_focus: None,
            direct_seq: 0,
            events_tx,
            events_rx,
            confirm: None,
            settings: SettingsState::default(),
            omp,
            update: UpdateTracker::default(),
            check_manual: false,
            moving: HashSet::new(),
            next_copy_id: 0,
            sidebar_cache: SidebarCache::default(),
            folder_dialog: None,
            home: dirs::home_dir(),
            picker_available: transfer::on_path("kdialog") || transfer::on_path("zenity"),
            status,
            status_uid: None,
            status_target: None,
            bar_view: statusbar::BarView::default(),
            omp_defaults: statusbar::Defaults::default(),
            picker: PickerState::default(),
            models,
            dialog: None,
            next_tab_uid: 0,
            closing: Vec::new(),
            deferred_opens: Vec::new(),
            agents: AgentsWatcher::spawn({
                let ctx = cc.egui_ctx.clone();
                move || ctx.request_repaint()
            }),
            agents_target: None,
            agents_view: AgentsView::default(),
            hub_open: false,
            hub: HubState::default(),
            popup: None,
            tree: None,
            usage,
            usage_open: false,
            usage_ui: usage_view::ViewState::default(),
            tree_busy: None,
            fonts_finalized: false,
            frame_count: 0,
            frame_mark: Instant::now(),
            repaint_causes: HashMap::new(),
            dirty: false,
            last_save: Instant::now(),
            last_title: String::new(),
            last_title_at: Instant::now()
                .checked_sub(WINDOW_TITLE_EVERY)
                .unwrap_or_else(Instant::now),
            started: Instant::now(),
            shot,
        };
        if let Some(e) = startup_error {
            app.toasts.error(&app.ctx, format!("Folder ignored: {e}"));
        }
        // The default model/thinking for fresh sessions come from omp's settings.
        if app.omp.is_some() {
            app.load_catalog();
        }
        app
    }

    // ---------------------------------------------------------------- tab management

    /// Per strip position: the terminal tab's uid (`None` for other tab kinds).
    fn tab_uids(&self) -> Vec<Option<u64>> {
        self.tabs.iter().map(Tab::uid).collect()
    }

    fn activate(&mut self, i: usize) {
        if i < self.tabs.len() {
            if i != self.active
                && let Some(prev) = self.tabs.get_mut(self.active).and_then(Tab::term_mut)
            {
                prev.term.set_focused(false);
            }
            if let Some(t) = self.tabs[i].term_mut() {
                t.bell = false;
            }
            self.active = i;
            self.activation_seq += 1;
            self.focus_pending = true;
            self.scroll_to_active = true;
            self.dirty = true;
        }
    }

    /// Queue a session tab. `tmux_name` is ignored in degraded mode.
    fn spawn_tab(
        &mut self,
        tmux_name: Option<String>,
        cwd: PathBuf,
        session_path: Option<PathBuf>,
        title: String,
    ) {
        self.spawn_tab_focus(tmux_name, cwd, session_path, title, Focus::Claim);
    }

    fn spawn_tab_focus(
        &mut self,
        tmux_name: Option<String>,
        cwd: PathBuf,
        session_path: Option<PathBuf>,
        title: String,
        focus: Focus,
    ) {
        let argv = tabs::omp_argv(session_path.as_deref());
        self.spawn_tab_post(SpawnReq {
            tmux_name,
            cwd,
            session_path,
            title,
            argv,
            role: TabRole::Session,
            post: Post::None,
            focus,
        });
    }

    /// Queue a terminal tab running `argv` (in tmux when `tmux_name` is given and tmux exists).
    /// Session tabs first get their control directory prepared on a worker (see `spawn.rs`).
    fn spawn_tab_with(
        &mut self,
        tmux_name: Option<String>,
        cwd: PathBuf,
        session_path: Option<PathBuf>,
        title: String,
        argv: Vec<String>,
        role: TabRole,
    ) {
        self.spawn_tab_post(SpawnReq {
            tmux_name,
            cwd,
            session_path,
            title,
            argv,
            role,
            post: Post::None,
            focus: Focus::Claim,
        });
    }

    fn open_session(&mut self, entry: &SessionEntry) {
        self.open_resolved(ClosedTab {
            tmux_name: Some(session_name_for(&entry.id)),
            cwd: entry.cwd.clone(),
            session_path: Some(entry.path.clone()),
            title: entry.label().to_owned(),
        });
    }

    /// Focus the tab already showing `req.session_path`, else spawn one on the tmux session that
    /// holds that file (or a unique name when the preferred one now runs another file). Before the
    /// first tmux snapshot exists the request waits in `deferred_opens`; if the poller could not
    /// start, the snapshot is empty and names are made unique instead.
    fn open_resolved(&mut self, req: ClosedTab) {
        self.open_resolved_focus(req, Focus::Claim);
    }

    /// [`open_resolved`](Self::open_resolved) with the focus rule of the new tab (an existing tab
    /// is always activated; a deferred open claims focus when it finally runs).
    fn open_resolved_focus(&mut self, req: ClosedTab, focus: Focus) {
        let Some(snap) = self.running.latest() else {
            self.deferred_opens.push(req);
            self.ctx.request_repaint();
            return;
        };
        let path = req.session_path.as_deref();
        let holds = |name: &str| path.is_some_and(|p| snap.files_of(name).iter().any(|f| f == p));
        let existing = path.and_then(|p| {
            self.tabs
                .iter()
                .position(|t| t.session_path() == Some(p) || t.tmux_name().is_some_and(holds))
        });
        if let Some(i) = existing {
            if let Some(t) = self.tabs[i].term_mut()
                && t.session_path.is_none()
            {
                t.session_path.clone_from(&req.session_path);
            }
            self.activate(i);
            return;
        }
        if self.spawn_queued_for(req.session_path.as_deref(), None) {
            return; // already on its way
        }
        let name = match (&self.tmux, req.tmux_name) {
            (Some(_), Some(preferred)) => Some(
                snap.resolve_for(
                    self.running.is_blind(),
                    &preferred,
                    req.session_path.as_deref(),
                    unix_ms(),
                )
                .into_name(),
            ),
            _ => None,
        };
        self.spawn_tab_focus(name, req.cwd, req.session_path, req.title, focus);
    }

    fn open_deferred(&mut self) {
        if self.pending_restore.is_some() || self.running.latest().is_none() {
            return;
        }
        for req in std::mem::take(&mut self.deferred_opens) {
            self.open_resolved(req);
        }
    }

    /// Open an `omp update …` tab: runs directly (no tmux, not persisted) in $HOME.
    fn open_update_tab(&mut self, kind: UpdateKind) {
        let cwd = self.home.clone().unwrap_or_else(|| PathBuf::from("/"));
        let argv = self.with_omp_program(update_argv(kind));
        self.spawn_tab_with(None, cwd, None, String::new(), argv, TabRole::Update(kind));
    }

    /// Use the detected omp binary as argv[0] (it may live outside PATH, e.g. `~/.local/bin`).
    fn with_omp_program(&self, mut argv: Vec<String>) -> Vec<String> {
        if let (Some(omp), Some(first)) = (&self.omp, argv.first_mut()) {
            *first = omp.program().to_string_lossy().into_owned();
        }
        argv
    }

    fn new_session(&mut self, cwd: PathBuf) {
        let name = self
            .tmux
            .is_some()
            .then(|| new_session_name(SystemTime::now()));
        self.spawn_tab(name, cwd, None, String::new());
    }

    /// Focus the settings page, creating its tab if needed (at most one).
    fn open_settings(&mut self) {
        match self.tabs.iter().position(Tab::is_settings) {
            Some(i) => self.activate(i),
            None => {
                self.tabs.push(Tab::Settings);
                self.activate(self.tabs.len() - 1);
            }
        }
    }

    fn close_tab(&mut self, i: usize, remember: bool) {
        if i >= self.tabs.len() {
            return;
        }
        let len_before = self.tabs.len();
        let tab = self.tabs.remove(i); // dropping the Terminal detaches; tmux keeps running
        if remember
            && let Tab::Terminal(t) = &tab
            && t.role == TabRole::Session
        {
            self.closed.push(ClosedTab {
                tmux_name: t.tmux_name.clone(),
                cwd: t.cwd.clone(),
                session_path: t.session_path.clone(),
                title: t.title(&self.labels).to_owned(),
            });
        }
        drop(tab);
        self.active = tabs::active_after_close(self.active, i, len_before);
        self.activation_seq += 1;
        if let Some(t) = self.tabs.get_mut(self.active).and_then(Tab::term_mut) {
            t.bell = false;
        }
        self.focus_pending = true;
        self.scroll_to_active = true;
        self.sync_ctl_keep();
        self.dirty = true;
    }

    fn reopen_closed(&mut self) {
        let Some(closed) = self.closed.pop() else {
            return;
        };
        self.open_resolved(closed);
    }

    /// Kill a tmux session on a worker thread; the outcome arrives as a toast.
    fn kill_tmux_session(&self, name: String, label: String) {
        let Some(tmux) = self.tmux.clone() else {
            return;
        };
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let event = match tmux.kill(&name) {
                Ok(()) => AppEvent::Toast(ToastKind::Success, format!("Stopped “{label}”")),
                Err(e) => {
                    log::warn!("kill {name}: {e}");
                    AppEvent::Toast(ToastKind::Error, format!("Could not stop “{label}”: {e}"))
                }
            };
            let _ = tx.send(event);
            ctx.request_repaint();
        });
        self.running.poke();
    }

    fn kill_tab(&mut self, i: usize) {
        let Some(Tab::Terminal(tab)) = self.tabs.get_mut(i) else {
            return;
        };
        match tab.tmux_name.clone() {
            Some(name) if self.tmux.is_some() => {
                let label = tab.title(&self.labels).to_owned();
                self.kill_tmux_session(name, label);
            }
            _ => tab.term.kill(),
        }
    }

    fn apply_tab_action(&mut self, action: TabAction) {
        match action {
            TabAction::Activate(i) => self.activate(i),
            TabAction::Close(i) => self.request_close(i),
            TabAction::Menu(a) => self.apply_sidebar_action(a),
        }
    }

    /// Re-open tabs from the previous run once the first tmux snapshot is available:
    /// alive tmux session → re-attach; else resume its session file; else drop.
    fn restore_tabs(&mut self, snap: &RunningSnapshot) {
        let Some((states, active)) = self.pending_restore.take() else {
            return;
        };
        for (i, st) in states.iter().enumerate() {
            if self
                .tabs
                .iter()
                .any(|t| t.tmux_name() == Some(st.tmux_name.as_str()))
            {
                continue;
            }
            let path = st.session_path.clone().filter(|p| p.is_file());
            if !snap.is_alive(&st.tmux_name) && path.is_none() {
                continue;
            }
            let name = snap
                .resolve_for(
                    self.running.is_blind(),
                    &st.tmux_name,
                    path.as_deref(),
                    unix_ms(),
                )
                .into_name();
            let argv = tabs::omp_argv(path.as_deref());
            self.spawn_tab_post(SpawnReq {
                tmux_name: Some(name),
                cwd: st.cwd.clone(),
                session_path: path,
                title: st.title.clone(),
                argv,
                role: TabRole::Session,
                post: Post::Restore {
                    active: Some(i) == active,
                },
                focus: Focus::Always,
            });
        }
        self.queue_restore_done();
    }

    /// A fresh `omp` tab learns its session file once omp has created it; a tab whose omp moved to
    /// another file (`/new`, `/fork`, `/resume`, also while the tab was hidden) follows when the
    /// monitor sees exactly one file open for its tmux session. A pending copy waits for
    /// `find_fork` instead: omp may hold the source file open too.
    fn sync_session_paths(&mut self, snap: &RunningSnapshot) {
        for tab in self.tabs.iter_mut().filter_map(Tab::term_mut) {
            if tab.copy.is_some() {
                continue;
            }
            let Some(name) = &tab.tmux_name else {
                continue;
            };
            if let Some(path) =
                tabs::followed_path(tab.session_path.as_deref(), snap.files_of(name))
            {
                tab.session_path = Some(path.clone());
                self.dirty = true;
            }
        }
    }

    fn refresh_sessions_cache(&mut self) {
        let generation = self.watcher.generation();
        if self.labels_gen == Some(generation) {
            return;
        }
        self.labels_gen = Some(generation);
        self.root_exists = self.watcher.root_exists();
        self.labels.clear();
        if let Some(groups) = self.watcher.latest() {
            for g in groups.iter() {
                for e in &g.sessions {
                    self.labels.insert(e.path.clone(), e.label().to_owned());
                }
            }
        }
    }

    // ---------------------------------------------------------------- settings & actions

    fn set_font_size(&mut self, size: f32) {
        let size = size.clamp(MIN_FONT, MAX_FONT);
        if (size - self.cfg.font_size).abs() > f32::EPSILON {
            self.cfg.font_size = size;
            self.dirty = true;
        }
    }

    /// Re-applies theme × style × density to egui and every open terminal.
    fn restyle(&mut self) {
        theme::apply(
            &self.ctx,
            self.cfg.style,
            self.cfg.density,
            self.cfg.theme == Theme::Dark,
        );
        let tt = term_theme(self.cfg.theme, self.cfg.style);
        for tab in self.tabs.iter_mut().filter_map(Tab::term_mut) {
            tab.term.set_theme(tt);
        }
        self.dirty = true;
    }

    fn set_theme(&mut self, theme: Theme) {
        if self.cfg.theme != theme {
            self.cfg.theme = theme;
            self.restyle();
        }
    }

    fn set_style(&mut self, style: UiStyle) {
        if self.cfg.style != style {
            self.cfg.style = style;
            self.restyle();
        }
    }

    fn set_density(&mut self, density: Density) {
        if self.cfg.density != density {
            self.cfg.density = density;
            self.restyle();
        }
    }

    fn set_native_decorations(&mut self, native: bool) {
        if self.cfg.native_decorations != native {
            self.cfg.native_decorations = native;
            self.dirty = true;
            self.toasts.with_action(
                &self.ctx,
                ToastKind::Info,
                "Title bar style changes after restarting OmpDeck.",
                ToastAction {
                    label: "Quit now".to_owned(),
                    id: TOAST_QUIT,
                },
            );
        }
    }

    /// Switch the sidebar between all projects (`None`) and one folder. Tabs stay open.
    fn set_scope(&mut self, dir: Option<PathBuf>) {
        if self.scope == dir {
            return;
        }
        if let Some(d) = &dir {
            self.cfg.push_recent_folder(d.clone());
        }
        self.cfg.folder_scope = dir.clone();
        self.scope = dir;
        self.sidebar_cache.scope_ui.error = None;
        self.dirty = true;
    }

    /// Ask the desktop's folder dialog for the scope (worker thread; the UI never waits).
    fn browse_scope(&mut self) {
        if self.picking_scope {
            return;
        }
        self.picking_scope = true;
        self.scope_gen += 1;
        let generation = self.scope_gen;
        let start = self
            .scope
            .clone()
            .or_else(|| self.home.clone())
            .unwrap_or_else(|| PathBuf::from("/"));
        let (home, base) = (self.home.clone(), start.clone());
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let picked = transfer::picker_command(&start, transfer::on_path)
                .and_then(|p| transfer::run_picker(&p));
            AppEvent::Omp(OmpEvent::ScopeChosen {
                generation,
                typed: false,
                result: picked
                    .map(|p| folder_scope::resolve(&p.to_string_lossy(), home.as_deref(), &base)),
            })
        });
    }

    /// Check a typed scope path (existence, symlinks) on a worker. Relative paths are relative to
    /// the folder OmpDeck was started in.
    fn resolve_scope(&mut self, text: String) {
        self.scope_gen += 1;
        let generation = self.scope_gen;
        let home = self.home.clone();
        let base = self.start_dir.clone();
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            AppEvent::Omp(OmpEvent::ScopeChosen {
                generation,
                typed: true,
                result: Some(folder_scope::resolve(&text, home.as_deref(), &base)),
            })
        });
    }

    fn on_scope_chosen(
        &mut self,
        generation: u64,
        typed: bool,
        result: Option<Result<PathBuf, String>>,
    ) {
        if !typed {
            self.picking_scope = false;
        }
        // A newer request supersedes this answer.
        if generation != self.scope_gen {
            return;
        }
        match result {
            None => {}
            Some(Ok(dir)) => {
                if typed {
                    self.sidebar_cache.scope_ui.clear_typed();
                }
                self.set_scope(Some(dir));
                self.focus_pending = true;
            }
            Some(Err(e)) if typed => self.sidebar_cache.scope_ui.error = Some(e),
            Some(Err(e)) => self.toasts.error(&self.ctx, e),
        }
    }

    fn refresh_sessions(&mut self) {
        self.watcher.refresh_now();
        self.running.poke();
        self.refresh_until = Some(Instant::now() + REFRESH_SPIN);
    }

    fn copy_text(&mut self, text: &str, what: &str) {
        self.clipboard.set_text(text);
        self.toasts.success(&self.ctx, format!("Copied {what}"));
    }

    /// Open a link with xdg-open off the UI thread; a failure arrives as an error toast.
    fn open_external(&self, url: String) {
        if !is_openable_url(&url) {
            log::warn!("refusing to open url with disallowed scheme: {url}");
            return;
        }
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            if let Err(e) = spawn_opener(url.as_ref()) {
                log::warn!("xdg-open {url}: {e}");
                let _ = tx.send(AppEvent::Toast(
                    ToastKind::Error,
                    format!("xdg-open failed: {e}"),
                ));
                ctx.request_repaint();
            }
        });
    }

    /// Open `dir` in the desktop file manager (xdg-open), entirely off the UI thread.
    fn open_folder(&self, dir: PathBuf) {
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let outcome = if !dir.is_absolute() || !dir.is_dir() {
                Err(format!("Folder not found: {}", dir.display()))
            } else {
                spawn_opener(dir.as_os_str()).map_err(|e| format!("xdg-open failed: {e}"))
            };
            if let Err(msg) = outcome {
                let _ = tx.send(AppEvent::Toast(ToastKind::Error, msg));
                ctx.request_repaint();
            }
        });
    }

    /// tmux session currently running `entry`: the holder of its file, a live tab on it, else the
    /// canonical name.
    fn running_tmux_name(&self, entry: &SessionEntry) -> Option<String> {
        let snap = self.running.latest();
        let holder = snap
            .as_ref()
            .and_then(|s| s.holder(&entry.path).map(str::to_owned));
        let tab = || {
            self.tabs.iter().find_map(|t| {
                let t = t.term()?;
                (t.session_path.as_deref() == Some(entry.path.as_path()) && !t.term.is_exited())
                    .then(|| t.tmux_name.clone())
                    .flatten()
            })
        };
        holder.or_else(tab).or_else(|| {
            let canonical = session_name_for(&entry.id);
            // Alive but holding another file (after /new, /fork) is not this session.
            snap.is_some_and(|s| s.is_alive(&canonical) && s.files_of(&canonical).is_empty())
                .then_some(canonical)
        })
    }

    /// Ask before stopping the tmux session that runs `entry`.
    fn request_stop(&mut self, entry: &SessionEntry) {
        let Some(tmux_name) = self.running_tmux_name(entry) else {
            self.toasts
                .info(&self.ctx, "That session is not running anymore.");
            return;
        };
        let label = entry.label().to_owned();
        self.confirm = Some((
            Confirm {
                title: "Stop session process?".to_owned(),
                body: format!(
                    "“{label}” will be terminated. Its session file stays in the sidebar, so you can resume it later."
                ),
                confirm_label: "Stop session".to_owned(),
                danger: true,
            },
            PendingConfirm::StopSession { tmux_name, label },
        ));
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let ctrl_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let hub_available = self.active_session_tab();
        // Most specific first (egui ignores extra Shift/Alt when matching).
        let tree_key = (hub_available || self.tree.is_some()) && self.tree_shortcut(ctx);
        let usage_key = self.usage_shortcut(ctx);
        let (reopen, prev, next, bigger, smaller, reset, hub) = ctx.input_mut(|i| {
            (
                i.consume_key(ctrl_shift, Key::T),
                i.consume_key(Modifiers::COMMAND, Key::PageUp),
                i.consume_key(Modifiers::COMMAND, Key::PageDown),
                i.consume_key(ctrl_shift, Key::Plus) | i.consume_key(ctrl_shift, Key::Equals),
                i.consume_key(ctrl_shift, Key::Minus),
                i.consume_key(ctrl_shift, Key::Num0),
                hub_available && i.consume_key(ctrl_shift, Key::A),
            )
        });
        if self.consume_popup_escape(ctx) {
            self.close_agent_popup();
        }
        if hub {
            self.toggle_agent_hub();
        }
        if tree_key {
            self.toggle_tree();
        }
        if usage_key {
            self.toggle_usage();
        }
        if reopen {
            self.reopen_closed();
        }
        // A modal question belongs to the tab it was asked about: switching away is blocked.
        let modal = self.modal_open();
        if prev && !modal && !self.tabs.is_empty() {
            self.activate(tabs::wrap_index(self.active, self.tabs.len(), -1));
        }
        if next && !modal && !self.tabs.is_empty() {
            self.activate(tabs::wrap_index(self.active, self.tabs.len(), 1));
        }
        if bigger {
            self.set_font_size(self.cfg.font_size + 1.0);
        }
        if smaller {
            self.set_font_size(self.cfg.font_size - 1.0);
        }
        if reset {
            self.set_font_size(DEFAULT_FONT);
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                AppEvent::Toast(kind, text) => self.toasts.notify(ctx, kind, text),
                AppEvent::Omp(ev) => self.on_omp_event(ev),
                AppEvent::ForkFound { copy_id, path } => self.on_fork_found(copy_id, path),
                AppEvent::RestartReady {
                    anchor,
                    tab,
                    stopped,
                } => {
                    self.on_restart_ready(anchor, tab, stopped);
                }
                AppEvent::SpawnPrepared(p) => self.on_spawn_prepared(p),
                AppEvent::TreeForked(r) => self.on_tree_forked(r),
                AppEvent::Usage(u) => self.on_usage_event(u),
            }
        }
    }

    // ---------------------------------------------------------------- panels

    fn show_bar(&mut self, ui: &mut egui::Ui, maximized: bool) {
        let active = self.tabs.get(self.active);
        let groups = self.watcher.latest();
        let input = BarInput {
            tabs: TabBarInput {
                tabs: &self.tabs,
                labels: &self.labels,
                active: self.active,
                scroll_to_active: self.scroll_to_active,
                menu: MenuEnv {
                    groups: groups.as_deref().map_or(&[][..], Vec::as_slice),
                    omp_available: self.omp.is_some(),
                    can_stop: self.tmux.is_some(),
                    busy: &self.moving,
                },
            },
            csd: !self.cfg.native_decorations,
            maximized,
            settings_active: self.tabs.get(self.active).is_some_and(Tab::is_settings),
            can_reopen: !self.closed.is_empty(),
            font_size: self.cfg.font_size,
            theme: self.cfg.theme,
            native_decorations: self.cfg.native_decorations,
            strip: titlebar::StripInput {
                title: active
                    .map(|t| titlebar::strip_omp_prefix(t.title(&self.labels)))
                    .unwrap_or_default(),
                cwd: active.and_then(Tab::term).map(|t| t.cwd.as_path()),
                session: active.and_then(Tab::session_path),
            },
            omp: self.omp.as_ref().map(|_| OmpBar {
                version: self.update.version.as_deref(),
                state: &self.update.state,
                available: self.update.available(),
                channel: self.update.channel(),
            }),
        };
        let action = titlebar::show(ui, &input);
        self.scroll_to_active = false;
        let Some(action) = action else {
            return;
        };
        self.focus_pending = true;
        match action {
            BarAction::Tab(a) => self.apply_tab_action(a),
            BarAction::OpenSettings => self.open_settings(),
            BarAction::Reopen => self.reopen_closed(),
            BarAction::Refresh => self.refresh_sessions(),
            BarAction::FontLarger => self.set_font_size(self.cfg.font_size + 1.0),
            BarAction::FontSmaller => self.set_font_size(self.cfg.font_size - 1.0),
            BarAction::OpenUsage => self.open_usage(),
            BarAction::FontReset => self.set_font_size(DEFAULT_FONT),
            BarAction::SetTheme(t) => self.set_theme(t),
            BarAction::SetNativeDecorations(v) => self.set_native_decorations(v),
            BarAction::Quit => self.ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            BarAction::RunUpdate(kind) => self.open_update_tab(kind),
            BarAction::SwitchChannel(kind) => self.confirm_channel_switch(kind),
            BarAction::CheckUpdates => self.check_updates_manually(),
        }
    }

    fn show_banners(&mut self, ui: &mut egui::Ui) {
        if self.tmux.is_none() {
            let pal = Palette::of_ui(ui);
            egui::Panel::top("no-tmux-banner")
                .show_separator_line(false)
                .frame(
                    Frame::NONE
                        .fill(theme::mix(pal.bg, pal.warn, 0.14))
                        .stroke(egui::Stroke::new(1.0, pal.warn.gamma_multiply(0.30)))
                        .inner_margin(Margin::symmetric(16, 8)),
                )
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 10.0;
                        ui.label(icon_text(ICON_TRIANGLE_ALERT, 18.0, pal.warn));
                        ui.label(RichText::new(NO_TMUX_BANNER).color(pal.text_strong));
                    });
                });
        }
    }

    fn show_sidebar(&mut self, ui: &mut egui::Ui, snap: &RunningSnapshot) {
        let pal = Palette::of_ui(ui);
        let refreshing = self.refresh_until.is_some_and(|t| Instant::now() < t);
        if refreshing {
            ui.ctx().request_repaint_after(Duration::from_millis(50));
        }
        let groups = self.watcher.latest();
        let groups_ref: Option<&[ProjectGroup]> = groups.as_deref().map(Vec::as_slice);
        let active_path = self.tabs.get(self.active).and_then(Tab::session_path);
        // A session open in a live tab counts as running even before tmux's fd mapping shows it.
        let open_paths: Vec<&Path> = self
            .tabs
            .iter()
            .filter(|t| t.term().is_some_and(|t| !t.term.is_exited()))
            .filter_map(Tab::session_path)
            .collect();
        // Their rows show the tab's live omp state instead of the running LED.
        let tab_activity: HashMap<&Path, TabStatus> = self
            .tabs
            .iter()
            .filter_map(|t| Some((t.session_path()?, t.status(false))))
            .filter(|(_, s)| matches!(s, TabStatus::Busy | TabStatus::Attention))
            .collect();
        let generation = self.watcher.generation();
        let input = SidebarInput {
            groups: groups_ref,
            running: snap,
            root: &self.root,
            root_exists: self.root_exists,
            active_path,
            active_cwd: self
                .tabs
                .get(self.active)
                .and_then(Tab::term)
                .map(|t| t.cwd.as_path()),
            now: SystemTime::now(),
            refreshing,
            can_stop: self.tmux.is_some(),
            omp_available: self.omp.is_some(),
            busy: &self.moving,
            generation,
            open_paths: &open_paths,
            scope: self.scope.as_deref(),
            recent: &self.cfg.recent_folders,
            picker_available: self.picker_available,
            picking: self.picking_scope,
            tabs: &self.tabs,
            labels: &self.labels,
            active_tab: self.active,
            tab_activity: &tab_activity,
        };
        let panel = egui::Panel::left("sidebar")
            .resizable(true)
            .default_size(self.cfg.sidebar_width)
            .size_range(220.0..=600.0)
            .frame(
                Frame::NONE
                    .fill(pal.panel)
                    .inner_margin(if theme::compact() {
                        Margin::symmetric(8, 8)
                    } else {
                        Margin::symmetric(10, 12)
                    }),
            )
            .show(ui, |ui| {
                sidebar::show(ui, &input, &mut self.filter, &mut self.sidebar_cache)
            });
        let width = panel.response.rect.width();
        if (width - self.cfg.sidebar_width).abs() > 1.0 {
            self.cfg.sidebar_width = width;
            self.dirty = true;
        }
        let out = panel.inner;
        if out.interacted {
            self.focus_pending = true;
        }
        for action in out.actions {
            self.apply_sidebar_action(action);
        }
    }

    fn apply_sidebar_action(&mut self, action: SidebarAction) {
        match action {
            SidebarAction::Open(entry) => self.open_session(&entry),
            SidebarAction::New(cwd) => self.new_session(cwd),
            SidebarAction::Refresh => self.refresh_sessions(),
            SidebarAction::CopyText { text, what } => self.copy_text(&text, what),
            SidebarAction::OpenFolder(dir) => self.open_folder(dir),
            SidebarAction::Stop(entry) => self.request_stop(&entry),
            SidebarAction::Restart(entry) => self.request_restart_session(&entry),
            SidebarAction::Delete(entry) => self.request_delete(entry),
            SidebarAction::Tree(entry) => {
                let title = entry.label().to_owned();
                self.open_tree(entry.path, entry.cwd, title, Some(entry.id));
            }
            SidebarAction::Transfer { kind, entry, dest } => {
                self.start_transfer(kind, entry, dest);
            }
            SidebarAction::SetScope(dir) => self.set_scope(dir),
            SidebarAction::BrowseScope => self.browse_scope(),
            SidebarAction::TypedScope(text) => self.resolve_scope(text),
            SidebarAction::ActivateTab(i) => self.activate(i),
            SidebarAction::CloseTab(i) => self.request_close(i),
            SidebarAction::KillTab(i) => self.kill_tab(i),
        }
    }

    fn show_central(&mut self, ui: &mut egui::Ui) {
        let pal = Palette::of_ui(ui);
        match self.tabs.get(self.active) {
            None => {
                self.terminal_focused = false;
                egui::CentralPanel::no_frame().show(ui, |ui| {
                    ui.painter().rect_filled(ui.max_rect(), 0, pal.bg);
                    ui.add_space((ui.available_height() / 2.0 - 90.0).max(20.0));
                    empty_state(
                        ui,
                        ICON_TERMINAL,
                        pal.accent,
                        "No session open",
                        "Pick a session from the sidebar, or press + next to a project to start a new one.",
                    );
                });
            }
            Some(Tab::Settings) => {
                self.terminal_focused = false;
                let input = SettingsInput {
                    font_size: self.cfg.font_size,
                    font_range: (MIN_FONT, MAX_FONT),
                    font_default: DEFAULT_FONT,
                    theme: self.cfg.theme,
                    style: self.cfg.style,
                    density: self.cfg.density,
                    native_decorations: self.cfg.native_decorations,
                    omp_found: self.omp.is_some(),
                    omp_version: self.update.version.as_deref(),
                    check_updates: self.cfg.check_updates,
                    update: &self.update.state,
                };
                let actions = egui::CentralPanel::no_frame()
                    .show(ui, |ui| settings_page::show(ui, &mut self.settings, &input))
                    .inner;
                for a in actions {
                    match a {
                        SettingsAction::FontSize(s) => self.set_font_size(s),
                        SettingsAction::Theme(t) => self.set_theme(t),
                        SettingsAction::Style(v) => self.set_style(v),
                        SettingsAction::Density(v) => self.set_density(v),
                        SettingsAction::NativeDecorations(v) => self.set_native_decorations(v),
                        SettingsAction::SetCheckUpdates(v) => {
                            self.cfg.check_updates = v;
                            self.dirty = true;
                        }
                        SettingsAction::CheckUpdates => self.check_updates_manually(),
                        SettingsAction::LoadCatalog => self.load_catalog(),
                        SettingsAction::WriteSetting { setting, value } => {
                            self.write_setting(*setting, value);
                        }
                        SettingsAction::ResetSetting { key } => self.reset_setting(key),
                        SettingsAction::CopyKey(key) => self.copy_text(&key, "setting key"),
                        SettingsAction::CopyDetails(text) => {
                            self.copy_text(&text, "error details");
                        }
                    }
                }
            }
            Some(Tab::Terminal(_)) => self.show_terminal(ui),
        }
    }

    fn show_terminal(&mut self, ui: &mut egui::Ui) {
        let mut open_url = None;
        let mut copy_link = None;
        egui::CentralPanel::no_frame().show(ui, |ui| {
            let Some(Tab::Terminal(tab)) = self.tabs.get_mut(self.active) else {
                return;
            };
            // Clicking app chrome (status bar, tabs, menus, dialogs) must not toggle the terminal's
            // focus: each toggle sends `ESC[O`/`ESC[I` to omp, which merges with an injected lone
            // `ESC` (Stop) or lands inside an injected sequence. Only the terminal widget ignores
            // "click elsewhere"; text fields keep egui's rule. Keys are held while a dialog or
            // menu is open (see `drive_injectors`), so the terminal keeping focus is harmless.
            let ctx = ui.ctx().clone();
            let surrender = ctx.options(|o| o.input_options.surrender_focus_on);
            ctx.options_mut(|o| o.input_options.surrender_focus_on = egui::SurrenderFocusOn::Never);
            let out = TerminalView::new(&mut tab.term)
                .font_size(self.cfg.font_size)
                .show(ui);
            ctx.options_mut(|o| o.input_options.surrender_focus_on = surrender);
            if self.focus_pending {
                out.response.request_focus();
                self.focus_pending = false;
                ui.ctx().request_repaint();
            }
            self.terminal_focused = out.response.has_focus();
            tab.bell = false;
            open_url = out.open_url;
            match terminal_menu(&out.response, &tab.term) {
                Some(TermMenu::Copy) => {
                    tab.term.copy_selection();
                }
                Some(TermMenu::Paste) => tab.term.paste_clipboard(),
                Some(TermMenu::OpenLink(url)) => open_url = Some(url),
                Some(TermMenu::CopyLink(url)) => copy_link = Some(url),
                None => {}
            }
        });
        if let Some(url) = open_url {
            self.open_external(url);
        }
        if let Some(url) = copy_link {
            self.copy_text(&url, "link");
        }
    }

    /// A confirmation or form dialog is open.
    fn modal_open(&self) -> bool {
        self.confirm.is_some() || self.dialog.is_some() || self.folder_dialog.is_some()
    }

    fn show_confirm(&mut self, ctx: &egui::Context) {
        let Some((question, _)) = &self.confirm else {
            return;
        };
        let Some(answer) = confirm::show(ctx, "app", question) else {
            return;
        };
        let Some((_, pending)) = self.confirm.take() else {
            return;
        };
        self.focus_pending = true;
        if let PendingConfirm::ClearDraft { uid } = pending {
            self.answer_draft(uid, answer);
            return;
        }
        if !answer {
            return;
        }
        match pending {
            PendingConfirm::StopSession { tmux_name, label } => {
                self.kill_tmux_session(tmux_name, label);
            }
            PendingConfirm::SwitchChannel(kind) => self.open_update_tab(kind),
            PendingConfirm::MoveSession { entry, dest } => self.execute_move(entry, dest),
            PendingConfirm::ClearDraft { .. } => {}
            PendingConfirm::KillAgent {
                id,
                uid,
                key,
                session,
            } => self.confirm_kill_agent(id, uid, &key, session.as_deref()),
            PendingConfirm::RestartLive { uid, session } => {
                self.execute_restart_live(uid, &session);
            }
            PendingConfirm::RestartSession(target) => self.execute_restart_session(target),
            PendingConfirm::DeleteSession(entry) => self.execute_delete(entry),
        }
    }

    // ---------------------------------------------------------------- housekeeping

    fn track_geometry(&mut self, ctx: &egui::Context) {
        let (inner, outer, size, maximized) = ctx.input(|i| {
            let v = i.viewport();
            (
                v.inner_rect,
                v.outer_rect,
                i.viewport_rect().size(),
                v.maximized.unwrap_or(false),
            )
        });
        let g = update_geometry(self.cfg.window, inner, outer, size, maximized);
        if self.cfg.window != Some(g) {
            self.cfg.window = Some(g);
            self.dirty = true;
        }
    }

    fn snapshot_config(&self) -> AppConfig {
        let mut cfg = self.cfg.clone();
        // Before restore ran (or without tmux) the stored tab list is left untouched.
        if self.tmux.is_some() && self.pending_restore.is_none() {
            // The settings page is never persisted; `active_tab` indexes the persisted list.
            let mut active_tab = None;
            let mut open_tabs = Vec::new();
            for (i, tab) in self.tabs.iter().enumerate() {
                let Tab::Terminal(t) = tab else {
                    continue;
                };
                let Some(tmux_name) = t.tmux_name.clone() else {
                    continue;
                };
                if i == self.active {
                    active_tab = Some(open_tabs.len());
                }
                open_tabs.push(TabState {
                    tmux_name,
                    cwd: t.cwd.clone(),
                    session_path: t.session_path.clone(),
                    title: t.title(&self.labels).to_owned(),
                });
            }
            let active_tab = active_tab.or_else(|| (!open_tabs.is_empty()).then_some(0));
            match &self.launch_folder {
                Some(dir) if open_tabs.is_empty() => {
                    cfg.folder_tabs.remove(dir);
                }
                Some(dir) => {
                    cfg.folder_tabs.insert(
                        dir.clone(),
                        FolderTabs {
                            open_tabs,
                            active_tab,
                        },
                    );
                }
                None => {
                    cfg.open_tabs = open_tabs;
                    cfg.active_tab = active_tab;
                }
            }
        }
        cfg
    }

    fn housekeeping(&mut self, ctx: &egui::Context) {
        self.track_geometry(ctx);

        let tab_title = self
            .tabs
            .get(self.active)
            .map(|t| titlebar::strip_omp_prefix(t.title(&self.labels)));
        let title = match &self.scope {
            Some(dir) => titlebar::window_title(tab_title, Some(&folder_scope::folder_name(dir))),
            None => titlebar::window_title(tab_title, None),
        };
        // omp animates a spinner in its title (~12/s); every window-title change costs a viewport
        // command and a repaint, so changes are applied at most once per second.
        if title != self.last_title {
            let since = self.last_title_at.elapsed();
            if since >= WINDOW_TITLE_EVERY {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
                self.last_title = title;
                self.last_title_at = Instant::now();
            } else {
                // The deferred update needs a frame of its own once the throttle opens.
                ctx.request_repaint_after(WINDOW_TITLE_EVERY - since);
            }
        }

        clipboard_bridge::forward_copies(ctx, &self.clipboard);

        if self.dirty {
            if self.last_save.elapsed() >= SAVE_DEBOUNCE {
                self.writer.save(self.snapshot_config());
                self.dirty = false;
                self.last_save = Instant::now();
            } else {
                ctx.request_repaint_after(SAVE_DEBOUNCE - self.last_save.elapsed());
            }
        }

        self.drive_screenshot(ctx);
    }

    fn drive_screenshot(&mut self, ctx: &egui::Context) {
        let Some(plan) = &mut self.shot else {
            return;
        };
        match &plan.state {
            ShotState::Waiting => {
                let elapsed = self.started.elapsed();
                if elapsed < plan.after {
                    ctx.request_repaint_after(plan.after - elapsed);
                    return;
                }
                let done = Arc::new(AtomicBool::new(false));
                let (flag, path, repaint) = (Arc::clone(&done), plan.path.clone(), ctx.clone());
                ctx.request_screenshot(move |img| {
                    if let Err(e) = write_png(&path, &img) {
                        log::error!("screenshot {}: {e}", path.display());
                    }
                    flag.store(true, Ordering::SeqCst);
                    repaint.request_repaint();
                });
                plan.state = ShotState::Requested(done);
            }
            ShotState::Requested(done) => {
                if done.load(Ordering::SeqCst) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else {
                    ctx.request_repaint_after(Duration::from_millis(100));
                }
            }
        }
    }
}

// -------------------------------------------------------------------- omp CLI features

impl App {
    /// Run `job` with the detected omp on a worker thread; its event comes back via the channel.
    fn spawn_omp(&self, job: impl FnOnce(Omp) -> OmpEvent + Send + 'static) {
        let Some(omp) = self.omp.clone() else {
            return;
        };
        omp_worker::spawn(&self.ctx, &self.events_tx, move || AppEvent::Omp(job(omp)));
    }

    fn start_update_check(&mut self) {
        if self.omp.is_none() || self.update.is_checking() {
            return;
        }
        self.update.begin(Instant::now());
        self.spawn_omp(|omp| {
            OmpEvent::UpdateChecked(omp.check_update().map_err(|e| e.to_string()))
        });
    }

    fn check_updates_manually(&mut self) {
        if self.update.is_checking() {
            return;
        }
        self.check_manual = true;
        self.start_update_check();
    }

    /// Startup check and the 6 h schedule (only while automatic checks are enabled).
    fn tick_updates(&mut self, ctx: &egui::Context) {
        if self.omp.is_none() || !self.cfg.check_updates {
            return;
        }
        if self.update.due(Instant::now()) {
            self.start_update_check();
        }
        ctx.request_repaint_after(Duration::from_secs(60));
    }

    fn confirm_channel_switch(&mut self, kind: UpdateKind) {
        let (title, body, label) = match kind {
            UpdateKind::Canary => (
                "Switch to the canary channel?",
                "omp will switch to the canary channel and install the latest canary build now. Canary builds are pre-releases and may be unstable; you can switch back to stable at any time.",
                "Switch to canary",
            ),
            _ => (
                "Switch back to the stable channel?",
                "omp will switch to the stable channel and install the latest stable build now.",
                "Switch to stable",
            ),
        };
        self.confirm = Some((
            Confirm {
                title: title.to_owned(),
                body: body.to_owned(),
                confirm_label: label.to_owned(),
                danger: false,
            },
            PendingConfirm::SwitchChannel(kind),
        ));
    }

    /// When an update tab's process has exited: re-check, and report success or failure.
    fn check_update_tabs(&mut self) {
        let mut finished = Vec::new();
        for tab in self.tabs.iter_mut().filter_map(Tab::term_mut) {
            if let TabRole::Update(kind) = tab.role
                && !tab.exit_handled
                && tab.term.is_exited()
            {
                tab.exit_handled = true;
                finished.push((kind, tab.term.exit_code()));
            }
        }
        for (kind, code) in finished {
            match code {
                Some(0) => self.update.recheck.push_back(Some(kind)),
                other => {
                    let code = other.map_or("signal".to_owned(), |c| c.to_string());
                    self.toasts.error(
                        &self.ctx,
                        format!(
                            "{} failed (exit {code}) — see the tab for details",
                            crate::update::tab_title(kind)
                        ),
                    );
                    self.update.recheck.push_back(None);
                }
            }
        }
    }

    /// Start the next owed post-update check once no other check is in flight; the announcement
    /// belongs to that new check, never to an older one that was still running.
    fn pump_update_queue(&mut self) {
        if self.omp.is_some() && self.update.start_queued(Instant::now()) {
            self.check_manual = false;
            self.spawn_omp(|omp| {
                OmpEvent::UpdateChecked(omp.check_update().map_err(|e| e.to_string()))
            });
        }
    }

    fn on_omp_event(&mut self, ev: OmpEvent) {
        match ev {
            OmpEvent::Version(Ok(v)) => {
                if self.update.version.is_none() {
                    self.update.version = Some(v);
                }
            }
            OmpEvent::Version(Err(e)) => log::warn!("omp --version failed: {e}"),
            OmpEvent::UpdateChecked(result) => self.on_update_checked(result),
            OmpEvent::Models(result) => {
                self.models.finish(result);
                self.ctx.request_repaint();
            }
            OmpEvent::Catalog(result) => {
                if let Ok(catalog) = &result {
                    self.omp_defaults = defaults_from_catalog(catalog);
                }
                let env = std::env::var("PI_CODING_AGENT_DIR").ok();
                let dir = config_dir_display(env.as_deref(), self.home.as_deref());
                self.settings.catalog_loaded(result, dir);
            }
            OmpEvent::SettingDone { key, outcome } => {
                let reset = matches!(outcome, Ok(None));
                let err = outcome.as_ref().err().cloned();
                let label = self.settings.write_done(&key, outcome);
                match err {
                    None if reset => self
                        .toasts
                        .success(&self.ctx, format!("Reset “{label}” to its default")),
                    None => self.toasts.success(&self.ctx, format!("Saved “{label}”")),
                    Some(e) => self.toasts.error(
                        &self.ctx,
                        format!("{label}: {}", crate::summary::strip_context(&e)),
                    ),
                }
            }
            OmpEvent::MoveDone {
                src,
                dest,
                title,
                tab,
                result,
            } => self.on_move_done(src, dest, title, tab, result),
            OmpEvent::DeleteDone {
                path,
                title,
                result,
            } => self.on_delete_done(&path, &title, result),
            OmpEvent::FolderPicked(path) => {
                if let Some(dlg) = &mut self.folder_dialog {
                    dlg.browsing = false;
                    if let Some(p) = path {
                        dlg.set_text(p.to_string_lossy().into_owned(), self.home.as_deref());
                    }
                }
                self.dispatch_folder_check();
            }
            OmpEvent::ScopeChosen {
                generation,
                typed,
                result,
            } => self.on_scope_chosen(generation, typed, result),
            OmpEvent::FolderChecked { generation, result } => {
                if let Some(dlg) = &mut self.folder_dialog {
                    dlg.apply_check(generation, result);
                }
            }
        }
    }

    fn on_update_checked(&mut self, result: Result<UpdateStatus, String>) {
        let manual = std::mem::take(&mut self.check_manual);
        let announce = self.update.announce.take();
        self.update.finish(result.clone());
        match (result, announce) {
            (Ok(_), Some(UpdateKind::Plugins)) => {
                self.toasts.success(&self.ctx, "omp plugins updated");
            }
            (Ok(status), Some(_)) => self.toasts.success(
                &self.ctx,
                format!(
                    "omp updated to {} — running sessions keep the old version until restarted",
                    status.current
                ),
            ),
            (Err(_), Some(_)) => self.toasts.success(
                &self.ctx,
                "omp update finished — running sessions keep the old version until restarted",
            ),
            (Ok(status), None) if manual => match status.latest {
                Some(latest) => self
                    .toasts
                    .info(&self.ctx, format!("omp {latest} is available")),
                None => self
                    .toasts
                    .info(&self.ctx, format!("omp {} is up to date", status.current)),
            },
            (Err(e), None) if manual => self.toasts.error(
                &self.ctx,
                format!("Update check failed: {}", crate::summary::strip_context(&e)),
            ),
            _ => {}
        }
    }

    fn load_catalog(&self) {
        self.spawn_omp(|omp| OmpEvent::Catalog(omp.load_settings().map_err(|e| e.to_string())));
    }

    fn write_setting(&self, setting: Setting, value: Value) {
        self.spawn_omp(move |omp| OmpEvent::SettingDone {
            key: setting.key.clone(),
            outcome: omp
                .set_setting(&setting, &value)
                .map(Some)
                .map_err(|e| e.to_string()),
        });
    }

    fn reset_setting(&self, key: String) {
        self.spawn_omp(move |omp| OmpEvent::SettingDone {
            outcome: omp
                .reset_setting(&key)
                .map(|()| None)
                .map_err(|e| e.to_string()),
            key,
        });
    }

    // ------------------------------------------------------------ copy / move

    fn start_transfer(&mut self, kind: TransferKind, entry: SessionEntry, dest: Option<PathBuf>) {
        if self.omp.is_none() || self.moving.contains(&entry.path) {
            return;
        }
        match dest {
            Some(dest) => self.begin_transfer(kind, entry, dest),
            None => self.folder_dialog = Some(FolderDialog::new(kind, entry)),
        }
    }

    fn begin_transfer(&mut self, kind: TransferKind, entry: SessionEntry, dest: PathBuf) {
        match kind {
            TransferKind::Copy => self.copy_session(&entry, &dest),
            TransferKind::Move => self.request_move(entry, dest),
        }
    }

    /// Copy = a new tab running `omp --fork <src> --cwd <dest>`. The tab starts "copy pending":
    /// success is only announced once the new session file is found (see `drive_copies`).
    fn copy_session(&mut self, entry: &SessionEntry, dest: &Path) {
        let argv = self.with_omp_program(fork_argv(&entry.path, dest));
        let name = self
            .tmux
            .is_some()
            .then(|| new_session_name(SystemTime::now()));
        let started_at = SystemTime::now();
        self.spawn_tab_post(SpawnReq {
            tmux_name: name,
            cwd: dest.to_path_buf(),
            session_path: None,
            title: entry.label().to_owned(),
            argv,
            role: TabRole::Session,
            post: Post::Copy {
                src_id: entry.id.clone(),
                label: entry.label().to_owned(),
                dest: dest.to_path_buf(),
                started_at,
            },
            focus: Focus::Claim,
        });
    }

    /// Per frame: poll for the new file of every pending copy (on a worker, about once a second)
    /// and fail copies whose process exited first or that took too long.
    fn drive_copies(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let mut errors = Vec::new();
        let mut polls = Vec::new();
        let mut pending = false;
        for tab in self.tabs.iter_mut().filter_map(Tab::term_mut) {
            let Some(copy) = &mut tab.copy else {
                continue;
            };
            if tab.term.is_exited() {
                errors.push(match tab.term.exit_code() {
                    Some(code) => format!(
                        "Copy failed: omp exited (code {code}) before the copy was created — see the tab for its output"
                    ),
                    None => "Copy failed: omp ended before the copy was created — see the tab for its output".to_owned(),
                });
                tab.copy = None;
            } else if now.duration_since(copy.started) >= COPY_TIMEOUT {
                errors.push(
                    "Copy failed: no new session appeared within 2 minutes — see the tab for omp's output".to_owned(),
                );
                tab.copy = None;
            } else {
                pending = true;
                if !copy.polling && now >= copy.next_poll {
                    copy.polling = true;
                    copy.next_poll = now + COPY_POLL;
                    polls.push((copy.id, copy.src_id.clone(), copy.started_at));
                }
            }
        }
        for msg in errors {
            self.toasts.error(&self.ctx, msg);
        }
        for (copy_id, src_id, started_at) in polls {
            let root = self.root.clone();
            // omp stamps the file slightly before we noted the start time.
            let since = started_at
                .checked_sub(Duration::from_secs(2))
                .unwrap_or(started_at);
            omp_worker::spawn(&self.ctx, &self.events_tx, move || AppEvent::ForkFound {
                copy_id,
                path: find_fork(&root, &src_id, since),
            });
        }
        if pending {
            ctx.request_repaint_after(COPY_POLL / 2);
        }
    }

    fn on_fork_found(&mut self, copy_id: u64, path: Option<PathBuf>) {
        let Some(tab) = self
            .tabs
            .iter_mut()
            .filter_map(Tab::term_mut)
            .find(|t| t.copy.as_ref().is_some_and(|c| c.id == copy_id))
        else {
            return; // the copy already failed or its tab is gone
        };
        let Some(path) = path else {
            if let Some(copy) = &mut tab.copy {
                copy.polling = false;
            }
            return;
        };
        tab.copy = None;
        tab.session_path = Some(path);
        let dest = abbreviate_home(&tab.cwd);
        self.dirty = true;
        self.toasts.success(
            &self.ctx,
            format!("Copied to {dest} — the copy is a new session"),
        );
        self.refresh_sessions();
    }

    fn request_move(&mut self, entry: SessionEntry, dest: PathBuf) {
        let running = self.running_tmux_name(&entry).is_some()
            || self.tabs.iter().any(|t| {
                t.term().is_some_and(|t| {
                    t.session_path.as_deref() == Some(entry.path.as_path()) && !t.term.is_exited()
                })
            });
        let mut body = format!(
            "Move “{}” to {}? omp moves the session file and its artifacts; the original is removed from {}.",
            entry.label(),
            abbreviate_home(&dest),
            abbreviate_home(&entry.cwd),
        );
        if running {
            body.push_str(" It is running and will be stopped first.");
        }
        self.confirm = Some((
            Confirm {
                title: "Move session?".to_owned(),
                body,
                confirm_label: "Move".to_owned(),
                danger: running,
            },
            PendingConfirm::MoveSession { entry, dest },
        ));
    }

    /// Stop the session if it runs (kill every tmux session involved + close its tabs), then
    /// `omp session pack/unpack` on a worker, retrying while omp still sees the dying process.
    /// A tab that was open comes back afterwards: on the new path after success, on the original
    /// path (re-attached if its tmux session survived) after a failure.
    fn execute_move(&mut self, entry: SessionEntry, dest: PathBuf) {
        if self.omp.is_none() || !self.moving.insert(entry.path.clone()) {
            return;
        }
        let holder = self.running_tmux_name(&entry);
        // The monitor snapshot can lag behind a tab that was just opened: kill the tmux session
        // each matching tab recorded as well, not only the snapshot's holder.
        let mut kill_names: Vec<String> = holder.iter().cloned().collect();
        let mut tab_state: Option<ClosedTab> = None;
        let (mut slot, mut was_active) = (0, false);
        for i in (0..self.tabs.len()).rev() {
            let hit = self.tabs[i].term().is_some_and(|t| {
                t.session_path.as_deref() == Some(entry.path.as_path())
                    || (holder.is_some() && t.tmux_name == holder)
            });
            if !hit {
                continue;
            }
            if let Some(t) = self.tabs[i].term_mut() {
                match &t.tmux_name {
                    Some(name) => {
                        if !kill_names.contains(name) {
                            kill_names.push(name.clone());
                        }
                    }
                    None => t.term.kill(),
                }
                if tab_state.is_none() {
                    slot = i;
                    tab_state = Some(ClosedTab {
                        tmux_name: t.tmux_name.clone(),
                        cwd: t.cwd.clone(),
                        session_path: Some(entry.path.clone()),
                        title: entry.label().to_owned(),
                    });
                } else if i < slot {
                    slot -= 1; // a tab in front of the first one is closed too
                }
            }
            was_active |= i == self.active;
            self.close_tab(i, false);
        }
        // The reopened tab takes focus only if this was the active tab and the user has not
        // switched since.
        let refocus = was_active.then_some(self.activation_seq);
        let anchor = Anchor::at(&self.tab_uids(), slot);
        let title = entry.label().to_owned();
        self.toasts.info(
            &self.ctx,
            format!("Moving “{title}” to {}…", abbreviate_home(&dest)),
        );
        let tmux = self.tmux.clone();
        let (src, id) = (entry.path, entry.id);
        self.spawn_omp(move |omp| {
            if let Some(tmux) = &tmux {
                for name in &kill_names {
                    if let Err(e) = tmux.kill(name) {
                        log::warn!("kill {name} before move: {e}");
                    }
                }
            }
            let result = omp_worker::retry_while_live(
                omp_worker::MOVE_ATTEMPTS,
                omp_worker::MOVE_RETRY_DELAY,
                || omp.move_session(&src, &dest).map_err(|e| e.to_string()),
            );
            let tab = tab_state.map(|mut tab| {
                match &result {
                    Ok(outcome) => {
                        let new_id = match &outcome.kind {
                            MoveKind::Forked { new_id } => new_id,
                            _ => &id,
                        };
                        tab.tmux_name = tmux.is_some().then(|| session_name_for(new_id));
                        tab.cwd = dest.clone();
                        tab.session_path = Some(outcome.new_path.clone());
                    }
                    Err(_) => {
                        // Re-attach a tmux session that survived; else resume the original file
                        // under its canonical name.
                        let alive = tmux
                            .as_ref()
                            .and_then(|t| kill_names.iter().find(|n| t.has_session(n)).cloned());
                        tab.tmux_name =
                            alive.or_else(|| tmux.is_some().then(|| session_name_for(&id)));
                    }
                }
                Box::new(MovedTab {
                    tab,
                    anchor,
                    refocus,
                })
            });
            OmpEvent::MoveDone {
                src,
                dest,
                title,
                tab,
                result,
            }
        });
        self.running.poke();
    }

    fn request_delete(&mut self, entry: SessionEntry) {
        if self.omp.is_none() || self.moving.contains(&entry.path) {
            return;
        }
        let label = entry.label().to_owned();
        let running = self.running_tmux_name(&entry).is_some();
        let busy = self.tabs.iter().any(|t| match t {
            Tab::Terminal(t) => {
                !t.term.is_exited()
                    && t.session_path.as_deref() == Some(entry.path.as_path())
                    && Activity::from_title(t.term.title()) != Activity::Idle
            }
            _ => false,
        });
        let live = if running {
            " Its running omp process is stopped first."
        } else {
            ""
        };
        let warn = if busy {
            " omp is working; deleting stops the current turn."
        } else {
            ""
        };
        let size = std::fs::metadata(&entry.path)
            .map(|m| format!("\nSize: {}", human_size(m.len())))
            .unwrap_or_default();
        let project = abbreviate_home(&entry.cwd);
        self.confirm = Some((
            Confirm {
                title: "Delete session?".to_owned(),
                body: format!(
                    "“{label}”\nProject: {project}{size}\n\
                     Subagent transcripts and artifacts are deleted too. This cannot be undone.{live}{warn}"
                ),
                confirm_label: "Delete session".to_owned(),
                danger: true,
            },
            PendingConfirm::DeleteSession(entry),
        ));
    }

    /// Stop whatever runs the session, then let omp delete it on a worker.
    fn execute_delete(&mut self, entry: SessionEntry) {
        if self.omp.is_none() || !self.moving.insert(entry.path.clone()) {
            return;
        }
        let holder = self.running_tmux_name(&entry);
        let mut kill_names: Vec<String> = holder.iter().cloned().collect();
        for i in (0..self.tabs.len()).rev() {
            let hit = self.tabs[i].term().is_some_and(|t| {
                t.session_path.as_deref() == Some(entry.path.as_path())
                    || (holder.is_some() && t.tmux_name == holder)
            });
            if !hit {
                continue;
            }
            if let Some(t) = self.tabs[i].term_mut() {
                match &t.tmux_name {
                    Some(name) => {
                        if !kill_names.contains(name) {
                            kill_names.push(name.clone());
                        }
                    }
                    None => t.term.kill(),
                }
            }
            self.close_tab(i, false);
        }
        let path = entry.path.clone();
        self.closed
            .retain(|c| c.session_path.as_deref() != Some(path.as_path()));
        let title = entry.label().to_owned();
        let tmux = self.tmux.clone();
        self.spawn_omp(move |omp| {
            if let Some(tmux) = &tmux {
                for name in &kill_names {
                    if let Err(e) = tmux.kill(name) {
                        log::warn!("kill {name} before delete: {e}");
                    }
                }
            }
            // Each attempt spawns a headless omp (~0.8 s): few attempts, short delay (~5 s total).
            let mut tries = 1;
            let result = loop {
                match omp.delete_session(&path) {
                    // A just-killed omp needs a moment to release the session lease.
                    Err(deck_core::omp::DeleteError::Live) if tries < DELETE_ATTEMPTS => {
                        tries += 1;
                        std::thread::sleep(DELETE_RETRY_DELAY);
                    }
                    Err(deck_core::omp::DeleteError::Live) => {
                        break Err("it is open in another omp process".to_owned());
                    }
                    other => break other.map_err(|e| e.to_string()),
                }
            };
            OmpEvent::DeleteDone {
                path,
                title,
                result,
            }
        });
        self.running.poke();
        self.toasts
            .info(&self.ctx, format!("Deleting “{}”…", entry.label()));
    }

    fn on_delete_done(&mut self, path: &std::path::Path, title: &str, result: Result<(), String>) {
        self.moving.remove(path);
        self.refresh_sessions();
        match result {
            Ok(()) => self.toasts.success(&self.ctx, format!("Deleted “{title}”")),
            Err(e) => self.toasts.error(
                &self.ctx,
                format!(
                    "Could not delete “{title}”: {}",
                    crate::summary::strip_context(&e)
                ),
            ),
        }
    }

    fn on_move_done(
        &mut self,
        src: PathBuf,
        dest: PathBuf,
        title: String,
        tab: Option<Box<MovedTab>>,
        result: Result<MoveOutcome, String>,
    ) {
        self.moving.remove(&src);
        self.refresh_sessions();
        match result {
            Ok(outcome) => {
                let to = abbreviate_home(&dest);
                let text = match outcome.kind {
                    MoveKind::Forked { .. } => {
                        format!("Moved “{title}” to {to} — original kept, moved as a new session")
                    }
                    _ => format!("Moved “{title}” to {to}"),
                };
                self.toasts.success(&self.ctx, text);
            }
            Err(e) => self.toasts.error(
                &self.ctx,
                format!(
                    "Could not move “{title}”: {}",
                    crate::summary::strip_context(&e)
                ),
            ),
        }
        if let Some(MovedTab {
            tab: t,
            anchor,
            refocus,
        }) = tab.map(|b| *b)
        {
            let argv = tabs::omp_argv(t.session_path.as_deref());
            self.spawn_tab_post(SpawnReq {
                tmux_name: t.tmux_name,
                cwd: t.cwd,
                session_path: t.session_path,
                title: t.title,
                argv,
                role: TabRole::Session,
                post: Post::Anchor(anchor),
                focus: if refocus == Some(self.activation_seq) {
                    Focus::Claim
                } else {
                    Focus::Never
                },
            });
        }
    }

    fn show_folder_dialog(&mut self, ctx: &egui::Context) {
        let Some(dlg) = self.folder_dialog.as_mut() else {
            return;
        };
        match dlg.show(ctx, self.home.as_deref(), self.picker_available) {
            FolderOutcome::Open => {}
            FolderOutcome::Cancel => {
                self.folder_dialog = None;
                self.focus_pending = true;
            }
            FolderOutcome::Confirm(dest) => {
                if let Some(d) = self.folder_dialog.take() {
                    self.focus_pending = true;
                    self.begin_transfer(d.kind, d.src, dest);
                }
            }
            FolderOutcome::Browse => {
                dlg.browsing = true;
                let start = dlg.start_dir(self.home.as_deref());
                omp_worker::spawn(&self.ctx, &self.events_tx, move || {
                    AppEvent::Omp(OmpEvent::FolderPicked(
                        transfer::picker_command(&start, transfer::on_path)
                            .and_then(|p| transfer::run_picker(&p)),
                    ))
                });
            }
        }
        self.dispatch_folder_check();
    }

    /// Run the dialog's pending filesystem check (is it a directory? is it the source project?)
    /// on a worker; the verdict comes back as `FolderChecked`.
    fn dispatch_folder_check(&mut self) {
        let Some(request) = self
            .folder_dialog
            .as_mut()
            .and_then(FolderDialog::take_request)
        else {
            return;
        };
        omp_worker::spawn(&self.ctx, &self.events_tx, move || {
            let (generation, result) = request.run();
            AppEvent::Omp(OmpEvent::FolderChecked { generation, result })
        });
    }
}

impl App {
    /// One debug line per second with the frame rate and what requested the repaints
    /// (repaint-storm diagnosis: `RUST_LOG=omp_deck::frames=debug`).
    fn count_frame(&mut self, ctx: &egui::Context) {
        self.frame_count += 1;
        if log::log_enabled!(target: "omp_deck::frames", log::Level::Debug) {
            for c in ctx.repaint_causes() {
                *self
                    .repaint_causes
                    .entry(format!("{}:{} {}", c.file, c.line, c.reason))
                    .or_default() += 1;
            }
        }
        let elapsed = self.frame_mark.elapsed();
        if elapsed >= Duration::from_secs(1) {
            let mut causes: Vec<_> = self.repaint_causes.drain().collect();
            causes.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            causes.truncate(10);
            log::debug!(
                target: "omp_deck::frames",
                "{:.1} frames/s; top causes: {causes:?}",
                f64::from(self.frame_count) / elapsed.as_secs_f64()
            );
            self.frame_count = 0;
            self.frame_mark = Instant::now();
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        Palette::of(visuals.dark_mode).bg.to_normalized_gamma_f32()
    }

    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        if !self.terminal_focused {
            clipboard_bridge::rewrite_raw_input(raw, &self.clipboard);
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if !self.fonts_finalized {
            self.fonts_finalized = true;
            ui_fonts::finalize(&ctx);
        }
        self.count_frame(&ctx);
        self.handle_shortcuts(&ctx);
        self.drain_events(&ctx);

        let latest = self.running.latest();
        if let Some(snap) = &latest {
            self.restore_tabs(snap);
            self.sync_session_paths(snap);
            self.open_deferred();
        }
        let snap = latest.unwrap_or_default();
        self.refresh_sessions_cache();

        // Only the visible terminal repaints promptly on output; the others coalesce.
        for (i, tab) in self.tabs.iter().enumerate() {
            if let Tab::Terminal(t) = tab {
                t.term.set_visible(i == self.active);
            }
        }
        // Hidden tabs still need title/bell/exit tracking.
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if i != self.active
                && let Tab::Terminal(t) = tab
                && t.term.poll().bell
            {
                t.bell = true;
            }
        }

        // The terminal keeps keyboard focus: take it back whenever nothing else (a text field,
        // an open menu, a modal) legitimately holds it.
        if self.confirm.is_none()
            && self.folder_dialog.is_none()
            && self.dialog.is_none()
            && self.popup.is_none()
            && self.tree.is_none()
            && !self.usage_open
            && !egui::Popup::is_any_open(&ctx)
            && ctx.memory(|m| m.focused()).is_none()
        {
            self.focus_pending = true;
        }

        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        self.show_bar(ui, maximized);
        self.show_banners(ui);
        self.drive_status(&ctx);
        self.drive_usage(&ctx);
        self.drive_agents();
        self.show_status_bar(ui);
        self.show_sidebar(ui, &snap);
        self.show_agent_hub(ui);
        self.show_central(ui);
        self.show_agent_popup(&ctx);
        self.show_tree(&ctx);
        self.show_usage(&ctx);
        self.show_confirm(&ctx);
        self.show_action_dialog(&ctx);
        self.show_folder_dialog(&ctx);
        self.check_update_tabs();
        self.pump_update_queue();
        self.drive_copies(&ctx);
        self.tick_updates(&ctx);
        // Age chips tick over without any other event.
        ctx.request_repaint_after(AGE_REFRESH);
        match self.toasts.show(&ctx) {
            Some(ToastClick::Action(TOAST_QUIT)) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            Some(ToastClick::Details(text)) => self.copy_text(&text, "error details"),
            Some(ToastClick::Action(_)) | None => {}
        }
        if !self.cfg.native_decorations {
            titlebar::window_chrome(&ctx, maximized);
        }
        self.housekeeping(&ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let cfg = self.snapshot_config();
        self.writer.finish(cfg);
    }
}

/// Choice from the terminal's right-click menu.
enum TermMenu {
    Copy,
    Paste,
    OpenLink(String),
    CopyLink(String),
}

/// Right-click menu of the terminal: clipboard actions, plus link actions when a link (OSC 8 or
/// a plain URL) was under the click.
fn terminal_menu(resp: &egui::Response, term: &deck_term::Terminal) -> Option<TermMenu> {
    let mut pick = None;
    theme::context_menu(resp, 220.0, |ui| {
        let copy_enabled = term.has_selection();
        if menu_item(
            ui,
            ICON_COPY,
            "Copy",
            Some("Ctrl+Shift+C"),
            copy_enabled,
            false,
        )
        .clicked()
        {
            pick = Some(TermMenu::Copy);
            ui.close();
        }
        if menu_item(
            ui,
            ICON_CLIPBOARD_LIST,
            "Paste",
            Some("Ctrl+Shift+V"),
            true,
            false,
        )
        .clicked()
        {
            pick = Some(TermMenu::Paste);
            ui.close();
        }
        if let Some(link) = term.context_link() {
            ui.separator();
            let openable = is_openable_url(link);
            if menu_item(
                ui,
                ICON_EXTERNAL_LINK,
                "Open link",
                Some("Ctrl+Click"),
                openable,
                false,
            )
            .clicked()
            {
                pick = Some(TermMenu::OpenLink(link.to_owned()));
                ui.close();
            }
            if menu_item(ui, ICON_COPY, "Copy link", None, true, false).clicked() {
                pick = Some(TermMenu::CopyLink(link.to_owned()));
                ui.close();
            }
        }
    });
    pick
}

/// Only `http`, `https`, `mailto` (scheme matched case-insensitively) may reach xdg-open.
fn is_openable_url(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once(':') else {
        return false;
    };
    !rest.is_empty()
        && ["http", "https", "mailto"]
            .iter()
            .any(|s| scheme.eq_ignore_ascii_case(s))
}

/// Start `xdg-open <arg>` and reap it; blocks until the opener exits, so call off the UI thread.
/// A non-zero exit status is an error (xdg-open found no handler, the handler failed, …).
fn spawn_opener(arg: &std::ffi::OsStr) -> std::io::Result<()> {
    let mut child = Command::new("xdg-open")
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let status = child.wait()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("exited with {status}")))
    }
}

fn write_png(path: &Path, img: &egui::ColorImage) -> anyhow::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(BufWriter::new(file), img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(img.as_raw())?;
    Ok(())
}

/// Pure geometry update. `inner`/`outer` are absent on Wayland, where only the viewport
/// `size` is known; position then stays as previously stored.
fn update_geometry(
    prev: Option<WindowGeometry>,
    inner: Option<egui::Rect>,
    outer: Option<egui::Rect>,
    size: egui::Vec2,
    maximized: bool,
) -> WindowGeometry {
    let size = inner.map_or(size, |r| r.size());
    let mut g = prev.unwrap_or(WindowGeometry {
        x: None,
        y: None,
        width: size.x,
        height: size.y,
        maximized,
    });
    g.maximized = maximized;
    if !maximized {
        g.width = size.x;
        g.height = size.y;
        if let Some(o) = outer {
            g.x = Some(o.min.x);
            g.y = Some(o.min.y);
        }
    }
    g
}

/// Deleting retries only while a just-killed omp releases the session lease; every attempt
/// spawns a headless omp (~0.8 s), so this bounds the whole delete to about 5 s.
const DELETE_ATTEMPTS: u32 = 5;
const DELETE_RETRY_DELAY: Duration = Duration::from_millis(250);

/// `1536` → `1.5 KB` (binary steps, one decimal above bytes).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::{WindowGeometry, is_openable_url, update_geometry};

    #[test]
    fn geometry_falls_back_to_viewport_size_without_inner_rect() {
        // Wayland: no inner/outer rect, only the viewport size.
        let g = update_geometry(None, None, None, egui::vec2(900.0, 700.0), false);
        assert_eq!(
            g,
            WindowGeometry {
                x: None,
                y: None,
                width: 900.0,
                height: 700.0,
                maximized: false
            }
        );
        // Resize updates; maximized keeps the previous size.
        let g2 = update_geometry(Some(g), None, None, egui::vec2(1000.0, 600.0), false);
        assert_eq!((g2.width, g2.height), (1000.0, 600.0));
        let g3 = update_geometry(Some(g2), None, None, egui::vec2(1920.0, 1080.0), true);
        assert_eq!((g3.width, g3.height, g3.maximized), (1000.0, 600.0, true));
    }

    #[test]
    fn url_scheme_allowlist() {
        for ok in [
            "http://a.b",
            "https://a.b/x?y",
            "HTTPS://A.B",
            "mailto:a@b.c",
            "MailTo:a@b.c",
        ] {
            assert!(is_openable_url(ok), "{ok}");
        }
        for bad in [
            "",
            "file:///etc/passwd",
            "-o",
            "--help",
            "-http://x",
            "javascript:alert(1)",
            "ftp://x",
            "http",
            "http:",
            "httpx://x",
            " http://x",
            "/etc/passwd",
            "ssh://h",
            "data:text/html,x",
            "x-http://y",
        ] {
            assert!(!is_openable_url(bad), "{bad:?}");
        }
    }
}
