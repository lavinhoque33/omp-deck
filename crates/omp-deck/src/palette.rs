//! The command palette (Ctrl+Shift+K) and the quick switcher (Ctrl+Shift+E): one modal overlay with a
//! search box and ranked rows, plus the in-house fuzzy scorer behind it. The app builds the
//! [`Item`]s (see `app/palette.rs`) and runs the [`Target`] the user picks; nothing here knows
//! what a command does.

use std::path::PathBuf;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align2, Color32, Context, FontId, Frame, Id, Key, Margin, Modal, Modifiers, Sense, Stroke,
    TextEdit, Ui, pos2, vec2,
};

use crate::icons::{ICON_SEARCH, Icon};
use crate::theme::{self, Palette};
use crate::widgets::paint_icon;

// ------------------------------------------------------------------ fuzzy scorer

const GAP_LEADING: i32 = -5;
const GAP_TRAILING: i32 = -5;
const GAP_INNER: i32 = -10;
const CONSECUTIVE: i32 = 1000;
const BONUS_SEGMENT: i32 = 900;
const BONUS_WORD: i32 = 800;
const BONUS_CAMEL: i32 = 700;
const BONUS_DOT: i32 = 600;
/// A matched character typed in the haystack's own case.
const BONUS_CASE: i32 = 10;
/// The whole text equals the query (ignoring case and spaces).
const BONUS_EXACT: i32 = 100_000;
/// Longer queries / texts are cut: the scorer allocates `query × text` cells.
const MAX_QUERY: usize = 48;
const MAX_TEXT: usize = 256;
const NEG: i32 = i32::MIN / 4;

/// A fuzzy hit: higher `score` is better; `hits` are char indices into the haystack (ascending).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub score: i32,
    pub hits: Vec<usize>,
}

/// Simple case folding: the first char of the lowercase mapping (keeps indices 1:1 with the text).
fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn bonus_at(chars: &[char], j: usize) -> i32 {
    let prev = if j == 0 { '/' } else { chars[j - 1] };
    let cur = chars[j];
    match prev {
        '/' | '\\' => BONUS_SEGMENT,
        ' ' | '-' | '_' | ':' | '(' | '…' | '·' => BONUS_WORD,
        '.' => BONUS_DOT,
        p if p.is_lowercase() && cur.is_uppercase() => BONUS_CAMEL,
        _ => 0,
    }
}

/// Case-folded subsequence match of `query` in `text` with the best alignment: consecutive runs,
/// word starts (after space `-` `_` `:`), path-segment starts (after `/` or `\`), camelCase humps
/// and dots score extra; gaps cost a little, long leading gaps cost slightly more than none.
/// Whitespace in `query` is ignored ("new wt" = "newwt"). `None` = not a subsequence.
pub fn fuzzy(query: &str, text: &str) -> Option<Match> {
    let q_orig: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .take(MAX_QUERY)
        .collect();
    if q_orig.is_empty() {
        return Some(Match {
            score: 0,
            hits: Vec::new(),
        });
    }
    let t_orig: Vec<char> = text.chars().take(MAX_TEXT).collect();
    let (m, n) = (q_orig.len(), t_orig.len());
    if m > n {
        return None;
    }
    let q: Vec<char> = q_orig.iter().copied().map(fold).collect();
    let t: Vec<char> = t_orig.iter().copied().map(fold).collect();
    // Cheap reject before the quadratic fill.
    let mut next = 0;
    for &c in &t {
        if next < m && c == q[next] {
            next += 1;
        }
    }
    if next < m {
        return None;
    }
    let bonus: Vec<i32> = (0..n).map(|j| bonus_at(&t_orig, j)).collect();
    let mut d = vec![NEG; m * n];
    let mut best = vec![NEG; m * n];
    for i in 0..m {
        let gap = if i == m - 1 { GAP_TRAILING } else { GAP_INNER };
        let mut prev = NEG;
        for j in 0..n {
            let idx = i * n + j;
            if q[i] == t[j] {
                let case = if q_orig[i] == t_orig[j] {
                    BONUS_CASE
                } else {
                    0
                };
                let score = if i == 0 {
                    j as i32 * GAP_LEADING + bonus[j] + case
                } else if j > 0 {
                    let up = (i - 1) * n + (j - 1);
                    (best[up] + bonus[j]).max(d[up] + CONSECUTIVE) + case
                } else {
                    NEG
                };
                d[idx] = score;
                prev = score.max(prev + gap);
            } else {
                prev += gap;
            }
            best[idx] = prev;
        }
    }
    let mut score = best[m * n - 1];
    if score <= NEG / 2 {
        return None;
    }
    // Backtrack (fzy): the last row's best alignment, preferring a forced consecutive link.
    let mut hits = vec![0usize; m];
    let mut required = false;
    let mut j = n;
    for i in (0..m).rev() {
        while j > 0 {
            j -= 1;
            let idx = i * n + j;
            if d[idx] > NEG / 2 && (required || d[idx] == best[idx]) {
                required = i > 0
                    && j > 0
                    && d[idx] - case_of(&q_orig, &t_orig, i, j)
                        == d[(i - 1) * n + (j - 1)] + CONSECUTIVE;
                hits[i] = j;
                break;
            }
        }
    }
    if t == q {
        score += BONUS_EXACT;
    }
    Some(Match { score, hits })
}

