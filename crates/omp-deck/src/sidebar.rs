//! Session sidebar: grouped, filterable list of persisted omp sessions (rounded cards, glowing
//! running dots, animated hover/selection, right-click menus).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::icons::Icon;
use crate::icons::{
    ICON_ARCHIVE_RESTORE, ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT, ICON_CHEVRONS_DOWN_UP,
    ICON_CHEVRONS_UP_DOWN, ICON_CIRCLE_STOP, ICON_CLOUD, ICON_CLOUD_UPLOAD, ICON_COPY,
    ICON_EXTERNAL_LINK, ICON_FILE_TEXT, ICON_FILES, ICON_FOCUS, ICON_FOLDER, ICON_FOLDER_OPEN,
    ICON_FOLDER_OUTPUT, ICON_FOLDER_X, ICON_KEY_ROUND, ICON_LIST_TREE, ICON_PLAY, ICON_PLUS,
    ICON_POWER, ICON_REFRESH_CW, ICON_ROTATE_CCW, ICON_SEARCH, ICON_SEARCH_X, ICON_TERMINAL,
    ICON_TRASH_2, ICON_X,
};
use deck_core::sessions::{ProjectGroup, SessionEntry, SessionsRoot};
use egui::collapsing_header::CollapsingState;
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align2, CursorIcon, Frame, Id, Margin, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::folder_scope;
use crate::running::RunningSnapshot;
use crate::scope_bar::{self, ScopeAction, ScopeInput, ScopeState};
use crate::tabs::{self, Tab, TabStatus};
use crate::theme::{self, Palette};
use crate::transfer::{self, TransferKind};
use crate::widgets::{empty_state, icon_text, menu_item, paint_icon};

/// "now", "5m", "3h", "2d", "4w", "2y".
pub fn format_age(now: SystemTime, modified: SystemTime) -> String {
    let secs = now
        .duration_since(modified)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    const WEEK: u64 = 7 * DAY;
    const YEAR: u64 = 365 * DAY;
    match secs {
        s if s < MIN => "now".to_owned(),
        s if s < HOUR => format!("{}m", s / MIN),
        s if s < DAY => format!("{}h", s / HOUR),
        s if s < WEEK => format!("{}d", s / DAY),
        s if s < YEAR => format!("{}w", s / WEEK),
        s => format!("{}y", s / YEAR),
    }
}

/// Lowercased query; empty matches everything. Matches title/label, first prompt, project cwd
/// (display and raw), and id prefix.
pub fn matches_filter(query_lower: &str, group: &ProjectGroup, entry: &SessionEntry) -> bool {
    if query_lower.is_empty() {
        return true;
    }
    entry.label().to_lowercase().contains(query_lower)
        || entry
            .title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(query_lower))
        || group.display.to_lowercase().contains(query_lower)
        || entry
            .cwd
            .to_string_lossy()
            .to_lowercase()
            .contains(query_lower)
        || entry.id.to_lowercase().starts_with(query_lower)
}

/// POSIX single-quote `s` (always quoted; embedded `'` becomes `'\''`).
pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `omp --resume '<path>'` for pasting into a shell.
pub fn resume_command(path: &Path) -> String {
    format!("omp --resume {}", shell_quote(&path.to_string_lossy()))
}

/// `("~/dev", "proj")` for `"~/dev/proj"`; a display without a usable last segment is all name.
fn split_display(display: &str) -> (&str, &str) {
    match display.rsplit_once(['/', '\\']) {
        Some((parent, name)) if !name.is_empty() => (parent, name),
        _ => ("", display),
    }
}

#[derive(Debug)]
pub enum SidebarAction {
    Open(SessionEntry),
    /// Show the session's tree (reads the file; no tab needed).
    Tree(SessionEntry),
    New(PathBuf),
    Refresh,
    /// Put `text` on the clipboard and confirm with a toast naming `what`.
    CopyText {
        text: String,
        what: &'static str,
    },
    /// Hand a directory to the desktop's opener.
    OpenFolder(PathBuf),
    /// Ask to stop the session running this omp session.
    Stop(SessionEntry),
    /// Ask to restart the running session (stop its process, resume it with live control).
    Restart(SessionEntry),
    /// Ask to delete the session (file and artifacts) through omp.
    Delete(SessionEntry),
    /// Copy/move a session; `dest: None` = ask for a folder first.
    Transfer {
        kind: TransferKind,
        entry: SessionEntry,
        dest: Option<PathBuf>,
    },
    /// Show only this folder's sessions (`None` = all projects).
    SetScope(Option<PathBuf>),
    /// Pick the scope folder with the native dialog.
    BrowseScope,
    /// A path typed into the scope menu.
    TypedScope(String),
    /// Switch to the tab at this strip index ("Open tabs").
    ActivateTab(usize),
    /// Close the tab at this strip index (same path as the tab strip's ✕).
    CloseTab(usize),
    /// Kill the process of the tab at this strip index (tabs whose session menu has no "Stop").
    KillTab(usize),
    /// Back up this project folder to Google Drive (opens the backup dialog).
    BackupProject(PathBuf),
    /// Restore a backup of this project folder from Google Drive (opens the restore dialog).
    RestoreProject(PathBuf),
}

/// The "Google Drive" menu entries: hidden without credentials, disabled with a reason while
/// signed out or a project job runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveMenu {
    Hidden,
    Off(&'static str),
    On,
}

#[derive(Default)]
pub struct SidebarOutput {
    pub actions: Vec<SidebarAction>,
    /// The user clicked something in the sidebar (other than the filter box).
    pub interacted: bool,
}

pub struct SidebarInput<'a> {
    pub groups: Option<&'a [ProjectGroup]>,
    pub running: &'a RunningSnapshot,
    pub root: &'a SessionsRoot,
    pub root_exists: bool,
    pub active_path: Option<&'a Path>,
    /// Working directory of the active terminal tab (its project group when the session file is
    /// not listed yet).
    pub active_cwd: Option<&'a Path>,
    pub now: SystemTime,
    /// A manual refresh is in flight (the refresh button spins).
    pub refreshing: bool,
    /// Sessions can be stopped (a session backend available).
    pub can_stop: bool,
    /// omp was found: session copy/move are offered.
    pub omp_available: bool,
    /// Session files with a move in flight (their copy/move items are disabled).
    pub busy: &'a HashSet<PathBuf>,
    /// Watcher generation of `groups` (part of the cache key).
    pub generation: u64,
    /// Session files open in a live tab: those rows count as running.
    pub open_paths: &'a [&'a Path],
    /// Per-folder mode: only sessions inside this folder are listed.
    pub scope: Option<&'a Path>,
    /// Recently focused folders, most recent first.
    pub recent: &'a [PathBuf],
    /// kdialog or zenity exists.
    pub picker_available: bool,
    /// The native folder dialog is open.
    pub picking: bool,
    /// Every open tab in strip order ("Open tabs" section) with the session labels and the active
    /// index.
    pub tabs: &'a [Tab],
    pub labels: &'a HashMap<PathBuf, String>,
    pub active_tab: usize,
    /// Live omp state (`Busy` / `Attention` only) of sessions open in a tab, by session file.
    pub tab_activity: &'a HashMap<&'a Path, TabStatus>,
    /// Google Drive project sync entries of the context menus.
    pub drive: DriveMenu,
}

/// Which project groups are open. A group is open while filtering, else by its user override,
/// else when it is the active group. The active group is the group of the most recent tab that
/// has one (a Settings tab keeps the previous group). Becoming the active group drops a
/// collapsed (`false`) override so it opens; an open (`true`) override survives. Re-activating
/// the same group changes nothing. Never persisted.
#[derive(Default)]
pub struct Expansion {
    overrides: HashMap<PathBuf, bool>,
    active: Option<PathBuf>,
}

