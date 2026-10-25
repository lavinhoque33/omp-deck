//! App-side glue of the session manager hub: inventory scan and statistics workers, the bulk
//! delete / copy / move / stop operations with their confirmations, and the hub's open / close.
//!
//! Rendering is [`crate::sessions_view`], the data rules are [`crate::sessions_model`]; this
//! module owns the threads and the cleanup shared with the sidebar's single-session actions.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf, absolute};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use deck_core::inventory::{self, InventoryEntry, SessionStats, StatsCache};
use deck_core::mux::{Mux, session_name_for};
use deck_core::omp::{DeleteError, DeleteTarget, MoveKind, MoveOutcome, Omp};
use deck_core::platform::TreeMemory;
use deck_core::sessions::{SessionEntry, abbreviate_home};
use egui::{Key, Modifiers};

use super::{App, AppEvent, PendingConfirm};
use crate::app::human_size;
use crate::backend::Backends;
use crate::confirm::Confirm;
use crate::omp_worker::{self, MovedTab};
use crate::running_model::{HubMode, RunModel, RunPoll};
use crate::sessions_model::{Live, Model, plural};
use crate::sessions_view::{self, OpProgress, SessionsAction, ViewInput, ViewState};
use crate::statusfmt::Activity;
use crate::tabs::{Anchor, ClosedTab, Tab};
use crate::transfer::{FolderDialog, Subject, TransferKind};
use crate::worktree_ui;

/// Statistics worker threads (each one parses whole transcripts: I/O and JSON bound).
const STATS_THREADS: usize = 4;
/// Results are forwarded to the UI in batches of this many, or at least this often.
const STATS_BATCH: usize = 8;
const STATS_FLUSH: Duration = Duration::from_millis(150);
/// How often the Running view re-reads the live sessions.
const RUN_POLL: Duration = Duration::from_secs(2);
/// How often the Sessions view refreshes its RAM column (only while something runs).
const RAM_POLL: Duration = Duration::from_secs(5);

/// What `prepare_move` captured about the tab(s) of a session that is about to be moved.
pub(crate) struct MovePrep {
    pub(super) entry: SessionEntry,
    pub(super) kill_names: Vec<String>,
    pub(super) tab: Option<ClosedTab>,
    pub(super) anchor: Anchor,
    pub(super) refocus: Option<u64>,
}

impl MovePrep {
    /// Kill what runs the session, move it (retrying while omp still sees the dying process)
    /// and work out the tab to bring back: on the new path after success, on the original one
    /// (re-attached if its session survived) after a failure. Blocking: run on a worker.
    pub(super) fn run(
        self,
        omp: &Omp,
        backends: &Backends,
        dest: &Path,
    ) -> (Result<MoveOutcome, String>, Option<Box<MovedTab>>) {
        kill_all(backends, &self.kill_names, "move");
        let result = omp_worker::retry_while_live(
            omp_worker::MOVE_ATTEMPTS,
            omp_worker::MOVE_RETRY_DELAY,
            || {
                omp.move_session(&self.entry.path, dest)
                    .map_err(|e| e.to_string())
            },
        );
        let persistent = !backends.is_empty();
        let MovePrep {
            entry,
            kill_names,
            tab,
            anchor,
            refocus,
        } = self;
        let tab = tab.map(|mut tab| {
            match &result {
                Ok(outcome) => {
                    let new_id = match &outcome.kind {
                        MoveKind::Forked { new_id } => new_id,
                        _ => &entry.id,
                    };
                    tab.mux_name = persistent.then(|| session_name_for(new_id));
                    tab.cwd = dest.to_path_buf();
                    tab.session_path = Some(outcome.new_path.clone());
                }
                Err(_) => {
                    // Re-attach a session that survived; else resume the original file under
                    // its canonical name.
                    let alive = kill_names.iter().find(|n| backends.has_session(n)).cloned();
                    tab.mux_name =
                        alive.or_else(|| persistent.then(|| session_name_for(&entry.id)));
                }
            }
            Box::new(MovedTab {
                tab,
                anchor,
                refocus,
            })
        });
        (result, tab)
    }
}

/// Kill every backend session in `names` (best effort, logged). Blocking.
pub(super) fn kill_all(backends: &Backends, names: &[String], why: &str) {
    for name in names {
        if let Err(e) = backends.kill(name) {
            log::warn!("kill {name} before {why}: {e}");
        }
    }
}

/// User-facing text of a failed delete.
pub(super) fn delete_error_text(e: &DeleteError) -> String {
    match e {
        DeleteError::Live => "it is open in another omp process".to_owned(),
        other => other.to_string(),
    }
}

fn session_entry(e: &InventoryEntry) -> SessionEntry {
    SessionEntry {
        id: e.id.clone(),
        path: e.path.clone(),
        cwd: e.cwd.clone(),
        title: e.title.clone(),
        first_prompt: e.first_prompt.clone(),
        modified: e.modified,
    }
}

/// The folder every session shares (`None` for none or a mix).
fn common_cwd<'a>(mut cwds: impl Iterator<Item = &'a Path>) -> Option<PathBuf> {
    let first = cwds.next()?;
    cwds.all(|c| c == first).then(|| first.to_path_buf())
}