fn case_of(q: &[char], t: &[char], i: usize, j: usize) -> i32 {
    if q[i] == t[j] { BONUS_CASE } else { 0 }
}

// ------------------------------------------------------------------ items and ranking

/// What a row stands for; the app runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A palette command (stable id, also what `palette_recent` stores).
    Command(String),
    /// An open tab (by uid).
    Tab(u64),
    /// A session file.
    Session(PathBuf),
    /// A project folder.
    Project(PathBuf),
}

/// One selectable row.
#[derive(Clone, Debug)]
pub struct Item {
    pub target: Target,
    pub label: String,
    /// Secondary text (project, branch, path): searched with a small penalty, shown dim.
    pub detail: String,
    /// Keyboard shortcut shown at the right edge.
    pub hint: Option<&'static str>,
    pub icon: Icon,
    /// `Some(reason)` = greyed out; the reason is the tooltip.
    pub disabled: Option<&'static str>,
    /// A live omp process (green icon).
    pub running: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Commands,
    Switcher,
}

/// A row that survived the query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ranked {
    pub index: usize,
    pub score: i32,
    pub label_hits: Vec<usize>,
    pub detail_hits: Vec<usize>,
}

/// A detail-only match ranks below the same quality label match.
const DETAIL_PENALTY: i32 = 400;
/// Boost of the most recent command; each older one gets 25 less.
const RECENT_BOOST: i32 = 600;
const RECENT_STEP: i32 = 25;

fn recent_pos(item: &Item, recent: &[String]) -> Option<usize> {
    match &item.target {
        Target::Command(id) => recent.iter().position(|r| r == id),
        _ => None,
    }
}

/// Rows matching `query`, best first (disabled rows after enabled ones). An empty query keeps the
/// items' own order except that recent commands (newest first) come first.
pub fn rank(items: &[Item], query: &str, recent: &[String]) -> Vec<Ranked> {
    let mut out = Vec::new();
    if query.chars().all(char::is_whitespace) {
        for (index, item) in items.iter().enumerate() {
            out.push((
                item.disabled.is_some(),
                recent_pos(item, recent).unwrap_or(usize::MAX),
                index,
            ));
        }
        out.sort();
        return out
            .into_iter()
            .map(|(_, _, index)| Ranked {
                index,
                score: 0,
                label_hits: Vec::new(),
                detail_hits: Vec::new(),
            })
            .collect();
    }
    let mut ranked = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let (mut score, label_hits, detail_hits) = match fuzzy(query, &item.label) {
            Some(m) => (m.score, m.hits, Vec::new()),
            None => match fuzzy(query, &item.detail) {
                Some(m) if !item.detail.is_empty() => {
                    (m.score - DETAIL_PENALTY, Vec::new(), m.hits)
                }
                _ => continue,
            },
        };
        if let Some(pos) = recent_pos(item, recent) {
            score += (RECENT_BOOST - RECENT_STEP * pos as i32).max(0);
        }
        ranked.push(Ranked {
            index,
            score,
            label_hits,
            detail_hits,
        });
    }
    ranked.sort_by_key(|r| (items[r.index].disabled.is_some(), -r.score, r.index));
    ranked
}