impl Expansion {
    fn is_open(&self, cwd: &Path, filtering: bool) -> bool {
        filtering
            || self
                .overrides
                .get(cwd)
                .copied()
                .unwrap_or(self.active.as_deref() == Some(cwd))
    }

    /// Notes the group of the active tab (`None`: the tab has no group, keep the previous one).
    fn on_activation(&mut self, group: Option<&Path>) {
        let Some(g) = group else { return };
        if self.active.as_deref() != Some(g) {
            if self.overrides.get(g) == Some(&false) {
                self.overrides.remove(g);
            }
            self.active = Some(g.to_path_buf());
        }
    }

    fn set(&mut self, cwd: &Path, open: bool) {
        self.overrides.insert(cwd.to_path_buf(), open);
    }

    fn set_all(&mut self, groups: &[ProjectGroup], open: bool) {
        self.overrides.clear();
        self.overrides
            .extend(groups.iter().map(|g| (g.cwd.clone(), open)));
    }
}

/// The group index holding `active_path`, else the group whose cwd is `active_cwd`.
fn active_group_of(
    groups: &[ProjectGroup],
    active_path: Option<&Path>,
    active_cwd: Option<&Path>,
) -> Option<usize> {
    let by_path = active_path.and_then(|p| {
        groups
            .iter()
            .position(|g| g.sessions.iter().any(|e| e.path == p))
    });
    by_path.or_else(|| active_cwd.and_then(|c| groups.iter().position(|g| g.cwd == c)))
}

/// `(data stamp, active path, active cwd)`.
type ActiveKey = ((u64, usize), Option<PathBuf>, Option<PathBuf>);

/// The active-group lookup, valid for one [`ActiveKey`].
#[derive(Default)]
struct ActiveGroupCache {
    key: Option<ActiveKey>,
    group: Option<usize>,
}

impl ActiveGroupCache {
    fn get<'a>(
        &mut self,
        stamp: (u64, usize),
        groups: &'a [ProjectGroup],
        active_path: Option<&Path>,
        active_cwd: Option<&Path>,
    ) -> Option<&'a Path> {
        let fresh = self.key.as_ref().is_some_and(|(s, p, c)| {
            *s == stamp && p.as_deref() == active_path && c.as_deref() == active_cwd
        });
        if !fresh {
            self.group = active_group_of(groups, active_path, active_cwd);
            self.key = Some((
                stamp,
                active_path.map(Path::to_path_buf),
                active_cwd.map(Path::to_path_buf),
            ));
        }
        self.group
            .and_then(|i| groups.get(i))
            .map(|g| g.cwd.as_path())
    }
}

/// Age strings are recomputed at most this often.
const AGE_BUCKET_SECS: u64 = 15;

/// Filtered row indices per project, valid for one `(data stamp, query)`; the stamp is the
/// watcher generation plus the identity of the groups snapshot.
#[derive(Default)]
struct FilterCache {
    key: Option<((u64, usize), String)>,
    visible: Vec<Vec<u32>>,
}

impl FilterCache {
    /// Per-group indices of the rows matching `query` (empty query: nothing is computed).
    fn sync(&mut self, stamp: (u64, usize), query: &str, groups: &[ProjectGroup]) -> &[Vec<u32>] {
        if query.is_empty() {
            return &[];
        }
        let fresh = self
            .key
            .as_ref()
            .is_some_and(|(s, q)| *s == stamp && q == query && self.visible.len() == groups.len());
        if !fresh {
            self.visible = groups
                .iter()
                .map(|group| {
                    group
                        .sessions
                        .iter()
                        .enumerate()
                        .filter(|(_, e)| matches_filter(query, group, e))
                        .map(|(i, _)| i as u32)
                        .collect()
                })
                .collect();
            self.key = Some((stamp, query.to_owned()));
        }
        &self.visible
    }
}

/// Age chip texts by session file, valid for one `(data stamp, time bucket)`.
#[derive(Default)]
struct AgeCache {
    key: Option<((u64, usize), u64)>,
    texts: HashMap<PathBuf, String>,
}

impl AgeCache {
    fn get(&mut self, key: ((u64, usize), u64), now: SystemTime, entry: &SessionEntry) -> &str {
        if self.key != Some(key) {
            self.texts.clear();
            self.key = Some(key);
        }
        if !self.texts.contains_key(entry.path.as_path()) {
            self.texts
                .insert(entry.path.clone(), format_age(now, entry.modified));
        }
        self.texts
            .get(entry.path.as_path())
            .map_or("", String::as_str)
    }
}

/// Work the sidebar would otherwise redo every frame.
#[derive(Default)]
pub struct SidebarCache {
    filter: FilterCache,
    ages: AgeCache,
    active_group: ActiveGroupCache,
    expansion: Expansion,
    pub scope_ui: ScopeState,
}

fn row_h() -> f32 {
    theme::dens(28.0, 36.0)
}

fn header_h() -> f32 {
    theme::dens(28.0, 32.0)
}

