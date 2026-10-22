//! The status bar's context tooltip: omp's `/context` breakdown as a stacked meter, a legend and
//! the tool-schema table. [`CtxTip`] is the pure, pre-formatted row model (built once per
//! breakdown); [`show`] only paints it.

use deck_core::ctl::ContextBreakdown;
use egui::{
    Align, Align2, Color32, CornerRadius, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Ui,
    pos2, vec2,
};

use crate::icons::ICON_CHART_PIE;
use crate::statusbar::{group_digits, level_color};
use crate::statusfmt::{ContextLevel, context_level, fmt_tokens};
use crate::theme::{self, Palette};
use crate::widgets::paint_icon;

/// Tool schemas listed before "N more".
pub const MAX_TOOLS: usize = 8;
const WIDTH: f32 = 340.0;
const METER_H: f32 = 9.0;
/// Screen height kept clear of the tooltip: the status bar plus the tooltip frame and its gap.
const RESERVED_H: f32 = 90.0;
/// The "Tool schemas" caption row plus the rule above it.
const CAPTION_H: f32 = 28.0;

/// Which colour a legend row / meter segment uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SegKind {
    SystemPrompt,
    SystemTools,
    SystemContext,
    Skills,
    Messages,
    /// A category id this version does not know; indexed so neighbours stay distinct.
    Other(u8),
    Free,
    Buffer,
}

impl SegKind {
    fn of_id(id: &str, index: usize) -> SegKind {
        match id {
            "systemPrompt" => SegKind::SystemPrompt,
            "systemTools" => SegKind::SystemTools,
            "systemContext" => SegKind::SystemContext,
            "skills" => SegKind::Skills,
            "messages" => SegKind::Messages,
            _ => SegKind::Other(u8::try_from(index).unwrap_or(u8::MAX)),
        }
    }

    fn color(self, pal: &Palette) -> Color32 {
        let dark = pal.bg.r() < 128;
        let tone = |d: (u8, u8, u8), l: (u8, u8, u8)| {
            let (r, g, b) = if dark { d } else { l };
            Color32::from_rgb(r, g, b)
        };
        match self {
            SegKind::SystemPrompt => tone((96, 165, 250), (37, 99, 235)),
            SegKind::SystemTools => tone((251, 191, 36), (202, 138, 4)),
            SegKind::SystemContext => tone((192, 132, 252), (147, 51, 234)),
            SegKind::Skills => tone((74, 222, 128), (22, 163, 74)),
            SegKind::Messages => pal.accent,
            SegKind::Other(i) => {
                const EXTRA: [(u8, u8, u8); 3] = [(244, 114, 182), (45, 212, 191), (251, 146, 60)];
                let (r, g, b) = EXTRA[usize::from(i) % EXTRA.len()];
                Color32::from_rgb(r, g, b)
            }
            SegKind::Free => theme::mix(pal.surface, pal.weak, 0.22),
            SegKind::Buffer => theme::mix(pal.surface, pal.weak, 0.55),
        }
    }
}

/// One legend row (and one meter segment).
#[derive(Clone, Debug, PartialEq)]
pub struct TipRow {
    pub kind: SegKind,
    pub label: String,
    pub tokens: String,
    pub pct: String,
    /// Share of the meter's scale (window, or the larger sum when the parts overflow it).
    pub frac: f32,
}

/// Where the conversation stands relative to auto-compaction.
#[derive(Clone, Debug, PartialEq)]
pub enum Compaction {
    Off,
    Left { text: String },
    Past { text: String },
}

/// The pre-formatted tooltip.
#[derive(Clone, Debug, PartialEq)]
pub struct CtxTip {
    pub model: Option<String>,
    /// `50K window`.
    pub window_label: String,
    /// `/ 50K`.
    pub window_slash: String,
    pub used_text: String,
    /// `(20.0%)`.
    pub pct_paren: String,
    pub level: ContextLevel,
    pub anchored: bool,
    pub rows: Vec<TipRow>,
    /// Auto-compaction threshold as a position on the meter (0..=1).
    pub threshold: Option<f32>,
    pub compaction: Compaction,
    pub snapcompact: Option<String>,
    /// `1.2K tokens`.
    pub tools_label: String,
    pub tools: Vec<(String, String)>,
    /// Every tool, descending. `show` lists as many as fit the window (at most [`MAX_TOOLS`]).
    /// `tools_more[k - 1]` is `+k more`, pre-formatted for `k` hidden tools.
    pub tools_more: Vec<String>,
    /// `3 skills in the catalog`.
    pub skills: Option<String>,
}

