//! The status bar's cost tooltip: omp's recorded dollars per source (assistant replies, side-model
//! calls) and per token type, as coloured badges and a small table instead of a wall of text.
//! Nothing is priced from the model catalog; records lacking a breakdown or a cost are listed,
//! never shown as $0.
//!
//! [`CostTip::build`] formats everything once per status snapshot; [`show`] only paints.

use deck_core::status::{CostDetail, CostSource};
use egui::{Align, Layout, Rect, RichText, Sense, Ui, vec2};

use crate::badge::{self, Badge, Hue};
use crate::icons::ICON_COINS;
use crate::statusbar::group_digits;
use crate::theme::{self, Palette};
use crate::widgets::icon_text;

const WIDTH: f32 = 340.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CostTip {
    /// `$1.5625`.
    total: String,
    /// Some dollars cannot be split by token type, or some tokens have no cost.
    partial: bool,
    sources: Vec<SourceTip>,
}

#[derive(Clone, Debug, PartialEq)]
struct SourceTip {
    title: &'static str,
    hue: Hue,
    /// `2 replies`.
    count: String,
    total: String,
    /// `By token type (1 of 2 replies)` when only some records carry a breakdown.
    caption: Option<String>,
    rows: Vec<RowTip>,
    /// Gaps in the data: unsplit dollars, records without a breakdown or a cost.
    notes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
struct RowTip {
    label: &'static str,
    hue: Hue,
    tokens: String,
    usd: String,
    /// No tokens and no dollars recorded for this type.
    zero: bool,
}

impl CostTip {
    pub fn build(total: f64, d: &CostDetail) -> CostTip {
        let sources = [
            (
                "Assistant replies",
                Hue::Blue,
                ("reply", "replies"),
                &d.assistant,
            ),
            ("Side-model calls", Hue::Violet, ("call", "calls"), &d.side),
        ];
        CostTip {
            total: format!("${total:.4}"),
            partial: sources.iter().any(|(.., s)| source_partial(s)),
            sources: sources
                .into_iter()
                .filter(|(.., s)| s.records > 0)
                .map(|(title, hue, noun, s)| source_tip(title, hue, noun, s))
                .collect(),
        }
    }
}

/// Some of the source's dollars cannot be split by token type, or some tokens have no cost.
fn source_partial(s: &CostSource) -> bool {
    shows_unitemized(s) || s.uncosted_tokens > 0 || shows_unreconciled(s)
}

fn shows_unitemized(s: &CostSource) -> bool {
    s.unitemized_records > 0 && (s.unitemized.usd > 0.0 || s.unitemized.tokens > 0)
}

/// Visible at the tooltip's 4 decimals.
fn shows_unreconciled(s: &CostSource) -> bool {
    s.itemized > 0 && s.unreconciled().abs() >= 0.000_05
}

fn source_tip(title: &'static str, hue: Hue, noun: (&str, &str), s: &CostSource) -> SourceTip {
    let count = |n: u64| {
        format!(
            "{} {}",
            group_digits(n),
            if n == 1 { noun.0 } else { noun.1 }
        )
    };
    let mut caption = None;
    let mut rows = Vec::new();
    if s.itemized > 0 {
        if s.itemized < s.records {
            caption = Some(format!(
                "By token type ({} of {})",
                group_digits(s.itemized),
                count(s.records),
            ));
        }
        rows = [
            ("↑ input", Hue::Blue, s.input),
            ("↓ output", Hue::Teal, s.output),
            ("cache read", Hue::Violet, s.cache_read),
            ("cache write", Hue::Amber, s.cache_write),
        ]
        .into_iter()
        .map(|(label, hue, p)| RowTip {
            label,
            hue,
            tokens: format!("{} tokens", group_digits(p.tokens)),
            usd: format!("${:.4}", p.usd),
            zero: p.tokens == 0 && p.usd == 0.0,
        })
        .collect();
    }
    let mut notes = Vec::new();
    if shows_unreconciled(s) {
        let u = s.unreconciled();
        notes.push(if u > 0.0 {
            format!("Not split by token type · ${u:.4}")
        } else {
            format!("Token types exceed the recorded total by ${:.4}", -u)
        });
    }
    if shows_unitemized(s) {
        notes.push(format!(
            "No token-type breakdown recorded · {} · {} tokens · ${:.4}",
            count(s.unitemized_records),
            group_digits(s.unitemized.tokens),
            s.unitemized.usd
        ));
    }
    if s.uncosted_tokens > 0 {
        notes.push(format!(
            "No cost recorded · {} · {} tokens (not in the total)",
            count(s.uncosted),
            group_digits(s.uncosted_tokens)
        ));
    }
    SourceTip {
        title,
        hue,
        count: count(s.records),
        total: format!("${:.4}", s.total),
        caption,
        rows,
        notes,
    }
}

/// Paints the tooltip body (call from `on_hover_ui`).
pub fn show(ui: &mut Ui, tip: &CostTip) {
    let pal = Palette::of_ui(ui);
    ui.set_width(WIDTH);
    ui.spacing_mut().item_spacing.y = 6.0;

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        ui.label(icon_text(ICON_COINS, 15.0, pal.glyph));
        ui.label(theme::strong("Session cost").color(pal.text_strong));
        if tip.partial {
            badge::show(ui, None, "breakdown partial", Hue::Amber, 10.5);
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            badge::show(ui, None, &tip.total, Hue::Green, 13.0);
        });
    });

    for s in &tip.sources {
        rule(ui, pal);
        source(ui, pal, s);
    }

    rule(ui, pal);
    for line in [
        "As recorded in the session file: every branch, before and after compaction.",
        "Not included: subagent (task) runs, and anything omp has not written to the file yet.",
    ] {
        ui.label(RichText::new(line).small().color(pal.weak));
    }
}

