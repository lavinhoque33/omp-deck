//! The Changes panel: view state, the file sections, and the virtualised diff viewer.
//!
//! The pure parts (section grouping, selection following a refreshed listing, the diff row model
//! with old/new line numbers) are unit-tested; `show` only paints them. The app glue (workers,
//! refresh rules, confirmations) lives in `app/changes.rs`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use deck_core::changes::{Change, ChangedFile, Changes, DiffSide};
use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align, Align2, Color32, CursorIcon, FontId, Galley, Layout, Rect, RichText, ScrollArea, Sense,
    TextStyle, Ui, pos2, vec2,
};

use crate::badge::{self, Hue};
use crate::icons::{
    ICON_CHEVRON_DOWN, ICON_CHEVRON_RIGHT, ICON_COPY, ICON_FOLDER_GIT_2, ICON_FOLDER_OPEN,
    ICON_GIT_BRANCH, ICON_MESSAGES_SQUARE, ICON_MINUS, ICON_PENCIL, ICON_PLUS, ICON_REFRESH_CW,
    ICON_ROTATE_CCW, ICON_TRASH_2, ICON_X, Icon,
};
use crate::review::{Comment, Loc, MAX_QUOTE_LINES, Review, Reviews, Side, Span};
use crate::theme::{self, Palette};
use crate::widgets::{icon_text, paint_icon};
use crate::{card, tips};

/// Columns of a diff line that are laid out; the rest is cut with an ellipsis (minified files).
const MAX_COLS: usize = 1000;
/// Height of one file row.
const FILE_ROW_H: f32 = 24.0;

// ------------------------------------------------------------------ sections

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Section {
    Conflicted,
    Staged,
    Changes,
    Untracked,
}

impl Section {
    pub const ALL: [Section; 4] = [
        Section::Conflicted,
        Section::Staged,
        Section::Changes,
        Section::Untracked,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Section::Conflicted => "Conflicted",
            Section::Staged => "Staged",
            Section::Changes => "Changes",
            Section::Untracked => "Untracked",
        }
    }

    /// Which diff shows this section's files.
    pub fn side(self) -> DiffSide {
        match self {
            Section::Staged => DiffSide::Staged,
            Section::Conflicted | Section::Changes => DiffSide::Unstaged,
            Section::Untracked => DiffSide::Untracked,
        }
    }

    fn slot(self) -> usize {
        match self {
            Section::Conflicted => 0,
            Section::Staged => 1,
            Section::Changes => 2,
            Section::Untracked => 3,
        }
    }
}

/// A file as shown in one section (a file with staged and unstaged edits is in two).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileKey {
    pub path: String,
    pub section: Section,
}

/// One non-empty section: indices into the listing, sorted by path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub section: Section,
    pub files: Vec<usize>,
}

/// Splits a listing into its non-empty sections, in display order.
pub fn group(files: &[ChangedFile]) -> Vec<Group> {
    let mut out: Vec<Group> = Section::ALL
        .iter()
        .map(|&section| Group {
            section,
            files: Vec::new(),
        })
        .collect();
    for (i, f) in files.iter().enumerate() {
        if f.conflicted {
            out[Section::Conflicted.slot()].files.push(i);
        } else if f.untracked {
            out[Section::Untracked.slot()].files.push(i);
        } else {
            if f.staged.is_some() {
                out[Section::Staged.slot()].files.push(i);
            }
            if f.unstaged.is_some() {
                out[Section::Changes.slot()].files.push(i);
            }
        }
    }
    for g in &mut out {
        g.files.sort_by(|&a, &b| files[a].path.cmp(&files[b].path));
    }
    out.retain(|g| !g.files.is_empty());
    out
}

/// Index of the file `key` names, when the listing still has it in that section.
pub fn find(groups: &[Group], files: &[ChangedFile], key: &FileKey) -> Option<usize> {
    groups
        .iter()
        .filter(|g| g.section == key.section)
        .flat_map(|g| g.files.iter().copied())
        .find(|&i| files[i].path == key.path)
}

/// The selection after a refresh: the same file and section, else the same path in another
/// section (it was staged or unstaged), else nothing.
pub fn follow_selection(
    groups: &[Group],
    files: &[ChangedFile],
    prev: Option<&FileKey>,
) -> Option<FileKey> {
    let prev = prev?;
    if find(groups, files, prev).is_some() {
        return Some(prev.clone());
    }
    groups.iter().find_map(|g| {
        g.files
            .iter()
            .find(|&&i| files[i].path == prev.path)
            .map(|_| FileKey {
                path: prev.path.clone(),
                section: g.section,
            })
    })
}

/// The change letter shown for `file` in `section`.
pub fn letter(file: &ChangedFile, section: Section) -> char {
    let change = match section {
        Section::Conflicted => return '!',
        Section::Untracked => return '?',
        Section::Staged => file.staged,
        Section::Changes => file.unstaged,
    };
    match change {
        Some(Change::Modified) | None => 'M',
        Some(Change::Added) => 'A',
        Some(Change::Deleted) => 'D',
        Some(Change::Renamed) => 'R',
        Some(Change::Copied) => 'C',
        Some(Change::TypeChanged) => 'T',
    }
}

// ------------------------------------------------------------------ diff row model

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    /// `diff --git`, rename / mode lines.
    Meta,
    Hunk,
    Context,
    Add,
    Del,
    /// `\ No newline at end of file`.
    NoNewline,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub kind: RowKind,
    pub old: Option<u32>,
    pub new: Option<u32>,
    start: usize,
    end: usize,
}

/// A parsed unified diff: rows with old/new line numbers over the original text, plus a cache of
/// the laid-out galleys of the rows that were visible.
pub struct DiffModel {
    text: String,
    rows: Vec<Row>,
    /// Digits of the largest line number.
    gutter: usize,
    /// Longest line in columns (capped).
    max_cols: usize,
    cache: Vec<Option<Arc<Galley>>>,
    cache_key: u64,
}

