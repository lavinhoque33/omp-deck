//! The agent inspector popup: facts header plus Transcript / Output / Task tabs. The transcript
//! watcher lives exactly as long as the popup; the output file is read on a worker thread.

use std::cell::Cell;
use std::path::PathBuf;
use std::time::SystemTime;

use crate::icons::Icon;
use crate::icons::{
    ICON_ARROW_DOWN_TO_LINE, ICON_CHEVRON_DOWN, ICON_CHEVRON_LEFT, ICON_CHEVRON_RIGHT,
    ICON_CHEVRONS_DOWN, ICON_CLIPBOARD_LIST, ICON_COPY, ICON_EXTERNAL_LINK, ICON_FILE_TEXT,
    ICON_MESSAGES_SQUARE, ICON_PLAY, ICON_SEND, ICON_SQUARE, ICON_X,
};
use deck_core::agents::{AgentStatus, AgentSummary, TranscriptWatcher};
use deck_core::ctl::LiveStatus;
use egui::{
    Align2, Color32, CursorIcon, Frame, Id, Key, KeyboardShortcut, Margin, Modifiers, Order,
    RichText, Sense, Sides, Stroke, StrokeKind, TextEdit, Ui, pos2, vec2,
};

use crate::agent_ctl::{self, CHAT_TIP, CtlView, ReqKind};
use crate::agent_hub::{Shown, shown, status_color, status_icon};
use crate::agent_model::{
    AgentsView, fmt_agent_cost, fmt_duration, live_duration, short_model, status_label,
};
use crate::agent_output::{MdPane, OutputPane};
use crate::sidebar::format_age;
use crate::statusfmt::fmt_tokens;
use crate::theme::{self, Palette};
use crate::transcript_view::TranscriptView;
use crate::widgets::{Spin, empty_state, icon_text, paint_icon};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopupTab {
    Transcript,
    Output,
    Task,
}

pub enum PopupAction {
    Close,
    Open(String),
    Copy {
        text: String,
        what: &'static str,
    },
    OpenFile(PathBuf),
    Error(String),
    /// Ask to kill this agent (the app confirms first).
    Kill(String),
    Revive(String),
    Chat {
        agent: String,
        text: String,
    },
    /// Open (or focus) the tab of the inspected agent's session for live control.
    OpenSession,
}

pub struct PopupState {
    /// The session file whose subagent this is.
    pub session: PathBuf,
    pub agent_id: String,
    pub tab: PopupTab,
    watcher: TranscriptWatcher,
    transcript: TranscriptView,
    output: OutputPane,
    task_md: MdPane,
    /// Position of this agent in the hub's filtered order, while it was in it.
    nav_pos: Cell<Option<usize>>,
    facts_key: Option<u64>,
    facts: Vec<(&'static str, String)>,
    /// The chat composer's text; the app clears it once omp accepted the message.
    pub draft: String,
    /// Give the composer the keyboard as soon as it can take text.
    focus_composer: bool,
    chat_was_pending: bool,
}

impl PopupState {
    /// Opens the inspector for `agent`; the transcript watcher starts here.
    pub fn new(
        ctx: &egui::Context,
        session: PathBuf,
        agent: &AgentSummary,
        tab: PopupTab,
    ) -> PopupState {
        let repaint = {
            let ctx = ctx.clone();
            move || ctx.request_repaint()
        };
        PopupState {
            session,
            agent_id: agent.id.clone(),
            tab,
            watcher: TranscriptWatcher::spawn(agent.path.clone(), repaint),
            transcript: TranscriptView::new(agent.status == AgentStatus::Running),
            output: OutputPane::new(),
            task_md: MdPane::default(),
            nav_pos: Cell::new(None),
            facts_key: None,
            facts: Vec::new(),
            draft: String::new(),
            focus_composer: true,
            chat_was_pending: false,
        }
    }
}

pub struct PopupInput<'a> {
    pub view: &'a AgentsView,
    /// Index of the inspected agent in `view.agents()`.
    pub index: usize,
    /// The hub's visible order (prev/next walk it).
    pub order: &'a [usize],
    /// The session has no live control from here (no tab, or not the active one): the popup
    /// is read-only and offers to open the session.
    pub needs_open: bool,
    pub ctl: &'a CtlView<'a>,
}