// ------------------------------------------------------------------ overlay

/// Rows drawn at most (a query narrows long session lists long before this matters).
const MAX_ROWS: usize = 200;
const WIDTH: f32 = 640.0;
const LIST_HEIGHT: f32 = 380.0;

/// What the overlay asks for after a frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Open,
    Close,
    Run(Target),
}

pub struct PaletteState {
    pub mode: Mode,
    query: String,
    selected: usize,
    items: Vec<Item>,
    ranked: Vec<Ranked>,
    ranked_for: Option<String>,
    recent: Vec<String>,
    scroll_to_selected: bool,
    /// Fixed for the overlay's lifetime so egui keeps its size and scroll state between frames.
    id: Id,
}

impl PaletteState {
    pub fn new(mode: Mode, items: Vec<Item>, recent: Vec<String>) -> Self {
        Self {
            mode,
            query: String::new(),
            selected: 0,
            items,
            ranked: Vec::new(),
            ranked_for: None,
            recent,
            scroll_to_selected: true,
            id: Id::unique("omp-deck-palette"),
        }
    }

    fn refresh(&mut self) {
        if self.ranked_for.as_deref() != Some(self.query.as_str()) {
            self.ranked = rank(&self.items, &self.query, &self.recent);
            self.ranked_for = Some(self.query.clone());
            self.selected = self.first_enabled();
            self.scroll_to_selected = true;
        }
    }

    fn first_enabled(&self) -> usize {
        self.ranked
            .iter()
            .position(|r| self.items[r.index].disabled.is_none())
            .unwrap_or(0)
    }

    fn shown(&self) -> usize {
        self.ranked.len().min(MAX_ROWS)
    }

    fn step(&mut self, delta: isize) {
        let n = self.shown();
        if n == 0 {
            return;
        }
        self.selected = self.selected.saturating_add_signed(delta).min(n - 1);
        self.scroll_to_selected = true;
    }

    fn selected_target(&self) -> Option<Target> {
        let r = self.ranked.get(self.selected)?;
        let item = &self.items[r.index];
        item.disabled.is_none().then(|| item.target.clone())
    }

