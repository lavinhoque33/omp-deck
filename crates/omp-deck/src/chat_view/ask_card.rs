//! omp's ask card (omp ≥ 19): every question of an `ask` as one form at the end of the transcript.
//! The composer stays in the dock below it, and a message sent from there skips the questions.
//! The question omp's focus is on is open (its context and controls); the others fold to their
//! question over a summary of the answer, and a click opens one. Roles as omp's
//! `native/ask-card.ts` names them. Before 19 an ask stood in the composer's place
//! (`omp.editor.ask`, drawn by [`dock`](super::dock)).

use egui::text::{LayoutJob, TextFormat};
use egui::{Align, CursorIcon, Layout, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use serde_json::Value;

use super::ctx::{Cx, Tx};
use super::dock::{self, AskHost};
use super::paint;
use super::render;
use super::snap::SNode;
use super::text::{self, Base};
use super::transcript::measure_size;
use super::tsel::RunOpts;

/// Room left of an open question for its rail.
const RAIL: f32 = 14.0;
const FOLD_PAD: egui::Vec2 = vec2(12.0, 8.0);

/// Ask-card roles; `true` when handled.
pub fn role_node(cx: &Cx, ui: &mut Ui, n: &SNode) -> bool {
    match n.role() {
        "omp.ask.card" => dock::ask_panel(cx, ui, n),
        "omp.ask.card.head" => head(cx, ui, n),
        "omp.ask.card.body" => body(cx, ui, n),
        "omp.ask.open" => open(cx, ui, n),
        "omp.ask.fold" => fold(cx, ui, n),
        "omp.ask.fold.well" => {
            dock::ask_box(cx, ui, false, |ui| render::text_node(cx, ui, n));
        }
        "omp.ask.skip" => skip(cx, ui, n),
        // The pre-19 dialog has labels too, drawn as its plain text.
        "omp.ask.legend" | "omp.ask.label"
            if matches!(dock::ask_host(cx, n), Some(AskHost::Card)) =>
        {
            caption(cx, ui, n, n.role() == "omp.ask.legend");
        }
        _ => return false,
    }
    true
}

/// The card's head: the `ask` tag, omp's progress (`2 of 5 answered`) at the right edge, and a
/// rule setting both off from the questions.
fn head(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
    let row = Rect::from_min_max(rect.min, pos2(rect.right(), rect.bottom() - 8.0));
    let mut tags = ui.new_child(
        UiBuilder::new()
            .id_salt(("ask-head", n.id.as_str()))
            .max_rect(row)
            .layout(Layout::left_to_right(Align::Center)),
    );
    for c in cx.snap.kids(n) {
        match c.role() {
            "omp.ask.card.progress" => {
                let g =
                    ui.painter()
                        .layout_no_wrap(text::node_plain(c), ink.label(11.5), ink.dim());
                let at = pos2(row.right() - g.size().x, row.center().y - g.size().y / 2.0);
                ui.painter().galley(at, g, ink.dim());
            }
            _ => {
                render::badge(cx, &mut tags, &text::node_plain(c), Some("accent"));
            }
        }
    }
    let [(_, a), (_, b)] = ink.rule_stops();
    crate::deco::gradient_rule(
        ui.painter(),
        rect.left_bottom(),
        rect.right_bottom(),
        1.0,
        &[
            (0.0, a),
            (0.6, b.gamma_multiply(0.5)),
            (1.0, b.gamma_multiply(0.0)),
        ],
    );
}

/// The questions. omp marks them `drop` while the composer holds a draft whose sending skips
/// them: they fade behind its warning.
fn body(cx: &Cx, ui: &mut Ui, n: &SNode) {
    ui.scope(|ui| {
        if n.str("mark") == Some("drop") {
            ui.multiply_opacity(0.45);
        }
        ui.spacing_mut().item_spacing.y = 10.0;
        render::children(cx, ui, n);
    });
}

/// The open question, hanging off an accent rail: a question, or a fieldset whose one live field
/// sits among its other questions' folds. When omp opens another field (or a card arrives) the
/// transcript brings it into view, its label with it (and the legend when it is the fieldset's
/// first), as omp's `reveal` asks: the card may be taller than the view, and the view follows
/// the transcript's end. While the view holds there it keeps the field in view as the items
/// above it settle their heights.
fn open(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let at = ui.cursor().min;
    let w = ui.available_width();
    let inner = Rect::from_min_size(pos2(at.x + RAIL, at.y), vec2(w - RAIL, 10_000.0));
    // The live field (its id and rect, the legend's included when the legend heads it).
    let mut live: Option<(&str, Rect)> = None;
    let drawn = ui
        .scope_builder(UiBuilder::new().max_rect(inner), |ui| {
            ui.set_width(w - RAIL);
            ui.spacing_mut().item_spacing.y = 8.0;
            let mut legend = None;
            for c in cx.snap.kids(n).filter(|c| !render::blank(cx, c)) {
                let r = ui.scope(|ui| render::node(cx, ui, c)).response.rect;
                match c.role() {
                    "omp.ask.legend" => {
                        legend = Some(r);
                        continue;
                    }
                    "omp.ask.field" => {
                        live = Some((c.id.as_str(), legend.map_or(r, |l| l.union(r))));
                    }
                    _ => {}
                }
                legend = None;
            }
        })
        .response
        .rect;
    let rail = Rect::from_min_max(pos2(at.x, drawn.top()), pos2(at.x + 3.0, drawn.bottom()));
    crate::deco::fill_vertical(
        ui.painter(),
        &crate::deco::rounded_rect(rail, 1.5),
        ink.a,
        ink.b,
    );
    if ui.is_sizing_pass() {
        return;
    }
    let (id, rect) = live.unwrap_or((n.id.as_str(), drawn));
    cx.loc(|l| {
        let fresh = l.revealed.as_deref() != Some(id);
        if fresh {
            l.revealed = Some(id.to_owned());
        }
        if fresh || l.ask_hold {
            l.reveal = Some(rect);
        }
    });
}

/// A folded question: its mark and question over its summary (answer pills, the typed answer or
/// an empty field), framed as one click target that opens it.
fn fold(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let w = ui.available_width();
    let draw = |ui: &mut Ui| {
        ui.spacing_mut().item_spacing.y = 6.0;
        render::children(cx, ui, n);
    };
    let size = measure_size(ui, w - 2.0 * FOLD_PAD.x, &draw);
    let (rect, _) = ui.allocate_exact_size(vec2(w, size.y + 2.0 * FOLD_PAD.y), Sense::hover());
    // Laid under its content: the pills in it keep their own clicks.
    let id = ui.scope_id().with(("ask-fold", n.id.as_str()));
    let resp = ui.interact(rect, id, Sense::click());
    let hov = ui
        .ctx()
        .animate_bool_with_time(id.with("h"), resp.hovered(), 0.1);
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        paint::boxed(
            p,
            rect,
            6,
            ink.p.hover_bg.gamma_multiply(hov),
            Stroke::new(1.0, ink.p.stroke.lerp_to_gamma(ink.p.hover_line, hov)),
        );
        paint::pop(p, rect, 6, ink, hov);
    }
    let mut child = ui.new_child(
        UiBuilder::new()
            .id_salt(("ask-fold", n.id.as_str()))
            .max_rect(rect.shrink2(FOLD_PAD))
            .layout(Layout::top_down(Align::Min)),
    );
    draw(&mut child);
    let resp = resp.on_hover_cursor(CursorIcon::PointingHand);
    let resp = match n.str("title") {
        Some(t) => resp.on_hover_text(text::clean(t).into_owned()),
        None => resp,
    };
    if resp.clicked()
        && let Some(act) = n.click()
    {
        cx.click(n, act);
    }
}