pub fn show(
    ui: &mut Ui,
    input: &SidebarInput<'_>,
    filter: &mut String,
    cache: &mut SidebarCache,
) -> SidebarOutput {
    let pal = Palette::of_ui(ui);
    let mut out = SidebarOutput::default();
    open_tabs_section(ui, input, &mut out);

    let scope_actions = scope_bar::chip(
        ui,
        &ScopeInput {
            scope: input.scope,
            recent: input.recent,
            picker_available: input.picker_available,
            picking: input.picking,
        },
        &mut cache.scope_ui,
    );
    for a in scope_actions {
        out.interacted = true;
        out.actions.push(match a {
            ScopeAction::Set(dir) => SidebarAction::SetScope(dir),
            ScopeAction::Browse => SidebarAction::BrowseScope,
            ScopeAction::Typed(text) => SidebarAction::TypedScope(text),
            ScopeAction::NewSession(dir) => SidebarAction::New(dir),
            ScopeAction::OpenInFileManager(dir) => SidebarAction::OpenFolder(dir),
            ScopeAction::CopyPath(dir) => SidebarAction::CopyText {
                text: dir.to_string_lossy().into_owned(),
                what: "folder path",
            },
        });
    }
    ui.add_space(theme::dens(6.0, 8.0));

    let field_h = theme::dens(28.0, 36.0);
    let neutral = theme::neutral();
    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), field_h),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            if refresh_button(ui, input.refreshing).clicked() {
                out.actions.push(SidebarAction::Refresh);
                out.interacted = true;
            }
            let id = Id::unique("sidebar-filter");
            let focused = ui.memory(|m| m.has_focus(id));
            ui.add(
                egui::TextEdit::singleline(filter)
                    .id(id)
                    .prefix(icon_text(
                        ICON_SEARCH,
                        if neutral { 15.0 } else { 17.0 },
                        pal.weak,
                    ))
                    .hint_text("Search sessions…")
                    .align(egui::Align2::LEFT_CENTER)
                    .min_size(vec2(0.0, field_h))
                    .desired_width(ui.available_width())
                    .margin(Margin::symmetric(
                        if neutral { 8 } else { 10 },
                        if neutral { 4 } else { 8 },
                    ))
                    .frame(
                        Frame::new()
                            .fill(if neutral { pal.bg } else { pal.surface })
                            .corner_radius(if neutral { 8 } else { 11 })
                            .stroke(Stroke::new(
                                1.0,
                                if focused { pal.accent } else { pal.stroke },
                            )),
                    ),
            );
        },
    );
    ui.add_space(theme::dens(6.0, 8.0));
    if let Some(dir) = input.scope
        && scope_bar::new_session_button(ui, dir)
    {
        out.actions.push(SidebarAction::New(dir.to_path_buf()));
        out.interacted = true;
    }
    if input.scope.is_some() {
        ui.add_space(8.0);
    }

    let Some(groups) = input.groups else {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.add(crate::widgets::Spin::new(22.0, pal.accent));
            ui.add_space(6.0);
            ui.label(RichText::new("Scanning sessions…").color(pal.weak));
        });
        return out;
    };
    if !input.root_exists && input.scope.is_none() {
        empty_state(
            ui,
            ICON_FOLDER_X,
            pal.warn,
            "Sessions directory not found",
            "It will appear here once omp has created sessions.",
        );
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(input.root.path.display().to_string())
                    .monospace()
                    .small()
                    .color(pal.weak),
            );
        });
        return out;
    }
    if groups.is_empty() && input.scope.is_none() {
        empty_state(
            ui,
            ICON_FOLDER_OPEN,
            pal.accent,
            "No sessions yet",
            "Start omp in a project and it will show up here.",
        );
        ui.add_space(6.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new(input.root.path.display().to_string())
                    .monospace()
                    .small()
                    .color(pal.weak),
            );
        });
        return out;
    }

    let query = filter.trim().to_lowercase();
    let filtering = !query.is_empty();
    let mut any_shown = false;
    let SidebarCache {
        filter: filter_cache,
        ages,
        active_group: active_cache,
        expansion,
        ..
    } = cache;
    let stamp = (input.generation, groups.as_ptr() as usize);
    let active_group = active_cache.get(stamp, groups, input.active_path, input.active_cwd);
    expansion.on_activation(active_group);
    let visible = filter_cache.sync(stamp, &query, groups);
    let age_key = (
        stamp,
        input
            .now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            / AGE_BUCKET_SECS,
    );

    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            for (gi, group) in groups.iter().enumerate() {
                if input
                    .scope
                    .is_some_and(|s| !folder_scope::contains(s, &group.cwd))
                {
                    continue;
                }
                // Unfiltered: borrow the group's own list; filtered: the cached indices.
                let idx: Option<&[u32]> = filtering.then(|| visible[gi].as_slice());
                let row_count = idx.map_or(group.sessions.len(), <[u32]>::len);
                if row_count == 0 {
                    continue;
                }
                any_shown = true;
                let open = expansion.is_open(&group.cwd, filtering);

                let pid = ui.make_persistent_id(("project", &group.cwd));
                let mut state = CollapsingState::load_with_default_open(ui.ctx(), pid, true);
                state.set_open(open);
                let open_t = state.openness(ui.ctx());
                let hdr = group_header(ui, group, row_count, open_t, &mut out);
                let mut menu_collapse = None;
                crate::theme::context_menu(&hdr.resp, 236.0, |ui| {
                    if menu_item(ui, ICON_PLUS, "New session here", None, true, false).clicked() {
                        out.actions.push(SidebarAction::New(group.cwd.clone()));
                        out.interacted = true;
                        ui.close();
                    }
                    let focused = input.scope == Some(group.cwd.as_path());
                    if menu_item(ui, ICON_FOCUS, "Focus this folder", None, !focused, false)
                        .on_hover_text("Show only the sessions of this folder")
                        .clicked()
                    {
                        out.actions
                            .push(SidebarAction::SetScope(Some(group.cwd.clone())));
                        out.interacted = true;
                        ui.close();
                    }
                    if menu_item(ui, ICON_FOLDER_OPEN, "Open folder", None, true, false).clicked() {
                        out.actions
                            .push(SidebarAction::OpenFolder(group.cwd.clone()));
                        ui.close();
                    }
                    if menu_item(ui, ICON_COPY, "Copy path", None, true, false).clicked() {
                        out.actions.push(SidebarAction::CopyText {
                            text: group.cwd.to_string_lossy().into_owned(),
                            what: "project path",
                        });
                        ui.close();
                    }
                    drive_items(ui, input.drive, &group.cwd, true, &mut out);
                    ui.separator();
                    if menu_item(ui, ICON_CHEVRONS_DOWN_UP, "Collapse all", None, true, false)
                        .clicked()
                    {
                        menu_collapse = Some(true);
                        ui.close();
                    }
                    if menu_item(ui, ICON_CHEVRONS_UP_DOWN, "Expand all", None, true, false)
                        .clicked()
                    {
                        menu_collapse = Some(false);
                        ui.close();
                    }
                });
                if let Some(collapse) = menu_collapse {
                    expansion.set_all(groups, !collapse);
                    state.set_open(filtering || !collapse);
                }
                if hdr.toggle && !filtering {
                    expansion.set(&group.cwd, !open);
                    out.interacted = true;
                    state.set_open(!open);
                }
                state.store(ui.ctx());
                state.show_body_unindented(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    for k in 0..row_count {
                        let entry = match idx {
                            Some(ix) => ix.get(k).and_then(|&i| group.sessions.get(i as usize)),
                            None => group.sessions.get(k),
                        };
                        let Some(entry) = entry else {
                            continue;
                        };
                        let selected = input.active_path == Some(entry.path.as_path());
                        let running = input.running.is_running(&entry.id, &entry.path)
                            || input.open_paths.contains(&entry.path.as_path());
                        let age = ages.get(age_key, input.now, entry);
                        session_row(ui, entry, age, running, selected, input, &mut out);
                    }
                    ui.add_space(2.0);
                });
                ui.add_space(4.0);
            }
            if !any_shown {
                if filtering {
                    empty_state(
                        ui,
                        ICON_SEARCH_X,
                        pal.weak,
                        "No matching sessions",
                        match input.scope {
                            Some(_) => "Nothing in this folder matches. Try another title or id.",
                            None => "Try a different title, folder or id.",
                        },
                    );
                } else if let Some(dir) = input.scope {
                    scope_empty(ui, dir);
                }
            }
        });
    out
}

/// Nothing under the scope yet: say so (the button above the list starts the first session).
fn scope_empty(ui: &mut Ui, dir: &Path) {
    let pal = Palette::of_ui(ui);
    empty_state(
        ui,
        ICON_FOLDER_OPEN,
        pal.accent,
        "No sessions in this folder",
        "Start one with the button above, or clear the folder to see all projects.",
    );
    ui.add_space(10.0);
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(dir.display().to_string())
                .monospace()
                .small()
                .color(pal.weak),
        );
    });
}

fn refresh_button(ui: &mut Ui, spinning: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(
        vec2(theme::dens(28.0, 32.0), theme::dens(28.0, 36.0)),
        Sense::click(),
    );
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    let neutral = theme::neutral();
    p.rect_filled(
        rect,
        if neutral { 8 } else { 10 },
        theme::hover_fill(pal).gamma_multiply(0.85 * hov),
    );
    if spinning {
        ui.put(
            Rect::from_center_size(rect.center(), vec2(16.0, 16.0)),
            crate::widgets::Spin::new(if neutral { 14.0 } else { 16.0 }, pal.accent_b),
        );
    } else {
        paint_icon(
            p,
            rect.center(),
            Align2::CENTER_CENTER,
            ICON_REFRESH_CW,
            if neutral { 15.0 } else { 19.0 },
            theme::mix(pal.weak, pal.text_strong, hov),
        );
    }
    resp.on_hover_text("Rescan sessions now")
        .on_hover_cursor(CursorIcon::PointingHand)
}

struct HeaderResult {
    resp: egui::Response,
    /// The header body (not the "+") was clicked.
    toggle: bool,
}