    /// Draws the overlay and reads its keys. Call every frame while it is open.
    pub fn show(&mut self, ctx: &Context) -> Outcome {
        self.refresh();
        let (down, up, page_down, page_up, enter) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::PageDown),
                i.consume_key(Modifiers::NONE, Key::PageUp),
                i.consume_key(Modifiers::NONE, Key::Enter),
            )
        });
        if down {
            self.step(1);
        }
        if up {
            self.step(-1);
        }
        if page_down {
            self.step(8);
        }
        if page_up {
            self.step(-8);
        }
        let mut outcome = Outcome::Open;

        let pal = Palette::of_ctx(ctx);
        let dark = ctx.global_style().visuals.dark_mode;
        let width = WIDTH.min(ctx.content_rect().width() - 40.0).max(280.0);
        let modal = Modal::new(self.id)
            .backdrop_color(theme::backdrop(dark))
            .area(Modal::default_area(self.id).anchor(Align2::CENTER_TOP, vec2(0.0, 72.0)))
            .frame(Frame::NONE.inner_margin(Margin::same(10)))
            .show(ctx, |ui| {
                let fill = if theme::flat() {
                    ctx.global_style().visuals.window_fill
                } else {
                    pal.raised
                };
                crate::card::chrome(ui, Margin::same(10), fill, |ui| {
                    ui.set_width(width);
                    if let Some(t) = self.body(ui, pal) {
                        outcome = Outcome::Run(t);
                    }
                });
            });
        // After the body ran: keys typed in the same frame as Enter have updated the query and ranking.
        if enter
            && matches!(outcome, Outcome::Open)
            && let Some(t) = self.selected_target()
        {
            outcome = Outcome::Run(t);
        }
        if matches!(outcome, Outcome::Open) && modal.should_close() {
            outcome = Outcome::Close;
        }
        outcome
    }

    fn body(&mut self, ui: &mut Ui, pal: &Palette) -> Option<Target> {
        let hint = match self.mode {
            Mode::Commands => "Type a command…",
            Mode::Switcher => "Go to a tab, session or project…",
        };
        ui.horizontal(|ui| {
            paint_icon_inline(ui, ICON_SEARCH, pal.weak);
            let edit = TextEdit::singleline(&mut self.query)
                .hint_text(hint)
                .desired_width(f32::INFINITY)
                .frame(egui::Frame::NONE)
                .font(theme::font(16.0));
            let resp = ui.add(edit);
            if !resp.has_focus() {
                resp.request_focus();
            }
        });
        ui.add_space(6.0);
        let (sep, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        crate::card::header_rule(
            ui,
            sep.left(),
            sep.right(),
            sep.center().y,
            Color32::TRANSPARENT,
        );
        ui.add_space(4.0);

        // The query changed this frame: rank before drawing so the list never lags a keystroke.
        self.refresh();
        let mut run = None;
        if self.ranked.is_empty() {
            ui.add_space(14.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("No matches").color(pal.weak));
            });
            ui.add_space(14.0);
            return None;
        }
        let row_h = theme::dens(30.0, 34.0);
        let scroll_to = std::mem::take(&mut self.scroll_to_selected);
        // Sized from the row count every frame, so a previous short result list never sticks.
        let list_h = (self.shown() as f32 * (row_h + 1.0)).min(LIST_HEIGHT);
        egui::ScrollArea::vertical()
            .min_scrolled_height(list_h)
            .max_height(list_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                for pos in 0..self.shown() {
                    let r = &self.ranked[pos];
                    let item = &self.items[r.index];
                    let (resp, hovered) = draw_row(ui, pal, item, r, pos == self.selected, row_h);
                    if scroll_to && pos == self.selected {
                        resp.scroll_to_me(None);
                    }
                    if hovered && ui.input(|i| i.pointer.delta()) != egui::Vec2::ZERO {
                        self.selected = pos;
                    }
                    if resp.clicked() && item.disabled.is_none() {
                        run = Some(item.target.clone());
                    }
                }
            });
        run
    }
}

fn paint_icon_inline(ui: &mut Ui, icon: Icon, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(20.0, 24.0), Sense::hover());
    paint_icon(
        ui.painter(),
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        16.0,
        color,
    );
}

/// Text with the matched chars (`hits`, ascending char indices) in `hit_color` and underlined.
fn highlight_job(
    text: &str,
    hits: &[usize],
    color: Color32,
    hit_color: Color32,
    font: FontId,
    max_width: f32,
) -> LayoutJob {
    let mut job = LayoutJob::default();
    job.wrap.max_width = max_width;
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    let plain = TextFormat {
        font_id: font.clone(),
        color,
        ..TextFormat::default()
    };
    let marked = TextFormat {
        font_id: font,
        color: hit_color,
        underline: Stroke::new(1.0, hit_color),
        ..TextFormat::default()
    };
    let mut run = String::new();
    let mut run_hit = false;
    let mut hit_iter = hits.iter().copied().peekable();
    for (idx, c) in text.chars().enumerate() {
        while hit_iter.next_if(|h| *h < idx).is_some() {}
        let is_hit = hit_iter.peek() == Some(&idx);
        if is_hit != run_hit && !run.is_empty() {
            job.append(
                &run,
                0.0,
                if run_hit {
                    marked.clone()
                } else {
                    plain.clone()
                },
            );
            run.clear();
        }
        run_hit = is_hit;
        run.push(c);
    }
    if !run.is_empty() {
        job.append(&run, 0.0, if run_hit { marked } else { plain });
    }
    job
}

