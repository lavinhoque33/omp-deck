//! The provider login wizard: OmpDeck's own `/login`. omp itself does every step (a headless omp
//! running OmpDeck's auth extension, driven by the gateway; see `Topic::OmpAccounts`); this modal
//! only shows the gateway's [`LoginView`] and sends what the user picks, types or cancels.
//!
//! Steps: the provider picker, the flow (browser link, device code, paste field, prompts) and the
//! result. The pure parts (rows, preselection, device code, the step shown) are unit-tested; the
//! app (`app/login.rs`) turns [`LoginAction`]s into gateway requests.

use deck_core::omp::auth::LoginProvider;
use deck_service::gateway::protocol::{
    LoginAuth, LoginOutcome, LoginPrompt, LoginView, OmpAccountsView, SecretText,
};
use egui::{
    Align, Align2, Color32, CursorIcon, Frame, Id, Key, Layout, Margin, RichText, Sense, Stroke,
    StrokeKind, TextEdit, Ui, UiBuilder, pos2, vec2,
};
use std::borrow::Cow;
use std::time::{Duration, Instant};

use zeroize::Zeroize;

use crate::badge::{self, Hue};
use crate::card;
use crate::icons::{
    ICON_ARROW_LEFT, ICON_CIRCLE_ALERT, ICON_CIRCLE_CHECK, ICON_COPY, ICON_EXTERNAL_LINK, ICON_EYE,
    ICON_EYE_OFF, ICON_KEY_ROUND, ICON_LOG_IN, ICON_REFRESH_CW, Icon,
};
use crate::theme::{self, Palette};
use crate::widgets::{DraftButton, DraftKind, Spin, icon_button, icon_text};

const WIDTH: f32 = 520.0;
const ROW_H: f32 = 40.0;
const LIST_H: f32 = 340.0;

/// What the user asked for this frame.
#[derive(Debug)]
pub enum LoginAction {
    /// Start a login to the provider with this login id.
    Start(String),
    /// Answer the prompt or paste field `seq` of `job`.
    Answer {
        job: u64,
        seq: u64,
        value: SecretText,
    },
    Cancel(u64),
    OpenUrl(String),
    /// Copy `text`; the second field names it for the notice.
    Copy(String, &'static str),
    /// Read omp's provider list again.
    Reload,
    /// Put the wizard away (a running login of this window is cancelled by the app).
    Close,
}

/// The wizard's own state; the login itself lives in the gateway's view.
#[derive(Default)]
pub struct LoginUi {
    /// A login id or store id to start with, resolved once the provider list arrives.
    pub preselect: Option<String>,
    search: String,
    /// The login this window started: its browser opens automatically, once.
    pub job: Option<u64>,
    /// The login id of the last start (Try again after the job is gone).
    provider: Option<String>,
    /// When `job` was accepted, and whether the gateway's view has shown it since.
    started_at: Option<Instant>,
    seen: bool,
    /// An `OmpLogin` sent and not answered yet (the provider's login id).
    pub starting: Option<String>,
    /// Why the gateway refused the last start (another login runs, omp is missing).
    pub refused: Option<(String, String)>,
    /// The job whose browser link was opened automatically.
    pub browser_opened: Option<u64>,
    answer: String,
    /// The prompt `seq` last answered and when: its field stays closed while the gateway takes
    /// it (the gateway clears the prompt on receipt; one still shown after [`RESEND`] was lost).
    answered: Option<(u64, Instant)>,
    paste: String,
    pasted: Option<(u64, Instant)>,
    reveal: bool,
    /// Give the step's text field the keyboard on the next frame.
    focus: bool,
    /// The secret fields drawn last frame and this frame: a field no longer drawn has its egui
    /// state (whose undo history holds what was typed) dropped and its text wiped.
    fields: Vec<(Field, Id)>,
    drawn: Vec<(Field, Id)>,
}

/// A secret text field of the flow step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Prompt,
    Paste,
}

/// How long an answered field stays closed waiting for the gateway to take the answer.
const RESEND: Duration = Duration::from_secs(4);
/// How long an accepted job may stay out of the gateway's view before it counts as gone.
const UNSEEN: Duration = Duration::from_secs(15);

impl Drop for LoginUi {
    fn drop(&mut self) {
        self.answer.zeroize();
        self.paste.zeroize();
    }
}

impl LoginUi {
    pub fn new(preselect: Option<String>) -> LoginUi {
        LoginUi {
            preselect: preselect.filter(|p| !p.trim().is_empty()),
            search: String::new(),
            job: None,
            provider: None,
            started_at: None,
            seen: false,
            starting: None,
            refused: None,
            browser_opened: None,
            answer: String::new(),
            answered: None,
            paste: String::new(),
            pasted: None,
            reveal: false,
            focus: true,
            fields: Vec::new(),
            drawn: Vec::new(),
        }
    }

    /// An `OmpLogin` for the login id `id` went out.
    pub fn begin(&mut self, id: String) {
        self.refused = None;
        self.provider = Some(id.clone());
        self.starting = Some(id);
    }

    /// Resolves the preselection once omp's provider list has arrived: an available provider
    /// whose login can start now is returned (the app starts it); anything else lands in the
    /// picker's search.
    pub fn take_preselect(&mut self, view: &OmpAccountsView) -> Option<String> {
        if view.providers.is_empty() {
            return None;
        }
        let wanted = self.preselect.take()?;
        let idle = view.login.as_ref().is_none_or(|l| l.outcome.is_some());
        match resolve(&view.providers, &wanted) {
            Some(p) if p.available && !p.disabled && idle => Some(p.id.clone()),
            Some(p) => {
                self.search = p.name.clone();
                None
            }
            None => {
                self.search = wanted;
                None
            }
        }
    }