/// The running-sessions worker: at most one poll in flight.
#[derive(Default)]
struct RunPolling {
    in_flight: bool,
    last: Option<Instant>,
}

// ------------------------------------------------------------------- state

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpKind {
    Delete,
    Copy,
    Move,
    Stop,
}

/// A bulk operation in flight.
struct Op {
    id: u64,
    kind: OpKind,
    progress: OpProgress,
    ok: usize,
    failed: usize,
    /// Moves that kept the original and landed as a new session.
    forked: usize,
    /// Copies left out because the session already lives in the destination.
    skipped: usize,
    /// Bytes of the sessions deleted so far.
    freed: u64,
    sizes: HashMap<PathBuf, u64>,
    dest: Option<PathBuf>,
    first_error: Option<String>,
}

pub(super) struct SessionHub {
    pub(super) open: bool,
    model: Model,
    view: ViewState,
    /// Parsed statistics; lives for the process so a reopen is instant for unchanged files.
    cache: Arc<Mutex<StatsCache>>,
    /// Id of the latest scan; older answers (scan or statistics) are dropped.
    generation: u64,
    scanning: bool,
    loaded: bool,
    stats_cancel: Option<Arc<AtomicBool>>,
    op: Option<Op>,
    next_op: u64,
    labels_gen: Option<u64>,
    mode: HubMode,
    run: RunModel,
    run_poll: RunPolling,
}

impl Default for SessionHub {
    fn default() -> Self {
        SessionHub {
            open: false,
            model: Model::default(),
            view: ViewState::default(),
            cache: Arc::new(Mutex::new(StatsCache::new())),
            generation: 0,
            scanning: false,
            loaded: false,
            stats_cancel: None,
            op: None,
            next_op: 0,
            labels_gen: None,
            mode: HubMode::default(),
            run: RunModel::default(),
            run_poll: RunPolling::default(),
        }
    }
}

/// What a worker reports to the hub.
pub(super) enum SessionsEvent {
    Scanned {
        generation: u64,
        entries: Vec<InventoryEntry>,
    },
    Stats {
        generation: u64,
        batch: Vec<(PathBuf, Result<SessionStats, String>)>,
    },
    /// One session of a bulk operation finished.
    Step {
        op: u64,
        path: PathBuf,
        outcome: Result<Done, String>,
        /// A moved session's tab to bring back.
        tab: Option<Box<MovedTab>>,
    },
    OpFinished {
        op: u64,
    },
    /// The running-sessions worker's answer.
    Running(Box<RunPoll>),
    /// A kill from the Running view finished; `failed` = (session name, why).
    Killed {
        total: usize,
        failed: Vec<(String, String)>,
    },
}

pub(super) enum Done {
    Deleted,
    Copied,
    Moved(MoveKind),
    Stopped,
}

// -------------------------------------------------------------------- impl

impl App {
    /// Ctrl+Shift+S.
    pub(super) fn sessions_shortcut(&self, ctx: &egui::Context) -> bool {
        ctx.input_mut(|i| i.consume_key(Modifiers::COMMAND | Modifiers::SHIFT, Key::S))
    }

    pub(super) fn toggle_sessions(&mut self) {
        if self.sessions.open {
            self.close_sessions();
        } else {
            self.open_sessions(None);
        }
    }

    /// Open the hub (scoped to `scope` when given) and scan the store.
    pub(super) fn open_sessions(&mut self, scope: Option<PathBuf>) {
        let hub = &mut self.sessions;
        hub.open = true;
        hub.view.release_focus = true;
        hub.model.filter = Default::default();
        hub.model.sel.clear();
        hub.model.detail = None;
        hub.model.scope = scope;
        hub.model.mark_view_dirty();
        hub.loaded = !hub.model.rows().is_empty();
        hub.mode = HubMode::Sessions;
        hub.run.reset();
        hub.run_poll.last = None;
        self.start_scan();
    }

    pub(super) fn close_sessions(&mut self) {
        let hub = &mut self.sessions;
        hub.open = false;
        hub.generation += 1;
        hub.scanning = false;
        if let Some(c) = hub.stats_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        self.focus_pending = true;
    }

    /// A worker walks the store; its answer starts the statistics worker.
    fn start_scan(&mut self) {
        if !self.sessions.open {
            return;
        }
        self.sessions.generation += 1;
        self.sessions.scanning = true;
        if let Some(c) = self.sessions.stats_cancel.take() {
            c.store(true, Ordering::Relaxed);
        }
        let generation = self.sessions.generation;
        let root = self.root.clone();
        let ok = self.spawn_worker("deck-inventory", move |tx, ctx| {
            let entries = inventory::scan(&root);
            send(
                tx,
                ctx,
                SessionsEvent::Scanned {
                    generation,
                    entries,
                },
            );
        });
        if !ok {
            self.sessions.scanning = false;
            self.sessions.loaded = true;
        }
    }