fn parse_range(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// `@@ -a[,b] +c[,d] @@` → `(a, b, c, d)`.
fn parse_hunk(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let (a, b) = parse_range(old)?;
    let (c, d) = parse_range(new)?;
    Some((a, b, c, d))
}

fn digits(n: u32) -> usize {
    n.max(1).ilog10() as usize + 1
}

impl DiffModel {
    pub fn parse(text: String) -> DiffModel {
        let mut rows = Vec::new();
        let (mut old_n, mut new_n) = (0u32, 0u32);
        let (mut rem_old, mut rem_new) = (0u32, 0u32);
        let mut loose = false; // inside a combined (`@@@`) conflict diff: no numbering
        let mut max_no = 0u32;
        let mut max_cols = 0usize;
        let mut pos = 0usize;
        for raw in text.split_inclusive('\n') {
            let start = pos;
            pos += raw.len();
            let line = raw.strip_suffix('\n').unwrap_or(raw);
            let line = line.strip_suffix('\r').unwrap_or(line);
            let end = start + line.len();
            let first = line.as_bytes().first().copied();
            let mut row = |kind, old, new| {
                rows.push(Row {
                    kind,
                    old,
                    new,
                    start,
                    end,
                });
            };
            if rem_old > 0 || rem_new > 0 {
                match first {
                    Some(b'+') => {
                        row(RowKind::Add, None, Some(new_n));
                        max_no = max_no.max(new_n);
                        new_n += 1;
                        rem_new = rem_new.saturating_sub(1);
                    }
                    Some(b'-') => {
                        row(RowKind::Del, Some(old_n), None);
                        max_no = max_no.max(old_n);
                        old_n += 1;
                        rem_old = rem_old.saturating_sub(1);
                    }
                    Some(b' ') | None => {
                        row(RowKind::Context, Some(old_n), Some(new_n));
                        max_no = max_no.max(old_n).max(new_n);
                        old_n += 1;
                        new_n += 1;
                        rem_old = rem_old.saturating_sub(1);
                        rem_new = rem_new.saturating_sub(1);
                    }
                    Some(b'\\') => row(RowKind::NoNewline, None, None),
                    Some(_) => {
                        // Counts promised more lines than came: treat as header text.
                        rem_old = 0;
                        rem_new = 0;
                        row(RowKind::Meta, None, None);
                    }
                }
            } else if let Some((a, b, c, d)) = parse_hunk(line) {
                loose = false;
                (old_n, new_n, rem_old, rem_new) = (a, c, b, d);
                row(RowKind::Hunk, None, None);
            } else if line.starts_with("@@@") {
                loose = true;
                row(RowKind::Hunk, None, None);
            } else if line.starts_with("diff ") {
                loose = false;
                row(RowKind::Meta, None, None);
            } else if first == Some(b'\\') {
                row(RowKind::NoNewline, None, None);
            } else if loose {
                let prefix = &line.as_bytes()[..line.len().min(2)];
                let kind = if prefix.contains(&b'+') {
                    RowKind::Add
                } else if prefix.contains(&b'-') {
                    RowKind::Del
                } else {
                    RowKind::Context
                };
                row(kind, None, None);
            } else if line.starts_with("index ")
                || line.starts_with("--- ")
                || line.starts_with("+++ ")
            {
                // The panel header names the file; these lines add nothing.
                continue;
            } else {
                row(RowKind::Meta, None, None);
            }
            max_cols = max_cols.max(line.chars().take(MAX_COLS + 1).count().min(MAX_COLS));
        }
        let n = rows.len();
        DiffModel {
            text,
            rows,
            gutter: digits(max_no),
            max_cols,
            cache: vec![None; n],
            cache_key: 0,
        }
    }

    #[cfg(test)]
    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn text_of(&self, row: &Row) -> &str {
        &self.text[row.start..row.end]
    }

    pub fn source(&self) -> &str {
        &self.text
    }

    /// Side and line number of a row a comment can point at: added/context rows on the new
    /// side, deleted rows on the old one; headers and unnumbered (conflict) rows are not.
    pub fn commentable(&self, i: usize) -> Option<(Side, u32)> {
        let r = self.rows.get(i)?;
        match r.kind {
            RowKind::Add | RowKind::Context => r.new.map(|n| (Side::New, n)),
            RowKind::Del => r.old.map(|n| (Side::Old, n)),
            _ => None,
        }
    }

    /// Whether row `i` is the row of `line` on `side`.
    pub fn is_row(&self, i: usize, side: Side, line: u32) -> bool {
        self.commentable(i) == Some((side, line))
    }

    pub fn find_row(&self, side: Side, line: u32) -> Option<usize> {
        (0..self.rows.len()).find(|&i| self.is_row(i, side, line))
    }

    /// The lines between rows `a` and `b` (either order) on their common side, with their quoted
    /// text. `None` when a row cannot be commented or the rows are on different sides. Rows of the
    /// other side in between (a deleted line inside a new-side range) are not part of it.
    pub fn select(&self, a: usize, b: usize) -> Option<Span> {
        let (side, _) = self.commentable(a)?;
        if self.commentable(b)?.0 != side {
            return None;
        }
        let (mut start, mut end, mut count) = (None, 0, 0usize);
        let mut quote = Vec::new();
        for r in a.min(b)..=a.max(b) {
            let Some((s, line)) = self.commentable(r) else {
                continue;
            };
            if s != side {
                continue;
            }
            start.get_or_insert(line);
            end = line;
            count += 1;
            if quote.len() < MAX_QUOTE_LINES {
                let text = self.text_of(&self.rows[r]);
                quote.push(text.get(1..).unwrap_or("").to_owned());
            }
        }
        Some(Span {
            side,
            start: start?,
            end,
            quote,
            more: count > MAX_QUOTE_LINES,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Columns of the gutter (two line numbers, separators and the +/- marker).
    fn gutter_cols(&self) -> usize {
        self.gutter * 2 + 3
    }

    /// The layout of row `i`, built on first use and reused until the style changes.
    fn galley(
        &mut self,
        painter: &egui::Painter,
        i: usize,
        font: &FontId,
        pal: &Palette,
        key: u64,
    ) -> Arc<Galley> {
        if self.cache_key != key {
            self.cache.fill(None);
            self.cache_key = key;
        }
        if let Some(g) = &self.cache[i] {
            return g.clone();
        }
        let job = self.row_job(i, font, pal);
        let galley = painter.layout_job(job);
        self.cache[i] = Some(galley.clone());
        galley
    }

    fn row_job(&self, i: usize, font: &FontId, pal: &Palette) -> LayoutJob {
        let row = &self.rows[i];
        let line = self.text_of(row);
        let (marker, ink) = match row.kind {
            RowKind::Add => ('+', theme::mix(pal.text, pal.ok, 0.55)),
            RowKind::Del => ('-', theme::mix(pal.text, pal.danger, 0.55)),
            RowKind::Context => (' ', pal.text),
            RowKind::Hunk => (' ', pal.accent),
            RowKind::Meta | RowKind::NoNewline => (' ', pal.weak),
        };
        let g = self.gutter;
        let gutter = match (row.old, row.new) {
            (None, None) => format!("{:g$} {:g$}  ", "", "", g = g),
            (o, n) => format!(
                "{} {}  ",
                o.map_or_else(|| " ".repeat(g), |v| format!("{v:>g$}")),
                n.map_or_else(|| " ".repeat(g), |v| format!("{v:>g$}")),
            ),
        };
        let body = match row.kind {
            // The marker is part of the line for Add/Del/Context; hunk/meta rows are whole.
            RowKind::Add | RowKind::Del | RowKind::Context => line.get(1..).unwrap_or(""),
            _ => line,
        };
        let mut shown = String::with_capacity(body.len() + 2);
        shown.push(marker);
        for (n, c) in body.chars().enumerate() {
            if n >= MAX_COLS {
                shown.push('…');
                break;
            }
            if c == '\t' {
                shown.push_str("    ");
            } else if c.is_control() {
                shown.push('·');
            } else {
                shown.push(c);
            }
        }
        let mut job = LayoutJob::default();
        job.wrap.max_width = f32::INFINITY;
        job.append(
            &gutter,
            0.0,
            TextFormat::simple(font.clone(), pal.weak.gamma_multiply(0.8)),
        );
        job.append(&shown, 0.0, TextFormat::simple(font.clone(), ink));
        job
    }
}

fn row_bg(kind: RowKind, pal: &Palette) -> Option<Color32> {
    match kind {
        RowKind::Add => Some(theme::mix(pal.panel, pal.ok, 0.16)),
        RowKind::Del => Some(theme::mix(pal.panel, pal.danger, 0.16)),
        RowKind::Hunk => Some(theme::mix(pal.panel, pal.accent, 0.10)),
        _ => None,
    }
}

// ------------------------------------------------------------------ view state

/// What the panel has to say about the listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadState {
    /// No terminal tab is active.
    NoTab,
    Loading,
    NotRepo,
    Failed(String),
    Ready,
}

/// A loaded diff with the facts the placeholders need.
pub struct ShownDiff {
    pub key: FileKey,
    pub model: DiffModel,
    pub binary: bool,
    pub truncated: bool,
    pub added: u32,
    pub removed: u32,
}

pub enum DiffPane {
    /// Nothing selected.
    Idle,
    Loading,
    Ready(Box<ShownDiff>),
    Failed(String),
}

/// A comment being written (or an existing one being edited).
pub struct Editor {
    pub kind: EditKind,
    pub draft: String,
    /// Ask the text edit for keyboard focus until it has it.
    focus: bool,
    /// Scroll the editor into view once (a diff row editor opened near the edge).
    reveal: bool,
    /// The text edit takes (or is about to take) keyboard input this frame.
    pub active: bool,
}

impl Editor {
    pub fn new(kind: EditKind) -> Editor {
        Editor {
            kind,
            draft: String::new(),
            focus: true,
            reveal: true,
            active: true,
        }
    }
}

pub enum EditKind {
    /// A new comment under a diff row; `at_row` is the row of the range's last line, `anchor_row`
    /// the row the range started at.
    Line {
        loc: Loc,
        at_row: usize,
        anchor_row: usize,
    },
    /// A new general comment (edited in the Review section).
    General,
    /// The body of comment `id` (edited in the Review section).
    Existing(u64),
}

pub struct ViewState {
    pub load: LoadState,
    pub root: Option<PathBuf>,
    pub branch: Option<String>,
    pub files: Vec<ChangedFile>,
    pub groups: Vec<Group>,
    pub selected: Option<FileKey>,
    pub diff: DiffPane,
    pub collapsed: HashSet<Section>,
    /// A write (stage, discard …) is running: the action buttons are disabled.
    pub busy: Option<String>,
    /// The last write's failure.
    pub notice: Option<String>,
    /// Review comments per repository root; they outlive refreshes, file and tab switches.
    pub reviews: Reviews,
    /// The comment being written or edited (one at a time).
    pub editor: Option<Editor>,
    /// Row of the last plain click in the shown diff: where a Shift+click range starts.
    pub anchor: Option<usize>,
    /// The "Review (N)" list is expanded.
    pub review_open: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            load: LoadState::NoTab,
            root: None,
            branch: None,
            files: Vec::new(),
            groups: Vec::new(),
            selected: None,
            diff: DiffPane::Idle,
            collapsed: HashSet::new(),
            busy: None,
            notice: None,
            reviews: Reviews::default(),
            editor: None,
            anchor: None,
            review_open: true,
        }
    }
}

impl ViewState {
    /// Takes a finished listing; the selection follows its file when it moved sections.
    pub fn apply_listing(&mut self, listing: Option<Changes>) {
        match listing {
            None => {
                self.load = LoadState::NotRepo;
                self.clear_listing();
            }
            Some(c) => {
                self.load = LoadState::Ready;
                self.root = Some(c.root);
                self.branch = c.branch;
                self.files = c.files;
                self.groups = group(&self.files);
                self.selected = follow_selection(&self.groups, &self.files, self.selected.as_ref());
                if self.selected.is_none() {
                    self.diff = DiffPane::Idle;
                }
            }
        }
    }

    pub fn clear_listing(&mut self) {
        self.root = None;
        self.branch = None;
        self.files.clear();
        self.groups.clear();
        self.selected = None;
        self.diff = DiffPane::Idle;
        self.editor = None;
        self.anchor = None;
    }

    pub fn selected_file(&self) -> Option<&ChangedFile> {
        let key = self.selected.as_ref()?;
        find(&self.groups, &self.files, key).map(|i| &self.files[i])
    }
}

// ------------------------------------------------------------------ painting

/// What the user asked for this frame; indices point into `ViewState::files`.
#[derive(Debug, PartialEq, Eq)]
pub enum ChangesAction {
    Select(FileKey),
    Stage(Vec<usize>),
    Unstage(Vec<usize>),
    Discard(Vec<usize>),
    DeleteUntracked(Vec<usize>),
    OpenFolder(usize),
    CopyPath(usize),
    CopyDiff,
    Refresh,
    /// Paste the review into omp's composer (keeps a draft, never submits).
    InsertReview,
    /// Clear the composer, paste the review and submit it.
    SendReview,
    Close,
}

fn mini_button(
    ui: &mut Ui,
    rect: Rect,
    id: egui::Id,
    icon: Icon,
    tip: &str,
    color: Color32,
    enabled: bool,
) -> bool {
    let resp = ui.interact(
        rect,
        id,
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    let pal = Palette::of_ui(ui);
    if resp.hovered() && enabled {
        ui.painter()
            .rect_filled(rect, 5, theme::hover_fill(pal).gamma_multiply(1.4));
    }
    paint_icon(
        ui.painter(),
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        15.0,
        if enabled {
            color
        } else {
            pal.weak.gamma_multiply(0.5)
        },
    );
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
        .clicked()
        && enabled
}

fn header_button(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool) -> bool {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::hover());
    let id = resp.id.with("btn");
    mini_button(ui, rect, id, icon, tip, pal.glyph, enabled)
}

/// The panel body. Returns what the user asked for.
pub fn show(ui: &mut Ui, st: &mut ViewState) -> Vec<ChangesAction> {
    let pal = Palette::of_ui(ui);
    let mut out = Vec::new();
    let ready = st.load == LoadState::Ready;
    let idle = st.busy.is_none();
    if let Some(ed) = st.editor.as_mut() {
        ed.active = false;
    }

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        ui.label(icon_text(ICON_GIT_BRANCH, 16.0, pal.glyph));
        ui.label(theme::strong("Changes").size(15.0).color(pal.text_strong));
        if let Some(b) = &st.branch {
            ui.label(RichText::new(b).small().color(pal.weak));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if header_button(ui, ICON_X, "Close (Ctrl+Shift+G)", true) {
                out.push(ChangesAction::Close);
            }
            if header_button(
                ui,
                ICON_COPY,
                "Copy diff (staged + unstaged)",
                ready && idle,
            ) {
                out.push(ChangesAction::CopyDiff);
            }
            if header_button(ui, ICON_REFRESH_CW, "Refresh", true) {
                out.push(ChangesAction::Refresh);
            }
        });
    });
    if let Some(root) = &st.root {
        let r = ui.label(
            RichText::new(deck_core::sessions::abbreviate_home(root))
                .small()
                .color(pal.weak),
        );
        card::hover(
            &r,
            320.0,
            false,
            &card::Header::new(ICON_FOLDER_GIT_2, "Repository"),
            |_| {},
            |ui| {
                card::kv(ui, 64.0, "Root", |ui| tips::path(ui, root));
                if let Some(b) = &st.branch {
                    card::kv(ui, 64.0, "Branch", |ui| {
                        badge::show(ui, Some(ICON_GIT_BRANCH), b, Hue::Blue, tips::VALUE);
                    });
                }
            },
        );
    }
    if let Some(busy) = &st.busy {
        ui.horizontal(|ui| {
            ui.add(crate::widgets::Spin::new(14.0, pal.accent_b));
            ui.label(RichText::new(busy).small().color(pal.weak));
        });
    }
    if let Some(n) = st.notice.clone() {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(pal.danger, RichText::new(n).small());
        });
    }
    ui.add_space(4.0);
    // The panel's height below the header: the Review list is sized from it, not from what is
    // left once the other regions took theirs.
    let total = ui.available_height();

    match st.load.clone() {
        LoadState::NoTab => message(ui, "Open a session tab to review its changes."),
        LoadState::Loading => {
            ui.horizontal(|ui| {
                ui.add(crate::widgets::Spin::new(16.0, pal.accent_b));
                ui.label(RichText::new("Reading git status…").color(pal.weak));
            });
        }
        LoadState::NotRepo => message(ui, "This folder is not inside a git repository."),
        LoadState::Failed(e) => {
            ui.colored_label(pal.danger, "Could not read the repository");
            ui.label(RichText::new(e).small().color(pal.weak));
        }
        LoadState::Ready if st.files.is_empty() => {
            // Comments of this repository stay reachable when the tree is clean.
            show_review(ui, st, &mut out, total);
            message(ui, "Working tree clean — nothing to review.");
        }
        LoadState::Ready => show_ready(ui, st, &mut out, total),
    }
    out
}