fn group_header(
    ui: &mut Ui,
    group: &ProjectGroup,
    shown: usize,
    open_t: f32,
    out: &mut SidebarOutput,
) -> HeaderResult {
    let pal = Palette::of_ui(ui);
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), header_h()), Sense::click());
    let plus_rect =
        Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(24.0, 24.0));
    let plus = ui.interact(plus_rect, resp.id.with("new"), Sense::click());
    let hov_raw = resp.hovered() || plus.hovered() || resp.context_menu_opened();
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), hov_raw, 0.1);
    let plus_hov = ui
        .ctx()
        .animate_bool_with_time(plus.id.with("hov"), plus.hovered(), 0.1);

    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if theme::neutral() {
            paint_header_neutral(
                p,
                pal,
                rect,
                plus_rect,
                group,
                shown,
                open_t,
                (hov, plus_hov),
            );
        } else {
            paint_header_chrome(p, pal, rect, open_t, hov);
            let folder = if open_t > 0.5 {
                ICON_FOLDER_OPEN
            } else {
                ICON_FOLDER
            };
            paint_icon(
                p,
                pos2(rect.left() + 34.0, rect.center().y),
                Align2::CENTER_CENTER,
                folder,
                18.0,
                pal.accent,
            );

            let (parent, name) = split_display(&group.display);
            let mut job = LayoutJob::default();
            job.append(
                name,
                0.0,
                TextFormat {
                    font_id: crate::theme::font(14.0),
                    color: pal.text_strong,
                    ..Default::default()
                },
            );
            if !parent.is_empty() {
                job.append(
                    &format!("  {parent}"),
                    0.0,
                    TextFormat {
                        font_id: crate::theme::font(11.5),
                        color: pal.weak,
                        ..Default::default()
                    },
                );
            }
            job.wrap.max_width = (rect.width() - 46.0 - 40.0).max(10.0);
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            let galley = p.layout_job(job);
            p.galley(
                pos2(rect.left() + 48.0, rect.center().y - galley.size().y / 2.0),
                galley,
                pal.text,
            );

            // Count chip crossfades into the "+" button on hover.
            let count = shown.to_string();
            let g = p.layout_no_wrap(
                count,
                crate::theme::font(11.5),
                pal.weak.gamma_multiply(1.0 - hov),
            );
            let chip = Rect::from_center_size(
                pos2(rect.right() - 16.0, rect.center().y),
                vec2((g.size().x + 12.0).max(22.0), 18.0),
            );
            p.rect_filled(chip, 9, pal.surface.gamma_multiply(1.0 - hov));
            p.galley(chip.center() - g.size() / 2.0, g, pal.weak);
            if hov > 0.01 {
                p.rect_filled(
                    plus_rect,
                    8,
                    theme::mix(pal.raised, pal.accent, plus_hov * 0.5).gamma_multiply(hov),
                );
                paint_icon(
                    p,
                    plus_rect.center(),
                    Align2::CENTER_CENTER,
                    ICON_PLUS,
                    17.0,
                    theme::mix(pal.weak, pal.text_strong, plus_hov).gamma_multiply(hov),
                );
            }
        }
    }
    let resp = resp
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_ui(|ui| {
            ui.label(group.cwd.display().to_string());
        });
    if plus.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
    }
    let _ = plus
        .clone()
        .on_hover_text("New omp session in this project");
    if plus.clicked() {
        out.actions.push(SidebarAction::New(group.cwd.clone()));
        out.interacted = true;
        return HeaderResult {
            resp,
            toggle: false,
        };
    }
    let toggle = resp.clicked() && !plus.hovered();
    HeaderResult { resp, toggle }
}

/// Neutral header: chevron, folder glyph, medium name + faint parent path, plain count that turns
/// into the "+" on hover. No fills except the hover wash.
#[allow(clippy::too_many_arguments)]
fn paint_header_neutral(
    p: &egui::Painter,
    pal: &Palette,
    rect: Rect,
    plus_rect: Rect,
    group: &ProjectGroup,
    shown: usize,
    open_t: f32,
    (hov, plus_hov): (f32, f32),
) {
    paint_header_chrome(p, pal, rect, open_t, hov);
    let y = rect.center().y;
    let folder = if open_t > 0.5 {
        ICON_FOLDER_OPEN
    } else {
        ICON_FOLDER
    };
    paint_icon(
        p,
        pos2(rect.left() + 31.0, y),
        Align2::CENTER_CENTER,
        folder,
        15.0,
        theme::mix(pal.glyph, pal.text_strong, open_t * 0.5),
    );
    let (parent, name) = split_display(&group.display);
    let mut job = LayoutJob::default();
    job.append(
        name,
        0.0,
        TextFormat {
            font_id: crate::theme::font(13.0),
            color: pal.text_strong,
            ..Default::default()
        },
    );
    if !parent.is_empty() {
        job.append(
            &format!("  {parent}"),
            0.0,
            TextFormat {
                font_id: crate::theme::font(11.0),
                color: pal.weak,
                ..Default::default()
            },
        );
    }
    job.wrap.max_width = (rect.width() - 44.0 - 40.0).max(10.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = p.layout_job(job);
    p.galley(
        pos2(rect.left() + 46.0, y - galley.size().y / 2.0),
        galley,
        pal.text,
    );
    let count = p.layout_no_wrap(
        shown.to_string(),
        crate::theme::font(11.0),
        pal.weak.gamma_multiply(1.0 - hov),
    );
    p.galley(
        pos2(
            rect.right() - 14.0 - count.size().x / 2.0,
            y - count.size().y / 2.0,
        ),
        count,
        pal.weak,
    );
    if hov > 0.01 {
        p.rect_filled(plus_rect, 6, pal.raised.gamma_multiply(hov * plus_hov));
        paint_icon(
            p,
            plus_rect.center(),
            Align2::CENTER_CENTER,
            ICON_PLUS,
            15.0,
            theme::mix(pal.weak, pal.text_strong, plus_hov).gamma_multiply(hov),
        );
    }
}

/// Header hover wash and the open/closed chevron at the left (Neon: rotated by `open_t`).
fn paint_header_chrome(p: &egui::Painter, pal: &Palette, rect: Rect, open_t: f32, hov: f32) {
    let color = theme::mix(pal.weak, pal.text_strong, hov);
    if theme::neutral() {
        p.rect_filled(rect, 6, pal.surface.gamma_multiply(0.7 * hov));
        let chev = if open_t > 0.5 {
            ICON_CHEVRON_DOWN
        } else {
            ICON_CHEVRON_RIGHT
        };
        let c = pos2(rect.left() + 12.0, rect.center().y);
        paint_icon(p, c, Align2::CENTER_CENTER, chev, 14.0, color);
        return;
    }
    p.rect_filled(rect, 9, pal.raised.gamma_multiply(0.55 * hov));
    // Rotated by the open animation (0 = right, 1 = down).
    let c = pos2(rect.left() + 14.0, rect.center().y);
    let a = open_t * std::f32::consts::FRAC_PI_2;
    let rot = |x: f32, y: f32| c + vec2(x * a.cos() - y * a.sin(), x * a.sin() + y * a.cos());
    p.add(Shape::line(
        vec![rot(-2.5, -4.0), rot(2.5, 0.0), rot(-2.5, 4.0)],
        Stroke::new(1.7, color),
    ));
}

/// Most rows the "Open tabs" list shows before it scrolls.
const OPEN_TABS_MAX_ROWS: f32 = 6.0;
/// Icon slot of the status indicators in sidebar rows.
const STATUS_SIZE: f32 = 14.0;

/// "Open tabs": every tab in strip order with its status indicator; a click activates, the ✕ or a
/// middle click closes. Pinned above the session list (its own capped scroll area).
fn open_tabs_section(ui: &mut Ui, input: &SidebarInput<'_>, out: &mut SidebarOutput) {
    if input.tabs.is_empty() {
        return;
    }
    let pal = Palette::of_ui(ui);
    let id = ui.make_persistent_id("open-tabs");
    let mut state = CollapsingState::load_with_default_open(ui.ctx(), id, true);
    let open_t = state.openness(ui.ctx());

    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), header_h()), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        paint_header_chrome(p, pal, rect, open_t, hov);
        let x = rect.left() + if theme::neutral() { 26.0 } else { 28.0 };
        let y = rect.center().y;
        let size = if theme::neutral() { 13.0 } else { 14.0 };
        p.text(
            pos2(x, y),
            Align2::LEFT_CENTER,
            "Open tabs",
            theme::font(size),
            pal.text_strong,
        );
        // Count, plus how many wait for input (the section may be collapsed).
        let waiting = input
            .tabs
            .iter()
            .enumerate()
            .filter(|(i, t)| t.status(*i == input.active_tab) == TabStatus::Attention)
            .count();
        let mut right = rect.right() - 10.0;
        let count = p.text(
            pos2(right, y),
            Align2::RIGHT_CENTER,
            input.tabs.len().to_string(),
            theme::font(size - 2.0),
            pal.weak,
        );
        right = count.left() - 10.0;
        if waiting > 0 {
            let c = pos2(right - STATUS_SIZE / 2.0, y);
            tabs::paint_status(ui, c, STATUS_SIZE, TabStatus::Attention);
        }
    }
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() {
        state.toggle(ui);
        out.interacted = true;
    }
    state.store(ui.ctx());
    state.show_body_unindented(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt("open-tabs-list")
            .max_height((row_h() + 2.0) * OPEN_TABS_MAX_ROWS)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for (i, tab) in input.tabs.iter().enumerate() {
                    open_tab_row(ui, input, i, tab, out);
                }
            });
    });
    ui.add_space(theme::dens(6.0, 8.0));
    let sep = ui.available_rect_before_wrap();
    ui.painter()
        .hline(sep.x_range(), sep.top(), Stroke::new(1.0, pal.stroke));
    ui.add_space(theme::dens(6.0, 8.0));
}