    /// Run `job` on a named thread with the event channel; false if the thread did not start.
    fn spawn_worker(
        &self,
        name: &str,
        job: impl FnOnce(&Sender<AppEvent>, &egui::Context) + Send + 'static,
    ) -> bool {
        let (tx, ctx) = (self.events_tx.clone(), self.ctx.clone());
        match std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || job(&tx, &ctx))
        {
            Ok(_) => true,
            Err(e) => {
                log::warn!("cannot start {name}: {e}");
                false
            }
        }
    }

    pub(super) fn on_sessions_event(&mut self, ev: SessionsEvent) {
        match ev {
            SessionsEvent::Scanned {
                generation,
                entries,
            } => {
                if !self.sessions.open || generation != self.sessions.generation {
                    return;
                }
                self.sessions.scanning = false;
                self.sessions.loaded = true;
                self.start_stats(generation, entries.clone());
                self.sessions.model.set_entries(entries);
                self.resolve_running();
                self.refresh_ram();
            }
            SessionsEvent::Stats { generation, batch } => {
                if generation != self.sessions.generation {
                    return;
                }
                for (path, result) in batch {
                    self.sessions.model.set_stats(&path, result);
                }
            }
            SessionsEvent::Step {
                op,
                path,
                outcome,
                tab,
            } => self.on_step(op, &path, outcome, tab),
            SessionsEvent::OpFinished { op } => self.on_op_finished(op),
            SessionsEvent::Running(poll) => {
                self.sessions.run_poll.in_flight = false;
                self.sessions.run.set_poll(*poll);
                self.resolve_running();
                self.refresh_ram();
            }
            SessionsEvent::Killed { total, failed } => self.on_killed(total, &failed),
        }
    }

    /// Parse the transcripts of `entries` (newest first) on a worker, streaming batches.
    fn start_stats(&mut self, generation: u64, entries: Vec<InventoryEntry>) {
        let cancel = Arc::new(AtomicBool::new(false));
        self.sessions.stats_cancel = Some(Arc::clone(&cancel));
        let cache = Arc::clone(&self.sessions.cache);
        self.spawn_worker("deck-session-stats", move |tx, ctx| {
            let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
            cache.prune(&entries);
            let mut batch = Vec::with_capacity(STATS_BATCH);
            let mut last = Instant::now();
            cache.compute_many(&entries, STATS_THREADS, &cancel, |entry, result| {
                batch.push((
                    entry.path.clone(),
                    result.cloned().map_err(|e| e.to_string()),
                ));
                if batch.len() >= STATS_BATCH || last.elapsed() >= STATS_FLUSH {
                    let b = std::mem::take(&mut batch);
                    send(
                        tx,
                        ctx,
                        SessionsEvent::Stats {
                            generation,
                            batch: b,
                        },
                    );
                    last = Instant::now();
                }
            });
            if !batch.is_empty() {
                send(tx, ctx, SessionsEvent::Stats { generation, batch });
            }
        });
    }

    /// Per frame while open: tell the model which sessions run / have tabs and what the
    /// worktree projects are called.
    fn sync_hub_inputs(&mut self) {
        let generation = self.watcher.generation();
        if self.sessions.labels_gen != Some(generation) {
            self.sessions.labels_gen = Some(generation);
            let labels: HashMap<PathBuf, String> = self
                .watcher
                .latest()
                .map(|groups| {
                    groups
                        .iter()
                        .filter_map(|g| {
                            g.worktree
                                .as_ref()
                                .map(|w| (g.cwd.clone(), worktree_ui::worktree_label(w)))
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.sessions.model.set_labels(labels);
            self.resolve_running();
        }
        let snap = self.running.latest();
        let mut live: HashMap<PathBuf, Live> = HashMap::new();
        if let Some(snap) = &snap {
            for r in self.sessions.model.rows() {
                if snap.is_running(&r.entry.id, &r.entry.path) {
                    live.entry(r.entry.path.clone()).or_default().running = true;
                }
            }
        }
        for t in self.tabs.iter().filter_map(Tab::term) {
            if t.term.is_exited() {
                continue;
            }
            if let Some(p) = &t.session_path {
                let l = live.entry(p.clone()).or_default();
                l.in_tab = true;
                l.busy |= Activity::from_title(t.term.title()) != Activity::Idle;
            }
        }
        self.sessions.model.set_live(live);
        let tabs = &self.tabs;
        self.sessions.run.set_tabs(|r| {
            tabs.iter()
                .filter_map(Tab::term)
                .find(|t| {
                    !t.term.is_exited()
                        && (t.mux_name.as_deref() == Some(r.name())
                            || t.session_path
                                .as_deref()
                                .is_some_and(|p| r.raw.files.iter().any(|f| f == p)))
                })
                .map(|t| Activity::from_title(t.term.title()))
        });
    }

    /// Per frame: draw the hub while it is open and run what it asks for.
    pub(super) fn show_sessions(&mut self, ctx: &egui::Context) {
        if !self.sessions.open {
            return;
        }
        self.sync_hub_inputs();
        self.drive_run_poll(ctx);
        let blocked = self.confirm.is_some()
            || self.folder_dialog.is_some()
            || self.dialog.is_some()
            || self.wt.dialog.is_some()
            || self.palette.is_some();
        let (omp, can_stop) = (self.omp.is_some(), !self.backends.is_empty());
        let hub = &mut self.sessions;
        let input = ViewInput {
            scanning: hub.scanning,
            loaded: hub.loaded,
            mode: hub.mode,
            op: hub.op.as_ref().map(|o| &o.progress),
            blocked,
            now: SystemTime::now(),
            omp,
            can_stop,
        };
        let actions = sessions_view::show(ctx, &mut hub.model, &mut hub.run, &mut hub.view, &input);
        // Ages tick over without any other event.
        ctx.request_repaint_after(Duration::from_secs(30));
        for a in actions {
            self.apply_sessions_action(a);
        }
    }

    // ------------------------------------------------------------ running

    /// Poll the live sessions: every 2 s in the Running view; in the Sessions view once on open
    /// and every 5 s while something runs (the RAM column). Never while the hub is closed.
    fn drive_run_poll(&mut self, ctx: &egui::Context) {
        let hub = &mut self.sessions;
        if self.backends.is_empty() {
            if !hub.run.loaded() {
                hub.run.set_poll(RunPoll::default());
            }
            return;
        }
        if hub.run_poll.in_flight {
            return;
        }
        let every = match hub.mode {
            HubMode::Running => Some(RUN_POLL),
            HubMode::Sessions => hub
                .model
                .rows()
                .iter()
                .any(|r| r.live.running)
                .then_some(RAM_POLL),
        };
        let last = hub.run_poll.last;
        let wait = match (last, every) {
            (None, _) => Duration::ZERO,
            (Some(t), Some(e)) => e.saturating_sub(t.elapsed()),
            (Some(_), None) => return,
        };
        if !wait.is_zero() {
            ctx.request_repaint_after(wait);
            return;
        }
        hub.run_poll.in_flight = true;
        hub.run_poll.last = Some(Instant::now());
        let (backends, root) = (self.backends.clone(), self.root.path.clone());
        let ok = self.spawn_worker("deck-running", move |tx, ctx| {
            let mut poll = RunPoll::default();
            let mut seen: HashSet<String> = HashSet::new();
            for mux in backends.all() {
                // A name listed by several backends belongs to the first.
                poll.sessions.extend(
                    mux.running(&root)
                        .into_iter()
                        .filter(|s| seen.insert(s.name.clone())),
                );
                if matches!(mux, Mux::Tmux(_)) {
                    poll.tmux = true;
                    poll.tmux_server = mux.server_memory();
                }
            }
            send(tx, ctx, SessionsEvent::Running(Box::new(poll)));
        });
        if !ok {
            self.sessions.run_poll.in_flight = false;
        }
    }

    /// Titles and project labels of the running rows, from the inventory.
    fn resolve_running(&mut self) {
        let hub = &mut self.sessions;
        let model = &hub.model;
        hub.run.resolve(
            |p| {
                model
                    .row_by_path(p)
                    .map(|r| (r.label().to_owned(), r.entry.cwd.clone()))
            },
            |cwd| model.project_label(cwd),
        );
    }

    /// The Sessions table's RAM column: each session file gets the memory of the live session
    /// that holds it (the bigger one when several do); a live session that holds no file counts
    /// for the session whose canonical name it carries (a fresh omp, same rule as "running").
    fn refresh_ram(&mut self) {
        let hub = &mut self.sessions;
        let mut ram: HashMap<PathBuf, TreeMemory> = HashMap::new();
        let mut put = |path: &Path, m: TreeMemory| {
            ram.entry(path.to_path_buf())
                .and_modify(|e| {
                    if m.bytes > e.bytes {
                        *e = m;
                    }
                })
                .or_insert(m);
        };
        let mut fileless: HashMap<&str, TreeMemory> = HashMap::new();
        for r in hub.run.rows() {
            let Some(m) = r.raw.memory else {
                continue;
            };
            if r.raw.files.is_empty() {
                fileless.insert(r.name(), m);
            }
            for f in &r.raw.files {
                put(f, m);
            }
        }
        if !fileless.is_empty() {
            for row in hub.model.rows() {
                if let Some(m) = fileless.get(session_name_for(&row.entry.id).as_str()) {
                    put(&row.entry.path, *m);
                }
            }
        }
        hub.model.set_ram(ram);
    }

    fn on_killed(&mut self, total: usize, failed: &[(String, String)]) {
        self.sessions.run.clear_killing();
        if failed.is_empty() {
            self.toasts.success(
                &self.ctx,
                format!("Killed {}", plural(total, "session", "sessions")),
            );
        } else {
            let why = crate::summary::strip_context(&failed[0].1);
            self.toasts.error(
                &self.ctx,
                format!("Could not kill {} of {total} — {why}", failed.len()),
            );
        }
        // The list refreshes at once.
        self.sessions.run_poll.last = None;
        self.running.poke();
        self.ctx.request_repaint();
    }

    fn request_kill_running(&mut self, names: Vec<String>) {
        let hub = &self.sessions;
        if hub.run.any_killing() {
            return;
        }
        let present: Vec<String> = names
            .into_iter()
            .filter(|n| hub.run.rows().iter().any(|r| r.name() == n))
            .collect();
        let plan = hub.run.plan_for(&present);
        if plan.count == 0 {
            self.toasts
                .info(&self.ctx, "None of those sessions is running anymore.");
            return;
        }
        self.confirm = Some((
            Confirm {
                title: plan.title(),
                body: plan.body(),
                confirm_label: "Kill".to_owned(),
                danger: true,
            },
            PendingConfirm::KillRunning(present),
        ));
    }

    /// Kill exactly `names` (same backend path as the single "Stop session process": tmux
    /// detaches its clients first). Their tabs stay open and show the exit.
    pub(super) fn execute_kill_running(&mut self, names: Vec<String>) {
        if names.is_empty() || self.sessions.run.any_killing() {
            return;
        }
        self.sessions.run.set_killing(names.iter().cloned());
        let backends = self.backends.clone();
        let total = names.len();
        let ok = self.spawn_worker("deck-kill-running", move |tx, ctx| {
            let failed: Vec<(String, String)> = names
                .into_iter()
                .filter_map(|n| backends.stop(&n).err().map(|e| (n, e)))
                .collect();
            send(tx, ctx, SessionsEvent::Killed { total, failed });
        });
        if !ok {
            self.sessions.run.clear_killing();
            self.toasts
                .error(&self.ctx, "Could not start the kill (no thread)");
        }
    }

    fn apply_sessions_action(&mut self, action: SessionsAction) {
        match action {
            SessionsAction::Close => self.close_sessions(),
            SessionsAction::SetMode(mode) => {
                if self.sessions.mode != mode {
                    self.sessions.mode = mode;
                    // Fresh numbers at once on entering the Running view.
                    self.sessions.run_poll.last = None;
                }
            }
            SessionsAction::KillRunning(names) => self.request_kill_running(names),
            SessionsAction::Refresh => {
                self.start_scan();
                self.refresh_sessions();
            }
            SessionsAction::Open(path) => {
                if let Some(row) = self.sessions.model.row_by_path(&path) {
                    let entry = session_entry(&row.entry);
                    self.close_sessions();
                    self.open_session(&entry);
                }
            }
            SessionsAction::Delete(paths) => self.request_delete_sessions(&paths),
            SessionsAction::Transfer { kind, paths, dest } => match dest {
                Some(dest) => self.begin_hub_transfer(kind, paths, dest),
                None => {
                    let cwd = common_cwd(
                        paths
                            .iter()
                            .filter_map(|p| self.sessions.model.row_by_path(p))
                            .map(|r| r.entry.cwd.as_path()),
                    );
                    self.folder_dialog = Some(FolderDialog::many(kind, paths, cwd));
                }
            },
            SessionsAction::Stop(paths) => self.request_stop_sessions(paths),
            SessionsAction::CopyText { text, what } => self.copy_text(&text, what),
            SessionsAction::OpenFolder(dir) => self.open_folder(dir),
            SessionsAction::Reveal(file) => {
                if let Some(dir) = file.parent() {
                    self.open_folder(dir.to_path_buf());
                }
            }
        }
    }

    /// The folder dialog was confirmed: continue the single-session or the bulk path.
    pub(super) fn finish_folder_dialog(&mut self, d: FolderDialog, dest: PathBuf) {
        let kind = d.kind;
        match d.subject {
            Subject::One(entry) => self.begin_transfer(kind, entry, dest),
            Subject::Many { paths, .. } => self.begin_hub_transfer(kind, paths, dest),
        }
    }

    fn begin_hub_transfer(&mut self, kind: TransferKind, paths: Vec<PathBuf>, dest: PathBuf) {
        match kind {
            TransferKind::Copy => self.execute_copy_sessions(paths, dest),
            TransferKind::Move => self.request_move_sessions(paths, dest),
        }
    }

    // ------------------------------------------------------------ helpers

    /// The rows behind `paths` that exist and are not busy with another operation. Each
    /// refusal is recorded on its row.
    fn claim_rows(&mut self, paths: &[PathBuf]) -> Vec<InventoryEntry> {
        let mut out = Vec::new();
        for p in paths {
            let Some(row) = self.sessions.model.row_by_path(p) else {
                continue;
            };
            let entry = row.entry.clone();
            if self.moving.contains(p) {
                self.sessions
                    .model
                    .set_error(p, "Another operation is working on this session".to_owned());
                continue;
            }
            out.push(entry);
        }
        out
    }

    fn begin_op(&mut self, kind: OpKind, entries: &[InventoryEntry], dest: Option<PathBuf>) -> u64 {
        let hub = &mut self.sessions;
        hub.next_op += 1;
        let id = hub.next_op;
        hub.model.clear_errors();
        let verb = match kind {
            OpKind::Delete => "Deleting",
            OpKind::Copy => "Copying",
            OpKind::Move => "Moving",
            OpKind::Stop => "Killing",
        };
        hub.op = Some(Op {
            id,
            kind,
            progress: OpProgress {
                verb,
                done: 0,
                total: entries.len(),
            },
            ok: 0,
            failed: 0,
            forked: 0,
            skipped: 0,
            freed: 0,
            sizes: entries
                .iter()
                .map(|e| (e.path.clone(), e.total_bytes()))
                .collect(),
            dest,
            first_error: None,
        });
        for e in entries {
            self.moving.insert(e.path.clone());
        }
        id
    }

    /// Start `job` as the worker of the running op; if no thread can be created the op is
    /// abandoned with an error toast.
    fn run_op(
        &mut self,
        entries: &[InventoryEntry],
        job: impl FnOnce(&Sender<AppEvent>, &egui::Context) + Send + 'static,
    ) {
        if !self.spawn_worker("deck-session-op", job) {
            for e in entries {
                self.moving.remove(&e.path);
            }
            self.sessions.op = None;
            self.toasts
                .error(&self.ctx, "Could not start the operation (no thread)");
        }
        self.running.poke();
    }

    fn on_step(
        &mut self,
        op: u64,
        path: &Path,
        outcome: Result<Done, String>,
        tab: Option<Box<MovedTab>>,
    ) {
        self.moving.remove(path);
        self.restore_moved_tab(path, tab);
        let current = self.sessions.op.as_mut().filter(|o| o.id == op);
        let Some(o) = current else {
            return;
        };
        o.progress.done += 1;
        match outcome {
            Ok(done) => {
                o.ok += 1;
                match done {
                    Done::Deleted => {
                        o.freed += o.sizes.get(path).copied().unwrap_or(0);
                        self.sessions.model.forget(path);
                    }
                    Done::Moved(kind) => {
                        if let MoveKind::Forked { .. } = kind {
                            o.forked += 1;
                        } else {
                            self.sessions.model.forget(path);
                        }
                    }
                    Done::Copied | Done::Stopped => {}
                }
            }
            Err(e) => {
                o.failed += 1;
                o.first_error.get_or_insert_with(|| e.clone());
                log::warn!("session manager: {}: {e}", path.display());
                self.sessions
                    .model
                    .set_error(path, failure_line(&e).to_owned());
            }
        }
    }

    fn on_op_finished(&mut self, op: u64) {
        let Some(o) = self.sessions.op.take_if(|o| o.id == op) else {
            return;
        };
        let to = o.dest.as_deref().map(abbreviate_home).unwrap_or_default();
        let total = o.progress.total;
        if o.failed == 0 {
            let text = match o.kind {
                OpKind::Delete => format!(
                    "Deleted {} · freed {}",
                    plural(o.ok, "session", "sessions"),
                    human_size(o.freed)
                ),
                OpKind::Copy if o.skipped > 0 => {
                    format!("Copied {} · skipped {} already in {to}", o.ok, o.skipped)
                }
                OpKind::Copy => format!("Copied {} to {to}", plural(o.ok, "session", "sessions")),
                OpKind::Move if o.forked > 0 => format!(
                    "Moved {} to {to} — {} kept the original and landed as a new session",
                    plural(o.ok, "session", "sessions"),
                    o.forked
                ),
                OpKind::Move => format!("Moved {} to {to}", plural(o.ok, "session", "sessions")),
                OpKind::Stop => format!("Killed {}", plural(o.ok, "session", "sessions")),
            };
            self.toasts.success(&self.ctx, text);
        } else {
            let verb = match o.kind {
                OpKind::Delete => "delete",
                OpKind::Copy => "copy",
                OpKind::Move => "move",
                OpKind::Stop => "kill",
            };
            let why = o
                .first_error
                .as_deref()
                .map(failure_line)
                .unwrap_or_default();
            if let Some(full) = o.first_error.as_deref() {
                log::warn!("session manager: {verb} failed: {full}");
            }
            // The toast is short: the skip note replaces the "failed rows are marked" hint.
            let tail = if o.skipped > 0 {
                format!(" Skipped {} already in {to}.", o.skipped)
            } else {
                " The failed rows are marked in the table.".to_owned()
            };
            self.toasts.error(
                &self.ctx,
                format!("Could not {verb} {} of {total} — {why}.{tail}", o.failed),
            );
        }
        self.focus_pending = true;
        self.start_scan();
        self.refresh_sessions();
    }

    // ------------------------------------------------------------- delete

    fn request_delete_sessions(&mut self, paths: &[PathBuf]) {
        if self.omp.is_none() || self.sessions.op.is_some() {
            return;
        }
        let model = &self.sessions.model;
        let rows: Vec<_> = paths.iter().filter_map(|p| model.row_by_path(p)).collect();
        if rows.is_empty() {
            return;
        }
        let bytes: u64 = rows.iter().map(|r| r.entry.total_bytes()).sum();
        let projects = rows
            .iter()
            .map(|r| r.entry.cwd.as_path())
            .collect::<HashSet<_>>()
            .len();
        let running = rows.iter().filter(|r| r.live.running).count();
        let busy = rows.iter().filter(|r| r.live.busy).count();
        let mut body = format!(
            "{} from {}.\nFrees {} (session files, subagent transcripts and artifacts). \
             This cannot be undone.",
            plural(rows.len(), "session", "sessions"),
            plural(projects, "project", "projects"),
            human_size(bytes),
        );
        if running > 0 {
            body.push_str(&format!(
                "\n{running} running — their omp processes are stopped first."
            ));
        }
        if busy > 0 {
            body.push_str(&format!(
                "\n{busy} {} working: the current turn is interrupted.",
                if busy == 1 { "is" } else { "are" }
            ));
        }
        let title = format!("Delete {}?", plural(rows.len(), "session", "sessions"));
        let paths: Vec<DeleteTarget> = rows
            .iter()
            .map(|r| DeleteTarget {
                path: r.entry.path.clone(),
                id: r.entry.id.clone(),
            })
            .collect();
        self.confirm = Some((
            Confirm {
                title,
                body,
                confirm_label: "Delete".to_owned(),
                danger: true,
            },
            PendingConfirm::DeleteSessions(paths),
        ));
    }

    pub(super) fn execute_delete_sessions(&mut self, targets: Vec<DeleteTarget>) {
        let Some(omp) = self.omp.clone() else {
            return;
        };
        if self.sessions.op.is_some() {
            return;
        }
        let paths: Vec<PathBuf> = targets.iter().map(|t| t.path.clone()).collect();
        let entries = self.claim_rows(&paths);
        if entries.is_empty() {
            return;
        }
        // The ids captured at confirmation; omp re-checks them under the session lease.
        let files: Vec<DeleteTarget> = targets
            .into_iter()
            .filter(|t| entries.iter().any(|e| e.path == t.path))
            .collect();
        let mut kill_names: Vec<String> = Vec::new();
        for e in &entries {
            for n in self.session_kill_names(&session_entry(e), true) {
                if !kill_names.contains(&n) {
                    kill_names.push(n);
                }
            }
        }
        let op = self.begin_op(OpKind::Delete, &entries, None);
        let backends = self.backends.clone();
        self.run_op(&entries, move |tx, ctx| {
            kill_all(&backends, &kill_names, "delete");
            let mut reported: HashSet<PathBuf> = HashSet::new();
            omp.delete_sessions(&files, |path, result| {
                reported.insert(path.to_path_buf());
                send(
                    tx,
                    ctx,
                    SessionsEvent::Step {
                        op,
                        path: path.to_path_buf(),
                        outcome: result
                            .map(|()| Done::Deleted)
                            .map_err(|e| delete_error_text(&e)),
                        tab: None,
                    },
                );
            });
            for f in files.iter().filter(|f| !reported.contains(&f.path)) {
                send(
                    tx,
                    ctx,
                    SessionsEvent::Step {
                        op,
                        path: f.path.clone(),
                        outcome: Err("omp did not report a result".to_owned()),
                        tab: None,
                    },
                );
            }
            send(tx, ctx, SessionsEvent::OpFinished { op });
        });
    }

    // --------------------------------------------------------------- copy

    /// Sessions already in `dest` are left out (same rule as the sidebar's target list).
    fn partition_by_dest(
        &mut self,
        entries: Vec<InventoryEntry>,
        dest: &Path,
    ) -> Vec<InventoryEntry> {
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let dest = canon(dest);
        let (here, other): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|e| canon(&e.cwd) == dest);
        for e in here {
            self.sessions
                .model
                .set_error(&e.path, "Already in that project".to_owned());
        }
        other
    }

    fn execute_copy_sessions(&mut self, paths: Vec<PathBuf>, dest: PathBuf) {
        let Some(omp) = self.omp.clone() else {
            return;
        };
        if self.sessions.op.is_some() {
            return;
        }
        let claimed = self.claim_rows(&paths);
        let claimed_n = claimed.len();
        let entries = self.partition_by_dest(claimed, &dest);
        if entries.is_empty() {
            self.toasts.info(
                &self.ctx,
                "Nothing to copy: the sessions are already there.",
            );
            return;
        }
        let op = self.begin_op(OpKind::Copy, &entries, Some(dest.clone()));
        if let Some(o) = self.sessions.op.as_mut() {
            o.skipped = claimed_n - entries.len();
        }
        let files: Vec<PathBuf> = entries.iter().map(|e| e.path.clone()).collect();
        self.run_op(&entries, move |tx, ctx| {
            let Ok(dest) = absolute(&dest) else {
                return;
            };
            for src in files {
                let outcome = omp
                    .copy_session(&src, &dest)
                    .map(|_| Done::Copied)
                    .map_err(|e| e.to_string());
                send(
                    tx,
                    ctx,
                    SessionsEvent::Step {
                        op,
                        path: src,
                        outcome,
                        tab: None,
                    },
                );
            }
            send(tx, ctx, SessionsEvent::OpFinished { op });
        });
    }

    // --------------------------------------------------------------- move

    fn request_move_sessions(&mut self, paths: Vec<PathBuf>, dest: PathBuf) {
        if self.omp.is_none() || self.sessions.op.is_some() {
            return;
        }
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        let dest_c = canon(&dest);
        let model = &self.sessions.model;
        let rows: Vec<_> = paths
            .iter()
            .filter_map(|p| model.row_by_path(p))
            .filter(|r| canon(&r.entry.cwd) != dest_c)
            .collect();
        if rows.is_empty() {
            self.toasts.info(
                &self.ctx,
                "Nothing to move: the sessions are already there.",
            );
            return;
        }
        let running = rows
            .iter()
            .filter(|r| r.live.running || r.live.in_tab)
            .count();
        let mut body = format!(
            "Move {} to {}? omp moves each session file and its artifacts; the originals are \
             removed from their projects.",
            plural(rows.len(), "session", "sessions"),
            abbreviate_home(&dest),
        );
        if running > 0 {
            body.push_str(&format!(" {running} running or open — stopped first."));
        }
        let paths: Vec<PathBuf> = rows.iter().map(|r| r.entry.path.clone()).collect();
        self.confirm = Some((
            Confirm {
                title: "Move sessions?".to_owned(),
                body,
                confirm_label: "Move".to_owned(),
                danger: running > 0,
            },
            PendingConfirm::MoveSessions { paths, dest },
        ));
    }

    pub(super) fn execute_move_sessions(&mut self, paths: Vec<PathBuf>, dest: PathBuf) {
        let Some(omp) = self.omp.clone() else {
            return;
        };
        if self.sessions.op.is_some() {
            return;
        }
        let claimed = self.claim_rows(&paths);
        let entries = self.partition_by_dest(claimed, &dest);
        if entries.is_empty() {
            return;
        }
        let op = self.begin_op(OpKind::Move, &entries, Some(dest.clone()));
        // Tabs close now and come back one by one as their session finishes.
        let preps: Vec<MovePrep> = entries
            .iter()
            .map(|e| self.prepare_move(session_entry(e)))
            .collect();
        let backends = self.backends.clone();
        self.run_op(&entries, move |tx, ctx| {
            for prep in preps {
                let path = prep.entry.path.clone();
                let (result, tab) = prep.run(&omp, &backends, &dest);
                send(
                    tx,
                    ctx,
                    SessionsEvent::Step {
                        op,
                        path,
                        outcome: result.map(|o| Done::Moved(o.kind)),
                        tab,
                    },
                );
            }
            send(tx, ctx, SessionsEvent::OpFinished { op });
        });
    }

    // --------------------------------------------------------------- stop

    fn request_stop_sessions(&mut self, paths: Vec<PathBuf>) {
        if self.sessions.op.is_some() {
            return;
        }
        let model = &self.sessions.model;
        let rows: Vec<_> = paths
            .iter()
            .filter_map(|p| model.row_by_path(p))
            .filter(|r| r.live.running)
            .collect();
        if rows.is_empty() {
            self.toasts
                .info(&self.ctx, "None of those sessions is running anymore.");
            return;
        }
        let busy = rows.iter().filter(|r| r.live.busy).count();
        let mut body = format!(
            "{} will be terminated. The session files stay, so you can resume them later.",
            plural(rows.len(), "running session", "running sessions"),
        );
        if busy > 0 {
            body.push_str(&format!(" {busy} of them is working right now."));
        }
        let title = format!(
            "Kill {}?",
            plural(rows.len(), "session process", "session processes")
        );
        let paths: Vec<PathBuf> = rows.iter().map(|r| r.entry.path.clone()).collect();
        self.confirm = Some((
            Confirm {
                title,
                body,
                confirm_label: "Kill".to_owned(),
                danger: true,
            },
            PendingConfirm::StopSessions(paths),
        ));
    }

    pub(super) fn execute_stop_sessions(&mut self, paths: Vec<PathBuf>) {
        if self.sessions.op.is_some() {
            return;
        }
        let entries = self.claim_rows(&paths);
        // Tabs stay open (they show the exit); only the processes die.
        let mut jobs: Vec<(PathBuf, Vec<String>)> = Vec::new();
        for e in &entries {
            let names = self.session_kill_names(&session_entry(e), false);
            jobs.push((e.path.clone(), names));
        }
        if entries.is_empty() {
            return;
        }
        let op = self.begin_op(OpKind::Stop, &entries, None);
        let backends = self.backends.clone();
        self.run_op(&entries, move |tx, ctx| {
            for (path, names) in jobs {
                let outcome = if names.is_empty() {
                    Err("it is not running anymore".to_owned())
                } else {
                    names
                        .iter()
                        .map(|n| backends.kill(n))
                        .find_map(Result::err)
                        .map_or(Ok(Done::Stopped), Err)
                };
                send(
                    tx,
                    ctx,
                    SessionsEvent::Step {
                        op,
                        path,
                        outcome,
                        tab: None,
                    },
                );
            }
            send(tx, ctx, SessionsEvent::OpFinished { op });
        });
    }
}

fn send(tx: &Sender<AppEvent>, ctx: &egui::Context, ev: SessionsEvent) {
    let _ = tx.send(AppEvent::Sessions(ev));
    ctx.request_repaint();
}

/// The message of a failure without omp's source excerpts and stack frames: the first
/// non-empty line that is not a `at …` frame (the full text goes to the log).
fn failure_line(text: &str) -> &str {
    let text = crate::summary::strip_context(text);
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("at "))
        .unwrap_or_else(|| text.trim())
}

#[cfg(test)]
mod tests {
    #[test]
    fn failure_line_drops_stack_frames() {
        let raw = "Could not restore model\n    at foo (bundle.js:1:2)\n    at bar (bundle.js:3:4)";
        assert_eq!(failure_line(raw), "Could not restore model");
        assert_eq!(failure_line("plain"), "plain");
    }

    use super::*;

    #[test]
    fn common_cwd_is_none_for_a_mix_or_nothing() {
        let a = PathBuf::from("/p");
        let b = PathBuf::from("/q");
        assert_eq!(
            common_cwd([a.as_path(), a.as_path()].into_iter()),
            Some(a.clone())
        );
        assert_eq!(common_cwd([a.as_path(), b.as_path()].into_iter()), None);
        assert_eq!(common_cwd(std::iter::empty()), None);
    }

    #[test]
    fn plural_forms() {
        assert_eq!(plural(1, "session", "sessions"), "1 session");
        assert_eq!(plural(0, "session", "sessions"), "0 sessions");
        assert_eq!(plural(12, "session", "sessions"), "12 sessions");
    }
}