/// `12.3%`, `<0.1%` for a non-zero sliver.
fn pct_text(tokens: u64, window: u64) -> String {
    if window == 0 || tokens == 0 {
        return "0%".to_owned();
    }
    let p = tokens as f64 / window as f64 * 100.0;
    if p < 0.1 {
        "<0.1%".to_owned()
    } else {
        format!("{p:.1}%")
    }
}

impl CtxTip {
    pub fn build(b: &ContextBreakdown) -> CtxTip {
        let window = b.context_window;
        let used_pct = if window > 0 {
            b.used_tokens as f64 / window as f64 * 100.0
        } else {
            0.0
        };
        let mut raw: Vec<(SegKind, &str, u64)> = b
            .categories
            .iter()
            .enumerate()
            .map(|(i, c)| (SegKind::of_id(&c.id, i), c.label.as_str(), c.tokens))
            .collect();
        raw.push((SegKind::Free, "Free space", b.free_tokens));
        if b.auto_compact_buffer > 0 {
            raw.push((SegKind::Buffer, "Autocompact buffer", b.auto_compact_buffer));
        }
        // The parts normally add up to the window; when they overflow it the meter rescales.
        let sum: u64 = raw.iter().map(|r| r.2).sum();
        let scale = sum.max(window).max(1) as f32;
        let rows = raw
            .into_iter()
            .map(|(kind, label, tokens)| TipRow {
                kind,
                label: label.to_owned(),
                tokens: group_digits(tokens),
                pct: pct_text(tokens, window),
                frac: tokens as f32 / scale,
            })
            .collect();
        let threshold = b
            .threshold_tokens
            .filter(|t| *t > 0 && window > 0)
            .map(|t| (t as f32 / scale).clamp(0.0, 1.0));
        let compaction = match b.threshold_tokens.filter(|t| *t > 0 && window > 0) {
            None => Compaction::Off,
            Some(t) => {
                let at = format!("{:.0}%", t as f64 / window as f64 * 100.0);
                if b.used_tokens < t {
                    Compaction::Left {
                        text: format!(
                            "{} left before auto-compaction at {at}",
                            fmt_tokens(t - b.used_tokens)
                        ),
                    }
                } else {
                    Compaction::Past {
                        text: format!(
                            "Past auto-compaction ({at}) by {}",
                            fmt_tokens(b.used_tokens - t)
                        ),
                    }
                }
            }
        };
        let mut tools: Vec<&(String, u64)> = b.tools.iter().collect();
        tools.sort_by(|a, c| c.1.cmp(&a.1).then_with(|| a.0.cmp(&c.0)));
        let tools_total: u64 = b.tools.iter().map(|t| t.1).sum();
        CtxTip {
            model: b.model.clone(),
            window_label: format!("{} window", fmt_tokens(window)),
            window_slash: format!("/ {}", fmt_tokens(window)),
            used_text: fmt_tokens(b.used_tokens),
            pct_paren: format!("({used_pct:.1}%)"),
            level: context_level(used_pct, window),
            anchored: b.anchored,
            rows,
            threshold,
            compaction,
            snapcompact: b
                .snapcompact_saved
                .filter(|s| *s > 0)
                .map(|s| format!("Snapcompact saves {} tokens", fmt_tokens(s))),
            tools_label: format!("{} tokens", fmt_tokens(tools_total)),
            tools: tools
                .iter()
                .map(|(n, t)| (n.clone(), fmt_tokens(*t)))
                .collect(),
            tools_more: (1..=tools.len()).map(|k| format!("+{k} more")).collect(),
            skills: (b.skills > 0).then(|| {
                let word = if b.skills == 1 { "skill" } else { "skills" };
                format!("{} {word} in the catalog", b.skills)
            }),
        }
    }
}