fn open_tab_row(
    ui: &mut Ui,
    input: &SidebarInput<'_>,
    i: usize,
    tab: &Tab,
    out: &mut SidebarOutput,
) {
    let pal = Palette::of_ui(ui);
    let active = i == input.active_tab;
    let status = tab.status(active);
    let label = tab.label(input.labels);
    let folder = tab
        .term()
        .and_then(|t| t.cwd.file_name())
        .and_then(|n| n.to_str());
    let neutral = theme::neutral();

    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h()), Sense::click());
    let close_rect =
        Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(20.0, 20.0));
    let close = ui.interact(close_rect, resp.id.with("close"), Sense::click());
    let ctx = ui.ctx().clone();
    let hov = ctx.animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || close.hovered() || resp.context_menu_opened(),
        0.12,
    );
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), active, 0.18);
    if active && sel < 1.0 {
        // Just activated (from anywhere): keep its row in view.
        ui.scroll_to_rect(rect, None);
    }
    if ui.is_rect_visible(rect) {
        let pill = rect.shrink2(vec2(2.0, 0.0));
        let p = ui.painter();
        paint_row_bg(p, pal, pill, (hov, sel));
        if status == TabStatus::Attention {
            tabs::paint_attention_pill(p, pal, pill, if neutral { 6 } else { 10 });
        }
        let ic = pos2(
            pill.left() + if neutral { 15.0 } else { 19.0 },
            pill.center().y,
        );
        if !tabs::paint_status(ui, ic, STATUS_SIZE, status) {
            let (icon, col) = tab.kind_icon(pal, sel);
            paint_icon(p, ic, Align2::CENTER_CENTER, icon, STATUS_SIZE, col);
        }
        let color = theme::mix(pal.text, pal.text_strong, sel.max(hov * 0.6));
        let mut job = LayoutJob::default();
        job.append(
            label,
            0.0,
            TextFormat {
                font_id: theme::font(if neutral { 13.0 } else { 13.5 }),
                color,
                ..Default::default()
            },
        );
        if let Some(folder) = folder {
            job.append(
                &format!("  {folder}"),
                0.0,
                TextFormat {
                    font_id: theme::font(11.0),
                    color: pal.weak,
                    ..Default::default()
                },
            );
        }
        let text_x = pill.left() + if neutral { 28.0 } else { 32.0 };
        job.wrap.max_width = (close_rect.left() - 4.0 - text_x).max(10.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let galley = p.layout_job(job);
        p.galley(
            pos2(text_x, pill.center().y - galley.size().y / 2.0),
            galley,
            color,
        );
        if hov > 0.01 {
            let ch = ctx.animate_bool_with_time(close.id.with("hov"), close.hovered(), 0.1);
            p.circle_filled(
                close_rect.center(),
                10.0,
                theme::hover_fill(pal).gamma_multiply(ch),
            );
            paint_icon(
                p,
                close_rect.center(),
                Align2::CENTER_CENTER,
                ICON_X,
                14.0,
                theme::mix(pal.weak, pal.text_strong, ch).gamma_multiply(hov),
            );
        }
    }
    let close = close.on_hover_text("Close tab");
    let resp = resp
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_ui(|ui| {
            ui.label(label);
            if let Some(hint) = status.hint() {
                ui.weak(hint);
            }
            if let Some(t) = tab.term() {
                ui.weak(t.cwd.display().to_string());
            }
        });
    if close.clicked() || resp.middle_clicked() {
        out.actions.push(SidebarAction::CloseTab(i));
    } else if resp.clicked() {
        out.actions.push(SidebarAction::ActivateTab(i));
        out.interacted = true;
    }
    let env = input.menu_env();
    theme::context_menu(&resp, 230.0, |ui| tab_menu(ui, i, tab, &env, out));
}

