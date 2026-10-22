//! Session sidebar: grouped, filterable list of persisted omp sessions (rounded cards, glowing
//! running dots, animated hover/selection, right-click menus).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use deck_core::sessions::{ProjectGroup, SessionEntry, SessionsRoot};
use egui::collapsing_header::CollapsingState;
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align2, CursorIcon, FontId, Frame, Id, Margin, Rect, RichText, Sense, Shape, Stroke,
    StrokeKind, Ui, pos2, vec2,
};
use egui_material_icons::MaterialIcon;
use egui_material_icons::icons::{
    ICON_ACCOUNT_TREE, ICON_ADD, ICON_CENTER_FOCUS_STRONG, ICON_CONTENT_COPY, ICON_DELETE,
    ICON_DESCRIPTION, ICON_DRIVE_FILE_MOVE, ICON_FILE_COPY, ICON_FOLDER, ICON_FOLDER_OFF,
    ICON_FOLDER_OPEN, ICON_KEY, ICON_OPEN_IN_NEW, ICON_PLAY_ARROW, ICON_REFRESH, ICON_RESTART_ALT,
    ICON_SEARCH, ICON_SEARCH_OFF, ICON_STOP_CIRCLE, ICON_TERMINAL, ICON_UNFOLD_LESS,
    ICON_UNFOLD_MORE,
};

use crate::folder_scope;
use crate::running::RunningSnapshot;
use crate::scope_bar::{self, ScopeAction, ScopeInput, ScopeState};
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
    match display.rsplit_once('/') {
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
    /// Ask to stop the tmux session running this omp session.
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
}

#[derive(Default)]
pub struct SidebarOutput {
    pub actions: Vec<SidebarAction>,
    pub collapsed_changed: bool,
    /// The user clicked something in the sidebar (other than the filter box).
    pub interacted: bool,
}

pub struct SidebarInput<'a> {
    pub groups: Option<&'a [ProjectGroup]>,
    pub running: &'a RunningSnapshot,
    pub root: &'a SessionsRoot,
    pub root_exists: bool,
    pub active_path: Option<&'a Path>,
    pub now: SystemTime,
    /// A manual refresh is in flight (the refresh button spins).
    pub refreshing: bool,
    /// Sessions can be stopped (tmux available).
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
    pub scope_ui: ScopeState,
}

const ROW_H: f32 = 36.0;
const HEADER_H: f32 = 32.0;

