//! The Activity tab of the agents dashboard: what the subagents of one session did, newest first,
//! as omp's agent hub lists it (responses, tool calls, IRC, lifecycle). The rows come from
//! [`ActivityWatcher`], which only runs while this tab is on screen.
//!
//! The pure parts (filter, scope, search, the transcript anchor a row opens at) are at the top and
//! unit-tested; the painting follows.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use deck_core::agents::{ActivityKind, ActivityRow, ActivityStatus, ActivityWatcher, AgentSummary};
use egui::{
    Align2, Color32, CursorIcon, Frame, Id, Margin, Rect, RichText, ScrollArea, Sense, TextEdit,
    Ui, pos2, vec2,
};

use super::{DashAction, DashInput, DashState, filter_chip, hash_of, small_button};
use crate::agent_ctl;
use crate::badge::{Badge, Hue};
use crate::icons::{
    ICON_ARROW_UP, ICON_EYE, ICON_MESSAGES_SQUARE, ICON_NETWORK, ICON_POWER, ICON_SEARCH,
    ICON_WRENCH, Icon,
};
use crate::sidebar::format_age;
use crate::theme::{self, Palette};
use crate::transcript_view::Anchor;
use crate::widgets::{Spin, empty_state, icon_text, paint_icon};

/// Height of one event row.
const ROW_H: f32 = 50.0;
/// Space between two rows.
const ROW_GAP: f32 = 6.0;

// ------------------------------------------------------------------ pure parts

/// Which events the list shows (omp's hub scopes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Kind {
    #[default]
    All,
    Errors,
    Responses,
    Tools,
}

impl Kind {
    pub const EACH: [Kind; 4] = [Kind::All, Kind::Errors, Kind::Responses, Kind::Tools];

    pub fn label(self) -> &'static str {
        match self {
            Kind::All => "All",
            Kind::Errors => "Errors",
            Kind::Responses => "Responses",
            Kind::Tools => "Tools",
        }
    }

    pub fn admits(self, row: &ActivityRow) -> bool {
        match self {
            Kind::All => true,
            Kind::Errors => matches!(row.status, ActivityStatus::Error | ActivityStatus::Aborted),
            Kind::Responses => row.kind == ActivityKind::Response,
            Kind::Tools => row.kind == ActivityKind::Tool,
        }
    }
}

/// Which agents' events the list shows.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Scope {
    #[default]
    All,
    /// This agent only.
    Agent(String),
    /// This agent and everything below it.
    Subtree(String),
}

/// The agent ids `scope` admits; `None` = all of them.
pub fn scope_ids(agents: &[AgentSummary], scope: &Scope) -> Option<HashSet<String>> {
    match scope {
        Scope::All => None,
        Scope::Agent(id) => Some(HashSet::from([id.clone()])),
        Scope::Subtree(root) => {
            let mut ids = HashSet::from([root.clone()]);
            // Parents may follow their children in the list: repeat until nothing is added.
            let mut grew = true;
            while grew {
                grew = false;
                for a in agents {
                    if let Some(p) = &a.parent_id
                        && ids.contains(p)
                        && ids.insert(a.id.clone())
                    {
                        grew = true;
                    }
                }
            }
            Some(ids)
        }
    }
}

/// Whether `row` matches the search text (lowercase, whitespace-separated terms, every one must
/// hit the title, the summary or the agent).
fn row_matches(row: &ActivityRow, terms: &[String]) -> bool {
    terms.iter().all(|t| {
        row.title.to_lowercase().contains(t.as_str())
            || row.summary.to_lowercase().contains(t.as_str())
            || row.agent_id.to_lowercase().contains(t.as_str())
    })
}

/// What a list shows once the agent scope and the search are applied, and how many of each kind
/// that leaves (the chips' counts); `rows` are indices in the watcher's order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub rows: Vec<usize>,
    /// Per [`Kind::EACH`].
    pub counts: [usize; 4],
}