/// `(label, value)` tiles under the header.
pub fn facts(a: &AgentSummary, now: SystemTime) -> Vec<(&'static str, String)> {
    let mut f = vec![
        ("Cost", fmt_agent_cost(a.cost)),
        ("Tokens", fmt_tokens(a.tokens)),
        ("Requests", a.requests.to_string()),
        ("Tools", a.tool_calls.to_string()),
    ];
    if let Some(d) = live_duration(a, now) {
        f.push(("Duration", fmt_duration(d)));
    }
    if let Some(c) = a.context_tokens {
        f.push(("Context", format!("{} tok", fmt_tokens(c))));
    }
    if let Some(t) = a.created {
        f.push(("Created", age_ago(now, t)));
    }
    if let Some(t) = a.last_activity {
        f.push(("Last activity", age_ago(now, t)));
    }
    f
}

/// `"10m ago"`, or `"just now"` when `format_age` says `"now"`.
fn age_ago(now: SystemTime, t: SystemTime) -> String {
    let age = format_age(now, t);
    if age == "now" {
        "just now".to_owned()
    } else {
        format!("{age} ago")
    }
}

/// Draws the popup (scrim + window). Call every frame while it is open.
pub fn show(ctx: &egui::Context, st: &mut PopupState, input: &PopupInput<'_>) -> Vec<PopupAction> {
    let mut out = Vec::new();
    let pal = Palette::of_ctx(ctx);
    let screen = ctx.content_rect();

    // Scrim: dims the app and swallows clicks; a click on it closes the popup.
    egui::Area::new(Id::unique("agent-popup-scrim"))
        .order(Order::Background)
        .fixed_pos(screen.min)
        .interactable(true)
        .show(ctx, |ui| {
            let (rect, resp) = ui.allocate_exact_size(screen.size(), Sense::click());
            let dark = ctx.global_style().visuals.dark_mode;
            ui.painter().rect_filled(rect, 0, theme::backdrop(dark));
            if resp.clicked() {
                out.push(PopupAction::Close);
            }
        });

    let Some(agent) = input.view.agents().get(input.index) else {
        return out;
    };
    let size = vec2(
        (screen.width() * 0.72)
            .clamp(560.0, 1500.0)
            .min(screen.width() - 24.0),
        (screen.height() * 0.78)
            .clamp(380.0, 1100.0)
            .min(screen.height() - 24.0),
    );
    let style = ctx.global_style();
    let frame = Frame::NONE
        .fill(pal.bar)
        .stroke(theme::popup_stroke(pal))
        .corner_radius(theme::popup_radius())
        .shadow(style.visuals.window_shadow);
    egui::Window::new("agent-popup")
        .id(Id::unique("agent-popup-window"))
        .title_bar(false)
        .frame(frame)
        .resizable(true)
        .movable(true)
        .collapsible(false)
        .constrain_to(screen)
        .min_size(vec2(520.0, 340.0))
        .default_size(size)
        .default_pos(screen.center() - size / 2.0)
        .show(ctx, |ui| {
            popup_body(ui, st, input, agent, &mut out);
        });
    out
}

fn popup_body(
    ui: &mut Ui,
    st: &mut PopupState,
    input: &PopupInput<'_>,
    a: &AgentSummary,
    out: &mut Vec<PopupAction>,
) {
    let pal = Palette::of_ui(ui);
    let view = input.view;
    match st.output.poll() {
        Some(Ok(text)) => out.push(PopupAction::Copy {
            text,
            what: "output",
        }),
        Some(Err(e)) => out.push(PopupAction::Error(format!(
            "Cannot read the output file: {e}"
        ))),
        None => {}
    }
    ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
    ui.spacing_mut().scroll = egui::style::ScrollStyle::floating();

    if st.facts_key != Some(view.tick) {
        st.facts = facts(a, SystemTime::now());
        st.facts_key = Some(view.tick);
    }
    let live = view.cards.get(input.index).and_then(|c| c.live);
    composer_panel(ui, st, input, a, live, out);

    // ---- header
    Frame::NONE
        .inner_margin(Margin {
            left: 22,
            right: 18,
            top: 16,
            bottom: 10,
        })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            header_rows(ui, st, input, a, out);
        });

    // ---- tabs
    tab_bar(ui, st, a, out);
    let line = ui.max_rect().left()..=ui.max_rect().right();
    ui.painter().hline(
        line,
        ui.cursor().top(),
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.9)),
    );

    // ---- body
    Frame::NONE
        .inner_margin(Margin {
            left: 18,
            right: 14,
            top: 10,
            bottom: 14,
        })
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            match st.tab {
                PopupTab::Transcript => {
                    let w = &st.watcher;
                    st.transcript.sync(w.revision(), || w.latest());
                    if st.transcript.is_empty() {
                        empty_state(
                            ui,
                            ICON_MESSAGES_SQUARE,
                            pal.accent,
                            if a.status == AgentStatus::Running {
                                "Waiting for the first entries…"
                            } else {
                                "Empty transcript"
                            },
                            "Tool calls, thinking and replies appear here as the agent works.",
                        );
                    } else {
                        st.transcript.show(ui, a.status == AgentStatus::Running);
                    }
                }
                PopupTab::Output => st.output.ui(ui, a),
                PopupTab::Task => {
                    if a.task.trim().is_empty() {
                        empty_state(
                            ui,
                            ICON_CLIPBOARD_LIST,
                            pal.weak,
                            "No assignment recorded",
                            "",
                        );
                    } else {
                        // The task text only changes with the file-inferred snapshot.
                        let generation = view.base_version as usize;
                        st.task_md
                            .show(ui, Id::unique("agent-task-md"), &a.task, generation);
                    }
                }
            }
        });
}