/// Paints the tooltip body (call from `on_hover_ui`).
pub fn show(ui: &mut Ui, tip: &CtxTip) {
    let pal = Palette::of_ui(ui);
    ui.set_width(WIDTH);
    ui.spacing_mut().item_spacing.y = 4.0;

    // Header: model + window.
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        let (rect, _) = ui.allocate_exact_size(vec2(16.0, 16.0), Sense::hover());
        paint_icon(
            ui.painter(),
            rect.center(),
            Align2::CENTER_CENTER,
            ICON_CHART_PIE,
            15.0,
            pal.glyph,
        );
        let name = tip.model.as_deref().unwrap_or("Context window");
        ui.add(
            egui::Label::new(theme::strong(name).color(pal.text_strong))
                .truncate()
                .selectable(false),
        );
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(&tip.window_label).small().color(pal.weak));
        });
    });

    // Headline: used / window (pct).
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 5.0;
        let level = level_color(pal, Some(tip.level));
        ui.label(
            theme::strong(&tip.used_text)
                .size(19.0)
                .color(pal.text_strong),
        );
        ui.label(RichText::new(&tip.window_slash).size(14.0).color(pal.weak));
        let pct_color = if tip.level == ContextLevel::Normal {
            pal.text
        } else {
            level
        };
        ui.label(theme::strong(&tip.pct_paren).size(14.0).color(pct_color));
        if !tip.anchored {
            ui.label(RichText::new("estimated").small().italics().color(pal.weak));
        }
    });

    meter(ui, pal, tip);

    match &tip.compaction {
        Compaction::Off => {
            ui.label(
                RichText::new("Auto-compaction is off")
                    .small()
                    .color(pal.weak),
            );
        }
        Compaction::Left { text } => {
            ui.label(RichText::new(text).small().color(pal.text));
        }
        Compaction::Past { text } => {
            ui.label(RichText::new(text).small().color(pal.warn));
        }
    }

    ui.add_space(4.0);
    rule(ui, pal);
    legend(ui, pal, tip);

    if let Some(s) = &tip.snapcompact {
        ui.add_space(2.0);
        ui.label(RichText::new(s).small().color(pal.ok));
    }

    if !tip.tools.is_empty() || tip.skills.is_some() {
        ui.add_space(4.0);
        rule(ui, pal);
    }
    if !tip.tools.is_empty() {
        // Height left for the table: the screen minus the status bar and tooltip chrome, minus
        // what is painted so far, the table caption, the skills line and the "+N more" line.
        let small = ui.text_style_height(&egui::TextStyle::Small);
        let line = small + ui.spacing().item_spacing.y;
        let used = ui.min_rect().height();
        let skills = if tip.skills.is_some() { line } else { 0.0 };
        let avail = ui.ctx().content_rect().height() - RESERVED_H - used - CAPTION_H - skills;
        let shown = tool_rows_fit(avail, line, line, tip.tools.len());
        tools_table(ui, pal, tip, shown);
    }
    if let Some(s) = &tip.skills {
        ui.label(RichText::new(s).small().color(pal.weak));
    }
}

fn rule(ui: &mut Ui, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0, pal.stroke);
    ui.add_space(2.0);
}

/// The stacked meter with the auto-compact mark.
fn meter(ui: &mut Ui, pal: &Palette, tip: &CtxTip) {
    let (rect, _) =
        ui.allocate_exact_size(vec2(ui.available_width(), METER_H + 8.0), Sense::hover());
    let bar = Rect::from_center_size(rect.center(), vec2(rect.width(), METER_H));
    let p = ui.painter();
    let r = CornerRadius::same(4);
    p.rect_filled(bar, r, pal.surface);
    let mut x = bar.left();
    let last = tip.rows.len().saturating_sub(1);
    for (i, row) in tip.rows.iter().enumerate() {
        let w = (bar.width() * row.frac).max(0.0);
        if w < 0.5 {
            continue;
        }
        let seg = Rect::from_min_size(pos2(x, bar.top()), vec2(w.min(bar.right() - x), METER_H));
        let cr = CornerRadius {
            nw: if x <= bar.left() + 0.5 { 4 } else { 0 },
            sw: if x <= bar.left() + 0.5 { 4 } else { 0 },
            ne: if i == last { 4 } else { 0 },
            se: if i == last { 4 } else { 0 },
        };
        p.rect_filled(seg, cr, row.kind.color(pal));
        x += w;
    }
    p.rect_stroke(bar, r, Stroke::new(1.0, pal.stroke), StrokeKind::Outside);
    if let Some(t) = tip.threshold {
        let tx = bar.left() + bar.width() * t;
        p.line_segment(
            [pos2(tx, bar.top() - 3.0), pos2(tx, bar.bottom() + 3.0)],
            Stroke::new(1.5, pal.text_strong),
        );
    }
}