pub fn list(
    rows: &[ActivityRow],
    kind: Kind,
    ids: Option<&HashSet<String>>,
    query: &str,
) -> Listing {
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut out = Listing::default();
    for (i, row) in rows.iter().enumerate() {
        if ids.is_some_and(|ids| !ids.contains(&row.agent_id)) || !row_matches(row, &terms) {
            continue;
        }
        for (slot, k) in Kind::EACH.iter().enumerate() {
            if k.admits(row) {
                out.counts[slot] += 1;
            }
        }
        if kind.admits(row) {
            out.rows.push(i);
        }
    }
    out
}

/// Where the agent's transcript opens for `row`: a tool event at its call, a response at the
/// assistant message written then, anything else at the end.
pub fn anchor_of(row: &ActivityRow) -> Anchor {
    match (row.kind, &row.call_id) {
        (ActivityKind::Tool, Some(call)) => Anchor::Tool(call.clone()),
        (ActivityKind::Response, _) => Anchor::Response(row.at),
        _ => Anchor::End,
    }
}

/// The row a list is scrolled to (the first one on screen) and where the listing had it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Held {
    id: String,
    pos: usize,
}

/// How many listing positions `held` moved by since it was recorded (new events arrive at the
/// top, so it only grows), `None` when it left the listing (a filter or scope change).
fn drift(rows: &[ActivityRow], listing: &Listing, held: &Held) -> Option<isize> {
    let id_at = |pos: usize| {
        listing
            .rows
            .get(pos)
            .and_then(|&i| rows.get(i))
            .map(|r| r.id.as_str())
    };
    if id_at(held.pos) == Some(held.id.as_str()) {
        return Some(0);
    }
    (0..listing.rows.len())
        .find(|&p| id_at(p) == Some(held.id.as_str()))
        .map(|p| p as isize - held.pos as isize)
}

// ------------------------------------------------------------------ state

/// What the Activity tab remembers.
pub struct ActivityState {
    /// The session whose subagents are listed (`None`: the focused card's, chosen on show).
    pub session: Option<PathBuf>,
    pub kind: Kind,
    pub scope: Scope,
    /// Stick to the newest event.
    pub follow: bool,
    pub query: String,
    watcher: Option<ActivityWatcher>,
    watched: Vec<(String, PathBuf)>,
    rows: Arc<Vec<ActivityRow>>,
    rev: Option<u64>,
    listing: Listing,
    listing_key: Option<u64>,
    /// Scroll to the newest event on the next frame.
    to_top: bool,
    /// The first row on screen when follow is off, and the scroll offset it was seen at.
    held: Option<Held>,
    offset: f32,
}

impl Default for ActivityState {
    fn default() -> Self {
        ActivityState {
            session: None,
            kind: Kind::All,
            scope: Scope::All,
            follow: true,
            query: String::new(),
            watcher: None,
            watched: Vec::new(),
            rows: Arc::default(),
            rev: None,
            listing: Listing::default(),
            listing_key: None,
            to_top: true,
            held: None,
            offset: 0.0,
        }
    }
}

impl ActivityState {
    /// Stops reading the transcripts (the tab is not shown) and drops what was read.
    pub fn stop(&mut self) {
        self.watcher = None;
        self.watched.clear();
        self.rows = Arc::default();
        self.rev = None;
        self.listing = Listing::default();
        self.listing_key = None;
        self.to_top = true;
        self.held = None;
        self.offset = 0.0;
    }

    /// Follows exactly `agents` (id, transcript), starting the watcher on first use.
    fn watch(&mut self, ctx: &egui::Context, agents: Vec<(String, PathBuf)>) {
        if self.watched != agents {
            match &self.watcher {
                Some(w) => w.set_agents(agents.clone()),
                None => {
                    let ctx = ctx.clone();
                    self.watcher = Some(ActivityWatcher::spawn(agents.clone(), move || {
                        ctx.request_repaint();
                    }));
                }
            }
            self.watched = agents;
        }
        if let Some(w) = &self.watcher {
            let rev = w.revision();
            if self.rev != Some(rev) {
                self.rev = Some(rev);
                self.rows = w.latest();
            }
        }
    }

    /// Re-lists when the rows or what filters them changed.
    fn relist(&mut self, agents: &[AgentSummary]) {
        let key = hash_of(&(
            self.rev,
            self.kind,
            &self.scope,
            &self.query,
            self.session.as_deref(),
            self.watched.len(),
        ));
        if self.listing_key == Some(key) {
            return;
        }
        let ids = scope_ids(agents, &self.scope);
        self.listing = list(&self.rows, self.kind, ids.as_ref(), &self.query);
        self.listing_key = Some(key);
    }
}