fn header_rows(
    ui: &mut Ui,
    st: &PopupState,
    input: &PopupInput<'_>,
    a: &AgentSummary,
    out: &mut Vec<PopupAction>,
) {
    let pal = Palette::of_ui(ui);
    let view = input.view;
    let agents = view.agents();
    let sh = shown(
        pal,
        a.status,
        view.cards.get(input.index).and_then(|c| c.live),
    );
    let color = sh.color;

    // Row 1: pill, id, badges … navigation.
    Sides::new().height(32.0).show(
        ui,
        |ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            status_pill(ui, &sh);
            ui.label(theme::strong(&a.id).size(19.0).color(pal.text_strong));
            if let Some(t) = &a.agent {
                badge(ui, t, pal.accent);
            }
            if a.advisor {
                badge(ui, "advisor", pal.warn);
            }
        },
        |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            if nav_button(ui, ICON_X, "Close (Esc)", true).clicked() {
                out.push(PopupAction::Close);
            }
            ui.add_space(6.0);
            let pos = input.order.iter().position(|&i| i == input.index);
            // The open agent can drop out of the filtered order (status change, new filter):
            // keep walking from where it was instead of jumping into the unfiltered list.
            let (prev, next) = match (pos, st.nav_pos.get()) {
                (Some(p), _) => (
                    p.checked_sub(1).map(|q| input.order[q]),
                    input.order.get(p + 1).copied(),
                ),
                (None, Some(p)) => (
                    p.checked_sub(1).and_then(|q| input.order.get(q)).copied(),
                    input.order.get(p).copied(),
                ),
                (None, None) => (None, None),
            };
            if nav_button(ui, ICON_CHEVRON_RIGHT, "Next agent", next.is_some()).clicked()
                && let Some(i) = next
            {
                out.push(PopupAction::Open(agents[i].id.clone()));
            }
            if let Some(p) = pos {
                st.nav_pos.set(Some(p));
                ui.label(
                    RichText::new(format!("{} / {}", p + 1, input.order.len()))
                        .size(11.5)
                        .color(pal.weak),
                );
            }
            if nav_button(ui, ICON_CHEVRON_LEFT, "Previous agent", prev.is_some()).clicked()
                && let Some(i) = prev
            {
                out.push(PopupAction::Open(agents[i].id.clone()));
            }
        },
    );

    // Row 2: model, thinking, lineage.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
        if let Some(m) = &a.model {
            chip_text(ui, &short_model(m), pal.text, pal.raised).on_hover_text(m);
        }
        if let Some(t) = &a.thinking {
            chip_text(
                ui,
                &format!("thinking · {t}"),
                if theme::flat() { pal.text } else { pal.accent },
                theme::accent_fill(pal, 0.14),
            );
        }
        lineage(ui, input, a, out);
        ctl_buttons(ui, input, a, out);
    });
    ui.add_space(6.0);

    // Facts.
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        for (label, value) in &st.facts {
            fact_tile(ui, label, value);
        }
    });

    // Current tool / error.
    if let Some(cur) = a
        .current
        .as_deref()
        .filter(|_| a.status == AgentStatus::Running)
    {
        ui.add_space(4.0);
        note_strip(ui, pal.accent_b, "Running", cur);
    }
    if let Some(e) = a.error.as_deref().filter(|e| {
        !e.trim().is_empty()
            && matches!(
                a.status,
                AgentStatus::Failed | AgentStatus::Aborted | AgentStatus::Interrupted
            )
    }) {
        ui.add_space(4.0);
        note_strip(ui, color, status_label(a.status), e);
    }
}