fn message(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.add_space(12.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(text).color(pal.weak));
    });
}

fn show_ready(ui: &mut Ui, st: &mut ViewState, out: &mut Vec<ChangesAction>, total: f32) {
    let share = (ui.available_height() - 120.0).max(96.0);
    // The file list is as tall as its rows, at most 45% of the panel (it scrolls beyond that);
    // a remembered divider position would keep a stale size when files come and go.
    let content = list_rows(st) as f32 * FILE_ROW_H + 6.0;
    let height = content.min(share * 0.45).max(FILE_ROW_H * 2.0);
    let enabled = st.busy.is_none();
    egui::Panel::top("changes-files")
        .resizable(false)
        .exact_size(height)
        .frame(egui::Frame::NONE)
        .show_separator_line(true)
        .show(ui, |ui| show_files(ui, st, out, enabled));
    show_review(ui, st, out, total);
    show_diff_pane(ui, st, out);
}

/// What a logical row of the file list is.
#[derive(Debug, PartialEq, Eq)]
enum ListRow {
    /// Section header of `groups[gi]`.
    Header(usize),
    /// File `files[i]` of `groups[gi]`.
    File(usize, usize),
}

/// Header rows plus the files of every expanded section.
fn list_rows(st: &ViewState) -> usize {
    st.groups
        .iter()
        .map(|g| {
            1 + if st.collapsed.contains(&g.section) {
                0
            } else {
                g.files.len()
            }
        })
        .sum()
}