// ------------------------------------------------------------------ view

/// The sessions with subagents the dashboard knows: `(path, title)` in card order.
fn sessions_with_agents(st: &DashState) -> Vec<(PathBuf, String)> {
    st.cards
        .iter()
        .filter_map(|c| {
            let path = c.session.as_ref()?;
            let sa = st.sessions.get(path)?;
            (sa.view.counts.total() > 0).then(|| (path.clone(), c.title.clone()))
        })
        .collect()
}

/// Draws the tab into the current ui.
pub fn show(ui: &mut Ui, st: &mut DashState, input: &DashInput<'_>, out: &mut Vec<DashAction>) {
    let pal = Palette::of_ui(ui);
    let sessions = sessions_with_agents(st);
    let focused = st
        .cards
        .iter()
        .find(|c| c.active)
        .and_then(|c| c.session.clone());
    let chosen = st
        .activity
        .session
        .clone()
        .filter(|s| sessions.iter().any(|(p, _)| p == s))
        .or_else(|| {
            focused
                .filter(|f| sessions.iter().any(|(p, _)| p == f))
                .or_else(|| sessions.first().map(|(p, _)| p.clone()))
        });
    let Some(session) = chosen else {
        st.activity.stop();
        empty_state(
            ui,
            ICON_NETWORK,
            pal.weak,
            "No subagent activity",
            "Sessions that ran subagents appear here once their cards have been read.",
        );
        return;
    };
    if st.activity.session.as_ref() != Some(&session) {
        st.activity.session = Some(session.clone());
        st.activity.scope = Scope::All;
        st.activity.to_top = true;
    }
    let Some(sa) = st.sessions.get(&session) else {
        return;
    };
    let agents = sa.view.agents();
    let watch: Vec<(String, PathBuf)> = agents
        .iter()
        .map(|a| (a.id.clone(), a.path.clone()))
        .collect();
    // Scope: an agent that is gone is no scope.
    if let Scope::Agent(id) | Scope::Subtree(id) = &st.activity.scope
        && !agents.iter().any(|a| &a.id == id)
    {
        st.activity.scope = Scope::All;
    }
    let ctx = ui.ctx().clone();
    st.activity.watch(&ctx, watch);
    st.activity.relist(agents);

    toolbar(ui, &mut st.activity, &sessions, agents);
    ui.add_space(10.0);
    let advisors: HashSet<&str> = agents
        .iter()
        .filter(|a| a.advisor)
        .map(|a| a.id.as_str())
        .collect();
    let view = st
        .cards
        .iter()
        .find(|c| c.session.as_deref() == Some(session.as_path()))
        .and_then(|c| input.views.get(&c.key))
        .copied()
        .unwrap_or_default();
    list_view(
        ui,
        &mut st.activity,
        &Ctx {
            session: &session,
            advisors: &advisors,
            view,
            now: SystemTime::now(),
        },
        out,
    );
}

struct Ctx<'a> {
    session: &'a Path,
    advisors: &'a HashSet<&'a str>,
    view: deck_core::config::SessionView,
    now: SystemTime,
}