/// A fieldset's legend (`strong`) or a field's label, in the label face over what they name.
fn caption(cx: &Cx, ui: &mut Ui, n: &SNode, strong: bool) {
    let ink = cx.ink;
    let tx = if strong {
        Tx {
            font: ink.label(12.5),
            color: ink.accent_text(),
            line: None,
        }
    } else {
        Tx {
            font: ink.label(12.0),
            color: ink.dim(),
            line: None,
        }
    };
    cx.with_tx(tx, || render::text_node(cx, ui, n));
}

/// omp's warning under the questions while the composer holds a draft (`⚠ Sending your message
/// skips these questions · ⇧⇥ answers them`). Its key hint arrives as Nerd-Font icons, which are
/// never drawn: here they read as key names, lit and set off from the words around them.
fn skip(cx: &Cx, ui: &mut Ui, n: &SNode) {
    let ink = cx.ink;
    let tx = cx.tx();
    let warn = ink.tone("warning");
    let mut base = Base::new(tx.font, warn, ink);
    base.line_height = tx.line;
    let words = base.format_plain();
    let keys = TextFormat {
        color: ink.strong(),
        ..words.clone()
    };
    let mut job = LayoutJob::default();
    let mut chord = false;
    for span in n.arr("spans") {
        let (t, tokens) = match span {
            Value::String(t) => (t.as_str(), ""),
            Value::Object(o) => (
                o.get("t").and_then(Value::as_str).unwrap_or_default(),
                o.get("s").and_then(Value::as_str).unwrap_or_default(),
            ),
            _ => continue,
        };
        if tokens.split_whitespace().any(|s| s == "icon") {
            for key in t.chars().filter_map(key_name) {
                if chord {
                    job.append("+", 0.0, keys.clone());
                } else if !job.text.is_empty() && !job.text.ends_with(' ') {
                    job.append(" ", 0.0, words.clone());
                }
                job.append(key, 0.0, keys.clone());
                chord = true;
            }
        } else {
            let t = text::clean(t);
            if t.is_empty() {
                continue;
            }
            if chord && !t.starts_with(' ') {
                job.append(" ", 0.0, words.clone());
            }
            job.append(&t, 0.0, words.clone());
            chord = false;
        }
    }
    render::para(cx, ui, job, warn, &[], RunOpts::LINE);
}

/// The key a keycap icon of omp's `nerd` symbol preset names (its key hints), in words.
fn key_name(c: char) -> Option<&'static str> {
    Some(match c {
        '\u{f0634}' => "Ctrl",
        '\u{f0635}' => "Option",
        '\u{f0633}' => "Cmd",
        '\u{f0636}' => "Shift",
        '\u{f0311}' => "Enter",
        '\u{f12b7}' => "Esc",
        '\u{f0312}' => "Tab",
        '\u{f006e}' => "Backspace",
        '\u{f0e7e}' => "Delete",
        '\u{f1050}' => "Space",
        _ => return None,
    })
}