fn lineage(ui: &mut Ui, input: &PopupInput<'_>, a: &AgentSummary, out: &mut Vec<PopupAction>) {
    let pal = Palette::of_ui(ui);
    let agents = input.view.agents();
    ui.label(RichText::new("·").color(pal.weak));
    ui.label(RichText::new("spawned by").size(12.5).color(pal.weak));
    match a.parent_id.as_deref() {
        None => {
            ui.label(RichText::new("the main session").size(12.5).color(pal.text));
        }
        Some(p) => {
            let known = agents.iter().any(|x| x.id == p);
            let r = ui.add(
                egui::Label::new(
                    RichText::new(p)
                        .size(12.5)
                        .color(if known { pal.accent_b } else { pal.text })
                        .underline(),
                )
                .sense(if known {
                    Sense::click()
                } else {
                    Sense::hover()
                }),
            );
            if known && r.on_hover_cursor(CursorIcon::PointingHand).clicked() {
                out.push(PopupAction::Open(p.to_owned()));
            }
        }
    }
    let kids: Vec<&AgentSummary> = agents
        .iter()
        .filter(|x| x.parent_id.as_deref() == Some(a.id.as_str()))
        .collect();
    if !kids.is_empty() {
        ui.label(RichText::new("·").color(pal.weak));
        let r = ui.add(
            egui::Button::new((
                RichText::new(format!(
                    "{} child{}",
                    kids.len(),
                    if kids.len() == 1 { "" } else { "ren" }
                ))
                .size(12.5)
                .color(pal.accent_b),
                icon_text(ICON_CHEVRON_DOWN, 14.0, pal.accent_b),
            ))
            .frame(false),
        );
        let r = r.on_hover_cursor(CursorIcon::PointingHand);
        theme::dropdown_start_menu(&r, 260.0, |ui| {
            for k in &kids {
                let kc = status_color(pal, k.status);
                let resp = ui.add(egui::Button::new((
                    icon_text(status_icon(k.status), 16.0, kc),
                    RichText::new(&k.id).color(pal.text),
                )));
                if resp.clicked() {
                    out.push(PopupAction::Open(k.id.clone()));
                }
            }
        });
    }
}

fn status_pill(ui: &mut Ui, sh: &Shown) {
    let (text, color) = (sh.label, sh.color);
    let g = ui
        .painter()
        .layout_no_wrap(text.to_owned(), crate::theme::font(12.5), color);
    let w = g.size().x + 40.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, 26.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, 13, color.gamma_multiply(0.16));
    ui.painter().rect_stroke(
        rect,
        13,
        Stroke::new(1.0, color.gamma_multiply(0.6)),
        StrokeKind::Inside,
    );
    if sh.spin {
        Spin::new(14.0, color).paint_at(ui, pos2(rect.left() + 15.0, rect.center().y));
    } else {
        paint_icon(
            ui.painter(),
            pos2(rect.left() + 15.0, rect.center().y),
            Align2::CENTER_CENTER,
            sh.icon,
            16.0,
            color,
        );
    }
    ui.painter().galley(
        pos2(rect.left() + 28.0, rect.center().y - g.size().y / 2.0),
        g,
        color,
    );
}

/// Kill / Revive for the inspected agent while the control link is up.
fn ctl_buttons(ui: &mut Ui, input: &PopupInput<'_>, a: &AgentSummary, out: &mut Vec<PopupAction>) {
    let pal = Palette::of_ui(ui);
    let ctl = input.ctl;
    let live = input.view.cards.get(input.index).and_then(|c| c.live);
    if let Some(kind) = ctl.pending(&a.id).filter(|k| *k != ReqKind::Chat) {
        ui.add(Spin::new(14.0, pal.accent));
        let text = if kind == ReqKind::Kill {
            "Killing…"
        } else {
            "Reviving…"
        };
        ui.label(RichText::new(text).size(12.0).color(pal.weak));
        return;
    }
    if agent_ctl::can_kill(ctl.link, live)
        && action_button(
            ui,
            ICON_SQUARE,
            "Kill",
            pal.danger,
            "Abort this agent and release it (it keeps its transcript)",
        )
        .clicked()
    {
        out.push(PopupAction::Kill(a.id.clone()));
    }
    if agent_ctl::can_revive(ctl.link, live)
        && action_button(
            ui,
            ICON_PLAY,
            "Revive",
            pal.ok,
            "Bring this parked agent back so it can be messaged",
        )
        .clicked()
    {
        out.push(PopupAction::Revive(a.id.clone()));
    }
}