/// One row; returns its response and whether the pointer is over it.
fn draw_row(
    ui: &mut Ui,
    pal: &Palette,
    item: &Item,
    r: &Ranked,
    selected: bool,
    height: f32,
) -> (egui::Response, bool) {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::click());
    let hovered = resp.hovered();
    if !ui.is_rect_visible(rect) {
        return (resp, hovered);
    }
    let disabled = item.disabled.is_some();
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, theme::rad(6, 9), theme::hover_fill(pal));
    }
    let (text, weak) = if disabled {
        (pal.weak.gamma_multiply(0.7), pal.weak.gamma_multiply(0.55))
    } else {
        (pal.text, pal.weak)
    };
    let icon_color = if item.running && !disabled {
        pal.ok
    } else {
        theme::mix(weak, pal.glyph, if disabled { 0.0 } else { 0.7 })
    };
    paint_icon(
        p,
        pos2(rect.left() + 20.0, rect.center().y),
        Align2::CENTER_CENTER,
        item.icon,
        16.0,
        icon_color,
    );
    let mut right = rect.right() - 12.0;
    let font = theme::font(14.0);
    if let Some(hint) = item.hint {
        let g = p.layout_no_wrap(hint.to_owned(), theme::font(12.0), weak);
        let x = right - g.size().x;
        p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, weak);
        right = x - 14.0;
    }
    let left = rect.left() + 40.0;
    let avail = (right - left).max(40.0);
    let detail_w = if item.detail.is_empty() {
        0.0
    } else {
        avail * 0.42
    };
    if detail_w > 0.0 {
        let job = highlight_job(
            &item.detail,
            &r.detail_hits,
            weak,
            pal.accent,
            theme::font(12.0),
            detail_w,
        );
        let g = p.layout_job(job);
        let x = right - g.size().x;
        p.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, weak);
    }
    let label_w = if detail_w > 0.0 {
        avail - detail_w - 12.0
    } else {
        avail
    };
    let job = highlight_job(
        &item.label,
        &r.label_hits,
        if selected && !disabled {
            pal.text_strong
        } else {
            text
        },
        pal.accent,
        font,
        label_w.max(40.0),
    );
    let g = p.layout_job(job);
    p.galley(pos2(left, rect.center().y - g.size().y / 2.0), g, text);
    let resp = match item.disabled {
        Some(reason) => resp.on_hover_text(reason),
        None => resp,
    };
    (resp, hovered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::icons::ICON_TERMINAL;

    fn hits(q: &str, t: &str) -> Vec<usize> {
        fuzzy(q, t).expect("match").hits
    }

    fn item(id: &'static str, label: &str) -> Item {
        Item {
            target: Target::Command(id.to_owned()),
            label: label.to_owned(),
            detail: String::new(),
            hint: None,
            icon: ICON_TERMINAL,
            disabled: None,
            running: false,
        }
    }

    #[test]
    fn non_subsequence_does_not_match() {
        assert_eq!(fuzzy("xyz", "settings"), None);
        assert_eq!(fuzzy("tt", "t"), None);
        assert_eq!(fuzzy("ba", "ab"), None);
    }

    #[test]
    fn long_labels_with_a_late_second_word_still_match() {
        assert!(fuzzy("model", "Model: anthropic / Opus 5").is_some());
        assert!(fuzzy("model", "Models: Reload model list").is_some());
    }

    #[test]
    fn empty_query_matches_everything_with_no_hits() {
        let m = fuzzy("  ", "anything").expect("empty query matches");
        assert!(m.hits.is_empty());
    }

    #[test]
    fn indices_follow_the_best_alignment() {
        // word starts win over the earlier scattered letters
        assert_eq!(hits("fb", "foo-bar"), vec![0, 4]);
        assert_eq!(hits("ns", "New session"), vec![0, 4]);
        // a consecutive run beats scattering
        assert_eq!(hits("term", "a terminal"), vec![2, 3, 4, 5]);
    }

    #[test]
    fn word_start_outranks_mid_word() {
        let a = fuzzy("fb", "foo-bar").expect("a").score;
        let b = fuzzy("fb", "fabric").expect("b").score;
        assert!(a > b, "{a} <= {b}");
    }

    #[test]
    fn consecutive_outranks_scattered() {
        let a = fuzzy("comp", "Compact session").expect("a").score;
        let b = fuzzy("comp", "Copy to my project").expect("b").score;
        assert!(a > b, "{a} <= {b}");
    }

    #[test]
    fn path_segment_starts_are_preferred() {
        assert_eq!(hits("sa", "src/app/sidebar"), vec![0, 4]);
        assert_eq!(hits("as", "src/app/sidebar"), vec![4, 8]);
    }

    #[test]
    fn matching_ignores_case_and_prefers_exact_case() {
        assert_eq!(hits("SET", "settings"), vec![0, 1, 2]);
        let exact = fuzzy("Set", "Settings").expect("a").score;
        let folded = fuzzy("Set", "settings").expect("b").score;
        assert!(exact > folded);
    }

    #[test]
    fn camel_case_humps_are_word_starts() {
        assert_eq!(hits("gb", "getBranch"), vec![0, 3]);
    }

    #[test]
    fn unicode_letters_fold_and_index_by_char() {
        // `Ü` is char 0 and `ü` char 5: the word start wins.
        assert_eq!(hits("ünï", "Über ünïcode"), vec![5, 6, 7]);
        assert_eq!(hits("é", "Café"), vec![3]);
    }

    #[test]
    fn whitespace_in_the_query_is_ignored() {
        assert_eq!(
            hits("new wt", "New session in worktree…"),
            hits("newwt", "New session in worktree…")
        );
    }

    #[test]
    fn exact_text_beats_longer_text() {
        let a = fuzzy("usage", "Usage").expect("a").score;
        let b = fuzzy("usage", "Usage statistics").expect("b").score;
        assert!(a > b);
    }

    #[test]
    fn rank_orders_by_score_and_demotes_disabled() {
        let mut off = item("b", "Compact session");
        off.disabled = Some("busy");
        let items = vec![off, item("c", "Copy to my project"), item("a", "Compact")];
        let r = rank(&items, "comp", &[]);
        let order: Vec<usize> = r.iter().map(|r| r.index).collect();
        assert_eq!(order, vec![2, 1, 0]);
        assert_eq!(r[0].label_hits, vec![0, 1, 2, 3]);
    }

    #[test]
    fn rank_searches_the_detail_with_a_penalty() {
        let mut a = item("a", "Alpha");
        a.detail = "~/work/omp-deck".to_owned();
        let items = vec![a, item("b", "omp deck settings")];
        let r = rank(&items, "ompdeck", &[]);
        assert_eq!(r.iter().map(|r| r.index).collect::<Vec<_>>(), vec![1, 0]);
        assert!(r[0].label_hits.len() == 7 && r[0].detail_hits.is_empty());
        assert!(r[1].label_hits.is_empty() && !r[1].detail_hits.is_empty());
    }

    #[test]
    fn recent_commands_come_first_on_an_empty_query_newest_first() {
        let items = vec![item("a", "A"), item("b", "B"), item("c", "C")];
        let recent = vec!["c".to_owned(), "b".to_owned()];
        let order: Vec<usize> = rank(&items, "", &recent).iter().map(|r| r.index).collect();
        assert_eq!(order, vec![2, 1, 0]);
    }

    #[test]
    fn unavailable_commands_sink_below_available_ones_on_an_empty_query() {
        let mut off = item("a", "A");
        off.disabled = Some("nope");
        let items = vec![off, item("b", "B"), item("c", "C")];
        let order: Vec<usize> = rank(&items, "", &[]).iter().map(|r| r.index).collect();
        assert_eq!(order, vec![1, 2, 0]);
    }

    #[test]
    fn recent_boost_breaks_ties_but_not_better_matches() {
        let items = vec![item("a", "Reload"), item("b", "Reload")];
        let r = rank(&items, "rel", &["b".to_owned()]);
        assert_eq!(r[0].index, 1);
        let items = vec![item("a", "Settings"), item("b", "Xset tings reset")];
        let r = rank(&items, "settings", &["b".to_owned()]);
        assert_eq!(r.first().map(|r| r.index), Some(0));
    }

    #[test]
    fn highlight_job_marks_exactly_the_hit_chars() {
        let job = highlight_job(
            "a-bc",
            &[0, 3],
            Color32::WHITE,
            Color32::RED,
            FontId::default(),
            100.0,
        );
        let marked: String = job
            .sections
            .iter()
            .filter(|s| s.format.color == Color32::RED)
            .map(|s| &job.text[s.byte_range.start.0..s.byte_range.end.0])
            .collect();
        assert_eq!(marked, "ac");
        assert_eq!(job.text, "a-bc");
    }
}