/// The row at logical index `row`, found by walking the (at most four) sections.
fn row_at(st: &ViewState, mut row: usize) -> Option<ListRow> {
    for (gi, g) in st.groups.iter().enumerate() {
        if row == 0 {
            return Some(ListRow::Header(gi));
        }
        row -= 1;
        if st.collapsed.contains(&g.section) {
            continue;
        }
        if row < g.files.len() {
            return Some(ListRow::File(gi, g.files[row]));
        }
        row -= g.files.len();
    }
    None
}

fn show_files(ui: &mut Ui, st: &mut ViewState, out: &mut Vec<ChangesAction>, enabled: bool) {
    ui.spacing_mut().item_spacing.y = 0.0;
    let total = list_rows(st);
    ScrollArea::vertical()
        .id_salt("changes-file-list")
        .auto_shrink([false, false])
        .show_rows(ui, FILE_ROW_H, total, |ui, range| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for r in range {
                match row_at(st, r) {
                    Some(ListRow::Header(gi)) => section_header(ui, st, gi, enabled, out),
                    Some(ListRow::File(gi, i)) => {
                        let section = st.groups[gi].section;
                        file_row(ui, st, section, i, enabled, out);
                    }
                    None => break,
                }
            }
        });
}

fn section_header(
    ui: &mut Ui,
    st: &mut ViewState,
    gi: usize,
    enabled: bool,
    out: &mut Vec<ChangesAction>,
) {
    let (section, count) = (st.groups[gi].section, st.groups[gi].files.len());
    let pal = Palette::of_ui(ui);
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), FILE_ROW_H), Sense::click());
    let open = !st.collapsed.contains(&section);
    if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 0, theme::hover_fill(pal).gamma_multiply(0.6));
    }
    paint_icon(
        ui.painter(),
        pos2(rect.left() + 12.0, rect.center().y),
        Align2::CENTER_CENTER,
        if open {
            ICON_CHEVRON_DOWN
        } else {
            ICON_CHEVRON_RIGHT
        },
        14.0,
        pal.weak,
    );
    ui.painter().text(
        pos2(rect.left() + 24.0, rect.center().y),
        Align2::LEFT_CENTER,
        format!("{}  {count}", section.label().to_uppercase()),
        theme::font(11.5),
        pal.weak,
    );
    let all = |st: &ViewState| st.groups[gi].files.clone();
    let mut x = rect.right() - 6.0;
    let mut button = |ui: &mut Ui, icon: Icon, tip: &str, color: Color32, id: &str| -> bool {
        let r = Rect::from_min_size(pos2(x - 22.0, rect.center().y - 11.0), vec2(22.0, 22.0));
        x -= 24.0;
        mini_button(ui, r, resp.id.with(id), icon, tip, color, enabled)
    };
    match section {
        Section::Conflicted => {
            if button(ui, ICON_PLUS, "Mark all resolved (stage)", pal.ok, "stage") {
                out.push(ChangesAction::Stage(all(st)));
            }
        }
        Section::Staged => {
            if button(ui, ICON_MINUS, "Unstage all", pal.warn, "unstage") {
                out.push(ChangesAction::Unstage(all(st)));
            }
        }
        Section::Changes => {
            if button(
                ui,
                ICON_ROTATE_CCW,
                "Discard all changes…",
                pal.danger,
                "discard",
            ) {
                out.push(ChangesAction::Discard(all(st)));
            }
            if button(ui, ICON_PLUS, "Stage all", pal.ok, "stage") {
                out.push(ChangesAction::Stage(all(st)));
            }
        }
        Section::Untracked => {
            if button(
                ui,
                ICON_TRASH_2,
                "Delete all untracked files…",
                pal.danger,
                "delete",
            ) {
                out.push(ChangesAction::DeleteUntracked(all(st)));
            }
            if button(ui, ICON_PLUS, "Stage all (add)", pal.ok, "stage") {
                out.push(ChangesAction::Stage(all(st)));
            }
        }
    }
    if resp.clicked() {
        if open {
            st.collapsed.insert(section);
        } else {
            st.collapsed.remove(&section);
        }
    }
}

fn file_row(
    ui: &mut Ui,
    st: &ViewState,
    section: Section,
    i: usize,
    enabled: bool,
    out: &mut Vec<ChangesAction>,
) {
    let pal = Palette::of_ui(ui);
    let file = &st.files[i];
    let key = FileKey {
        path: file.path.clone(),
        section,
    };
    let selected = st.selected.as_ref() == Some(&key);
    let (rect, resp) =
        ui.allocate_exact_size(vec2(ui.available_width(), FILE_ROW_H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let hot = resp.hovered() || selected;
    if selected {
        ui.painter()
            .rect_filled(rect, 0, theme::accent_fill(pal, 0.18));
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(rect, 0, theme::hover_fill(pal).gamma_multiply(0.7));
    }
    let ch = letter(file, section);
    let ch_color = match section {
        Section::Conflicted => pal.danger,
        Section::Staged => pal.ok,
        Section::Changes => match ch {
            'D' => pal.danger,
            _ => pal.warn,
        },
        Section::Untracked => pal.weak,
    };
    ui.painter().text(
        pos2(rect.left() + 14.0, rect.center().y),
        Align2::CENTER_CENTER,
        ch,
        theme::font(12.5),
        ch_color,
    );
    // Buttons for this row, from the right edge.
    let mut x = rect.right() - 6.0;
    let mut button = |ui: &mut Ui, icon: Icon, tip: &str, color: Color32, id: &str| -> bool {
        let r = Rect::from_min_size(pos2(x - 22.0, rect.center().y - 11.0), vec2(22.0, 22.0));
        x -= 24.0;
        mini_button(ui, r, resp.id.with(id), icon, tip, color, enabled)
    };
    let mut reserved = 8.0;
    if hot {
        let ids = vec![i];
        match section {
            Section::Conflicted => {
                if button(ui, ICON_PLUS, "Mark resolved (stage)", pal.ok, "stage") {
                    out.push(ChangesAction::Stage(ids));
                }
                reserved = 30.0;
            }
            Section::Staged => {
                if button(ui, ICON_MINUS, "Unstage", pal.warn, "unstage") {
                    out.push(ChangesAction::Unstage(ids));
                }
                reserved = 30.0;
            }
            Section::Changes => {
                if button(
                    ui,
                    ICON_ROTATE_CCW,
                    "Discard changes…",
                    pal.danger,
                    "discard",
                ) {
                    out.push(ChangesAction::Discard(ids.clone()));
                }
                if button(ui, ICON_PLUS, "Stage", pal.ok, "stage") {
                    out.push(ChangesAction::Stage(ids));
                }
                reserved = 54.0;
            }
            Section::Untracked => {
                if button(ui, ICON_TRASH_2, "Delete file…", pal.danger, "delete") {
                    out.push(ChangesAction::DeleteUntracked(ids.clone()));
                }
                if button(ui, ICON_PLUS, "Stage (add)", pal.ok, "stage") {
                    out.push(ChangesAction::Stage(ids));
                }
                reserved = 54.0;
            }
        }
    }
    // Name (strong) and folder (weak) on one elided line; a rename shows where it came from.
    let (dir, name) = match file.path.trim_end_matches('/').rsplit_once('/') {
        Some((d, n)) => (format!("{d}/"), n.to_owned()),
        None => (String::new(), file.path.clone()),
    };
    let name = if file.is_dir && !name.ends_with('/') {
        format!("{name}/")
    } else {
        name
    };
    let mut job = LayoutJob::default();
    job.wrap.max_width = (rect.width() - 30.0 - reserved).max(20.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.append(
        &name,
        0.0,
        TextFormat::simple(theme::font(13.0), pal.text_strong),
    );
    if !dir.is_empty() {
        job.append(
            &format!("  {dir}"),
            0.0,
            TextFormat::simple(theme::font(11.5), pal.weak),
        );
    }
    if let Some(orig) = &file.orig_path {
        job.append(
            &format!("  ← {orig}"),
            0.0,
            TextFormat::simple(theme::font(11.5), pal.weak),
        );
    }
    let galley = ui.painter().layout_job(job);
    ui.painter().galley(
        pos2(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0),
        galley,
        pal.text,
    );
    if resp.clicked() {
        out.push(ChangesAction::Select(key));
    }
    crate::theme::context_menu(&resp, 190.0, |ui| {
        if crate::widgets::menu_item(
            ui,
            ICON_FOLDER_OPEN,
            "Open containing folder",
            None,
            true,
            false,
        )
        .clicked()
        {
            out.push(ChangesAction::OpenFolder(i));
            ui.close();
        }
        if crate::widgets::menu_item(ui, ICON_COPY, "Copy path", None, true, false).clicked() {
            out.push(ChangesAction::CopyPath(i));
            ui.close();
        }
    });
}

fn show_diff_pane(ui: &mut Ui, st: &mut ViewState, _out: &mut Vec<ChangesAction>) {
    let pal = Palette::of_ui(ui);
    ui.add_space(4.0);
    let side_label = |s: Section| match s {
        Section::Staged => "staged",
        Section::Untracked => "untracked",
        Section::Conflicted => "conflicted",
        Section::Changes => "unstaged",
    };
    match &mut st.diff {
        DiffPane::Idle => message(ui, "Select a file to see its diff."),
        DiffPane::Loading => {
            ui.horizontal(|ui| {
                ui.add(crate::widgets::Spin::new(16.0, pal.accent_b));
                ui.label(RichText::new("Loading diff…").color(pal.weak));
            });
        }
        DiffPane::Failed(e) => {
            ui.colored_label(pal.danger, RichText::new(e.as_str()).small());
        }
        DiffPane::Ready(d) => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(theme::strong(&d.key.path).size(13.0).color(pal.text_strong));
                ui.label(
                    RichText::new(side_label(d.key.section))
                        .small()
                        .color(pal.weak),
                );
                if d.added > 0 {
                    ui.label(RichText::new(format!("+{}", d.added)).small().color(pal.ok));
                }
                if d.removed > 0 {
                    ui.label(
                        RichText::new(format!("-{}", d.removed))
                            .small()
                            .color(pal.danger),
                    );
                }
            });
            if d.binary {
                message(ui, "Binary file — no text preview.");
            } else if d.model.is_empty() {
                message(
                    ui,
                    "No textual changes (file mode or line endings changed only).",
                );
            } else {
                if d.truncated {
                    ui.label(
                        RichText::new("Diff is too large: showing the first part only.")
                            .small()
                            .color(pal.warn),
                    );
                }
                if let Some(root) = st.root.as_deref() {
                    let cx = DiffCtx {
                        key: &d.key,
                        review: st.reviews.of_mut(root),
                        editor: &mut st.editor,
                        anchor: &mut st.anchor,
                    };
                    show_rows(ui, &mut d.model, cx);
                }
            }
        }
    }
}