fn session_row(
    ui: &mut Ui,
    entry: &SessionEntry,
    age_text: &str,
    running: bool,
    selected: bool,
    input: &SidebarInput<'_>,
    out: &mut SidebarOutput,
) {
    let pal = Palette::of_ui(ui);
    // A delete/move/copy of this session is in flight on a worker.
    let busy = input.busy.contains(&entry.path);
    // The live omp state of a tab showing this session (replaces the LED).
    let live = input
        .tab_activity
        .get(entry.path.as_path())
        .copied()
        .filter(|_| !busy);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h()), Sense::click());
    let ctx = ui.ctx().clone();
    let hov = ctx.animate_bool_with_time(
        resp.id.with("hov"),
        resp.hovered() || resp.context_menu_opened(),
        0.12,
    );
    let sel = ctx.animate_bool_with_time(resp.id.with("sel"), selected, 0.18);
    if ui.is_rect_visible(rect) {
        let pill = rect.shrink2(vec2(2.0, 0.0));
        let p = ui.painter();
        if theme::neutral() {
            paint_row_neutral(
                p,
                pal,
                pill,
                entry.label(),
                age_text,
                (running, busy || live.is_some()),
                (hov, sel),
            );
        } else {
            paint_row_bg(p, pal, pill, (hov, sel));
            // Status dot.
            let dot = pos2(pill.left() + 19.0, pill.center().y);
            if busy || live.is_some() {
                // The spinner / tab status is placed below, once the painter borrow ends.
            } else if running {
                theme::glow_dot(p, dot, 3.5, pal.ok, theme::GLOW_REST);
            } else {
                p.circle_filled(dot, 2.8, pal.weak.gamma_multiply(0.45));
            }
            // Age chip.
            let age = p.layout_no_wrap(age_text.to_owned(), crate::theme::font(11.5), pal.weak);
            let chip = Rect::from_center_size(
                pos2(
                    pill.right() - 10.0 - (age.size().x + 12.0) / 2.0,
                    pill.center().y,
                ),
                vec2(age.size().x + 12.0, 18.0),
            );
            p.rect_filled(chip, 9, pal.bg.gamma_multiply(0.55 + 0.45 * sel));
            p.galley(chip.center() - age.size() / 2.0, age, pal.weak);
            // Title.
            let color = theme::mix(pal.text, pal.text_strong, sel.max(hov * 0.5));
            let mut job = LayoutJob::simple_singleline(
                entry.label().to_owned(),
                crate::theme::font(13.5),
                color,
            );
            job.wrap.max_width = (chip.left() - pill.left() - 32.0 - 8.0).max(10.0);
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            let galley = p.layout_job(job);
            p.galley(
                pos2(pill.left() + 32.0, pill.center().y - galley.size().y / 2.0),
                galley,
                color,
            );
        }
    }
    let dot = pos2(
        rect.left() + 2.0 + if theme::neutral() { 15.0 } else { 19.0 },
        rect.center().y,
    );
    if busy {
        ui.put(
            Rect::from_center_size(dot, vec2(12.0, 12.0)),
            crate::widgets::Spin::new(12.0, pal.accent_b),
        );
    } else if let Some(status) = live {
        tabs::paint_status(ui, dot, STATUS_SIZE, status);
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if resp.clicked() {
        out.actions.push(SidebarAction::Open(entry.clone()));
        out.interacted = true;
    }
    let env = input.menu_env();
    theme::context_menu(&resp, 230.0, |ui| {
        session_menu(ui, entry, running, &env, out)
    });
    resp.on_hover_ui(|ui| {
        ui.label(entry.label());
        if let Some(hint) = live.and_then(TabStatus::hint) {
            ui.weak(hint);
        }
        ui.weak(format!("id: {}", entry.id));
        ui.weak(format!("cwd: {}", entry.cwd.display()));
        ui.weak(format!("file: {}", entry.path.display()));
    });
}

/// Neutral session row: status dot at the left, age at the right (background: [`paint_row_bg`]).
fn paint_row_neutral(
    p: &egui::Painter,
    pal: &Palette,
    pill: Rect,
    label: &str,
    age_text: &str,
    (running, placed): (bool, bool),
    (hov, sel): (f32, f32),
) {
    paint_row_bg(p, pal, pill, (hov, sel));
    let y = pill.center().y;
    let dot = pos2(pill.left() + 15.0, y);
    if placed {
        // The spinner / tab status is placed by the caller.
    } else if running {
        p.circle_filled(dot, 3.0, pal.ok);
    } else {
        p.circle_filled(dot, 2.5, pal.weak.gamma_multiply(0.4));
    }
    let age = p.layout_no_wrap(age_text.to_owned(), crate::theme::font(11.0), pal.weak);
    let age_x = pill.right() - 10.0 - age.size().x;
    let color = theme::mix(pal.text, pal.text_strong, sel.max(hov * 0.6));
    let mut job = LayoutJob::simple_singleline(label.to_owned(), crate::theme::font(13.0), color);
    job.wrap.max_width = (age_x - pill.left() - 28.0 - 8.0).max(10.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    let galley = p.layout_job(job);
    p.galley(
        pos2(pill.left() + 28.0, y - galley.size().y / 2.0),
        galley,
        color,
    );
    p.galley(pos2(age_x, y - age.size().y / 2.0), age, pal.weak);
}

/// List row hover wash and selection. Neutral: surface fill + 2 px accent edge; Neon: accent-tinted
/// card with a gradient edge.
fn paint_row_bg(p: &egui::Painter, pal: &Palette, pill: Rect, (hov, sel): (f32, f32)) {
    if theme::neutral() {
        p.rect_filled(pill, 6, pal.surface.gamma_multiply(0.6 * hov * (1.0 - sel)));
        if sel > 0.01 {
            p.rect_filled(pill, 6, pal.surface.gamma_multiply(sel));
            let bar = Rect::from_center_size(
                pos2(pill.left() + 1.0, pill.center().y),
                vec2(2.0, 14.0 * sel),
            );
            p.rect_filled(bar, 1, pal.accent);
        }
        return;
    }
    p.rect_filled(
        pill,
        10,
        pal.raised.gamma_multiply(0.75 * hov * (1.0 - sel)),
    );
    if sel > 0.01 {
        p.rect(
            pill,
            10,
            theme::mix(pal.surface, pal.accent, 0.20).gamma_multiply(sel),
            Stroke::new(1.0, pal.accent.gamma_multiply(0.5 * sel)),
            StrokeKind::Inside,
        );
        let bar = Rect::from_center_size(
            pos2(pill.left() + 3.0, pill.center().y),
            vec2(3.0, 20.0 * sel),
        );
        let mut m = egui::epaint::Mesh::default();
        m.colored_vertex(bar.left_top(), pal.accent_a);
        m.colored_vertex(bar.right_top(), pal.accent_a);
        m.colored_vertex(bar.left_bottom(), pal.accent_b);
        m.colored_vertex(bar.right_bottom(), pal.accent_b);
        m.add_triangle(0, 1, 2);
        m.add_triangle(1, 3, 2);
        p.add(Shape::mesh(m));
    }
}

/// What the session and tab context menus need besides the row itself.
pub struct MenuEnv<'a> {
    pub groups: &'a [ProjectGroup],
    /// omp was found: copy/move/delete are offered.
    pub omp_available: bool,
    /// Sessions can be stopped and restarted (a session backend available).
    pub can_stop: bool,
    /// Session files with a move/copy/delete in flight.
    pub busy: &'a HashSet<PathBuf>,
    pub drive: DriveMenu,
}

impl MenuEnv<'_> {
    fn entry(&self, path: &Path) -> Option<&SessionEntry> {
        self.groups
            .iter()
            .flat_map(|g| &g.sessions)
            .find(|e| e.path == path)
    }
}

impl SidebarInput<'_> {
    fn menu_env(&self) -> MenuEnv<'_> {
        MenuEnv {
            groups: self.groups.unwrap_or(&[]),
            omp_available: self.omp_available,
            can_stop: self.can_stop,
            busy: self.busy,
            drive: self.drive,
        }
    }
}

/// A tab's context menu (tab strip and "Open tabs" rows): the tab items, then its session's full
/// menu once the session file is listed.
pub fn tab_menu(ui: &mut Ui, i: usize, tab: &Tab, env: &MenuEnv<'_>, out: &mut SidebarOutput) {
    if menu_item(ui, ICON_X, "Close tab", None, true, false).clicked() {
        out.actions.push(SidebarAction::CloseTab(i));
        ui.close();
    }
    let Some(t) = tab.term() else {
        return;
    };
    let running = !t.term.is_exited();
    let entry = t.session_path.as_deref().and_then(|p| env.entry(p));
    // The session menu's "Stop session process" covers this whenever it is offered.
    if (entry.is_none() || !env.can_stop)
        && menu_item(ui, ICON_POWER, "Kill session process", None, running, false).clicked()
    {
        out.actions.push(SidebarAction::KillTab(i));
        ui.close();
    }
    if let Some(entry) = entry {
        ui.separator();
        session_menu(ui, entry, running, env, out);
    }
}