    /// The gateway accepted the start of `job`.
    pub fn started(&mut self, job: u64) {
        self.started_at_now(job, Instant::now());
    }

    fn started_at_now(&mut self, job: u64, now: Instant) {
        self.starting = None;
        self.refused = None;
        self.job = Some(job);
        self.started_at = Some(now);
        self.seen = false;
        self.answered = None;
        self.pasted = None;
        self.reveal = false;
        self.answer.zeroize();
        self.paste.zeroize();
        self.focus = true;
    }

    /// The gateway refused to start a login to `provider` (its display name) with `why`.
    pub fn refuse(&mut self, provider: String, why: String) {
        self.starting = None;
        self.refused = Some((provider, why));
    }

    /// Back to the provider picker.
    fn back(&mut self) {
        self.job = None;
        self.started_at = None;
        self.seen = false;
        self.starting = None;
        self.refused = None;
        self.search.clear();
        self.focus = true;
    }

    /// Notes whether the gateway's view shows this window's job.
    fn observe(&mut self, view: Option<&OmpAccountsView>) {
        if let Some(job) = self.job
            && view
                .and_then(|v| v.login.as_ref())
                .is_some_and(|l| l.job == job)
        {
            self.seen = true;
        }
    }

    /// The job closing the wizard cancels: this window's, unless the view shows it finished
    /// (one not in the view yet is cancelled too; a stale cancel is harmless).
    pub fn job_to_cancel(&self, view: Option<&OmpAccountsView>) -> Option<u64> {
        let job = self.job?;
        let finished = view
            .and_then(|v| v.login.as_ref())
            .is_some_and(|l| l.job == job && l.outcome.is_some());
        (!finished).then_some(job)
    }

    /// Drops the egui state of every secret field and wipes what was typed: call before the
    /// wizard goes away.
    pub fn forget_fields(&mut self, ctx: &egui::Context) {
        for (_, id) in self.fields.drain(..).chain(self.drawn.drain(..)) {
            forget_edit(ctx, id);
        }
        self.answer.zeroize();
        self.paste.zeroize();
    }

    /// After drawing: the secret fields drawn last frame and not this one lose their state, and
    /// a kind of field not drawn at all loses its text.
    fn retire_fields(&mut self, ctx: &egui::Context) {
        for f in &self.fields {
            if !self.drawn.contains(f) {
                forget_edit(ctx, f.1);
            }
        }
        if !self.drawn.iter().any(|f| f.0 == Field::Prompt) {
            self.answer.zeroize();
        }
        if !self.drawn.iter().any(|f| f.0 == Field::Paste) {
            self.paste.zeroize();
        }
        std::mem::swap(&mut self.fields, &mut self.drawn);
        self.drawn.clear();
    }

    /// Whether the field for `seq` (answered at `at`) is still closed: the gateway has not
    /// taken the answer yet and the answer is not given up as lost.
    fn waiting(sent: Option<(u64, Instant)>, seq: u64, now: Instant) -> bool {
        sent.is_some_and(|(s, at)| s == seq && now.saturating_duration_since(at) < RESEND)
    }

    /// The job of this window still running in `view`.
    pub fn running_job(&self, view: Option<&OmpAccountsView>) -> Option<u64> {
        let job = self.job?;
        view.and_then(|v| v.login.as_ref())
            .filter(|l| l.job == job && l.outcome.is_none())
            .map(|l| l.job)
    }
}

// ------------------------------------------------------------------ pure logic

/// The picker's lists: the providers signed in (stored accounts or another origin) and every
/// provider, both in omp's `/login` order. omp's `disabledProviders` are left out; a search
/// keeps the providers whose name or id contains every word.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Rows {
    pub signed_in: Vec<usize>,
    pub all: Vec<usize>,
}

pub fn picker_rows(providers: &[LoginProvider], query: &str) -> Rows {
    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    let mut rows = Rows::default();
    for (i, p) in providers.iter().enumerate() {
        if p.disabled {
            continue;
        }
        let hay = format!("{} {}", p.name, p.id).to_lowercase();
        if !words.iter().all(|w| hay.contains(w.as_str())) {
            continue;
        }
        if signed_in(p) {
            rows.signed_in.push(i);
        }
        rows.all.push(i);
    }
    rows
}

fn signed_in(p: &LoginProvider) -> bool {
    !p.accounts.is_empty() || p.origin.is_some()
}

/// The entry a preselection names: the login id itself, else (a store id) the entry whose id is
/// that store, else its first alias.
pub fn resolve<'a>(providers: &'a [LoginProvider], wanted: &str) -> Option<&'a LoginProvider> {
    let wanted = wanted.trim();
    providers
        .iter()
        .find(|p| p.id == wanted)
        .or_else(|| providers.iter().find(|p| p.store == wanted))
}

/// How a provider is signed in, as its badge says it: `login`, `API key`, `env: VAR`.
pub fn origin_badge(p: &LoginProvider) -> Option<String> {
    let kind = match &p.origin {
        Some(o) => {
            if o.kind == "env" {
                return Some(match &o.env_var {
                    Some(v) => format!("env: {v}"),
                    None => "env".to_owned(),
                });
            }
            o.kind.as_str()
        }
        None => p.accounts.first()?.kind.as_str(),
    };
    Some(kind_label(kind).to_owned())
}

/// `oauth` → `login`, `api_key` → `API key`; anything else as omp names it.
pub fn kind_label(kind: &str) -> &str {
    match kind {
        "oauth" => "login",
        "api_key" => "API key",
        other => other,
    }
}