/// Height of the inline comment editor in pixels (rounded up to whole diff rows).
const EDITOR_H: f32 = 136.0;
/// Left margin of a diff row: the hover "+" and the comment marker live here.
const MARGIN_W: f32 = 20.0;

/// The review state `show_rows` reads and changes.
struct DiffCtx<'a> {
    key: &'a FileKey,
    review: &'a mut Review,
    editor: &'a mut Option<Editor>,
    anchor: &'a mut Option<usize>,
}

/// Row index of the open line editor in this diff, when it belongs to this file and its line is
/// still there (a refresh may have moved the row). Otherwise the editor is dropped.
fn placed_editor(model: &DiffModel, key: &FileKey, editor: &mut Option<Editor>) -> Option<usize> {
    let Some(Editor {
        kind: EditKind::Line { loc, at_row, .. },
        ..
    }) = editor
    else {
        return None;
    };
    let (side, line) = (loc.span.side, loc.span.end);
    let found = if loc.path != key.path {
        None
    } else if model.is_row(*at_row, side, line) {
        Some(*at_row)
    } else {
        model.find_row(side, line)
    };
    match found {
        Some(r) => {
            *at_row = r;
            Some(r)
        }
        None => {
            *editor = None;
            None
        }
    }
}

/// A click on row `i` (Shift: extend from the anchor, same file and side).
fn open_line_editor(model: &DiffModel, cx: &mut DiffCtx<'_>, i: usize, shift: bool) {
    let from = if shift {
        match cx.editor {
            Some(Editor {
                kind: EditKind::Line {
                    loc, anchor_row, ..
                },
                ..
            }) if loc.path == cx.key.path => Some(*anchor_row),
            _ => *cx.anchor,
        }
    } else {
        None
    };
    let ranged = from.and_then(|a| model.select(a, i).map(|s| (a, s)));
    let Some((anchor_row, span)) = ranged.or_else(|| model.select(i, i).map(|s| (i, s))) else {
        return;
    };
    *cx.anchor = Some(anchor_row);
    // Moving the editor to another row keeps what was typed.
    let draft = match cx.editor.take() {
        Some(Editor {
            kind: EditKind::Line { .. },
            draft,
            ..
        }) => draft,
        _ => String::new(),
    };
    let mut ed = Editor::new(EditKind::Line {
        loc: Loc {
            path: cx.key.path.clone(),
            section: cx.key.section,
            span,
        },
        at_row: i.max(anchor_row),
        anchor_row,
    });
    ed.draft = draft;
    *cx.editor = Some(ed);
}

/// Paints the visible rows only; wide lines scroll horizontally. A click on a numbered row opens
/// the comment editor under it: the editor takes `slots` virtual rows after its row, so the
/// virtualised row list keeps its uniform row height.
fn show_rows(ui: &mut Ui, model: &mut DiffModel, mut cx: DiffCtx<'_>) {
    let pal = Palette::of_ui(ui);
    let font = TextStyle::Monospace.resolve(ui.style());
    let (glyph, row_h) = ui
        .ctx()
        .fonts_mut(|f| (f.glyph_width(&font, '0'), f.row_height(&font)));
    let row_h = row_h.ceil() + 2.0;
    let content_w = (model.gutter_cols() + 1 + model.max_cols) as f32 * glyph + 16.0 + MARGIN_W;
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        font.size.to_bits().hash(&mut h);
        ui.visuals().dark_mode.hash(&mut h);
        ui.ctx().pixels_per_point().to_bits().hash(&mut h);
        theme::flat().hash(&mut h);
        h.finish() | 1
    };
    let placed = placed_editor(model, cx.key, cx.editor);
    let slots = if placed.is_some() {
        (EDITOR_H / row_h).ceil() as usize
    } else {
        0
    };
    let at = placed.map_or(0, |r| r + 1);
    let selected = match cx.editor {
        Some(Editor {
            kind: EditKind::Line { loc, .. },
            ..
        }) => Some((loc.span.side, loc.span.start, loc.span.end)),
        _ => None,
    };
    ui.spacing_mut().item_spacing.y = 0.0;
    let n = model.rows.len();
    let mut click: Option<(usize, bool)> = None;
    let mut outcome = EditorOutcome::None;
    ScrollArea::both()
        .id_salt("changes-diff")
        .auto_shrink([false, false])
        .show_rows(ui, row_h, n + slots, |ui, range| {
            ui.set_min_width(content_w);
            let width = ui.clip_rect().width().max(content_w);
            let y0 = ui.next_widget_position().y;
            for l in range.clone() {
                if l >= at && l < at + slots {
                    ui.allocate_exact_size(vec2(width, row_h), Sense::hover());
                    continue;
                }
                let i = if l >= at + slots { l - slots } else { l };
                let commentable = model.commentable(i);
                let (rect, resp) = ui.allocate_exact_size(
                    vec2(width, row_h),
                    if commentable.is_some() {
                        Sense::click()
                    } else {
                        Sense::hover()
                    },
                );
                let kind = model.rows[i].kind;
                if let Some(bg) = row_bg(kind, pal) {
                    ui.painter().rect_filled(rect, 0, bg);
                }
                if let (Some((side, line)), Some((s, a, b))) = (commentable, selected)
                    && side == s
                    && (a..=b).contains(&line)
                {
                    ui.painter()
                        .rect_filled(rect, 0, theme::accent_fill(pal, 0.22));
                }
                let galley = model.galley(ui.painter(), i, &font, pal, key);
                ui.painter().galley(
                    pos2(
                        rect.left() + 6.0 + MARGIN_W,
                        rect.center().y - galley.size().y / 2.0,
                    ),
                    galley,
                    pal.text,
                );
                let Some((side, line)) = commentable else {
                    continue;
                };
                let marks = |c: &&Comment| {
                    c.loc
                        .as_ref()
                        .is_some_and(|lc| lc.path == cx.key.path && lc.span.contains(side, line))
                };
                let n_marks = cx.review.comments().iter().filter(marks).count();
                let spot = pos2(rect.left() + MARGIN_W / 2.0 + 2.0, rect.center().y);
                if resp.hovered() {
                    ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                    paint_icon(
                        ui.painter(),
                        spot,
                        Align2::CENTER_CENTER,
                        ICON_PLUS,
                        14.0,
                        pal.accent,
                    );
                } else if n_marks > 0 {
                    ui.painter().circle_filled(spot, 3.5, pal.accent);
                }
                if resp.clicked() {
                    click = Some((i, ui.input(|s| s.modifiers.shift)));
                }
                if n_marks > 0 {
                    let review = &*cx.review;
                    let head = card::Header::new(
                        ICON_MESSAGES_SQUARE,
                        if n_marks == 1 {
                            "Review comment"
                        } else {
                            "Review comments"
                        },
                    )
                    .subtitle(&cx.key.path);
                    card::hover(
                        &resp,
                        360.0,
                        false,
                        &head,
                        |ui| {
                            let n = n_marks.to_string();
                            badge::show(ui, None, &n, Hue::Amber, tips::VALUE);
                        },
                        |ui| {
                            for c in review.comments().iter().filter(marks) {
                                if let Some(lc) = &c.loc {
                                    card::kv(ui, 64.0, "Lines", |ui| {
                                        badge::show(
                                            ui,
                                            None,
                                            &lc.span.label(),
                                            Hue::Teal,
                                            tips::VALUE,
                                        );
                                    });
                                }
                                tips::strong(ui, &c.body);
                            }
                            card::footer_hints(
                                ui,
                                &[("click", "comment"), ("shift+click", "extend the range")],
                            );
                        },
                    );
                }
            }
            if slots > 0 {
                let clip = ui.clip_rect();
                let top = y0 + (at as f32 - range.start as f32) * row_h;
                let rect = Rect::from_min_size(
                    pos2(clip.left() + 4.0, top + 2.0),
                    vec2((clip.width() - 8.0).max(240.0), slots as f32 * row_h - 4.0),
                );
                if let Some(ed) = cx.editor.as_mut() {
                    if ed.reveal {
                        // Just opened under a row at the edge of the view: scroll it into sight.
                        ed.reveal = false;
                        ui.scroll_to_rect(rect, None);
                    }
                    if range.start < at + slots && range.end > at {
                        let title = editor_title(ed);
                        outcome = ui
                            .scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                                comment_editor(ui, ed, &title, 60.0)
                            })
                            .inner;
                    }
                }
            }
        });
    finish_editor(cx.editor, cx.review, outcome);
    if let Some((i, shift)) = click {
        open_line_editor(model, &mut cx, i, shift);
    }
}