fn action_button(
    ui: &mut Ui,
    icon: Icon,
    label: &str,
    color: Color32,
    tip: &str,
) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let g =
        ui.painter()
            .layout_no_wrap(label.to_owned(), crate::theme::font(12.5), pal.text_strong);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 40.0, 24.0), Sense::click());
    let p = ui.painter();
    p.rect_filled(
        rect,
        12,
        color.gamma_multiply(if resp.hovered() { 0.3 } else { 0.14 }),
    );
    p.rect_stroke(
        rect,
        12,
        Stroke::new(1.0, color.gamma_multiply(0.6)),
        StrokeKind::Inside,
    );
    paint_icon(
        p,
        pos2(rect.left() + 15.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        15.0,
        color,
    );
    p.galley(
        pos2(rect.left() + 27.0, rect.center().y - g.size().y / 2.0),
        g,
        pal.text_strong,
    );
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

/// The chat footer: multi-line editor (Enter sends, Shift+Enter adds a line) and a send button.
fn composer_panel(
    ui: &mut Ui,
    st: &mut PopupState,
    input: &PopupInput<'_>,
    a: &AgentSummary,
    live: Option<LiveStatus>,
    out: &mut Vec<PopupAction>,
) {
    if input.needs_open {
        open_session_panel(ui, out);
        return;
    }
    let pal = Palette::of_ui(ui);
    let ctl = input.ctl;
    egui::Panel::bottom(Id::unique(("agent-popup-composer", &a.id)))
        .resizable(false)
        .show_separator_line(false)
        .frame(
            Frame::NONE
                .fill(pal.surface)
                .inner_margin(Margin {
                    left: 18,
                    right: 14,
                    top: 10,
                    bottom: 12,
                })
                .corner_radius(egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: theme::popup_radius(),
                    se: theme::popup_radius(),
                }),
        )
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 6.0);
            let reason = agent_ctl::chat_disabled_reason(ctl.link, live);
            let pending = ctl.pending(&a.id) == Some(ReqKind::Chat);
            if let Some(err) = ctl.error(&a.id) {
                note_strip(ui, pal.danger, "Not sent", err);
            }
            let enabled = reason.is_none() && !pending;
            let id = Id::unique(("agent-chat-draft", &a.id));
            if st.chat_was_pending && !pending {
                st.focus_composer = true;
            }
            st.chat_was_pending = pending;
            if st.focus_composer && enabled {
                // The terminal behind may reclaim focus on the frames right after the popup
                // opens, so keep asking until the editor actually holds it.
                if ui.memory(|m| m.has_focus(id)) {
                    st.focus_composer = false;
                } else {
                    ui.memory_mut(|m| m.request_focus(id));
                    ui.ctx().request_repaint();
                }
            }
            // Plain Enter sends; the editor's own return key is Shift+Enter (new line).
            let enter = enabled
                && ui.memory(|m| m.has_focus(id))
                && ui.input(|i| {
                    i.events.iter().any(|e| {
                        matches!(
                            e,
                            egui::Event::Key {
                                key: Key::Enter,
                                pressed: true,
                                modifiers,
                                ..
                            } if !modifiers.shift
                        )
                    })
                });
            let mut sent = None;
            let hint = reason.unwrap_or_else(|| agent_ctl::composer_hint(live));
            let send_w = 38.0;
            ui.horizontal_top(|ui| {
                let w = (ui.available_width() - send_w - ui.spacing().item_spacing.x).max(80.0);
                let focused = ui.memory(|m| m.has_focus(id));
                let edit = TextEdit::multiline(&mut st.draft)
                    .id(id)
                    .hint_text(hint)
                    .desired_rows(2)
                    .min_size(vec2(w, 56.0))
                    .desired_width(w)
                    .return_key(KeyboardShortcut::new(Modifiers::SHIFT, Key::Enter))
                    .margin(Margin::symmetric(10, 8))
                    .frame(
                        Frame::new()
                            .fill(pal.bar)
                            .corner_radius(theme::rad(8, 10))
                            .stroke(Stroke::new(
                                1.0,
                                if focused && enabled {
                                    pal.accent
                                } else {
                                    pal.stroke
                                },
                            )),
                    );
                let resp = ui
                    .vertical(|ui| {
                        ui.set_width(w);
                        egui::ScrollArea::vertical()
                            .max_height(120.0)
                            .auto_shrink([false, true])
                            .show(ui, |ui| ui.add_enabled(enabled, edit))
                            .inner
                    })
                    .inner;
                if enabled {
                    resp.on_hover_text(CHAT_TIP);
                } else {
                    resp.on_disabled_hover_text(if pending { "Sending…" } else { hint });
                }
                let can_send = enabled && agent_ctl::prepare_message(&st.draft).is_some();
                let sense = if can_send {
                    Sense::click()
                } else {
                    Sense::hover()
                };
                let (rect, resp) = ui.allocate_exact_size(vec2(send_w, 36.0), sense);
                let clicked = resp.clicked();
                let fill = if can_send {
                    pal.accent
                        .gamma_multiply(if resp.hovered() { 1.0 } else { 0.85 })
                } else {
                    pal.raised.gamma_multiply(0.7)
                };
                ui.painter().rect_filled(rect, theme::rad(8, 10), fill);
                if pending {
                    Spin::new(16.0, pal.accent).paint_at(ui, rect.center());
                } else {
                    paint_icon(
                        ui.painter(),
                        rect.center(),
                        Align2::CENTER_CENTER,
                        ICON_SEND,
                        18.0,
                        if can_send {
                            theme::on_accent(pal)
                        } else {
                            pal.weak.gamma_multiply(0.5)
                        },
                    );
                }
                resp.on_hover_text(CHAT_TIP)
                    .on_hover_cursor(CursorIcon::PointingHand);
                if can_send && (enter || clicked) {
                    sent = agent_ctl::prepare_message(&st.draft);
                }
            });
            if let Some(text) = sent {
                out.push(PopupAction::Chat {
                    agent: a.id.clone(),
                    text,
                });
            }
        });
}