fn toolbar(
    ui: &mut Ui,
    a: &mut ActivityState,
    sessions: &[(PathBuf, String)],
    agents: &[AgentSummary],
) {
    let pal = Palette::of_ui(ui);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        // Session chooser.
        let current = a
            .session
            .as_ref()
            .and_then(|s| sessions.iter().find(|(p, _)| p == s))
            .map_or("", |(_, t)| t.as_str());
        egui::ComboBox::from_id_salt("activity-session")
            .selected_text(RichText::new(current).color(pal.text_strong))
            .width(220.0)
            .show_ui(ui, |ui| {
                for (path, title) in sessions {
                    let on = a.session.as_ref() == Some(path);
                    if ui.selectable_label(on, title).clicked() && !on {
                        a.session = Some(path.clone());
                        a.scope = Scope::All;
                        a.to_top = true;
                    }
                }
            })
            .response
            .on_hover_text("The session whose subagents are listed");
        // Agent scope.
        let scope_text = match &a.scope {
            Scope::All => "All agents".to_owned(),
            Scope::Agent(id) => id.clone(),
            Scope::Subtree(id) => format!("{id} and below"),
        };
        egui::ComboBox::from_id_salt("activity-scope")
            .selected_text(RichText::new(scope_text).color(pal.text_strong))
            .width(200.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(a.scope == Scope::All, "All agents")
                    .clicked()
                {
                    a.scope = Scope::All;
                }
                for ag in agents {
                    let one = Scope::Agent(ag.id.clone());
                    if ui.selectable_label(a.scope == one, &ag.id).clicked() {
                        a.scope = one;
                    }
                    if agents
                        .iter()
                        .any(|c| c.parent_id.as_deref() == Some(&ag.id))
                    {
                        let tree = Scope::Subtree(ag.id.clone());
                        if ui
                            .selectable_label(a.scope == tree, format!("{} and below", ag.id))
                            .clicked()
                        {
                            a.scope = tree;
                        }
                    }
                }
            })
            .response
            .on_hover_text("Which agents' events are listed");
        if small_button(
            ui,
            ICON_ARROW_UP,
            "Follow: stay on the newest event as it arrives",
            a.follow,
        )
        .clicked()
        {
            a.follow = !a.follow;
            a.to_top |= a.follow;
        }
    });
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
        let focused = ui.memory(|m| m.has_focus(Id::unique("activity-search")));
        ui.add(
            TextEdit::singleline(&mut a.query)
                .id(Id::unique("activity-search"))
                .prefix(icon_text(ICON_SEARCH, 16.0, pal.weak))
                .hint_text("Search events…")
                .align(Align2::LEFT_CENTER)
                .min_size(vec2(0.0, 30.0))
                .desired_width(300.0)
                .margin(Margin::symmetric(10, 6))
                .frame(
                    Frame::new()
                        .fill(pal.surface)
                        .corner_radius(theme::ui_rad(theme::rad(8, 10)))
                        .stroke(egui::Stroke::new(
                            1.0,
                            if focused { pal.accent } else { pal.stroke },
                        )),
                ),
        );
        for (i, k) in Kind::EACH.iter().enumerate() {
            if filter_chip(ui, k.label(), a.listing.counts[i], a.kind == *k).clicked() {
                a.kind = *k;
            }
        }
    });
}

fn list_view(ui: &mut Ui, a: &mut ActivityState, cx: &Ctx<'_>, out: &mut Vec<DashAction>) {
    let pal = Palette::of_ui(ui);
    let n = a.listing.rows.len();
    if n == 0 {
        let (title, sub) = if a.rows.is_empty() {
            (
                "No activity yet",
                "Events appear here as the subagents of this session respond and call tools.",
            )
        } else {
            ("Nothing matches", "Try another filter, agent or search.")
        };
        empty_state(ui, ICON_NETWORK, pal.weak, title, sub);
        return;
    }
    let stride = ROW_H + ROW_GAP;
    let mut area = ScrollArea::vertical()
        .id_salt("activity-list")
        .auto_shrink([false, false]);
    if a.follow || a.to_top {
        // Follow on: stick to the newest event.
        area = area.vertical_scroll_offset(0.0);
        a.to_top = false;
        a.held = None;
    } else if let Some(shift) = a
        .held
        .as_ref()
        .and_then(|h| drift(&a.rows, &a.listing, h))
        .filter(|&s| s != 0)
    {
        // Follow off: rows that arrived above the ones being read push them down by their height;
        // the offset grows by the same, so what is on screen stays where it is.
        area = area.vertical_scroll_offset((a.offset + shift as f32 * stride).max(0.0));
    }
    ui.spacing_mut().item_spacing.y = ROW_GAP;
    let shown = area.show_rows(ui, ROW_H, n, |ui, range| {
        for pos in range {
            let Some(row) = a.listing.rows.get(pos).and_then(|&i| a.rows.get(i)) else {
                continue;
            };
            match event_row(ui, row, cx) {
                Click::None => {}
                Click::Open => out.push(DashAction::AgentAt {
                    session: cx.session.to_path_buf(),
                    id: row.agent_id.clone(),
                    anchor: anchor_of(row),
                }),
                Click::Chat => out.push(DashAction::OpenInChat {
                    session: cx.session.to_path_buf(),
                    id: row.agent_id.clone(),
                }),
            }
        }
    });
    // The wheel (or a drag on the scrollbar) away from the top releases "follow"; back at the top
    // it engages again.
    let user = ui.input(|i| i.smooth_scroll_delta.y != 0.0);
    let offset = shown.state.clamped_offset().y;
    if user {
        a.follow = offset <= 2.0;
    }
    // The first row on screen is what the next frame keeps in place (the id is copied only when
    // another row became the first).
    a.offset = offset;
    let pos = (offset / stride) as usize;
    if let Some(row) = a.listing.rows.get(pos).and_then(|&i| a.rows.get(i))
        && a.held
            .as_ref()
            .is_none_or(|h| h.pos != pos || h.id != row.id)
    {
        a.held = Some(Held {
            id: row.id.clone(),
            pos,
        });
    }
}