fn editor_title(ed: &Editor) -> String {
    match &ed.kind {
        EditKind::Line { loc, .. } => format!(
            "Comment on {}:{}{}",
            loc.path,
            loc.span.label(),
            if loc.span.side == Side::Old {
                " (removed)"
            } else {
                ""
            }
        ),
        EditKind::General => "General comment".to_owned(),
        EditKind::Existing(_) => "Edit comment".to_owned(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorOutcome {
    None,
    Save,
    Cancel,
}

/// The multi-line comment editor. Keeps keyboard focus once it has it (it asks again every frame
/// until the text edit really holds it, as the terminal may grab it back on the frames right after
/// a click), turns Esc into Cancel and Ctrl+Enter into Save, and records in `ed.active` whether it
/// takes text input this frame, so the app does not give the keyboard back to the terminal.
fn comment_editor(ui: &mut Ui, ed: &mut Editor, title: &str, text_h: f32) -> EditorOutcome {
    let pal = Palette::of_ui(ui);
    let id = ui.make_persistent_id("review-editor-text");
    let mut outcome = EditorOutcome::None;
    if ui.memory(|m| m.has_focus(id)) {
        if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
            outcome = EditorOutcome::Cancel;
        } else if !ed.draft.trim().is_empty()
            && ui.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::Enter))
        {
            outcome = EditorOutcome::Save;
        }
    }
    egui::Frame::new()
        .fill(theme::mix(pal.panel, pal.accent, 0.10))
        .stroke(egui::Stroke::new(1.0, pal.accent))
        .corner_radius(6)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(title).small().color(pal.weak));
            egui::ScrollArea::vertical()
                .id_salt("review-editor-scroll")
                .max_height(text_h)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    let r = ui.add(
                        egui::TextEdit::multiline(&mut ed.draft)
                            .id(id)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY)
                            .lock_focus(true)
                            .hint_text("Write a comment (Ctrl+Enter saves, Esc cancels)"),
                    );
                    // Esc must reach us instead of dropping focus.
                    ui.memory_mut(|m| {
                        m.set_focus_lock_filter(
                            id,
                            egui::EventFilter {
                                tab: true,
                                horizontal_arrows: true,
                                vertical_arrows: true,
                                escape: true,
                            },
                        );
                    });
                    if ed.focus {
                        r.request_focus();
                        if r.has_focus() {
                            ed.focus = false;
                        }
                    }
                    ed.active = ed.focus || r.has_focus();
                });
            ui.horizontal(|ui| {
                let save = ui.add_enabled(
                    !ed.draft.trim().is_empty(),
                    egui::Button::new(RichText::new("Save comment").color(theme::on_accent(pal)))
                        .fill(pal.accent),
                );
                if save.clicked() {
                    outcome = EditorOutcome::Save;
                }
                if ui.button("Cancel").clicked() {
                    outcome = EditorOutcome::Cancel;
                }
            });
        });
    outcome
}

/// Applies what the editor answered: a save adds/updates the comment and closes it.
fn finish_editor(editor: &mut Option<Editor>, review: &mut Review, outcome: EditorOutcome) {
    match outcome {
        EditorOutcome::None => {}
        EditorOutcome::Cancel => *editor = None,
        EditorOutcome::Save => {
            let Some(ed) = editor.take() else {
                return;
            };
            let saved = match &ed.kind {
                EditKind::Line { loc, .. } => review.add(Some(loc.clone()), &ed.draft).is_some(),
                EditKind::General => review.add(None, &ed.draft).is_some(),
                EditKind::Existing(id) => review.set_body(*id, &ed.draft),
            };
            if !saved {
                *editor = Some(ed);
            }
        }
    }
}