/// Footer of a read-only popup: the way to live control.
fn open_session_panel(ui: &mut Ui, out: &mut Vec<PopupAction>) {
    let pal = Palette::of_ui(ui);
    egui::Panel::bottom(Id::unique("agent-popup-open-session"))
        .resizable(false)
        .show_separator_line(false)
        .frame(
            Frame::NONE
                .fill(pal.surface)
                .inner_margin(Margin {
                    left: 18,
                    right: 14,
                    top: 10,
                    bottom: 12,
                })
                .corner_radius(egui::CornerRadius {
                    nw: 0,
                    ne: 0,
                    sw: theme::popup_radius(),
                    se: theme::popup_radius(),
                }),
        )
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(
                        "Read-only: this session has no tab here, so its agents cannot be \
                         killed, revived or messaged.",
                    )
                    .size(12.0)
                    .color(pal.weak),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button("Open session for live control")
                        .on_hover_text(
                            "Attach a tab to this session; chat, kill and revive work there",
                        )
                        .clicked()
                    {
                        out.push(PopupAction::OpenSession);
                    }
                });
            });
        });
}

fn badge(ui: &mut Ui, text: &str, color: Color32) {
    let g = ui
        .painter()
        .layout_no_wrap(text.to_owned(), crate::theme::font(11.5), color);
    let (rect, _) = ui.allocate_exact_size(vec2(g.size().x + 16.0, 20.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, 10, color.gamma_multiply(0.16));
    ui.painter()
        .galley(rect.center() - g.size() / 2.0, g, color);
}

fn chip_text(ui: &mut Ui, text: &str, fg: Color32, bg: Color32) -> egui::Response {
    let g = ui
        .painter()
        .layout_no_wrap(text.to_owned(), crate::theme::font(12.0), fg);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 18.0, 22.0), Sense::hover());
    ui.painter().rect_filled(rect, 11, bg);
    ui.painter().galley(rect.center() - g.size() / 2.0, g, fg);
    resp
}