/// A device code in omp's instructions (`Enter code: 0B90-E292`): groups of four or more
/// capitals or digits joined by single dashes, at least two groups (the first such run).
pub fn device_code(text: &str) -> Option<&str> {
    let bytes = text.as_bytes();
    let is_code = |b: u8| b.is_ascii_uppercase() || b.is_ascii_digit();
    let mut i = 0;
    while i < bytes.len() {
        if !is_code(bytes[i]) {
            i += 1;
            continue;
        }
        // A run of groups starting at `i`: each group 4+ long, joined by one dash.
        let start = i;
        let mut end = start;
        let mut groups = 0;
        let mut j = i;
        loop {
            let g = j;
            while j < bytes.len() && is_code(bytes[j]) {
                j += 1;
            }
            if j - g < 4 {
                break;
            }
            groups += 1;
            end = j;
            if j + 1 < bytes.len() && bytes[j] == b'-' && is_code(bytes[j + 1]) {
                j += 1;
            } else {
                break;
            }
        }
        if groups >= 2 {
            return text.get(start..end);
        }
        // No match from here: skip this group (a shorter or lone group cannot start one later
        // inside itself, since a later start would only be shorter).
        while i < bytes.len() && is_code(bytes[i]) {
            i += 1;
        }
    }
    None
}

/// The link the browser opens: omp's launch URL when it has one (it serves the real one).
pub fn browser_url(auth: &LoginAuth) -> &str {
    auth.launch_url.as_deref().unwrap_or(&auth.url)
}

/// The link to open automatically now: once per job this window started, as soon as omp
/// reports it.
pub fn auto_open<'a>(st: &LoginUi, view: Option<&'a OmpAccountsView>) -> Option<(u64, &'a str)> {
    let job = st.job?;
    let login = view?.login.as_ref().filter(|l| l.job == job)?;
    if st.browser_opened == Some(job) || login.outcome.is_some() {
        return None;
    }
    Some((job, browser_url(login.auth.as_ref()?)))
}

/// Which step the wizard shows.
#[derive(Debug, PartialEq)]
pub enum Step<'a> {
    Pick,
    /// A start is on its way to the gateway, or accepted and not in the view yet.
    Starting,
    /// The gateway refused the start ([`LoginUi::refused`]).
    Refused,
    Flow(&'a LoginView),
    Result(&'a LoginView, &'a LoginOutcome),
    /// This window's job left the gateway's view (its result expired, the gateway restarted,
    /// another login replaced it) or never showed up.
    Gone,
}

pub fn step<'a>(st: &LoginUi, view: Option<&'a OmpAccountsView>, now: Instant) -> Step<'a> {
    if st.starting.is_some() {
        return Step::Starting;
    }
    if st.refused.is_some() {
        return Step::Refused;
    }
    let Some(job) = st.job else {
        return Step::Pick;
    };
    match view.and_then(|v| v.login.as_ref()).filter(|l| l.job == job) {
        Some(l) => match &l.outcome {
            Some(o) => Step::Result(l, o),
            None => Step::Flow(l),
        },
        None if st.seen => Step::Gone,
        // Accepted, the view has not caught up yet (for a while).
        None if st
            .started_at
            .is_some_and(|at| now.saturating_duration_since(at) < UNSEEN) =>
        {
            Step::Starting
        }
        None => Step::Gone,
    }
}

// ------------------------------------------------------------------ UI

/// Draws the wizard; `view` is the gateway's accounts view (`None` until it arrives).
pub fn show(
    ctx: &egui::Context,
    st: &mut LoginUi,
    view: Option<&OmpAccountsView>,
    omp_found: bool,
) -> Vec<LoginAction> {
    let mut out = Vec::new();
    let pal = Palette::of_ctx(ctx);
    st.observe(view);
    let current = step(st, view, Instant::now());
    let (title, subtitle, tone): (Cow<'_, str>, Cow<'_, str>, Color32) = match &current {
        Step::Pick => (
            "Log in to a provider".into(),
            "omp stores the credential; OmpDeck never sees it".into(),
            Color32::TRANSPARENT,
        ),
        Step::Starting => (
            "Log in".into(),
            "Starting omp's login…".into(),
            Color32::TRANSPARENT,
        ),
        Step::Refused => (
            st.refused.as_ref().map_or("Log in".into(), |(name, _)| {
                format!("Log in to {name}").into()
            }),
            "The login could not start".into(),
            pal.danger,
        ),
        Step::Flow(l) => (
            format!("Log in to {}", l.name).into(),
            l.provider.as_str().into(),
            Color32::TRANSPARENT,
        ),
        Step::Result(l, o) => (
            format!("Log in to {}", l.name).into(),
            l.provider.as_str().into(),
            match o {
                LoginOutcome::Done { .. } => pal.ok,
                LoginOutcome::Failed(_) => pal.danger,
                LoginOutcome::Cancelled => pal.warn,
            },
        ),
        Step::Gone => (
            "Log in".into(),
            "The login is no longer running".into(),
            pal.warn,
        ),
    };
    let head = card::Header::new(ICON_KEY_ROUND, &title)
        .subtitle(&subtitle)
        .tone(tone);
    let dismissed = card::modal(ctx, Id::unique("omp-deck-login"), WIDTH, &head, |ui| {
        ui.spacing_mut().item_spacing.y = 8.0;
        match current {
            Step::Pick => pick(ui, st, view, omp_found, &mut out),
            Step::Starting => starting(ui, &mut out),
            Step::Refused => refused(ui, st, &mut out),
            Step::Flow(l) => flow(ui, st, l, &mut out),
            Step::Result(l, o) => result(ui, st, l, o, &mut out),
            Step::Gone => gone(ui, st, &mut out),
        }
    });
    st.retire_fields(ctx);
    if dismissed {
        out.push(LoginAction::Close);
    }
    out
}