fn source(ui: &mut Ui, pal: &Palette, s: &SourceTip) {
    let hue = s.hue.color(pal);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 7.0;
        swatch(ui, hue);
        ui.label(theme::strong(s.title).color(pal.text_strong));
        ui.label(RichText::new(&s.count).small().color(pal.weak));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            Badge::single(ui.painter(), None, &s.total, hue, 12.0).show(ui);
        });
    });
    if let Some(c) = &s.caption {
        ui.label(RichText::new(c).small().color(pal.weak));
    }
    if !s.rows.is_empty() {
        egui::Grid::new(("cost-tip-rows", s.title))
            .num_columns(4)
            .spacing([10.0, 4.0])
            .show(ui, |ui| {
                for r in &s.rows {
                    // A component with nothing recorded stays listed (it is a real zero), dimmed.
                    let (c, usd) = if r.zero {
                        (pal.weak, pal.weak)
                    } else {
                        (r.hue.color(pal), pal.text_strong)
                    };
                    swatch(ui, c);
                    ui.label(RichText::new(r.label).color(c));
                    right(ui, |ui| {
                        ui.label(RichText::new(&r.tokens).small().color(pal.weak));
                    });
                    right(ui, |ui| {
                        ui.label(RichText::new(&r.usd).color(usd));
                    });
                    ui.end_row();
                }
            });
    }
    for n in &s.notes {
        ui.label(RichText::new(n).small().color(Hue::Amber.color(pal)));
    }
}

fn swatch(ui: &mut Ui, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(10.0, 14.0), Sense::hover());
    let sw = Rect::from_center_size(rect.center(), vec2(9.0, 9.0));
    ui.painter().rect_filled(sw, 3, color);
}

fn rule(ui: &mut Ui, pal: &Palette) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
    ui.painter().rect_filled(rect, 0, pal.stroke);
}

fn right(ui: &mut Ui, f: impl FnOnce(&mut Ui)) {
    ui.with_layout(Layout::right_to_left(Align::Center), f);
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::status::CostPart;

    fn part(usd: f64, tokens: u64) -> CostPart {
        CostPart { usd, tokens }
    }

    /// Everything the tooltip shows, as one string.
    fn flat(t: &CostTip) -> String {
        let mut out = vec![t.total.clone()];
        if t.partial {
            out.push("partial".to_owned());
        }
        for s in &t.sources {
            out.extend([s.title.to_owned(), s.count.clone(), s.total.clone()]);
            out.extend(s.caption.clone());
            for r in &s.rows {
                out.extend([r.label.to_owned(), r.tokens.clone(), r.usd.clone()]);
            }
            out.extend(s.notes.clone());
        }
        out.join("\n")
    }

    #[test]
    fn shows_recorded_components_and_flags_missing_breakdowns() {
        let detailed = CostDetail {
            assistant: CostSource {
                records: 2,
                total: 1.5,
                itemized: 2,
                input: part(0.25, 1_000),
                output: part(1.25, 50),
                ..CostSource::default()
            },
            side: CostSource {
                records: 1,
                total: 0.0625,
                itemized: 1,
                cache_read: part(0.0625, 70_000),
                ..CostSource::default()
            },
        };
        let tip = CostTip::build(1.5625, &detailed);
        let text = flat(&tip);
        for want in [
            "$1.5625", "$1.5000", "$0.2500", "1,000", "$1.2500", "$0.0625", "70,000",
        ] {
            assert!(text.contains(want), "{want} missing:\n{text}");
        }
        assert!(!tip.partial, "{text}");

        // An older file: totals only, one reply without any cost.
        let older = CostDetail {
            assistant: CostSource {
                records: 4,
                total: 0.75,
                unitemized_records: 3,
                unitemized: part(0.75, 1_234),
                uncosted: 1,
                uncosted_tokens: 9,
                ..CostSource::default()
            },
            side: CostSource::default(),
        };
        let tip = CostTip::build(0.75, &older);
        let text = flat(&tip);
        assert!(
            tip.partial && text.contains("$0.7500") && text.contains("1,234"),
            "{text}"
        );
        assert!(
            tip.sources.iter().all(|s| s.rows.is_empty()),
            "no invented $0 components:\n{text}"
        );
        assert_eq!(tip.sources.len(), 1, "no side-model section:\n{text}");

        // Components short of the recorded totals: the gap is shown, not hidden.
        let short = CostDetail {
            assistant: CostSource {
                records: 1,
                total: 1.0,
                itemized: 1,
                input: part(0.25, 10),
                output: part(0.5, 5),
                ..CostSource::default()
            },
            side: CostSource::default(),
        };
        let tip = CostTip::build(1.0, &short);
        let text = flat(&tip);
        assert!(tip.partial && text.contains("$0.2500"), "{text}");
        assert!(
            tip.sources[0].notes[0].contains("$0.2500"),
            "the unsplit quarter dollar is listed: {text}"
        );
    }
}