fn fact_tile(ui: &mut Ui, label: &str, value: &str) {
    let pal = Palette::of_ui(ui);
    let lg = ui
        .painter()
        .layout_no_wrap(label.to_uppercase(), crate::theme::font(9.5), pal.weak);
    let vg =
        ui.painter()
            .layout_no_wrap(value.to_owned(), crate::theme::font(14.0), pal.text_strong);
    let w = lg.size().x.max(vg.size().x) + 22.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w.max(66.0), 46.0), Sense::hover());
    ui.painter().rect_filled(rect, 10, pal.surface);
    ui.painter().rect_stroke(
        rect,
        10,
        Stroke::new(1.0, pal.stroke.gamma_multiply(0.8)),
        StrokeKind::Inside,
    );
    ui.painter()
        .galley(pos2(rect.left() + 11.0, rect.top() + 8.0), lg, pal.weak);
    ui.painter().galley(
        pos2(rect.left() + 11.0, rect.top() + 22.0),
        vg,
        pal.text_strong,
    );
}

fn note_strip(ui: &mut Ui, color: Color32, label: &str, text: &str) {
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(color.gamma_multiply(0.11))
        .stroke(theme::status_stroke(pal, color, 0.35))
        .corner_radius(theme::rad(8, 9))
        .inner_margin(Margin::symmetric(12, 7))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                ui.label(theme::section_caption(label, color));
                ui.add(
                    egui::Label::new(RichText::new(text).size(12.5).color(color))
                        .wrap_mode(egui::TextWrapMode::Wrap),
                );
            });
        });
}

fn nav_button(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), sense);
    let hov = ui
        .ctx()
        .animate_bool_with_time(resp.id.with("hov"), resp.hovered() && enabled, 0.1);
    ui.painter()
        .rect_filled(rect, 9, pal.raised.gamma_multiply(0.9 * hov));
    paint_icon(
        ui.painter(),
        rect.center(),
        Align2::CENTER_CENTER,
        icon,
        20.0,
        if enabled {
            theme::mix(pal.weak, pal.text_strong, hov)
        } else {
            pal.weak.gamma_multiply(0.35)
        },
    );
    resp.on_hover_text(tip).on_hover_cursor(if enabled {
        CursorIcon::PointingHand
    } else {
        CursorIcon::Default
    })
}

// ------------------------------------------------------------------ tabs

fn tab_bar(ui: &mut Ui, st: &mut PopupState, a: &AgentSummary, out: &mut Vec<PopupAction>) {
    let current = st.tab;
    let mut picked = None;
    Frame::NONE
        .inner_margin(Margin::symmetric(18, 0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            Sides::new().height(36.0).show(
                ui,
                |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (tab, icon, label, dim) in [
                        (
                            PopupTab::Transcript,
                            ICON_MESSAGES_SQUARE,
                            "Transcript",
                            false,
                        ),
                        (
                            PopupTab::Output,
                            ICON_FILE_TEXT,
                            "Output",
                            a.output_path.is_none(),
                        ),
                        (PopupTab::Task, ICON_CLIPBOARD_LIST, "Task", false),
                    ] {
                        if tab_button(ui, icon, label, current == tab, dim).clicked() {
                            picked = Some(tab);
                        }
                    }
                },
                |ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    match current {
                        PopupTab::Transcript => {
                            if action_chip(
                                ui,
                                ICON_COPY,
                                "Transcript path",
                                "Copy the transcript file path",
                                false,
                            )
                            .clicked()
                            {
                                out.push(PopupAction::Copy {
                                    text: a.path.to_string_lossy().into_owned(),
                                    what: "transcript path",
                                });
                            }
                            if action_chip(
                                ui,
                                ICON_CHEVRONS_DOWN,
                                "Bottom",
                                "Jump to the latest entry",
                                false,
                            )
                            .clicked()
                            {
                                st.transcript.jump_to_end();
                            }
                            let follow = st.transcript.follow;
                            if action_chip(
                                ui,
                                ICON_ARROW_DOWN_TO_LINE,
                                "Follow",
                                "Keep the newest entry in view (scroll up to pause)",
                                follow,
                            )
                            .clicked()
                            {
                                if follow {
                                    st.transcript.follow = false;
                                } else {
                                    st.transcript.jump_to_end();
                                }
                            }
                        }
                        PopupTab::Output => {
                            if let Some(path) = &a.output_path {
                                if action_chip(
                                    ui,
                                    ICON_EXTERNAL_LINK,
                                    "Open file",
                                    "Open the output file with the desktop default",
                                    false,
                                )
                                .clicked()
                                {
                                    out.push(PopupAction::OpenFile(path.clone()));
                                }
                                if st.output.can_copy()
                                    && action_chip(
                                        ui,
                                        ICON_COPY,
                                        st.output.copy_label(),
                                        "Copy the whole output text",
                                        false,
                                    )
                                    .clicked()
                                {
                                    match st.output.copy_clicked(ui.ctx()) {
                                        Ok(Some(text)) => out.push(PopupAction::Copy {
                                            text,
                                            what: "output",
                                        }),
                                        Ok(None) => {}
                                        Err(e) => out.push(PopupAction::Error(format!(
                                            "Cannot read the output file: {e}"
                                        ))),
                                    }
                                }
                            }
                        }
                        PopupTab::Task => {
                            if !a.task.is_empty()
                                && action_chip(ui, ICON_COPY, "Copy", "Copy the assignment", false)
                                    .clicked()
                            {
                                out.push(PopupAction::Copy {
                                    text: a.task.clone(),
                                    what: "assignment",
                                });
                            }
                        }
                    }
                },
            );
        });
    if let Some(t) = picked {
        st.tab = t;
    }
}