fn pick(
    ui: &mut Ui,
    st: &mut LoginUi,
    view: Option<&OmpAccountsView>,
    omp_found: bool,
    out: &mut Vec<LoginAction>,
) {
    let pal = Palette::of_ui(ui);
    if !omp_found {
        notice(
            ui,
            ICON_CIRCLE_ALERT,
            pal.warn,
            "omp was not found: install it first.",
        );
        card::footer(ui, &[("Esc", "closes")], |ui| {
            if DraftButton::new("Close").min_width(84.0).show(ui).clicked() {
                out.push(LoginAction::Close);
            }
        });
        return;
    }
    // The search field.
    let edit = ui.add(
        TextEdit::singleline(&mut st.search)
            .id(Id::unique("login-search"))
            .hint_text("Search providers…")
            .desired_width(f32::INFINITY)
            .margin(Margin::symmetric(10, 7)),
    );
    if std::mem::take(&mut st.focus) {
        edit.request_focus();
    }
    let other = view
        .and_then(|v| v.login.as_ref())
        .filter(|l| l.outcome.is_none() && Some(l.job) != st.job);
    if let Some(l) = other {
        notice(
            ui,
            ICON_CIRCLE_ALERT,
            pal.warn,
            &format!(
                "A login to {} is in progress in another window; it must finish first.",
                l.name
            ),
        );
    }
    let mut picked = None;
    match view {
        None => loading(ui, "Asking omp for its providers…"),
        Some(v) if v.providers.is_empty() && v.loading => {
            loading(ui, "Asking omp for its providers…");
        }
        Some(v) if v.providers.is_empty() => {
            let msg = v
                .error
                .as_deref()
                .map_or("omp listed no providers.".to_owned(), |e| {
                    format!("Could not list the providers: {e}")
                });
            notice(ui, ICON_CIRCLE_ALERT, pal.danger, &msg);
        }
        Some(v) => {
            if let Some(e) = &v.error {
                notice(
                    ui,
                    ICON_CIRCLE_ALERT,
                    pal.danger,
                    &format!("Could not refresh the providers: {e}"),
                );
            }
            let rows = picker_rows(&v.providers, &st.search);
            let enter = ui.input(|i| i.key_pressed(Key::Enter)) && edit.lost_focus();
            if enter && let Some(&first) = rows.all.iter().find(|&&i| v.providers[i].available) {
                picked = Some(first);
            }
            egui::ScrollArea::vertical()
                .id_salt("login-providers")
                .max_height(LIST_H)
                .min_scrolled_height(LIST_H)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    if rows.all.is_empty() {
                        ui.label(RichText::new("No provider matches").small().color(pal.weak));
                    }
                    if !rows.signed_in.is_empty() {
                        card::section(ui, "Signed in");
                        for &i in &rows.signed_in {
                            if provider_row(ui, &v.providers[i], ("in", i)) {
                                picked = Some(i);
                            }
                        }
                        ui.add_space(6.0);
                        card::section(ui, "All providers");
                    }
                    for &i in &rows.all {
                        if provider_row(ui, &v.providers[i], ("all", i)) {
                            picked = Some(i);
                        }
                    }
                });
            if let Some(i) = picked {
                out.push(LoginAction::Start(v.providers[i].id.clone()));
            }
        }
    }
    let loading_now = view.is_some_and(|v| v.loading);
    card::footer(ui, &[("Enter", "first match"), ("Esc", "closes")], |ui| {
        if DraftButton::new("Close").min_width(84.0).show(ui).clicked() {
            out.push(LoginAction::Close);
        }
        if loading_now {
            ui.add(Spin::new(16.0, pal.accent));
        } else if icon_button(ui, ICON_REFRESH_CW, "Read omp's providers again", false).clicked() {
            out.push(LoginAction::Reload);
        }
    });
}