fn session_menu(
    ui: &mut Ui,
    entry: &SessionEntry,
    running: bool,
    env: &MenuEnv<'_>,
    out: &mut SidebarOutput,
) {
    fn copy(ui: &mut Ui, out: &mut SidebarOutput, text: String, what: &'static str) {
        out.actions.push(SidebarAction::CopyText { text, what });
        ui.close();
    }
    if menu_item(ui, ICON_PLAY, "Open", None, true, false).clicked() {
        out.actions.push(SidebarAction::Open(entry.clone()));
        out.interacted = true;
        ui.close();
    }
    if menu_item(ui, ICON_LIST_TREE, "Session tree…", None, true, false).clicked() {
        out.actions.push(SidebarAction::Tree(entry.clone()));
        ui.close();
    }
    ui.separator();
    if menu_item(ui, ICON_FILE_TEXT, "Copy session path", None, true, false).clicked() {
        copy(
            ui,
            out,
            entry.path.to_string_lossy().into_owned(),
            "session path",
        );
    }
    if menu_item(ui, ICON_KEY_ROUND, "Copy session id", None, true, false).clicked() {
        copy(ui, out, entry.id.clone(), "session id");
    }
    if menu_item(ui, ICON_TERMINAL, "Copy resume command", None, true, false).clicked() {
        copy(ui, out, resume_command(&entry.path), "resume command");
    }
    ui.separator();
    if menu_item(
        ui,
        ICON_FOLDER_OPEN,
        "Open project folder",
        None,
        true,
        false,
    )
    .clicked()
    {
        out.actions
            .push(SidebarAction::OpenFolder(entry.cwd.clone()));
        ui.close();
    }
    if menu_item(
        ui,
        ICON_EXTERNAL_LINK,
        "Show session file",
        None,
        true,
        false,
    )
    .clicked()
    {
        let dir = entry
            .path
            .parent()
            .map_or_else(|| entry.path.clone(), Path::to_path_buf);
        out.actions.push(SidebarAction::OpenFolder(dir));
        ui.close();
    }
    if env.drive != DriveMenu::Hidden {
        let pal = Palette::of_ui(ui);
        ui.menu_button(
            (
                icon_text(ICON_CLOUD, 17.0, pal.glyph),
                RichText::new("Google Drive").color(pal.text),
            ),
            |ui| drive_items(ui, env.drive, &entry.cwd, false, out),
        );
    }
    if env.omp_available {
        ui.separator();
        let idle = !env.busy.contains(&entry.path);
        for (kind, icon, label) in [
            (TransferKind::Copy, ICON_FILES, "Copy to project"),
            (TransferKind::Move, ICON_FOLDER_OUTPUT, "Move to project"),
        ] {
            transfer_menu(ui, entry, (kind, icon, label), idle, env, out);
        }
        ui.separator();
        if menu_item(ui, ICON_TRASH_2, "Delete session…", None, idle, true).clicked() {
            out.actions.push(SidebarAction::Delete(entry.clone()));
            ui.close();
        }
    }
    if running && env.can_stop {
        ui.separator();
        if menu_item(ui, ICON_ROTATE_CCW, "Restart session", None, true, false).clicked() {
            out.actions.push(SidebarAction::Restart(entry.clone()));
            ui.close();
        }
        if menu_item(
            ui,
            ICON_CIRCLE_STOP,
            "Stop session process",
            None,
            true,
            true,
        )
        .clicked()
        {
            out.actions.push(SidebarAction::Stop(entry.clone()));
            ui.close();
        }
    }
}

/// "Back up to Google Drive…" and "Restore from Google Drive…" for `cwd`.
fn drive_items(
    ui: &mut Ui,
    drive: DriveMenu,
    cwd: &Path,
    separator: bool,
    out: &mut SidebarOutput,
) {
    let (enabled, why) = match drive {
        DriveMenu::Hidden => return,
        DriveMenu::Off(why) => (false, Some(why)),
        DriveMenu::On => (true, None),
    };
    if separator {
        ui.separator();
    }
    for (icon, label, restore) in [
        (ICON_CLOUD_UPLOAD, "Back up to Google Drive…", false),
        (ICON_ARCHIVE_RESTORE, "Restore from Google Drive…", true),
    ] {
        let mut resp = menu_item(ui, icon, label, None, enabled, false);
        if let Some(why) = why {
            resp = resp.on_disabled_hover_text(why);
        }
        if resp.clicked() {
            out.actions.push(if restore {
                SidebarAction::RestoreProject(cwd.to_path_buf())
            } else {
                SidebarAction::BackupProject(cwd.to_path_buf())
            });
            out.interacted = true;
            ui.close();
        }
    }
}