fn legend(ui: &mut Ui, pal: &Palette, tip: &CtxTip) {
    egui::Grid::new("ctx-tip-legend")
        .num_columns(4)
        .spacing([14.0, 3.0])
        .show(ui, |ui| {
            for row in &tip.rows {
                let (rect, _) = ui.allocate_exact_size(vec2(10.0, 14.0), Sense::hover());
                let sw = Rect::from_center_size(rect.center(), vec2(9.0, 9.0));
                let color = row.kind.color(pal);
                ui.painter().rect_filled(sw, 3, color);
                let dim = matches!(row.kind, SegKind::Free | SegKind::Buffer);
                ui.label(RichText::new(&row.label).color(if dim { pal.weak } else { pal.text }));
                right(ui, |ui| {
                    ui.label(RichText::new(&row.tokens).color(pal.text_strong));
                });
                right(ui, |ui| {
                    ui.label(RichText::new(&row.pct).small().color(pal.weak));
                });
                ui.end_row();
            }
        });
}

/// How many tool rows to list: at most [`MAX_TOOLS`], and only as many as `avail` points of
/// height hold (each `row_h`), leaving `more_h` for the "+N more" line when some are hidden.
pub fn tool_rows_fit(avail: f32, row_h: f32, more_h: f32, total: usize) -> usize {
    let want = total.min(MAX_TOOLS);
    let row_h = row_h.max(1.0);
    let hides_some = |n: usize| n < total;
    let fits = |n: usize| n as f32 * row_h + if hides_some(n) { more_h } else { 0.0 } <= avail;
    if fits(want) {
        return want;
    }
    (0..want).rev().find(|n| fits(*n)).unwrap_or(0)
}

fn tools_table(ui: &mut Ui, pal: &Palette, tip: &CtxTip, shown: usize) {
    ui.horizontal(|ui| {
        ui.label(theme::section_caption("Tool schemas", pal.weak));
        right(ui, |ui| {
            ui.label(RichText::new(&tip.tools_label).small().color(pal.weak));
        });
    });
    egui::Grid::new("ctx-tip-tools")
        .num_columns(2)
        .spacing([10.0, 2.0])
        .show(ui, |ui| {
            for (name, tokens) in tip.tools.iter().take(shown) {
                ui.label(RichText::new(name).small().color(pal.text));
                right(ui, |ui| {
                    ui.label(RichText::new(tokens).small().color(pal.weak));
                });
                ui.end_row();
            }
        });
    if let Some(more) = tip
        .tools
        .len()
        .checked_sub(shown + 1)
        .and_then(|k| tip.tools_more.get(k))
    {
        ui.label(RichText::new(more).small().color(pal.weak));
    }
}

fn right(ui: &mut Ui, f: impl FnOnce(&mut Ui)) {
    ui.with_layout(Layout::right_to_left(Align::Center), f);
}