/// One provider: name, id, origin badge, account count. Unavailable ones are greyed and
/// inert. True when picked.
fn provider_row(ui: &mut Ui, p: &LoginProvider, salt: (&str, usize)) -> bool {
    let pal = Palette::of_ui(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    let resp = ui.interact(rect, Id::unique(("login-row", salt)), Sense::click());
    let hov =
        ui.ctx()
            .animate_bool_with_time(resp.id.with("hov"), resp.hovered() && p.available, 0.08);
    let painter = ui.painter();
    if hov > 0.0 {
        painter.rect(
            rect,
            theme::rad(6, 9),
            theme::hover_fill(pal).gamma_multiply(hov),
            Stroke::new(1.0, theme::mix(pal.stroke, theme::hover_stroke(pal), hov)),
            StrokeKind::Inside,
        );
    }
    let (name_c, id_c) = if p.available {
        (pal.text_strong, pal.weak)
    } else {
        (pal.weak.gamma_multiply(0.7), pal.weak.gamma_multiply(0.6))
    };
    let left = rect.left() + 12.0;
    let name = painter.layout_no_wrap(p.name.clone(), theme::font(14.0), name_c);
    let name_w = name.size().x;
    painter.galley(
        pos2(left, rect.center().y - name.size().y / 2.0),
        name,
        Color32::PLACEHOLDER,
    );
    painter.text(
        pos2(left + name_w + 8.0, rect.center().y),
        Align2::LEFT_CENTER,
        &p.id,
        theme::font(12.0),
        id_c,
    );
    // Right side: origin badge and account count, laid out right to left.
    let mut right = ui.new_child(
        UiBuilder::new()
            .max_rect(rect.shrink2(vec2(10.0, 0.0)))
            .layout(Layout::right_to_left(Align::Center)),
    );
    right.spacing_mut().item_spacing.x = 6.0;
    if !p.available {
        right.label(RichText::new("not available").small().color(pal.weak));
    } else if hov > 0.5 {
        right.label(icon_text(ICON_LOG_IN, 15.0, pal.accent));
    }
    if p.accounts.len() > 1 {
        right.label(
            RichText::new(format!("{} accounts", p.accounts.len()))
                .small()
                .color(pal.weak),
        );
    }
    if let Some(b) = origin_badge(p) {
        let hue = if b.starts_with("env") {
            Hue::Amber
        } else if b == "login" {
            Hue::Green
        } else {
            Hue::Blue
        };
        badge::show(&mut right, None, &b, hue, 10.5);
    }
    let resp = if p.available {
        resp.on_hover_cursor(CursorIcon::PointingHand)
    } else {
        resp.on_hover_text("omp cannot log in to this provider on this system")
    };
    p.available && resp.clicked()
}

fn starting(ui: &mut Ui, out: &mut Vec<LoginAction>) {
    loading(ui, "Starting omp's login…");
    card::footer(ui, &[("Esc", "closes")], |ui| {
        if DraftButton::new("Close").min_width(84.0).show(ui).clicked() {
            out.push(LoginAction::Close);
        }
    });
}

fn refused(ui: &mut Ui, st: &mut LoginUi, out: &mut Vec<LoginAction>) {
    let pal = Palette::of_ui(ui);
    let why = st.refused.as_ref().map_or("", |(_, why)| why.as_str());
    notice(ui, ICON_CIRCLE_ALERT, pal.danger, why);
    card::footer(ui, &[("Esc", "closes")], |ui| {
        if DraftButton::new("Close").min_width(84.0).show(ui).clicked() {
            out.push(LoginAction::Close);
        }
        if DraftButton::new("Back")
            .icon(ICON_ARROW_LEFT)
            .min_width(84.0)
            .show(ui)
            .clicked()
        {
            st.back();
        }
    });
}

fn gone(ui: &mut Ui, st: &mut LoginUi, out: &mut Vec<LoginAction>) {
    let pal = Palette::of_ui(ui);
    notice(
        ui,
        ICON_CIRCLE_ALERT,
        pal.warn,
        "This login is no longer running. omp may have finished or dropped it; start it again \
         to be sure.",
    );
    card::footer(ui, &[("Esc", "closes")], |ui| {
        if let Some(id) = st.provider.clone()
            && DraftButton::new("Try again")
                .kind(DraftKind::Primary)
                .icon(ICON_REFRESH_CW)
                .min_width(110.0)
                .show(ui)
                .clicked()
        {
            out.push(LoginAction::Start(id));
        }
        if DraftButton::new("Back")
            .icon(ICON_ARROW_LEFT)
            .min_width(84.0)
            .show(ui)
            .clicked()
        {
            st.back();
        }
    });
}

fn flow(ui: &mut Ui, st: &mut LoginUi, l: &LoginView, out: &mut Vec<LoginAction>) {
    let pal = Palette::of_ui(ui);
    // Progress: omp's latest word, with a spinner while it works; none while a field waits for
    // the user. An answered field stays closed while the gateway takes the answer (it clears the
    // prompt then); still asked after `RESEND`, the answer was lost and the field returns.
    let now = Instant::now();
    let show_paste = l
        .manual
        .filter(|&seq| !LoginUi::waiting(st.pasted, seq, now));
    let show_prompt = l
        .prompt
        .as_ref()
        .filter(|p| !LoginUi::waiting(st.answered, p.seq, now));
    let asking = show_paste.is_some() || show_prompt.is_some();
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let text = if asking {
            ui.label(icon_text(ICON_KEY_ROUND, 15.0, pal.weak));
            "Waiting for your answer"
        } else {
            ui.add(Spin::new(15.0, pal.accent));
            l.progress.last().map_or("Waiting for omp…", String::as_str)
        };
        ui.add(egui::Label::new(RichText::new(text).color(pal.text)).truncate());
    });
    if let Some(auth) = &l.auth {
        auth_block(ui, auth, out);
    }
    if let Some(seq) = show_paste {
        paste_field(ui, st, l.job, seq, out);
    }
    if let Some(p) = show_prompt {
        prompt_field(ui, st, l.job, p, out);
    }
    let job = l.job;
    card::footer(ui, &[("Esc", "cancels")], |ui| {
        if DraftButton::new("Cancel")
            .min_width(96.0)
            .show(ui)
            .clicked()
        {
            out.push(LoginAction::Cancel(job));
        }
    });
}

/// The browser link: Open in browser, Copy link, omp's instructions and, when they carry one,
/// the device code shown large.
fn auth_block(ui: &mut Ui, auth: &LoginAuth, out: &mut Vec<LoginAction>) {
    let pal = Palette::of_ui(ui);
    block_frame(pal).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 8.0;
        if let Some(text) = &auth.instructions {
            ui.add(egui::Label::new(RichText::new(text).color(pal.text)).wrap());
            if let Some(code) = device_code(text) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.label(
                        RichText::new(code)
                            .font(egui::FontId::monospace(26.0))
                            .color(pal.text_strong),
                    );
                    if DraftButton::new("Copy code")
                        .icon(ICON_COPY)
                        .show(ui)
                        .clicked()
                    {
                        out.push(LoginAction::Copy(code.to_owned(), "code"));
                    }
                });
            }
        }
        ui.add(egui::Label::new(RichText::new(&auth.url).small().color(pal.weak)).truncate())
            .on_hover_text(&auth.url);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 10.0;
            if DraftButton::new("Open in browser")
                .kind(DraftKind::Primary)
                .icon(ICON_EXTERNAL_LINK)
                .show(ui)
                .clicked()
            {
                out.push(LoginAction::OpenUrl(browser_url(auth).to_owned()));
            }
            if DraftButton::new("Copy link")
                .icon(ICON_COPY)
                .show(ui)
                .clicked()
            {
                out.push(LoginAction::Copy(auth.url.clone(), "link"));
            }
        });
    });
}