/// "Copy to project" / "Move to project" submenu: every other project, then "Choose folder…".
fn transfer_menu(
    ui: &mut Ui,
    entry: &SessionEntry,
    spec: (TransferKind, Icon, &str),
    enabled: bool,
    env: &MenuEnv<'_>,
    out: &mut SidebarOutput,
) {
    let (kind, icon, label) = spec;
    let pal = Palette::of_ui(ui);
    let mut send = |ui: &mut Ui, dest: Option<PathBuf>| {
        out.actions.push(SidebarAction::Transfer {
            kind,
            entry: entry.clone(),
            dest,
        });
        out.interacted = true;
        ui.close();
    };
    ui.add_enabled_ui(enabled, |ui| {
        ui.menu_button(
            (
                icon_text(icon, 17.0, pal.glyph),
                RichText::new(label).color(pal.text),
            ),
            |ui| {
                ui.set_min_width(260.0);
                let targets = transfer::targets(env.groups, &entry.cwd);
                if targets.is_empty() {
                    ui.add_enabled(
                        false,
                        egui::Label::new(RichText::new("No other projects").color(pal.weak)),
                    );
                }
                egui::ScrollArea::vertical()
                    .max_height(300.0)
                    .auto_shrink([true, true])
                    .show(ui, |ui| {
                        for g in targets {
                            if menu_item(ui, ICON_FOLDER, &g.display, None, true, false)
                                .on_hover_text(g.cwd.display().to_string())
                                .clicked()
                            {
                                send(ui, Some(g.cwd.clone()));
                            }
                        }
                    });
                ui.separator();
                if menu_item(ui, ICON_FOLDER_OPEN, "Choose folder…", None, true, false).clicked()
                {
                    send(ui, None);
                }
            },
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> (SystemTime, SystemTime) {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000_000);
        (now, now - Duration::from_secs(secs))
    }

    fn age(secs: u64) -> String {
        let (n, m) = at(secs);
        format_age(n, m)
    }

    #[test]
    fn age_boundaries() {
        assert_eq!(age(0), "now");
        assert_eq!(age(59), "now");
        assert_eq!(age(60), "1m");
        assert_eq!(age(3599), "59m");
        assert_eq!(age(3600), "1h");
        assert_eq!(age(86_399), "23h");
        assert_eq!(age(86_400), "1d");
        assert_eq!(age(7 * 86_400 - 1), "6d");
        assert_eq!(age(7 * 86_400), "1w");
        assert_eq!(age(28 * 86_400), "4w");
        assert_eq!(age(365 * 86_400), "1y");
    }

    #[test]
    fn age_future_mtime_is_now() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        assert_eq!(format_age(now, now + Duration::from_secs(50)), "now");
    }

    fn entry(id: &str, title: Option<&str>, prompt: Option<&str>) -> SessionEntry {
        SessionEntry {
            id: id.into(),
            path: PathBuf::from(format!("/s/{id}.jsonl")),
            cwd: PathBuf::from("/home/u/proj"),
            title: title.map(Into::into),
            first_prompt: prompt.map(Into::into),
            modified: SystemTime::UNIX_EPOCH,
        }
    }

    fn group(e: &SessionEntry) -> ProjectGroup {
        ProjectGroup {
            cwd: e.cwd.clone(),
            display: "~/proj".into(),
            sessions: vec![e.clone()],
        }
    }

    #[test]
    fn filter_cache_follows_query_and_data() {
        let a = entry("aaa", Some("alpha"), None);
        let b = entry("bbb", Some("beta"), None);
        let groups = vec![ProjectGroup {
            cwd: a.cwd.clone(),
            display: "~/proj".into(),
            sessions: vec![a, b],
        }];
        let mut cache = FilterCache::default();
        let stamp = (1, groups.as_ptr() as usize);

        assert!(
            cache.sync(stamp, "", &groups).is_empty(),
            "no query: nothing cached"
        );
        assert_eq!(cache.sync(stamp, "beta", &groups), [vec![1]]);
        assert_eq!(cache.sync(stamp, "alpha", &groups), [vec![0]]);
        // New data under the same query is picked up.
        assert_eq!(cache.sync((2, stamp.1), "alpha", &groups), [vec![0]]);
        assert_eq!(cache.key.as_ref().map(|k| k.0), Some((2, stamp.1)));
    }

    #[test]
    fn age_cache_refreshes_per_time_bucket() {
        let e = entry("aaa", None, None);
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let mut cache = AgeCache::default();
        assert_eq!(
            cache.get(((1, 0), 1), t0, &e),
            "11d".replace("11d", &format_age(t0, e.modified))
        );
        // Same bucket: the cached text is served even if `now` moved on.
        let later = t0 + Duration::from_secs(10 * 86_400);
        assert_eq!(
            cache.get(((1, 0), 1), later, &e),
            format_age(t0, e.modified)
        );
        assert_eq!(
            cache.get(((1, 0), 2), later, &e),
            format_age(later, e.modified)
        );
    }

    #[test]
    fn filter_matching() {
        let e = entry("AbC123", Some("Fix Parser"), Some("please help"));
        let g = group(&e);
        assert!(matches_filter("", &g, &e));
        assert!(matches_filter("parser", &g, &e)); // case-insensitive title
        assert!(matches_filter("~/pro", &g, &e)); // cwd display
        assert!(matches_filter("abc1", &g, &e)); // id prefix
        assert!(!matches_filter("c123", &g, &e)); // id infix is not a match
        assert!(!matches_filter("nomatch", &g, &e));
    }

    #[test]
    fn filter_uses_label_fallback() {
        let e = entry("x", None, None);
        assert!(matches_filter("(untitled)", &group(&e), &e));
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("/a/b.jsonl"), "'/a/b.jsonl'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(
            resume_command(Path::new("/h/my dir/s.jsonl")),
            "omp --resume '/h/my dir/s.jsonl'"
        );
    }

    #[test]
    fn display_split() {
        assert_eq!(split_display("~/dev/proj"), ("~/dev", "proj"));
        assert_eq!(split_display("~"), ("", "~"));
        assert_eq!(split_display("/"), ("", "/"));
        assert_eq!(split_display("/x"), ("", "x"));
    }

    fn proj(cwd: &str, ids: &[&str]) -> ProjectGroup {
        ProjectGroup {
            cwd: PathBuf::from(cwd),
            display: cwd.into(),
            sessions: ids
                .iter()
                .map(|id| SessionEntry {
                    id: (*id).into(),
                    path: PathBuf::from(format!("{cwd}/{id}.jsonl")),
                    cwd: PathBuf::from(cwd),
                    title: None,
                    first_prompt: None,
                    modified: SystemTime::UNIX_EPOCH,
                })
                .collect(),
        }
    }

    fn open_set(ex: &Expansion, groups: &[ProjectGroup]) -> Vec<bool> {
        groups.iter().map(|g| ex.is_open(&g.cwd, false)).collect()
    }

    fn group_at<'a>(groups: &'a [ProjectGroup], path: &str) -> Option<&'a Path> {
        active_group_of(groups, Some(Path::new(path)), None).map(|i| groups[i].cwd.as_path())
    }

    #[test]
    fn only_the_active_tabs_group_is_open_at_startup() {
        let groups = [
            proj("/a", &["a1"]),
            proj("/b", &["b1"]),
            proj("/c", &["c1"]),
        ];
        let active = group_at(&groups, "/b/b1.jsonl");
        assert_eq!(active, Some(Path::new("/b")));
        let mut ex = Expansion::default();
        ex.on_activation(active);
        assert_eq!(open_set(&ex, &groups), [false, true, false]);
    }

    #[test]
    fn new_session_tab_opens_its_cwd_group() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        assert_eq!(
            active_group_of(&groups, None, Some(Path::new("/a"))),
            Some(0)
        );
        assert_eq!(
            active_group_of(&groups, None, Some(Path::new("/zzz"))),
            None
        );
    }

    #[test]
    fn manual_open_survives_activating_it_then_another_group() {
        let groups = [
            proj("/a", &["a1"]),
            proj("/b", &["b1"]),
            proj("/c", &["c1"]),
        ];
        let mut ex = Expansion::default();
        ex.on_activation(group_at(&groups, "/a/a1.jsonl"));
        ex.set(Path::new("/c"), true); // user opens /c by hand
        assert_eq!(open_set(&ex, &groups), [true, false, true]);
        ex.on_activation(group_at(&groups, "/c/c1.jsonl"));
        ex.on_activation(group_at(&groups, "/b/b1.jsonl"));
        assert_eq!(open_set(&ex, &groups), [false, true, true]);
    }

    #[test]
    fn changing_group_reopens_a_collapsed_group_but_reclicking_does_not() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        let mut ex = Expansion::default();
        let a = group_at(&groups, "/a/a1.jsonl");
        ex.on_activation(a);
        ex.set(Path::new("/a"), false);
        ex.on_activation(a); // re-click the active tab: stays collapsed
        assert_eq!(open_set(&ex, &groups), [false, false]);
        ex.on_activation(group_at(&groups, "/b/b1.jsonl"));
        ex.set(Path::new("/a"), false);
        ex.on_activation(a); // group changed back: reopens
        assert_eq!(open_set(&ex, &groups), [true, false]);
    }

    #[test]
    fn a_tab_without_group_keeps_the_previous_group_open() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        let mut ex = Expansion::default();
        ex.on_activation(group_at(&groups, "/b/b1.jsonl"));
        ex.on_activation(None); // Settings tab
        assert_eq!(open_set(&ex, &groups), [false, true]);
    }

    #[test]
    fn active_group_lookup_is_cached_per_key() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        let mut cache = ActiveGroupCache::default();
        let stamp = (1, groups.as_ptr() as usize);
        let b = Some(Path::new("/b/b1.jsonl"));
        assert_eq!(cache.get(stamp, &groups, b, None), Some(Path::new("/b")));
        assert_eq!(cache.group, Some(1));
        assert_eq!(
            cache.get(stamp, &groups, Some(Path::new("/a/a1.jsonl")), None),
            Some(Path::new("/a"))
        );
    }

    #[test]
    fn filtering_shows_everything_and_collapse_expand_all_set_overrides() {
        let groups = [proj("/a", &["a1"]), proj("/b", &["b1"])];
        let mut ex = Expansion::default();
        ex.on_activation(Some(Path::new("/a")));
        assert!(groups.iter().all(|g| ex.is_open(&g.cwd, true)));
        ex.set_all(&groups, false);
        assert_eq!(open_set(&ex, &groups), [false, false]);
        ex.set_all(&groups, true);
        assert_eq!(open_set(&ex, &groups), [true, true]);
    }
}