/// Why the full breakdown is missing.
pub const NEEDS_LIVE: &str = "The full breakdown (system prompt, tools, messages, free space, auto-compaction) needs live control — start or restart the session with live control.";

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::ctl::ContextCategory;

    fn cat(id: &str, label: &str, tokens: u64) -> ContextCategory {
        ContextCategory {
            id: id.to_owned(),
            label: label.to_owned(),
            tokens,
        }
    }

    fn sample() -> ContextBreakdown {
        ContextBreakdown {
            model: Some("Fake One".to_owned()),
            context_window: 50_000,
            used_tokens: 10_000,
            anchored: true,
            categories: vec![
                cat("systemPrompt", "System prompt", 2_900),
                cat("systemTools", "System tools", 4_380),
                cat("messages", "Messages", 2_720),
            ],
            auto_compact_buffer: 16_384,
            free_tokens: 23_616,
            threshold_tokens: Some(33_616),
            snapcompact_saved: None,
            tools: (0..11).map(|i| (format!("t{i}"), 100 + i)).collect(),
            skills: 3,
            computed_at_ms: 0,
        }
    }

    #[test]
    fn rows_cover_categories_free_and_buffer() {
        let tip = CtxTip::build(&sample());
        let labels: Vec<&str> = tip.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "System prompt",
                "System tools",
                "Messages",
                "Free space",
                "Autocompact buffer"
            ]
        );
        assert_eq!(tip.rows[1].tokens, "4,380");
        assert_eq!(tip.rows[1].pct, "8.8%");
        let total: f32 = tip.rows.iter().map(|r| r.frac).sum();
        assert!((total - 1.0).abs() < 0.001);
    }

    #[test]
    fn compaction_reads_left_then_past() {
        let tip = CtxTip::build(&sample());
        assert_eq!(
            tip.compaction,
            Compaction::Left {
                text: "24K left before auto-compaction at 67%".to_owned()
            }
        );
        assert!((tip.threshold.unwrap_or(0.0) - 0.67232).abs() < 0.001);
        let mut b = sample();
        b.used_tokens = 40_000;
        let Compaction::Past { text } = CtxTip::build(&b).compaction else {
            panic!("expected Past");
        };
        assert_eq!(text, "Past auto-compaction (67%) by 6.4K");
        b.threshold_tokens = None;
        b.auto_compact_buffer = 0;
        let tip = CtxTip::build(&b);
        assert_eq!(tip.compaction, Compaction::Off);
        assert_eq!(tip.threshold, None);
        assert!(tip.rows.iter().all(|r| r.label != "Autocompact buffer"));
    }

    #[test]
    fn tools_are_sorted_capped_and_counted() {
        let tip = CtxTip::build(&sample());
        assert_eq!(tip.tools.len(), 11);
        assert_eq!(tip.tools[0], ("t10".to_owned(), "110".to_owned()));
        assert_eq!(tip.tools_more[2], "+3 more");
        assert_eq!(tool_rows_fit(1000.0, 16.0, 16.0, 11), MAX_TOOLS);
        assert_eq!(tip.skills.as_deref(), Some("3 skills in the catalog"));
        assert_eq!(tip.window_label, "50K window");
        assert_eq!(tip.window_slash, "/ 50K");
        assert_eq!(tip.pct_paren, "(20.0%)");
    }

    #[test]
    fn tool_rows_shrink_to_the_available_height() {
        // Room for everything (no "+N more" needed).
        assert_eq!(tool_rows_fit(200.0, 16.0, 16.0, 5), 5);
        // 11 tools, 100 high: 5 rows (80) + the more line (16) = 96.
        assert_eq!(tool_rows_fit(100.0, 16.0, 16.0, 11), 5);
        // Exactly the cap fits only without a more line when nothing is hidden.
        assert_eq!(tool_rows_fit(8.0 * 16.0, 16.0, 16.0, 8), 8);
        assert_eq!(tool_rows_fit(8.0 * 16.0, 16.0, 16.0, 9), 7);
        // Nothing fits: no rows, the caller still shows "+N more".
        assert_eq!(tool_rows_fit(5.0, 16.0, 16.0, 11), 0);
        assert_eq!(tool_rows_fit(-20.0, 16.0, 16.0, 11), 0);
    }

    #[test]
    fn overflowing_parts_rescale_the_meter() {
        let mut b = sample();
        b.free_tokens = 0;
        b.auto_compact_buffer = 0;
        b.categories = vec![cat("messages", "Messages", 80_000)];
        let tip = CtxTip::build(&b);
        assert!(tip.rows.iter().map(|r| r.frac).sum::<f32>() <= 1.001);
        assert_eq!(tip.rows[0].pct, "160.0%");
    }

    #[test]
    fn unknown_category_ids_stay_distinct() {
        let mut b = sample();
        b.categories = vec![cat("mcp", "MCP", 10), cat("rules", "Rules", 5)];
        let tip = CtxTip::build(&b);
        assert_ne!(tip.rows[0].kind, tip.rows[1].kind);
        assert_eq!(tip.rows[1].pct, "<0.1%");
    }

    #[test]
    fn snapcompact_line_only_when_savings() {
        let mut b = sample();
        b.snapcompact_saved = Some(0);
        assert_eq!(CtxTip::build(&b).snapcompact, None);
        b.snapcompact_saved = Some(12_300);
        assert_eq!(
            CtxTip::build(&b).snapcompact.as_deref(),
            Some("Snapcompact saves 12K tokens")
        );
    }
}