/// omp's manual field: the redirect URL or code, racing its local callback.
fn paste_field(ui: &mut Ui, st: &mut LoginUi, job: u64, seq: u64, out: &mut Vec<LoginAction>) {
    let pal = Palette::of_ui(ui);
    ui.label(
        RichText::new("Or paste the final redirect URL or the code here")
            .small()
            .color(pal.weak),
    );
    let id = Id::unique(("login-paste", job, seq));
    st.drawn.push((Field::Paste, id));
    let mut submit = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        let w = ui.available_width() - 110.0;
        let edit = ui.add(
            TextEdit::singleline(&mut st.paste)
                .id(id)
                .hint_text("https://… or code")
                .desired_width(w.max(120.0))
                .margin(Margin::symmetric(10, 7)),
        );
        submit = edit.lost_focus()
            && ui.input(|i| i.key_pressed(Key::Enter))
            && !st.paste.trim().is_empty();
        if DraftButton::new("Submit")
            .enabled(!st.paste.trim().is_empty())
            .min_width(96.0)
            .show(ui)
            .clicked()
        {
            submit = true;
        }
    });
    if submit {
        st.pasted = Some((seq, Instant::now()));
        forget_edit(ui.ctx(), id);
        out.push(LoginAction::Answer {
            job,
            seq,
            value: SecretText::new(std::mem::take(&mut st.paste).trim().to_owned()),
        });
    }
}

/// One of omp's questions: masked (with a reveal toggle) when it asks for a secret; Continue
/// is enabled on an empty answer only when omp allows one.
fn prompt_field(
    ui: &mut Ui,
    st: &mut LoginUi,
    job: u64,
    p: &LoginPrompt,
    out: &mut Vec<LoginAction>,
) {
    let pal = Palette::of_ui(ui);
    ui.add(egui::Label::new(theme::strong(&p.message).color(pal.text_strong)).wrap());
    let id = Id::unique(("login-prompt", job, p.seq));
    st.drawn.push((Field::Prompt, id));
    let can = !st.answer.is_empty() || p.allow_empty;
    let mut submit = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let reserve = if p.masked { 150.0 } else { 116.0 };
        let mut edit = TextEdit::singleline(&mut st.answer)
            .id(id)
            .password(p.masked && !st.reveal)
            .desired_width((ui.available_width() - reserve).max(140.0))
            .margin(Margin::symmetric(10, 7));
        if let Some(h) = &p.placeholder {
            edit = edit.hint_text(h.as_str());
        } else if p.allow_empty {
            edit = edit.hint_text("Leave empty for the default");
        }
        let resp = ui.add(edit);
        if std::mem::take(&mut st.focus) {
            resp.request_focus();
        }
        submit = resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && can;
        if p.masked {
            let (icon, tip): (Icon, &str) = if st.reveal {
                (ICON_EYE_OFF, "Hide")
            } else {
                (ICON_EYE, "Show what you typed")
            };
            if icon_button(ui, icon, tip, st.reveal).clicked() {
                st.reveal = !st.reveal;
            }
        }
        if DraftButton::new("Continue")
            .kind(DraftKind::Primary)
            .enabled(can)
            .min_width(96.0)
            .show(ui)
            .clicked()
        {
            submit = true;
        }
    });
    if submit {
        st.answered = Some((p.seq, Instant::now()));
        st.reveal = false;
        st.focus = true;
        forget_edit(ui.ctx(), id);
        out.push(LoginAction::Answer {
            job,
            seq: p.seq,
            value: SecretText::new(std::mem::take(&mut st.answer)),
        });
    }
}

/// Drops a text field's egui state (its undo history holds copies of what was typed).
fn forget_edit(ctx: &egui::Context, id: Id) {
    ctx.data_mut(|d| d.remove::<egui::text_edit::TextEditState>(id));
}

fn result(
    ui: &mut Ui,
    st: &mut LoginUi,
    l: &LoginView,
    o: &LoginOutcome,
    out: &mut Vec<LoginAction>,
) {
    let pal = Palette::of_ui(ui);
    match o {
        LoginOutcome::Done { label } => {
            let who = match label {
                Some(label) => format!("Logged in to {} as {label}", l.name),
                None => format!("Logged in to {}", l.name),
            };
            block_frame(pal).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 10.0;
                    ui.label(icon_text(ICON_CIRCLE_CHECK, 22.0, pal.ok));
                    ui.vertical(|ui| {
                        ui.label(theme::strong(who).size(15.0).color(pal.text_strong));
                        ui.label(
                            RichText::new("Running sessions pick it up on their next request.")
                                .small()
                                .color(pal.weak),
                        );
                    });
                });
            });
            card::footer(ui, &[("Enter", "closes")], |ui| {
                let done = DraftButton::new("Done")
                    .kind(DraftKind::Primary)
                    .min_width(96.0)
                    .show(ui)
                    .clicked();
                if done || ui.input(|i| i.key_pressed(Key::Enter)) {
                    out.push(LoginAction::Close);
                }
                if DraftButton::new("Log in to another")
                    .icon(ICON_ARROW_LEFT)
                    .show(ui)
                    .clicked()
                {
                    st.back();
                }
            });
        }
        LoginOutcome::Failed(_) | LoginOutcome::Cancelled => {
            let (msg, tone) = match o {
                LoginOutcome::Failed(e) => (format!("The login failed: {e}"), pal.danger),
                _ => ("The login was cancelled.".to_owned(), pal.warn),
            };
            notice(ui, ICON_CIRCLE_ALERT, tone, &msg);
            let provider = l.provider.clone();
            card::footer(ui, &[("Esc", "closes")], |ui| {
                if DraftButton::new("Try again")
                    .kind(DraftKind::Primary)
                    .icon(ICON_REFRESH_CW)
                    .min_width(110.0)
                    .show(ui)
                    .clicked()
                {
                    out.push(LoginAction::Start(provider.clone()));
                }
                if DraftButton::new("Back")
                    .icon(ICON_ARROW_LEFT)
                    .min_width(84.0)
                    .show(ui)
                    .clicked()
                {
                    st.back();
                }
            });
        }
    }
}