fn tab_button(ui: &mut Ui, icon: Icon, label: &str, on: bool, dim: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let g = ui
        .painter()
        .layout_no_wrap(label.to_owned(), crate::theme::font(13.5), pal.text);
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 46.0, 36.0), Sense::click());
    let hov = resp.hovered();
    let color = if on {
        pal.text_strong
    } else if dim {
        pal.weak.gamma_multiply(0.55)
    } else if hov {
        pal.text
    } else {
        pal.weak
    };
    let p = ui.painter();
    paint_icon(
        p,
        pos2(rect.left() + 17.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        17.0,
        if on { pal.accent } else { color },
    );
    p.galley(
        pos2(rect.left() + 32.0, rect.center().y - g.size().y / 2.0),
        g,
        color,
    );
    if on {
        p.rect_filled(
            egui::Rect::from_min_max(
                pos2(rect.left() + 6.0, rect.bottom() - 2.5),
                pos2(rect.right() - 6.0, rect.bottom()),
            ),
            2,
            pal.accent,
        );
    }
    resp.on_hover_cursor(CursorIcon::PointingHand)
}

fn action_chip(ui: &mut Ui, icon: Icon, label: &str, tip: &str, on: bool) -> egui::Response {
    let pal = Palette::of_ui(ui);
    let g = ui
        .painter()
        .layout_no_wrap(label.to_owned(), crate::theme::font(12.0), pal.text);
    // Right-to-left layout: allocate, then paint.
    let (rect, resp) = ui.allocate_exact_size(vec2(g.size().x + 40.0, 26.0), Sense::click());
    let p = ui.painter();
    let fill = if on {
        theme::accent_fill(pal, 0.26)
    } else if resp.hovered() {
        pal.raised
    } else {
        pal.surface.gamma_multiply(0.8)
    };
    p.rect_filled(rect, 13, fill);
    p.rect_stroke(
        rect,
        13,
        Stroke::new(1.0, if on { pal.accent } else { pal.stroke }),
        StrokeKind::Inside,
    );
    let color = if on { pal.text_strong } else { pal.text };
    paint_icon(
        p,
        pos2(rect.left() + 15.0, rect.center().y),
        Align2::CENTER_CENTER,
        icon,
        15.0,
        if on { pal.accent_b } else { pal.weak },
    );
    p.galley(
        pos2(rect.left() + 26.0, rect.center().y - g.size().y / 2.0),
        g,
        color,
    );
    resp.on_hover_text(tip)
        .on_hover_cursor(CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn facts_skip_unknowns_and_format_values() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10_000);
        let a = AgentSummary {
            id: "A".into(),
            parent_id: None,
            depth: 0,
            path: PathBuf::from("/a.jsonl"),
            advisor: false,
            agent: None,
            model: None,
            thinking: None,
            task_gist: String::new(),
            task: String::new(),
            status: AgentStatus::Done,
            error: None,
            current: None,
            created: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - 600)),
            last_activity: None,
            duration: Some(Duration::from_secs(95)),
            requests: 3,
            tool_calls: 7,
            tokens: 12_345,
            cost: 0.5,
            context_tokens: None,
            output_path: None,
        };
        let f = facts(&a, now);
        let get = |k: &str| f.iter().find(|(l, _)| *l == k).map(|(_, v)| v.as_str());
        assert_eq!(get("Cost"), Some("$0.50"));
        assert_eq!(get("Tokens"), Some("12K"));
        assert_eq!(get("Duration"), Some("1m 35s"));
        assert_eq!(get("Created"), Some("10m ago"));
        assert_eq!(get("Context"), None);
        assert_eq!(get("Last activity"), None);
    }
}