enum Click {
    None,
    Open,
    Chat,
}

/// The glyph and tone of an event: the kind picks the icon, the outcome the colour.
fn look(pal: &Palette, row: &ActivityRow) -> (Icon, Color32) {
    let icon = match row.kind {
        ActivityKind::Response => ICON_MESSAGES_SQUARE,
        ActivityKind::Tool => ICON_WRENCH,
        ActivityKind::Irc => ICON_NETWORK,
        ActivityKind::Lifecycle => ICON_POWER,
    };
    let tone = match row.status {
        ActivityStatus::Success => pal.ok,
        ActivityStatus::Error => pal.danger,
        ActivityStatus::Aborted => pal.warn,
        ActivityStatus::Pending => pal.accent_b,
    };
    (icon, tone)
}

fn event_row(ui: &mut Ui, row: &ActivityRow, cx: &Ctx<'_>) -> Click {
    let pal = Palette::of_ui(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::click());
    if !ui.is_rect_visible(rect) {
        return Click::None;
    }
    let id = Id::unique(("activity-row", &row.id));
    let hovered = resp.hovered();
    let (icon, tone) = look(pal, row);
    let p = ui.painter_at(rect);
    let fill = if hovered {
        theme::mix(pal.surface, pal.raised, 0.7)
    } else {
        pal.surface
    };
    theme::acc::paint_sub(&p, id, pal, rect, hovered, false, fill);
    p.rect_filled(
        Rect::from_min_max(
            pos2(rect.left() + 1.0, rect.top() + 8.0),
            pos2(rect.left() + 4.0, rect.bottom() - 8.0),
        ),
        2,
        tone,
    );

    let left = rect.left() + 14.0;
    let right = rect.right() - 12.0;
    let cy = rect.center().y;
    if row.status == ActivityStatus::Pending {
        Spin::new(16.0, tone).paint_at(ui, pos2(left + 9.0, cy));
    } else {
        paint_icon(
            &p,
            pos2(left + 9.0, cy),
            Align2::CENTER_CENTER,
            icon,
            18.0,
            tone,
        );
    }

    // Right side: age, then the hover button (its slot is always reserved).
    let age = p.layout_no_wrap(format_age(cx.now, row.at), theme::font(11.5), pal.weak);
    p.galley(
        pos2(right - age.size().x, cy - age.size().y / 2.0),
        age.clone(),
        pal.weak,
    );
    let blocked = agent_ctl::chat_blocked(cx.advisors.contains(row.agent_id.as_str()), cx.view);
    let eye = Rect::from_center_size(pos2(right - age.size().x - 20.0, cy), vec2(24.0, 24.0));
    let b = ui.interact(eye, id.with("chat"), Sense::click());
    if hovered || b.hovered() {
        let on = blocked.is_none();
        if b.hovered() && on {
            p.rect_filled(eye, 12, pal.accent.gamma_multiply(0.22));
        }
        paint_icon(
            &p,
            eye.center(),
            Align2::CENTER_CENTER,
            ICON_EYE,
            16.0,
            if on {
                pal.accent
                    .gamma_multiply(if b.hovered() { 1.0 } else { 0.6 })
            } else {
                pal.weak.gamma_multiply(0.6)
            },
        );
    }
    let mut click = Click::None;
    match blocked {
        None => {
            if b.on_hover_text("Open in chat view")
                .on_hover_cursor(CursorIcon::PointingHand)
                .clicked()
            {
                click = Click::Chat;
            }
        }
        Some(why) => {
            b.on_hover_text(why);
        }
    }
    let text_right = eye.left() - 10.0;

    // Title and the agent it belongs to, then the one-line summary.
    let text_left = left + 28.0;
    let title = p.layout_no_wrap(row.title.clone(), theme::font(13.5), pal.text_strong);
    let ty = rect.top() + 8.0;
    let title_w = title.size().x;
    p.galley(pos2(text_left, ty), title, pal.text_strong);
    let badge = Badge::single(&p, None, &row.agent_id, Hue::Violet.color(pal), 10.5);
    let bx = text_left + title_w + 8.0;
    if bx + badge.width() < text_right {
        badge.paint_at(&p, bx, ty + 8.0);
    }
    if !row.summary.is_empty() {
        let mut job = egui::text::LayoutJob::simple_singleline(
            row.summary.clone(),
            theme::font(12.0),
            pal.text.gamma_multiply(0.85),
        );
        job.wrap.max_width = (text_right - text_left).max(20.0);
        job.wrap.max_rows = 1;
        job.wrap.break_anywhere = true;
        let g = p.layout_job(job);
        p.galley(
            pos2(text_left, rect.bottom() - 9.0 - g.size().y),
            g,
            pal.text.gamma_multiply(0.85),
        );
    }

    resp.context_menu(|ui| {
        if ui.button("Open transcript here").clicked() {
            click = Click::Open;
            ui.close();
        }
        let b = ui.add_enabled(blocked.is_none(), egui::Button::new("Open in chat view"));
        let b = match blocked {
            Some(why) => b.on_disabled_hover_text(why),
            None => b,
        };
        if b.clicked() {
            click = Click::Chat;
            ui.close();
        }
    });
    if resp.on_hover_cursor(CursorIcon::PointingHand).clicked() && matches!(click, Click::None) {
        click = Click::Open;
    }
    click
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, agent: &str, kind: ActivityKind, status: ActivityStatus) -> ActivityRow {
        ActivityRow {
            id: id.to_owned(),
            agent_id: agent.to_owned(),
            at: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(100),
            kind,
            title: match kind {
                ActivityKind::Response => "Response".to_owned(),
                ActivityKind::Tool => "bash".to_owned(),
                ActivityKind::Irc => "IRC from Main".to_owned(),
                ActivityKind::Lifecycle => "Spawned".to_owned(),
            },
            summary: format!("summary of {id}"),
            status,
            call_id: (kind == ActivityKind::Tool).then(|| format!("call-{id}")),
        }
    }

    fn agent(id: &str, parent: Option<&str>) -> AgentSummary {
        AgentSummary {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            depth: 0,
            path: PathBuf::from(format!("/x/{id}.jsonl")),
            advisor: false,
            agent: None,
            model: None,
            model_role: None,
            thinking: None,
            task_gist: String::new(),
            task: String::new(),
            status: deck_core::agents::AgentStatus::Done,
            error: None,
            current: None,
            created: None,
            last_activity: None,
            duration: None,
            requests: 0,
            tool_calls: 0,
            tokens: 0,
            cost: 0.0,
            context_tokens: None,
            output_path: None,
        }
    }

    fn rows() -> Vec<ActivityRow> {
        use ActivityKind::{Irc, Lifecycle, Response, Tool};
        use ActivityStatus::{Aborted, Error, Pending, Success};
        vec![
            row("1", "A", Tool, Pending),
            row("2", "A.k", Response, Success),
            row("3", "A.k", Tool, Error),
            row("4", "B", Lifecycle, Aborted),
            row("5", "A", Irc, Success),
        ]
    }

    #[test]
    fn kinds_follow_omps_hub_scopes() {
        let rows = rows();
        let pick = |k: Kind| -> Vec<usize> { list(&rows, k, None, "").rows };
        assert_eq!(pick(Kind::All), [0, 1, 2, 3, 4]);
        // Errors are failed tools and aborted agents, not every non-success.
        assert_eq!(pick(Kind::Errors), [2, 3]);
        assert_eq!(pick(Kind::Responses), [1]);
        assert_eq!(pick(Kind::Tools), [0, 2]);
    }

    #[test]
    fn the_chips_count_what_the_scope_and_search_leave_whatever_the_kind() {
        let rows = rows();
        let ids = HashSet::from(["A".to_owned(), "A.k".to_owned()]);
        let l = list(&rows, Kind::Tools, Some(&ids), "");
        assert_eq!(l.rows, [0, 2]);
        // All, Errors, Responses, Tools.
        assert_eq!(l.counts, [4, 1, 1, 2]);
        let l = list(&rows, Kind::All, None, "summary  OF 3 ");
        assert_eq!(l.rows, [2], "terms are case-insensitive and all must hit");
        let l = list(&rows, Kind::All, None, "b");
        assert_eq!(
            l.rows,
            [0, 2, 3],
            "matches the title (bash) and the agent (B)"
        );
    }

    #[test]
    fn a_subtree_scope_includes_every_descendant_however_the_list_is_ordered() {
        // Children listed before their parent.
        let agents = [
            agent("A.k.deep", Some("A.k")),
            agent("A.k", Some("A")),
            agent("A", None),
            agent("B", None),
        ];
        let ids = scope_ids(&agents, &Scope::Subtree("A".into())).expect("scoped");
        let mut got: Vec<&str> = ids.iter().map(String::as_str).collect();
        got.sort_unstable();
        assert_eq!(got, ["A", "A.k", "A.k.deep"]);
        let one = scope_ids(&agents, &Scope::Agent("A".into())).expect("scoped");
        assert_eq!(one.len(), 1);
        assert!(scope_ids(&agents, &Scope::All).is_none());
    }

    #[test]
    fn a_row_opens_the_transcript_at_its_call_its_message_or_the_end() {
        use ActivityKind::{Irc, Lifecycle, Response, Tool};
        use ActivityStatus::Success;
        let tool = row("t", "A", Tool, Success);
        assert_eq!(anchor_of(&tool), Anchor::Tool("call-t".into()));
        let resp = row("r", "A", Response, Success);
        assert_eq!(anchor_of(&resp), Anchor::Response(resp.at));
        assert_eq!(anchor_of(&row("i", "A", Irc, Success)), Anchor::End);
        assert_eq!(anchor_of(&row("l", "A", Lifecycle, Success)), Anchor::End);
        // A tool row without a call id has no call to scroll to.
        let mut orphan = row("o", "A", Tool, Success);
        orphan.call_id = None;
        assert_eq!(anchor_of(&orphan), Anchor::End);
    }

    #[test]
    fn stopping_drops_the_watcher_and_everything_read() {
        let mut s = ActivityState {
            watched: vec![("A".into(), "/x".into())],
            listing_key: Some(7),
            ..ActivityState::default()
        };
        s.stop();
        assert!(s.watcher.is_none());
        assert!(s.watched.is_empty());
        assert_eq!(s.listing_key, None);
    }

    #[test]
    fn rows_arriving_above_the_one_being_read_shift_it_by_their_count() {
        use ActivityKind::Tool;
        use ActivityStatus::Success;
        let ids = |names: &[&str]| -> Vec<ActivityRow> {
            names.iter().map(|n| row(n, "A", Tool, Success)).collect()
        };
        // Newest first; the reader's first visible row is `c` at position 2.
        let held = Held {
            id: "c".to_owned(),
            pos: 2,
        };
        let before = ids(&["a", "b", "c", "d"]);
        let listing = list(&before, Kind::All, None, "");
        assert_eq!(drift(&before, &listing, &held), Some(0));
        // Two events arrive on top: `c` is now at 4.
        let after = ids(&["x", "y", "a", "b", "c", "d"]);
        let listing = list(&after, Kind::All, None, "");
        assert_eq!(drift(&after, &listing, &held), Some(2));
        // A filter that drops the row leaves nothing to keep in place.
        let listing = list(&after, Kind::All, None, "no such text");
        assert_eq!(drift(&after, &listing, &held), None);
    }
}