fn block_frame(pal: &Palette) -> Frame {
    Frame::NONE
        .fill(theme::mix(pal.bar, pal.accent, 0.06))
        .stroke(Stroke::new(1.0, pal.stroke))
        .corner_radius(theme::rad(8, 12))
        .inner_margin(Margin::symmetric(14, 12))
}

/// A one-line notice strip: icon + wrapped text in `tone`.
fn notice(ui: &mut Ui, icon: Icon, tone: Color32, text: &str) {
    let pal = Palette::of_ui(ui);
    Frame::NONE
        .fill(theme::mix(pal.bar, tone, 0.10))
        .stroke(theme::status_stroke(pal, tone, 0.35))
        .corner_radius(theme::rad(8, 12))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(icon_text(icon, 15.0, tone));
                ui.add(egui::Label::new(RichText::new(text).color(pal.text)).wrap());
            });
        });
}

fn loading(ui: &mut Ui, text: &str) {
    let pal = Palette::of_ui(ui);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        ui.add(Spin::new(15.0, pal.accent));
        ui.label(RichText::new(text).color(pal.weak));
    });
}

#[cfg(test)]
mod tests {
    use deck_core::omp::auth::{AuthOrigin, StoredAccount};

    use super::*;

    fn provider(id: &str, name: &str, store: &str) -> LoginProvider {
        LoginProvider {
            id: id.into(),
            name: name.into(),
            available: true,
            store: store.into(),
            disabled: false,
            origin: None,
            accounts: Vec::new(),
        }
    }

    fn account(kind: &str) -> StoredAccount {
        StoredAccount {
            credential_id: 1,
            label: "me@example.test".into(),
            detail: String::new(),
            kind: kind.into(),
            active: true,
        }
    }

    #[test]
    fn device_codes_are_dash_joined_groups_of_four_or_more() {
        assert_eq!(device_code("Enter code: 0B90-E292"), Some("0B90-E292"));
        assert_eq!(
            device_code("code ABCD-EFGH-IJKL then"),
            Some("ABCD-EFGH-IJKL")
        );
        // A short trailing group ends the code; a short leading one is not part of it.
        assert_eq!(device_code("ABCD-EFGH-IJ"), Some("ABCD-EFGH"));
        assert_eq!(device_code("AB-CDEF-GHIJ"), Some("CDEF-GHIJ"));
        assert_eq!(device_code("XYZ: WDJB-MJHT."), Some("WDJB-MJHT"));
        for text in [
            "paste the final redirect URL or authorization code",
            "ABCD-EF",
            "ABCD--EFGH",
            "abcd-efgh",
            "ABCDEFGH",
            "",
        ] {
            assert_eq!(device_code(text), None, "{text}");
        }
    }

    #[test]
    fn the_picker_lists_signed_in_providers_first_hides_disabled_ones_and_searches() {
        let mut list = vec![
            provider("anthropic", "Anthropic (Claude Pro/Max)", "anthropic"),
            provider("github-copilot", "GitHub Copilot", "github-copilot"),
            provider("vllm", "vLLM", "vllm"),
            provider("zai-coding-plan", "Z.AI Coding Plan", "zai"),
        ];
        list[1].accounts.push(account("oauth"));
        list[2].origin = Some(AuthOrigin {
            kind: "env".into(),
            env_var: Some("VLLM_API_KEY".into()),
        });
        list[3].disabled = true;
        assert_eq!(
            picker_rows(&list, ""),
            Rows {
                signed_in: vec![1, 2],
                all: vec![0, 1, 2],
            }
        );
        assert_eq!(picker_rows(&list, "COPI git").all, vec![1]);
        assert_eq!(picker_rows(&list, "zai").all, Vec::<usize>::new());
        assert_eq!(origin_badge(&list[1]).as_deref(), Some("login"));
        assert_eq!(origin_badge(&list[2]).as_deref(), Some("env: VLLM_API_KEY"));
        assert_eq!(origin_badge(&list[0]), None);
    }

    #[test]
    fn a_preselection_names_a_login_id_or_a_store() {
        let list = vec![
            provider("openai-codex", "ChatGPT", "openai-codex"),
            provider("zai-coding-plan", "Z.AI Coding Plan", "zai"),
            provider("devin-cli", "Devin CLI", "devin"),
            provider("devin-windsurf", "Devin Windsurf", "devin"),
            provider("devin", "Devin", "devin"),
        ];
        let id = |w: &str| resolve(&list, w).map(|p| p.id.as_str());
        assert_eq!(id("zai-coding-plan"), Some("zai-coding-plan"));
        // A store id: the entry with that id, else its first alias.
        assert_eq!(id("devin"), Some("devin"));
        assert_eq!(id("zai"), Some("zai-coding-plan"));
        assert_eq!(id(" openai-codex "), Some("openai-codex"));
        assert_eq!(id("nope"), None);
    }