/// The "Review (N)" section under the file list: comments with edit/delete, general comment,
/// and the two ways to hand the review to omp.
fn show_review(ui: &mut Ui, st: &mut ViewState, out: &mut Vec<ChangesAction>, total: f32) {
    let pal = Palette::of_ui(ui);
    let ViewState {
        root,
        reviews,
        editor,
        review_open,
        files,
        groups,
        busy,
        ..
    } = st;
    let Some(root) = root.as_deref() else {
        return;
    };
    let review = reviews.of_mut(root);
    let n = review.len();
    let mut goto: Option<FileKey> = None;
    let mut delete: Option<u64> = None;
    let mut edit: Option<u64> = None;
    let mut outcome = EditorOutcome::None;
    egui::Panel::bottom("changes-review")
        .resizable(false)
        .frame(egui::Frame::NONE)
        .show_separator_line(true)
        .show(ui, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let chevron = if *review_open {
                    ICON_CHEVRON_DOWN
                } else {
                    ICON_CHEVRON_RIGHT
                };
                let a = ui.add(
                    egui::Label::new(icon_text(chevron, 14.0, pal.weak)).sense(Sense::click()),
                );
                let b = ui.add(
                    egui::Label::new(
                        theme::strong(format!("Review ({n})"))
                            .size(13.0)
                            .color(pal.text_strong),
                    )
                    .sense(Sense::click()),
                );
                if a.clicked() || b.clicked() {
                    *review_open = !*review_open;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let general = ui
                        .add(egui::Button::new(
                            RichText::new("+ General comment").small(),
                        ))
                        .on_hover_text("A comment that is not about a line");
                    if general.clicked() {
                        *review_open = true;
                        *editor = Some(Editor::new(EditKind::General));
                    }
                });
            });
            // Written in a region of its own (not the scrolling list), so it is always whole.
            let editing_here = matches!(
                editor,
                Some(Editor {
                    kind: EditKind::General | EditKind::Existing(_),
                    ..
                })
            );
            if editing_here && let Some(ed) = editor.as_mut() {
                let title = editor_title(ed);
                outcome = comment_editor(ui, ed, &title, 90.0);
            }
            if *review_open && n > 0 {
                // ~4 items of the panel's total height; less while an editor takes room.
                let share = if editing_here { 0.2 } else { 0.45 };
                egui::ScrollArea::vertical()
                    .id_salt("review-list")
                    .max_height((total * share).clamp(96.0, 300.0))
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for c in review.comments() {
                            ui.add_space(2.0);
                            egui::Frame::new()
                                .fill(theme::mix(pal.panel, pal.text, 0.05))
                                .corner_radius(6)
                                .inner_margin(egui::Margin::symmetric(8, 5))
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    review_item(
                                        ui,
                                        c,
                                        (files, groups),
                                        (&mut goto, &mut delete, &mut edit),
                                    );
                                });
                        }
                    });
            }
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                let ok = n > 0 && busy.is_none();
                if ui
                    .add_enabled(ok, egui::Button::new("Insert into omp"))
                    .on_hover_text(
                        "Paste the review into omp's composer at the cursor, without sending it",
                    )
                    .clicked()
                {
                    out.push(ChangesAction::InsertReview);
                }
                if ui
                    .add_enabled(
                        ok,
                        egui::Button::new(RichText::new("Send").color(theme::on_accent(pal)))
                            .fill(pal.accent),
                    )
                    .on_hover_text(
                        "Replace the composer's text with the review and send it (omp must be idle)",
                    )
                    .clicked()
                {
                    out.push(ChangesAction::SendReview);
                }
            });
            ui.add_space(4.0);
        });
    if let Some(id) = delete {
        review.remove(id);
        if matches!(editor, Some(Editor { kind: EditKind::Existing(e), .. }) if *e == id) {
            *editor = None;
        }
    }
    if let Some(id) = edit
        && let Some(c) = review.get(id)
    {
        let mut ed = Editor::new(EditKind::Existing(id));
        ed.draft.clone_from(&c.body);
        *editor = Some(ed);
        *review_open = true;
    }
    finish_editor(editor, review, outcome);
    if let Some(key) = goto {
        out.push(ChangesAction::Select(key));
    }
}