pub fn show(
    ui: &mut Ui,
    input: &SidebarInput<'_>,
    filter: &mut String,
    collapsed: &mut Vec<PathBuf>,
    cache: &mut SidebarCache,
) -> SidebarOutput {
    let pal = Palette::of_ui(ui);
    let mut out = SidebarOutput::default();

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
    ui.add_space(8.0);

    ui.allocate_ui_with_layout(
        vec2(ui.available_width(), 36.0),
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
                    .prefix(icon_text(ICON_SEARCH, 17.0, pal.weak))
                    .hint_text("Search sessions…")
                    .align(egui::Align2::LEFT_CENTER)
                    .min_size(vec2(0.0, 36.0))
                    .desired_width(ui.available_width())
                    .margin(Margin::symmetric(10, 8))
                    .frame(
                        Frame::new()
                            .fill(pal.surface)
                            .corner_radius(11)
                            .stroke(Stroke::new(
                                1.0,
                                if focused { pal.accent } else { pal.stroke },
                            )),
                    ),
            );
        },
    );
    ui.add_space(8.0);
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
            ICON_FOLDER_OFF,
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
        ..
    } = cache;
    let stamp = (input.generation, groups.as_ptr() as usize);
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
                let is_collapsed = !filtering && collapsed.contains(&group.cwd);

                let pid = ui.make_persistent_id(("project", &group.cwd));
                let mut state = CollapsingState::load_with_default_open(ui.ctx(), pid, true);
                state.set_open(!is_collapsed);
                let open_t = state.openness(ui.ctx());
                let hdr = group_header(ui, group, row_count, open_t, &mut out);
                let mut menu_collapse = None;
                crate::theme::context_menu(&hdr.resp, 210.0, |ui| {
                    if menu_item(ui, ICON_ADD, "New session here", None, true, false).clicked() {
                        out.actions.push(SidebarAction::New(group.cwd.clone()));
                        out.interacted = true;
                        ui.close();
                    }
                    let focused = input.scope == Some(group.cwd.as_path());
                    if menu_item(
                        ui,
                        ICON_CENTER_FOCUS_STRONG,
                        "Focus this folder",
                        None,
                        !focused,
                        false,
                    )
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
                    if menu_item(ui, ICON_CONTENT_COPY, "Copy path", None, true, false).clicked() {
                        out.actions.push(SidebarAction::CopyText {
                            text: group.cwd.to_string_lossy().into_owned(),
                            what: "project path",
                        });
                        ui.close();
                    }
                    ui.separator();
                    if menu_item(ui, ICON_UNFOLD_LESS, "Collapse all", None, true, false).clicked()
                    {
                        menu_collapse = Some(true);
                        ui.close();
                    }
                    if menu_item(ui, ICON_UNFOLD_MORE, "Expand all", None, true, false).clicked() {
                        menu_collapse = Some(false);
                        ui.close();
                    }
                });
                match menu_collapse {
                    Some(true) => {
                        collapsed.clear();
                        collapsed.extend(groups.iter().map(|g| g.cwd.clone()));
                        out.collapsed_changed = true;
                    }
                    Some(false) => {
                        collapsed.clear();
                        out.collapsed_changed = true;
                    }
                    None => {}
                }
                if hdr.toggle && !filtering {
                    if let Some(i) = collapsed.iter().position(|c| *c == group.cwd) {
                        collapsed.remove(i);
                    } else {
                        collapsed.push(group.cwd.clone());
                    }
                    out.collapsed_changed = true;
                    out.interacted = true;
                    state.set_open(is_collapsed);
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
                        ICON_SEARCH_OFF,
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
    let (rect, resp) = ui.allocate_exact_size(vec2(32.0, 36.0), Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered(), 0.1);
    let p = ui.painter();
    p.rect_filled(rect, 10, pal.raised.gamma_multiply(0.85 * hov));
    if spinning {
        ui.put(
            Rect::from_center_size(rect.center(), vec2(16.0, 16.0)),
            crate::widgets::Spin::new(16.0, pal.accent_b),
        );
    } else {
        paint_icon(
            p,
            rect.center(),
            Align2::CENTER_CENTER,
            ICON_REFRESH,
            19.0,
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
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), HEADER_H), Sense::click());
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
        p.rect_filled(rect, 9, pal.raised.gamma_multiply(0.55 * hov));
        // Chevron, rotated by the open animation (0 = right, 1 = down).
        let c = pos2(rect.left() + 14.0, rect.center().y);
        let a = open_t * std::f32::consts::FRAC_PI_2;
        let rot = |x: f32, y: f32| c + vec2(x * a.cos() - y * a.sin(), x * a.sin() + y * a.cos());
        p.add(Shape::line(
            vec![rot(-2.5, -4.0), rot(2.5, 0.0), rot(-2.5, 4.0)],
            Stroke::new(1.7, theme::mix(pal.weak, pal.text_strong, hov)),
        ));
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
                font_id: FontId::proportional(14.0),
                color: pal.text_strong,
                ..Default::default()
            },
        );
        if !parent.is_empty() {
            job.append(
                &format!("  {parent}"),
                0.0,
                TextFormat {
                    font_id: FontId::proportional(11.5),
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
            FontId::proportional(11.5),
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
                ICON_ADD,
                17.0,
                theme::mix(pal.weak, pal.text_strong, plus_hov).gamma_multiply(hov),
            );
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
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
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
        // Status dot.
        let dot = pos2(pill.left() + 19.0, pill.center().y);
        if busy {
            // The spinner is placed below, once the painter borrow ends.
        } else if running {
            theme::glow_dot(p, dot, 3.5, pal.ok, theme::GLOW_REST);
        } else {
            p.circle_filled(dot, 2.8, pal.weak.gamma_multiply(0.45));
        }
        // Age chip.
        let age = p.layout_no_wrap(age_text.to_owned(), FontId::proportional(11.5), pal.weak);
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
            FontId::proportional(13.5),
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
    if busy {
        ui.put(
            Rect::from_center_size(
                pos2(rect.left() + 2.0 + 19.0, rect.center().y),
                vec2(12.0, 12.0),
            ),
            crate::widgets::Spin::new(12.0, pal.accent_b),
        );
    }
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    if resp.clicked() {
        out.actions.push(SidebarAction::Open(entry.clone()));
        out.interacted = true;
    }
    theme::context_menu(&resp, 230.0, |ui| {
        fn copy(ui: &mut Ui, out: &mut SidebarOutput, text: String, what: &'static str) {
            out.actions.push(SidebarAction::CopyText { text, what });
            ui.close();
        }
        if menu_item(ui, ICON_PLAY_ARROW, "Open", None, true, false).clicked() {
            out.actions.push(SidebarAction::Open(entry.clone()));
            out.interacted = true;
            ui.close();
        }
        if menu_item(ui, ICON_ACCOUNT_TREE, "Session tree…", None, true, false).clicked() {
            out.actions.push(SidebarAction::Tree(entry.clone()));
            ui.close();
        }
        ui.separator();
        if menu_item(ui, ICON_DESCRIPTION, "Copy session path", None, true, false).clicked() {
            copy(
                ui,
                out,
                entry.path.to_string_lossy().into_owned(),
                "session path",
            );
        }
        if menu_item(ui, ICON_KEY, "Copy session id", None, true, false).clicked() {
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
        if menu_item(ui, ICON_OPEN_IN_NEW, "Show session file", None, true, false).clicked() {
            let dir = entry
                .path
                .parent()
                .map_or_else(|| entry.path.clone(), Path::to_path_buf);
            out.actions.push(SidebarAction::OpenFolder(dir));
            ui.close();
        }
        if input.omp_available {
            ui.separator();
            let idle = !input.busy.contains(&entry.path);
            for (kind, icon, label) in [
                (TransferKind::Copy, ICON_FILE_COPY, "Copy to project"),
                (TransferKind::Move, ICON_DRIVE_FILE_MOVE, "Move to project"),
            ] {
                transfer_menu(ui, entry, (kind, icon, label), idle, input, out);
            }
        }
        if input.omp_available {
            ui.separator();
            let idle = !input.busy.contains(&entry.path);
            if menu_item(ui, ICON_DELETE, "Delete session…", None, idle, true).clicked() {
                out.actions.push(SidebarAction::Delete(entry.clone()));
                ui.close();
            }
        }
        if running && input.can_stop {
            ui.separator();
            if menu_item(ui, ICON_RESTART_ALT, "Restart session", None, true, false).clicked() {
                out.actions.push(SidebarAction::Restart(entry.clone()));
                ui.close();
            }
            if menu_item(
                ui,
                ICON_STOP_CIRCLE,
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
    });
    resp.on_hover_ui(|ui| {
        ui.label(entry.label());
        ui.weak(format!("id: {}", entry.id));
        ui.weak(format!("cwd: {}", entry.cwd.display()));
        ui.weak(format!("file: {}", entry.path.display()));
    });
}

/// "Copy to project" / "Move to project" submenu: every other project, then "Choose folder…".
fn transfer_menu(
    ui: &mut Ui,
    entry: &SessionEntry,
    spec: (TransferKind, MaterialIcon, &str),
    enabled: bool,
    input: &SidebarInput<'_>,
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
                icon_text(icon, 17.0, pal.accent),
                RichText::new(label).color(pal.text),
            ),
            |ui| {
                ui.set_min_width(260.0);
                let groups = input.groups.unwrap_or(&[]);
                let targets = transfer::targets(groups, &entry.cwd);
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
}