    fn view_with(login: Option<LoginView>) -> OmpAccountsView {
        OmpAccountsView {
            login,
            ..OmpAccountsView::default()
        }
    }

    fn login(job: u64, auth: Option<LoginAuth>, outcome: Option<LoginOutcome>) -> LoginView {
        LoginView {
            job,
            provider: "anthropic".into(),
            name: "Anthropic".into(),
            auth,
            prompt: None,
            manual: None,
            progress: Vec::new(),
            outcome,
        }
    }

    #[test]
    fn the_browser_opens_once_for_a_job_this_window_started_preferring_the_launch_url() {
        let auth = LoginAuth {
            url: "https://claude.ai/x".into(),
            launch_url: Some("http://localhost:54545/launch".into()),
            instructions: None,
        };
        let mut st = LoginUi::new(None);
        let v = view_with(Some(login(4, Some(auth.clone()), None)));
        // Not this window's job.
        assert_eq!(auto_open(&st, Some(&v)), None);
        st.started(4);
        assert_eq!(
            auto_open(&st, Some(&v)),
            Some((4, "http://localhost:54545/launch"))
        );
        st.browser_opened = Some(4);
        assert_eq!(auto_open(&st, Some(&v)), None);
        // A finished job never opens it.
        let mut st = LoginUi::new(None);
        st.started(5);
        let done = view_with(Some(login(5, Some(auth), Some(LoginOutcome::Cancelled))));
        assert_eq!(auto_open(&st, Some(&done)), None);
        // No launch URL: the URL itself.
        let plain = LoginAuth {
            url: "https://github.com/login/device".into(),
            launch_url: None,
            instructions: None,
        };
        assert_eq!(browser_url(&plain), "https://github.com/login/device");
    }

    #[test]
    fn the_step_follows_this_windows_job_in_the_gateways_view() {
        let t0 = Instant::now();
        let mut st = LoginUi::new(Some("vllm".into()));
        assert_eq!(step(&st, None, t0), Step::Pick);
        st.begin("vllm".into());
        assert_eq!(step(&st, None, t0), Step::Starting);
        st.started_at_now(7, t0);
        // Accepted before the view shows it: closing cancels it all the same.
        let empty = view_with(None);
        assert_eq!(step(&st, Some(&empty), t0), Step::Starting);
        assert_eq!(st.job_to_cancel(Some(&empty)), Some(7));
        // Never showing up within `UNSEEN` counts as gone.
        assert_eq!(step(&st, Some(&empty), t0 + UNSEEN), Step::Gone);
        let running = view_with(Some(login(7, None, None)));
        st.observe(Some(&running));
        assert!(matches!(step(&st, Some(&running), t0), Step::Flow(_)));
        assert_eq!(st.running_job(Some(&running)), Some(7));
        let done = view_with(Some(login(
            7,
            None,
            Some(LoginOutcome::Done { label: None }),
        )));
        assert!(matches!(step(&st, Some(&done), t0), Step::Result(..)));
        assert_eq!(st.running_job(Some(&done)), None);
        assert_eq!(st.job_to_cancel(Some(&done)), None);
        // Once seen, a job leaving the view (expired, gateway restarted, replaced) is gone.
        assert_eq!(step(&st, Some(&empty), t0), Step::Gone);
        let other = view_with(Some(login(9, None, None)));
        assert_eq!(step(&st, Some(&other), t0), Step::Gone);
        assert_eq!(st.running_job(Some(&other)), None);
        st.back();
        assert_eq!(step(&st, Some(&other), t0), Step::Pick);
        st.refuse("vLLM".into(), "Another login is in progress".into());
        assert_eq!(step(&st, Some(&other), t0), Step::Refused);
    }

    #[test]
    fn an_answered_field_returns_when_the_gateway_still_asks_after_a_while() {
        let t0 = Instant::now();
        let sent = Some((3, t0));
        assert!(LoginUi::waiting(sent, 3, t0 + Duration::from_secs(1)));
        assert!(!LoginUi::waiting(sent, 3, t0 + RESEND));
        // omp asking again (a new seq) opens the field at once.
        assert!(!LoginUi::waiting(sent, 4, t0));
        assert!(!LoginUi::waiting(None, 3, t0));
    }

    #[test]
    fn a_preselection_starts_an_available_provider_once_the_list_arrives() {
        let mut list = vec![
            provider("vllm", "vLLM", "vllm"),
            provider("cursor", "Cursor", "cursor"),
        ];
        list[1].available = false;
        let mut v = view_with(None);
        let mut st = LoginUi::new(Some("vllm".into()));
        // Nothing listed yet: kept for later.
        assert_eq!(st.take_preselect(&v), None);
        assert_eq!(st.preselect.as_deref(), Some("vllm"));
        v.providers = list;
        assert_eq!(st.take_preselect(&v).as_deref(), Some("vllm"));
        assert_eq!(st.preselect, None);
        // Unavailable, unknown, or another login running: the picker opens searching for it.
        let mut st = LoginUi::new(Some("cursor".into()));
        assert_eq!(st.take_preselect(&v), None);
        assert_eq!(st.search, "Cursor");
        let mut st = LoginUi::new(Some("nope".into()));
        assert_eq!(st.take_preselect(&v), None);
        assert_eq!(st.search, "nope");
        v.login = Some(login(3, None, None));
        let mut st = LoginUi::new(Some("vllm".into()));
        assert_eq!(st.take_preselect(&v), None);
        assert_eq!(st.search, "vLLM");
    }
}