/// One comment of the list: where it points, its quote, the body (or its editor).
fn review_item(
    ui: &mut Ui,
    c: &Comment,
    listing: (&[ChangedFile], &[Group]),
    (goto, delete, edit): (&mut Option<FileKey>, &mut Option<u64>, &mut Option<u64>),
) {
    let pal = Palette::of_ui(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        match &c.loc {
            Some(l) => {
                let key = FileKey {
                    path: l.path.clone(),
                    section: l.section,
                };
                let here = find(listing.1, listing.0, &key).is_some();
                let text = format!(
                    "{}:{}{}",
                    l.path,
                    l.span.label(),
                    if l.span.side == Side::Old {
                        " (removed)"
                    } else {
                        ""
                    }
                );
                let r = ui.add(
                    egui::Label::new(RichText::new(text).small().strong().color(if here {
                        pal.text_strong
                    } else {
                        pal.weak
                    }))
                    .truncate()
                    .sense(if here {
                        Sense::click()
                    } else {
                        Sense::hover()
                    }),
                );
                if here {
                    if r.on_hover_text("Show this file's diff")
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .clicked()
                    {
                        *goto = Some(key);
                    }
                } else {
                    r.on_hover_text("This file is no longer in the changes");
                }
            }
            None => {
                ui.label(
                    RichText::new("General")
                        .small()
                        .strong()
                        .color(pal.text_strong),
                );
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if header_button(ui, ICON_TRASH_2, "Delete comment", true) {
                *delete = Some(c.id);
            }
            if header_button(ui, ICON_PENCIL, "Edit comment", true) {
                *edit = Some(c.id);
            }
        });
    });
    if let Some(l) = &c.loc {
        for q in l.span.quote.iter().take(1) {
            ui.add(
                egui::Label::new(
                    RichText::new(format!("> {q}"))
                        .small()
                        .monospace()
                        .color(pal.weak),
                )
                .truncate(),
            );
        }
        if l.span.quote.len() > 1 || l.span.more {
            ui.label(RichText::new("> …").small().monospace().color(pal.weak));
        }
    }
    ui.label(RichText::new(c.body.as_str()).color(pal.text));
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn file(
        path: &str,
        staged: Option<Change>,
        unstaged: Option<Change>,
        untracked: bool,
        conflicted: bool,
    ) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            os_path: OsString::from(path),
            orig_path: None,
            staged,
            unstaged,
            untracked,
            conflicted,
            is_dir: false,
        }
    }

    fn listing(files: Vec<ChangedFile>) -> Changes {
        Changes {
            root: PathBuf::from("/repo"),
            branch: Some("main".into()),
            files,
        }
    }

    #[test]
    fn sections_split_a_mixed_listing_and_count_both_sides_of_a_file() {
        let files = vec![
            file(
                "b.rs",
                Some(Change::Modified),
                Some(Change::Modified),
                false,
                false,
            ),
            file("a.rs", None, Some(Change::Deleted), false, false),
            file("new.txt", None, None, true, false),
            file("m.rs", Some(Change::Renamed), None, false, false),
            file(
                "clash.rs",
                Some(Change::Modified),
                Some(Change::Modified),
                false,
                true,
            ),
        ];
        let groups = group(&files);
        let shape: Vec<(Section, Vec<&str>)> = groups
            .iter()
            .map(|g| {
                (
                    g.section,
                    g.files.iter().map(|&i| files[i].path.as_str()).collect(),
                )
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (Section::Conflicted, vec!["clash.rs"]),
                (Section::Staged, vec!["b.rs", "m.rs"]),
                (Section::Changes, vec!["a.rs", "b.rs"]),
                (Section::Untracked, vec!["new.txt"]),
            ]
        );
    }

    #[test]
    fn logical_rows_skip_collapsed_sections_and_end_at_the_last_file() {
        let mut st = ViewState::default();
        st.apply_listing(Some(listing(vec![
            file("a", Some(Change::Modified), None, false, false),
            file(
                "b",
                Some(Change::Modified),
                Some(Change::Modified),
                false,
                false,
            ),
            file("u", None, None, true, false),
        ])));
        // Staged: a, b · Changes: b · Untracked: u  → 3 headers + 4 files
        assert_eq!(list_rows(&st), 7);
        assert_eq!(row_at(&st, 0), Some(ListRow::Header(0)));
        assert_eq!(row_at(&st, 2), Some(ListRow::File(0, 1)));
        assert_eq!(row_at(&st, 3), Some(ListRow::Header(1)));
        assert_eq!(row_at(&st, 6), Some(ListRow::File(2, 2)));
        assert_eq!(row_at(&st, 7), None);
        st.collapsed.insert(Section::Staged);
        assert_eq!(list_rows(&st), 5);
        assert_eq!(row_at(&st, 1), Some(ListRow::Header(1)));
    }

    #[test]
    fn empty_sections_are_left_out() {
        let files = vec![file("x", None, Some(Change::Modified), false, false)];
        let groups = group(&files);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].section, Section::Changes);
        assert!(group(&[]).is_empty());
    }

    #[test]
    fn selection_follows_a_file_to_the_section_it_moved_to() {
        let mut st = ViewState::default();
        st.apply_listing(Some(listing(vec![file(
            "a.rs",
            None,
            Some(Change::Modified),
            false,
            false,
        )])));
        st.selected = Some(FileKey {
            path: "a.rs".into(),
            section: Section::Changes,
        });
        // Staged: the unstaged side is gone.
        st.apply_listing(Some(listing(vec![file(
            "a.rs",
            Some(Change::Modified),
            None,
            false,
            false,
        )])));
        assert_eq!(
            st.selected,
            Some(FileKey {
                path: "a.rs".into(),
                section: Section::Staged
            })
        );
        // Committed: nothing left to select, the diff pane resets.
        st.apply_listing(Some(listing(Vec::new())));
        assert_eq!(st.selected, None);
        assert!(matches!(st.diff, DiffPane::Idle));
    }

    #[test]
    fn not_a_repo_drops_the_listing() {
        let mut st = ViewState::default();
        st.apply_listing(Some(listing(vec![file("a", None, None, true, false)])));
        st.apply_listing(None);
        assert_eq!(st.load, LoadState::NotRepo);
        assert!(st.files.is_empty() && st.groups.is_empty() && st.root.is_none());
    }

    #[test]
    fn diff_rows_carry_old_and_new_line_numbers() {
        let text = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -3,4 +3,4 @@ fn x()\n ctx\n-old\n+new\n+newer\n tail\n";
        let m = DiffModel::parse(text.to_owned());
        let rows: Vec<(RowKind, Option<u32>, Option<u32>, &str)> = m
            .rows()
            .iter()
            .map(|r| (r.kind, r.old, r.new, m.text_of(r)))
            .collect();
        assert_eq!(
            rows,
            vec![
                (RowKind::Meta, None, None, "diff --git a/f b/f"),
                (RowKind::Hunk, None, None, "@@ -3,4 +3,4 @@ fn x()"),
                (RowKind::Context, Some(3), Some(3), " ctx"),
                (RowKind::Del, Some(4), None, "-old"),
                (RowKind::Add, None, Some(4), "+new"),
                (RowKind::Add, None, Some(5), "+newer"),
                (RowKind::Context, Some(5), Some(6), " tail"),
            ]
        );
    }

    #[test]
    fn a_removed_line_starting_with_dashes_is_not_mistaken_for_a_file_header() {
        let text = "@@ -1,2 +1,1 @@\n--- not a header\n keep\n";
        let m = DiffModel::parse(text.to_owned());
        assert_eq!(m.rows().len(), 3);
        assert_eq!(m.rows()[1].kind, RowKind::Del);
        assert_eq!(m.rows()[1].old, Some(1));
        assert_eq!(m.rows()[2].new, Some(1));
    }

    #[test]
    fn several_hunks_and_files_restart_numbering_and_crlf_is_dropped() {
        let text = "diff --git a/x b/x\n@@ -1 +1 @@\n-a\r\n+b\r\n\\ No newline at end of file\n@@ -10,0 +11,2 @@\n+p\n+q\ndiff --git a/y b/y\nnew file mode 100644\n@@ -0,0 +1 @@\n+z\n";
        let m = DiffModel::parse(text.to_owned());
        let kinds: Vec<RowKind> = m.rows().iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RowKind::Meta,
                RowKind::Hunk,
                RowKind::Del,
                RowKind::Add,
                RowKind::NoNewline,
                RowKind::Hunk,
                RowKind::Add,
                RowKind::Add,
                RowKind::Meta,
                RowKind::Meta,
                RowKind::Hunk,
                RowKind::Add,
            ]
        );
        assert_eq!(m.text_of(&m.rows()[2]), "-a");
        assert_eq!(m.rows()[6].new, Some(11));
        assert_eq!(m.rows()[7].new, Some(12));
        assert_eq!(m.rows()[11].new, Some(1));
        assert_eq!(m.gutter, 2);
    }

    #[test]
    fn empty_text_has_no_rows() {
        assert!(DiffModel::parse(String::new()).is_empty());
    }

    #[test]
    fn combined_conflict_diffs_are_shown_without_numbers() {
        let text = "diff --cc f\n@@@ -1,2 -1,2 +1,4 @@@\n  a\n++<<<<<<< x\n +b\n- c\n";
        let m = DiffModel::parse(text.to_owned());
        let kinds: Vec<RowKind> = m.rows().iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            vec![
                RowKind::Meta,
                RowKind::Hunk,
                RowKind::Context,
                RowKind::Add,
                RowKind::Add,
                RowKind::Del,
            ]
        );
        assert!(m.rows().iter().all(|r| r.old.is_none() && r.new.is_none()));
    }

    #[test]
    fn letters_name_the_change_of_each_section() {
        let f = file(
            "a",
            Some(Change::Added),
            Some(Change::Deleted),
            false,
            false,
        );
        assert_eq!(letter(&f, Section::Staged), 'A');
        assert_eq!(letter(&f, Section::Changes), 'D');
        assert_eq!(letter(&f, Section::Untracked), '?');
        assert_eq!(letter(&f, Section::Conflicted), '!');
    }

    fn diff_text() -> String {
        "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -3,4 +3,4 @@ fn x()\n ctx\n-old\n+new\n+newer\n tail\n"
            .to_owned()
    }

    #[test]
    fn rows_belong_to_the_side_their_line_number_refers_to() {
        let m = DiffModel::parse(diff_text());
        // rows: 0 diff, 1 hunk, 2 ctx(3/3), 3 -old(4), 4 +new(4), 5 +newer(5), 6 tail(5/6)
        assert_eq!(m.commentable(0), None);
        assert_eq!(m.commentable(1), None);
        assert_eq!(m.commentable(2), Some((Side::New, 3)));
        assert_eq!(m.commentable(3), Some((Side::Old, 4)));
        assert_eq!(m.commentable(4), Some((Side::New, 4)));
        assert_eq!(m.commentable(6), Some((Side::New, 6)));
        assert_eq!(m.find_row(Side::Old, 4), Some(3));
        assert_eq!(m.find_row(Side::New, 4), Some(4));
        assert!(m.is_row(5, Side::New, 5) && !m.is_row(5, Side::Old, 5));
    }

    #[test]
    fn a_range_stays_on_one_side_and_skips_the_other_sides_rows() {
        let m = DiffModel::parse(diff_text());
        let s = m.select(4, 5).unwrap();
        assert_eq!((s.side, s.start, s.end), (Side::New, 4, 5));
        assert_eq!(s.quote, ["new", "newer"]);
        // The deleted row inside the range is not part of a new-side range.
        let s = m.select(6, 2).unwrap();
        assert_eq!((s.side, s.start, s.end), (Side::New, 3, 6));
        assert_eq!(s.quote, ["ctx", "new", "newer", "tail"]);
        // Old side: just the deleted line.
        let s = m.select(3, 3).unwrap();
        assert_eq!((s.side, s.start, s.end), (Side::Old, 4, 4));
        assert_eq!(s.quote, ["old"]);
        // Across sides, or onto a hunk/meta row: no range.
        assert_eq!(m.select(3, 4), None);
        assert_eq!(m.select(2, 1), None);
        assert_eq!(m.select(0, 2), None);
    }

    #[test]
    fn long_ranges_quote_twelve_lines_and_say_there_is_more() {
        let mut text = String::from("@@ -0,0 +1,20 @@\n");
        for i in 0..20 {
            text.push_str(&format!("+line {i}\n"));
        }
        let m = DiffModel::parse(text);
        let s = m.select(1, 20).unwrap();
        assert_eq!((s.start, s.end), (1, 20));
        assert_eq!(s.quote.len(), crate::review::MAX_QUOTE_LINES);
        assert_eq!(s.quote[11], "line 11");
        assert!(s.more);
        assert!(!m.select(1, 12).unwrap().more);
    }

    #[test]
    fn combined_conflict_rows_cannot_be_commented() {
        let m =
            DiffModel::parse("diff --cc f\n@@@ -1,2 -1,2 +1,4 @@@\n  a\n++<<<<<<< x\n".to_owned());
        assert!((0..m.rows().len()).all(|i| m.commentable(i).is_none()));
    }

    #[test]
    fn comments_survive_refreshes_and_file_switches_but_the_open_editor_does_not() {
        let mut st = ViewState::default();
        st.apply_listing(Some(listing(vec![file("a", None, None, true, false)])));
        let root = st.root.clone().unwrap();
        st.reviews.of_mut(&root).add(None, "keep me").unwrap();
        st.editor = Some(Editor::new(EditKind::General));
        st.apply_listing(Some(listing(vec![file("a", None, None, true, false)])));
        st.selected = Some(FileKey {
            path: "a".into(),
            section: Section::Untracked,
        });
        assert_eq!(st.reviews.of(&root).map(Review::len), Some(1));
        assert!(st.editor.is_some());
        st.clear_listing();
        assert!(st.editor.is_none());
        assert_eq!(st.reviews.of(&root).map(Review::len), Some(1));
    }
}
