//! Dialog tokens bind a conversation root, omp PID and UI id across reconnects.
use std::collections::{HashMap, HashSet};
use std::fmt::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use deck_core::ctl::{
    CtlClient, CtlCommand, CtlReply, LiveSnapshot, PendingUi, PendingUiKind, UiFormBlock,
    UiFormField, UiSpan, ctl_dir,
};
use serde_json::{Value, json};

use super::*;
use crate::ui_answer::{self, ApproveDecision, AskAnswer, FormAnswer, UiAnswer};

const GONE: &str = "That question was already answered or is gone.";
const INSTRUCTION: &str = "The user can also answer it in that session; the first answer wins. Answer with ompdeck_answer_question only if the user asked you to answer this session's questions; otherwise tell the user about it.";
const WAIT_INSTRUCTION: &str = "\nanswer it, then call ompdeck_wait_session again";

pub(super) struct QuestionWatch {
    ctl_key: Option<String>,
    client: Option<CtlClient>,
    subscribed: bool,
    snapshot: Option<Arc<LiveSnapshot>>,
    queue: QuestionQueue,
    discovery_until: Instant,
    attention: Option<AttentionRefresh>,
    retired: bool,
}

struct AttentionRefresh {
    rid: u64,
    until: Instant,
    complete: bool,
}
impl AttentionRefresh {
    fn waiting(&self, now: Instant) -> bool {
        !self.complete && now < self.until
    }
}
impl QuestionWatch {
    pub(super) fn new(ctl_key: Option<String>) -> Self {
        Self {
            ctl_key,
            client: None,
            subscribed: false,
            snapshot: None,
            queue: QuestionQueue::default(),
            discovery_until: Instant::now() + Duration::from_secs(10),
            attention: None,
            retired: false,
        }
    }

    pub(super) fn pending(&self) -> &[PendingUi] {
        let Some(snapshot) = &self.snapshot else {
            return &[];
        };
        if self
            .current_snapshot()
            .is_some_and(|current| Arc::ptr_eq(&current, snapshot))
        {
            &snapshot.pending_ui
        } else {
            &[]
        }
    }

    pub(super) fn pending_tokens(&self) -> Vec<String> {
        let Some(snapshot) = self.current_snapshot() else {
            return Vec::new();
        };
        self.queue.pending_tokens(&snapshot)
    }

    fn current_snapshot(&self) -> Option<Arc<LiveSnapshot>> {
        let client = self
            .client
            .as_ref()
            .filter(|c| c.connected() && c.ui_hook())?;
        let snapshot = client.latest()?;
        if snapshot.pid == 0 || snapshot.session_file.is_none() {
            return None;
        }
        Some(snapshot)
    }

    fn observe(&mut self, snapshot: Option<Arc<LiveSnapshot>>, forward: bool) {
        let Some(snapshot) = snapshot else {
            self.snapshot = None;
            return; // No authoritative snapshot yet after reconnect; retain held questions.
        };
        if self.client.as_ref().is_some_and(|c| c.ui_hook()) {
            self.queue.observe(&snapshot, forward);
        }
        self.snapshot = Some(snapshot);
    }

    pub(super) fn stop(&mut self) {
        self.retired = true;
        self.client = None;
        self.subscribed = false;
        self.snapshot = None;
        self.queue.held.clear();
        self.attention = None;
    }

    fn take_question(&mut self, file: Option<&Path>) -> Option<(String, PendingUi)> {
        let snapshot = self.current_snapshot()?;
        if snapshot.session_file.as_deref() != file {
            return None;
        }
        self.queue.take(&snapshot)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct QuestionIncarnation {
    root: PathBuf,
    pid: u32,
}
impl QuestionIncarnation {
    fn of(snapshot: &LiveSnapshot) -> Option<Self> {
        let root = snapshot.session_file.clone()?;
        (snapshot.pid != 0).then_some(Self {
            root,
            pid: snapshot.pid,
        })
    }

    fn matches(&self, snapshot: &LiveSnapshot) -> bool {
        self.pid != 0
            && self.pid == snapshot.pid
            && snapshot.session_file.as_ref() == Some(&self.root)
    }
}

// Gateway-local opaque IDs never contain (or accept) the extension's reusable numeric ID.
static NEXT_QUESTION: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
struct QuestionQueue {
    incarnation: Option<QuestionIncarnation>,
    tokens: HashMap<u64, String>,
    seen: HashSet<String>,
    held: Vec<String>,
}
impl QuestionQueue {
    fn observe(&mut self, snapshot: &LiveSnapshot, forward: bool) {
        if snapshot.pid == 0 || snapshot.session_file.is_none() {
            return;
        }
        if !self.matches(snapshot) {
            self.incarnation = QuestionIncarnation::of(snapshot);
            self.tokens.clear();
            self.seen.clear();
            self.held.clear();
        }
        for ui in &snapshot.pending_ui {
            let token = self.tokens.entry(ui.id).or_insert_with(|| {
                format!("q-{:016x}", NEXT_QUESTION.fetch_add(1, Ordering::Relaxed))
            });
            if forward && !self.seen.contains(token) {
                self.seen.insert(token.clone());
                self.held.push(token.clone());
            }
        }
        self.held.retain(|token| {
            snapshot
                .pending_ui
                .iter()
                .any(|ui| self.tokens.get(&ui.id) == Some(token))
        });
    }

    fn matches(&self, snapshot: &LiveSnapshot) -> bool {
        self.incarnation
            .as_ref()
            .is_some_and(|incarnation| incarnation.matches(snapshot))
    }

    fn pending_tokens(&self, snapshot: &LiveSnapshot) -> Vec<String> {
        if !self.matches(snapshot) {
            return Vec::new();
        }
        snapshot
            .pending_ui
            .iter()
            .filter_map(|ui| self.tokens.get(&ui.id).cloned())
            .collect()
    }

    fn resolve<'a>(&self, token: &str, snapshot: &'a LiveSnapshot) -> Option<&'a PendingUi> {
        if !self.matches(snapshot) {
            return None;
        }
        snapshot
            .pending_ui
            .iter()
            .find(|ui| self.tokens.get(&ui.id).is_some_and(|known| known == token))
    }

    fn answer_command(
        &self,
        args: &AnswerArgs,
        snapshot: &LiveSnapshot,
    ) -> Result<Option<(QuestionIncarnation, CtlCommand)>, String> {
        args.validate()?;
        let Some(ui) = self.resolve(&args.question, snapshot) else {
            return Ok(None);
        };
        let incarnation = self
            .incarnation
            .as_ref()
            .expect("resolved token incarnation")
            .clone();
        let command = CtlCommand::AnswerUi {
            id: ui.id,
            value: args.value(ui)?,
            root: incarnation.root.clone(),
            // Fence the actual extension dispatch, not just this snapshot lookup.
            pid: Some(incarnation.pid),
        };
        Ok(Some((incarnation, command)))
    }

    fn take(&mut self, snapshot: &LiveSnapshot) -> Option<(String, PendingUi)> {
        if !self.matches(snapshot) {
            return None;
        }
        while !self.held.is_empty() {
            let token = self.held.remove(0);
            if let Some(ui) = self.resolve(&token, snapshot) {
                return Some((token, ui.clone()));
            }
        }
        None
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerArgs {
    session: String,
    question: String,
    choice: Option<String>,
    choices: Option<Vec<String>>,
    yes: Option<bool>,
    text: Option<String>,
    decision: Option<String>,
    fields: Option<Vec<FieldArgs>>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldArgs {
    choices: Option<Vec<String>>,
    text: Option<String>,
}
pub(super) struct AnswerRequest {
    handle: String,
    args: AnswerArgs,
    until: Instant,
    rid: Option<u64>,
    incarnation: Option<QuestionIncarnation>,
}

impl AnswerRequest {
    fn matches_incarnation(&self, snapshot: &LiveSnapshot) -> bool {
        self.incarnation
            .as_ref()
            .is_some_and(|sent| sent.matches(snapshot))
    }
}

fn kind_name(kind: PendingUiKind) -> &'static str {
    match kind {
        PendingUiKind::Select => "select",
        PendingUiKind::Confirm => "confirm",
        PendingUiKind::Input => "input",
        PendingUiKind::Editor => "editor",
        PendingUiKind::Ask => "ask",
        PendingUiKind::Form => "form",
        PendingUiKind::Approve => "approve",
    }
}

fn expected(ui: &PendingUi) -> String {
    let shape = match ui.kind {
        PendingUiKind::Select if ui.multi => {
            "choices: [option label or 1-based number] (exactly one toggle at a time)"
        }
        PendingUiKind::Select => "choice: option label or 1-based number",
        PendingUiKind::Confirm => "yes: boolean",
        PendingUiKind::Input | PendingUiKind::Editor => "text: string",
        PendingUiKind::Ask | PendingUiKind::Form => {
            "fields: one answer per listed field, in order; choice fields use choices: [option label or 1-based number], text fields use text: string; optional other text only where offered"
        }
        PendingUiKind::Approve => {
            "decision: once or deny; session and always grants are not allowed"
        }
    };
    format!("Expected {shape} for this {} question", kind_name(ui.kind))
}

fn option_index<'a>(
    input: &str,
    labels: impl Iterator<Item = &'a str> + Clone,
) -> Result<u32, String> {
    let input = input.trim();
    let folded = input.to_lowercase();
    // Prefer a literal label (including a numeric label) to its displayed position.
    let mut matches = labels
        .clone()
        .enumerate()
        .filter(|(_, label)| label.trim().to_lowercase() == folded);
    if let Some((index, _)) = matches.next() {
        if matches.next().is_some() {
            return Err(format!(
                "Option {input:?} is ambiguous; use its listed number"
            ));
        }
        return Ok(index as u32);
    }
    if let Ok(number) = input.parse::<usize>()
        && number > 0
        && number <= labels.count()
    {
        return Ok((number - 1) as u32);
    }
    Err(format!(
        "No option named {input:?}; use an option label or its listed 1-based number"
    ))
}
fn indices<'a>(
    inputs: &[String],
    labels: impl Iterator<Item = &'a str> + Clone,
) -> Result<Vec<u32>, String> {
    inputs
        .iter()
        .map(|input| option_index(input, labels.clone()))
        .collect()
}
fn spans(spans: &[UiSpan]) -> String {
    let mut text = String::new();
    for span in spans {
        text.push_str(&span.text);
    }
    text
}

impl AnswerArgs {
    fn validate(&self) -> Result<(), String> {
        let bounded = |text: &str| text.len() <= 16 * 1024;
        if self.question.is_empty() || self.question.len() > 32 {
            return Err("question must contain 1 to 32 bytes".into());
        }
        for text in [
            self.choice.as_deref(),
            self.text.as_deref(),
            self.decision.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            if !bounded(text) {
                return Err("answer strings must not exceed 16384 bytes".into());
            }
        }
        if self
            .choices
            .as_ref()
            .is_some_and(|v| v.len() > 40 || v.iter().any(|v| !bounded(v)))
            || self.fields.as_ref().is_some_and(|v| {
                v.len() > 40
                    || v.iter().any(|f| {
                        f.text.as_ref().is_some_and(|v| !bounded(v))
                            || f.choices
                                .as_ref()
                                .is_some_and(|v| v.len() > 40 || v.iter().any(|v| !bounded(v)))
                    })
            })
        {
            return Err(
                "answers allow at most 40 fields/choices and 16384 bytes per string".into(),
            );
        }
        Ok(())
    }

    fn value(&self, ui: &PendingUi) -> Result<Value, String> {
        let wrong = || expected(ui);
        let supplied = [
            self.choice.is_some(),
            self.choices.is_some(),
            self.yes.is_some(),
            self.text.is_some(),
            self.decision.is_some(),
            self.fields.is_some(),
        ];
        let only = |index: usize| {
            supplied
                .iter()
                .enumerate()
                .all(|(i, present)| *present == (i == index))
        };
        let answer = match ui.kind {
            PendingUiKind::Select if ui.multi && only(1) => UiAnswer::Choices {
                indices: indices(
                    self.choices.as_ref().expect("shape"),
                    ui.options.iter().map(|o| o.label.as_str()),
                )?,
            },
            PendingUiKind::Select if !ui.multi && only(0) => UiAnswer::Choice {
                index: option_index(
                    self.choice.as_deref().expect("shape"),
                    ui.options.iter().map(|o| o.label.as_str()),
                )?,
            },
            PendingUiKind::Confirm if only(2) => UiAnswer::Confirm {
                yes: self.yes.expect("shape"),
            },
            PendingUiKind::Input | PendingUiKind::Editor if only(3) => UiAnswer::Text {
                text: self.text.clone().expect("shape"),
            },
            PendingUiKind::Approve if only(4) => UiAnswer::Approve {
                decision: match self.decision.as_deref() {
                    Some("once") => ApproveDecision::Once,
                    Some("deny") => ApproveDecision::Deny,
                    _ => return Err(wrong()),
                },
            },
            PendingUiKind::Ask if only(5) => {
                let fields = self.fields.as_ref().expect("shape");
                if fields.len() != ui.questions.len() {
                    return Err(wrong());
                }
                let answers = fields
                    .iter()
                    .zip(&ui.questions)
                    .map(|(field, q)| {
                        if field.choices.is_none() && field.text.is_none() {
                            return Err(wrong());
                        }
                        Ok(AskAnswer {
                            id: q.id.clone(),
                            selected: indices(
                                field.choices.as_deref().unwrap_or_default(),
                                q.options.iter().map(|o| o.label.as_str()),
                            )?,
                            custom: field.text.clone(),
                        })
                    })
                    .collect::<Result<_, String>>()?;
                UiAnswer::Ask { answers }
            }
            PendingUiKind::Form if only(5) => {
                let fields = self.fields.as_ref().expect("shape");
                let form = ui.form.as_ref().ok_or_else(wrong)?;
                if fields.len() != form.fields.len() {
                    return Err(wrong());
                }
                let fields = fields
                    .iter()
                    .zip(&form.fields)
                    .map(|(answer, field)| match field {
                        UiFormField::Choice { options, other, .. } => {
                            if (answer.choices.is_none() && answer.text.is_none())
                                || (answer.text.is_some() && !other)
                            {
                                return Err(wrong());
                            }
                            let labels: Vec<_> = options.iter().map(|o| spans(&o.label)).collect();
                            Ok(FormAnswer::Choice {
                                checked: indices(
                                    answer.choices.as_deref().unwrap_or_default(),
                                    labels.iter().map(String::as_str),
                                )?,
                                other: answer.text.clone(),
                            })
                        }
                        UiFormField::Text { .. }
                            if answer.choices.is_none() && answer.text.is_some() =>
                        {
                            Ok(FormAnswer::Text {
                                text: answer.text.clone().expect("shape"),
                            })
                        }
                        _ => Err(wrong()),
                    })
                    .collect::<Result<_, String>>()?;
                UiAnswer::Form { fields }
            }
            _ => return Err(wrong()),
        };
        ui_answer::value_for(ui, &answer).map_err(|error| format!("{}: {error}", expected(ui)))
    }
}

// Bound during formatting, not after allocating the full form/approval. Always reserve the
// question id and the user-authorization instruction, even for an oversized dialog.
struct RenderText {
    text: String,
    limit: usize,
    truncated: bool,
}
impl Write for RenderText {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let available = self.limit.saturating_sub(self.text.len());
        let prefix = utf8_prefix(text, available);
        self.text.push_str(prefix);
        self.truncated |= prefix.len() != text.len();
        Ok(())
    }
}
fn render(ui: &PendingUi, token: &str, waiting: bool) -> String {
    let decisions = if ui.kind == PendingUiKind::Approve {
        "allowed decisions: once, deny\n"
    } else {
        ""
    };
    let footer = format!(
        "\n\n{decisions}question: \"{}\"\n{INSTRUCTION}{}",
        token,
        if waiting { WAIT_INSTRUCTION } else { "" }
    );
    let mut out = RenderText {
        text: String::new(),
        limit: REPLY_CAP - footer.len() - 24,
        truncated: false,
    };
    let _ = writeln!(out, "kind: {}\n{}", kind_name(ui.kind), ui.title);
    if let Some(message) = &ui.message {
        let _ = writeln!(out, "{message}");
    }
    match ui.kind {
        PendingUiKind::Select => {
            let _ = writeln!(
                out,
                "{}",
                if ui.multi {
                    "multi: true (one toggle per answer)"
                } else {
                    "multi: false"
                }
            );
            for (i, option) in ui.options.iter().enumerate() {
                let _ = writeln!(out, "{}. {}", i + 1, option.label);
                if let Some(description) = &option.description {
                    let _ = writeln!(out, "   {description}");
                }
            }
        }
        PendingUiKind::Ask => {
            for (i, q) in ui.questions.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "Field {} (choice, multi: {}, other text allowed): {}",
                    i + 1,
                    q.multi,
                    q.question
                );
                if let Some(header) = &q.header {
                    let _ = writeln!(out, "{header}");
                }
                for (j, option) in q.options.iter().enumerate() {
                    let recommended = if q.recommended == Some(j as u32) {
                        " (recommended)"
                    } else {
                        ""
                    };
                    let _ = writeln!(out, "  {}. {}{recommended}", j + 1, option.label);
                    if let Some(description) = &option.description {
                        let _ = writeln!(out, "     {description}");
                    }
                }
            }
        }
        PendingUiKind::Form => {
            if let Some(form) = &ui.form {
                for section in &form.sections {
                    for span in &section.title {
                        let _ = write!(out, "{}", span.text);
                    }
                    let _ = writeln!(out);
                    for block in &section.blocks {
                        match block {
                            UiFormBlock::Text { spans, .. } => {
                                for span in spans {
                                    let _ = write!(out, "{}", span.text);
                                }
                                let _ = writeln!(out);
                            }
                            UiFormBlock::Pre { text } | UiFormBlock::Image { alt: text } => {
                                let _ = writeln!(out, "{text}");
                            }
                            UiFormBlock::List { items, .. } => {
                                for item in items {
                                    let _ = write!(out, "- ");
                                    for span in item {
                                        let _ = write!(out, "{}", span.text);
                                    }
                                    let _ = writeln!(out);
                                }
                            }
                            UiFormBlock::Table { rows, .. } => {
                                for row in rows {
                                    for cell in row {
                                        for span in cell {
                                            let _ = write!(out, "{}", span.text);
                                        }
                                        let _ = write!(out, " | ");
                                    }
                                    let _ = writeln!(out);
                                }
                            }
                            UiFormBlock::Field { .. } | UiFormBlock::Other => {}
                        }
                    }
                }
                for (i, field) in form.fields.iter().enumerate() {
                    match field {
                        UiFormField::Choice {
                            question,
                            label,
                            options,
                            multi,
                            other,
                            required,
                            ..
                        } => {
                            let _ = write!(
                                out,
                                "Field {} (choice, multi: {multi}, other text: {other}, required: {required}): {question} ",
                                i + 1
                            );
                            for span in label {
                                let _ = write!(out, "{}", span.text);
                            }
                            let _ = writeln!(out);
                            for (j, option) in options.iter().enumerate() {
                                let _ = write!(out, "  {}. ", j + 1);
                                for span in &option.label {
                                    let _ = write!(out, "{}", span.text);
                                }
                                let _ = writeln!(
                                    out,
                                    "{}",
                                    if option.disabled { " (disabled)" } else { "" }
                                );
                            }
                        }
                        UiFormField::Text {
                            question,
                            label,
                            input_type,
                            multiline,
                            required,
                            ..
                        } => {
                            let _ = write!(
                                out,
                                "Field {} (text, type: {input_type}, multiline: {multiline}, required: {required}): {question} ",
                                i + 1
                            );
                            for span in label {
                                let _ = write!(out, "{}", span.text);
                            }
                            let _ = writeln!(out);
                        }
                    }
                }
            }
        }
        PendingUiKind::Approve => {
            if let Some(approve) = &ui.approve {
                let _ = writeln!(out, "{}", approve.compact);
            }
        }
        PendingUiKind::Confirm => {
            let _ = writeln!(out, "Answer with yes: true or false.");
        }
        PendingUiKind::Input | PendingUiKind::Editor => {
            let _ = writeln!(out, "Answer with text.");
            if let Some(prefill) = &ui.prefill {
                let _ = writeln!(out, "Initial text: {prefill}");
            }
        }
    }
    if out.truncated {
        out.text.push_str("\n[… truncated]");
    }
    out.text.push_str(&footer);
    out.text
}

impl ChildSession {
    fn monitors_questions(&self) -> bool {
        !self.invalidated && !self.questions.retired
    }
}

impl Registry {
    fn question_poll_needed(&self) -> bool {
        self.entries.values().any(ChildSession::monitors_questions) || !self.answers.is_empty()
    }
}

impl Engine {
    pub(in crate::gateway::engine) fn child_question_ctl_keys(
        &self,
    ) -> impl Iterator<Item = String> + '_ {
        self.agent_tools
            .children
            .entries
            .iter()
            .filter(|(_, child)| child.monitors_questions())
            .filter_map(|(_, child)| child.questions.ctl_key.clone())
    }

    pub(super) fn child_question_snapshot(&self, handle: &str) -> Option<Arc<LiveSnapshot>> {
        self.agent_tools
            .children
            .entries
            .get(handle)
            .filter(|child| child.monitors_questions())
            .and_then(|child| {
                child
                    .questions
                    .client
                    .as_ref()
                    .filter(|client| client.connected())
                    .and_then(CtlClient::latest)
            })
    }

    pub(in crate::gateway::engine) fn begin_answer_question(
        &mut self,
        request: u64,
        args: Value,
        now: Instant,
    ) -> Result<(), String> {
        let args: AnswerArgs = serde_json::from_value(args).map_err(|e| e.to_string())?;
        args.validate()?;
        let root = canonical(
            self.agent_tools.pending[&request]
                .root
                .as_deref()
                .ok_or_else(|| UNKNOWN.to_owned())?,
        );
        let child = self.agent_tools.children.owned(&args.session, &root)?;
        if self.agent_tools.pending[&request].target.key == child.key {
            return Err(UNKNOWN.into());
        }
        self.agent_tools.children.answers.insert(
            request,
            AnswerRequest {
                handle: args.session.clone(),
                args,
                until: now + DELIVERY,
                rid: None,
                incarnation: None,
            },
        );
        self.waker.now();
        Ok(())
    }

    pub(super) fn step_child_questions(&mut self, now: Instant) {
        let handles: Vec<_> = self.agent_tools.children.entries.keys().cloned().collect();
        let mut replies = Vec::new();
        for handle in handles {
            let child = &self.agent_tools.children.entries[&handle];
            // Manual answers need listed tokens even when forwarding is disabled.
            let wanted = child.monitors_questions();
            if !wanted && child.questions.client.is_none() {
                continue;
            }
            let opener_ended = self.background_opener_ended(child);
            let mut invalid = child.invalidated;
            if let Some(i) = self.sess.find(&child.key) {
                invalid |= self.sess.managed[i].exited();
                if !invalid {
                    invalid |= self.validate_child_identity(&handle, i).is_err();
                }
            } else {
                let child = &self.agent_tools.children.entries[&handle];
                invalid |= child.attach.mux_name.as_deref().is_none_or(|name| {
                    self.running
                        .latest()
                        .filter(|r| !r.blind)
                        .is_some_and(|running| {
                            (!running.is_alive(name)
                                && now >= child.questions.discovery_until
                                && !child
                                    .questions
                                    .client
                                    .as_ref()
                                    .is_some_and(CtlClient::connected))
                                || child
                                    .backend
                                    .zip(running.owner_of(name))
                                    .is_some_and(|(a, b)| a != b)
                                || child.file.as_ref().is_some_and(|file| {
                                    !running.files_of(name).is_empty()
                                        && !running.files_of(name).iter().any(|p| p == file)
                                })
                        })
                });
            }
            let child = self
                .agent_tools
                .children
                .entries
                .get_mut(&handle)
                .expect("child");
            child.invalidated |= invalid;
            if invalid || opener_ended || !wanted {
                if invalid || opener_ended {
                    child.forward_questions = false;
                }
                child.questions.stop();
                let key = child.key.clone();
                let opener = child.opener.key.clone();
                self.release_idle_child_attachment(&key);
                self.release_idle_child_attachment(&opener);
                continue;
            }
            let watch = &mut child.questions;
            if watch.client.is_none()
                && let Some(key) = &watch.ctl_key
            {
                let waker = self.waker.clone();
                watch.client = Some(CtlClient::spawn(ctl_dir(&self.ctl_ns, key), move || {
                    waker.now()
                }));
            }
            let Some(client) = &watch.client else {
                continue;
            };
            if !client.connected() {
                watch.subscribed = false;
                watch.snapshot = None;
                continue;
            }
            if !watch.subscribed {
                client.send(CtlCommand::Subscribe {
                    snapshots: true,
                    tools: false,
                });
                watch.subscribed = true;
            }
            for (rid, reply) in client.take_replies() {
                if let Some(refresh) = &mut watch.attention
                    && refresh.rid == rid
                {
                    refresh.complete = true;
                }
                replies.push((handle.clone(), rid, reply));
            }
            let snapshot = client.latest();
            if let Some(snapshot) = &snapshot {
                if child
                    .file
                    .as_ref()
                    .is_some_and(|known| snapshot.session_file.as_ref() != Some(known))
                {
                    child.invalidated = true;
                    child.forward_questions = false;
                    watch.stop();
                    let key = child.key.clone();
                    let opener = child.opener.key.clone();
                    self.release_idle_child_attachment(&key);
                    self.release_idle_child_attachment(&opener);
                    continue;
                }
                if child.file.is_none() {
                    child.file.clone_from(&snapshot.session_file);
                }
            }
            watch.observe(snapshot, child.forward_questions);
        }
        for (handle, rid, reply) in replies {
            let request =
                self.agent_tools
                    .children
                    .answers
                    .iter()
                    .find_map(|(&request, answer)| {
                        (answer.handle == handle && answer.rid == Some(rid)).then_some(request)
                    });
            if let Some(request) = request {
                let current = self.agent_tools.children.entries[&handle]
                    .questions
                    .current_snapshot();
                let answer = &self.agent_tools.children.answers[&request];
                let (ok, text) = if current
                    .as_ref()
                    .is_some_and(|s| answer.matches_incarnation(s))
                {
                    answer_result(reply)
                } else {
                    (true, GONE.to_owned())
                };
                self.finish_agent_tool(request, ok, text);
            }
        }
        self.step_question_answers(now);
        if self.agent_tools.children.question_poll_needed() {
            self.wake_within(POLL);
        }
    }

    fn step_question_answers(&mut self, now: Instant) {
        let requests: Vec<_> = self.agent_tools.children.answers.keys().copied().collect();
        for request in requests {
            let mut answer = self
                .agent_tools
                .children
                .answers
                .remove(&request)
                .expect("answer");
            let result = (|| {
                let child = self
                    .agent_tools
                    .children
                    .entries
                    .get(&answer.handle)
                    .ok_or_else(|| UNKNOWN.to_owned())?;
                if child.invalidated {
                    return Err(STALE.to_owned());
                }
                if self.background_opener_ended(child) {
                    return Err("The opener's session ended or changed".to_owned());
                }
                if now >= answer.until {
                    return Err("The question could not be answered within 30 seconds".to_owned());
                }
                let watch = &child.questions;
                if watch.ctl_key.is_none() {
                    return Err("This session runs without live control".to_owned());
                }
                let Some(client) = watch.client.as_ref().filter(|c| c.connected()) else {
                    return if answer.rid.is_some() {
                        Err("OmpDeck disconnected before the answer was acknowledged".to_owned())
                    } else {
                        Ok(None)
                    };
                };
                if !client.ui_hook() {
                    return Err("This omp cannot be answered remotely".to_owned());
                }
                let Some(snapshot) = watch.current_snapshot() else {
                    return Ok(None);
                };
                if answer.incarnation.is_some() && !answer.matches_incarnation(&snapshot) {
                    return Ok(Some(GONE.to_owned()));
                }
                if answer.rid.is_some() {
                    return Ok(None);
                }
                let Some((incarnation, command)) =
                    watch.queue.answer_command(&answer.args, &snapshot)?
                else {
                    return Ok(Some(GONE.to_owned()));
                };
                answer.incarnation = Some(incarnation);
                answer.rid = Some(client.send(command));
                Ok(None)
            })();
            self.agent_tools.children.answers.insert(request, answer);
            match result {
                Ok(Some(text)) => self.finish_agent_tool(request, true, text),
                Err(error) => self.finish_agent_tool(request, false, error),
                Ok(None) => {}
            }
        }
    }

    pub(super) fn take_forwarded_question(&mut self, handle: &str) -> Option<String> {
        let child = self.agent_tools.children.entries.get_mut(handle)?;
        if !child.forward_questions || child.invalidated {
            return None;
        }
        child
            .questions
            .take_question(child.file.as_deref())
            .map(|(token, ui)| render(&ui, &token, true))
    }

    pub(super) fn has_forwardable_question(&self, handle: &str) -> bool {
        self.agent_tools
            .children
            .entries
            .get(handle)
            .is_some_and(|child| {
                child.forward_questions
                    && !child.invalidated
                    && !child.questions.pending().is_empty()
            })
    }

    pub(super) fn question_attention_refresh(
        &mut self,
        handle: &str,
        attention: bool,
        now: Instant,
    ) -> bool {
        let Some(child) = self.agent_tools.children.entries.get_mut(handle) else {
            return false;
        };
        let watch = &mut child.questions;
        if !attention || !child.forward_questions || !watch.pending().is_empty() {
            watch.attention = None;
            return false;
        }
        let Some(client) = watch.client.as_ref().filter(|client| client.connected()) else {
            return now < watch.discovery_until;
        };
        if !client.ui_hook() {
            return false;
        }
        if let Some(refresh) = &watch.attention {
            return refresh.waiting(now);
        }
        // Native attention and ctl snapshots travel independently. Subscribe emits a fresh
        // snapshot before its ACK, proving whether this attention has a wrapped question.
        let rid = client.send(CtlCommand::Subscribe {
            snapshots: true,
            tools: false,
        });
        watch.attention = Some(AttentionRefresh {
            rid,
            until: now + Duration::from_secs(2),
            complete: false,
        });
        true
    }

    pub(super) fn deliver_child_questions(&mut self) {
        let handles: Vec<_> = self
            .agent_tools
            .children
            .entries
            .iter()
            .filter(|(_, child)| {
                child.forward_questions
                    && !child.invalidated
                    && !child.questions.queue.held.is_empty()
            })
            .map(|(handle, _)| handle.clone())
            .collect();
        for handle in handles {
            let child = &self.agent_tools.children.entries[&handle];
            if self.background_opener_ended(child) {
                continue;
            }
            let Some(index) = self.background_delivery_target(&child.opener_root) else {
                continue;
            };
            let title = self
                .sess
                .find(&child.key)
                .map(|i| self.sess.managed[i].display_label())
                .unwrap_or_else(|| child.attach.label.clone());
            let title = format!("Question from {} ({handle})", utf8_prefix(&title, 160));
            let root = child.opener_root.clone();
            while let Some((token, ui)) = {
                let child = self
                    .agent_tools
                    .children
                    .entries
                    .get_mut(&handle)
                    .expect("child");
                child.questions.take_question(child.file.as_deref())
            } {
                let command = CtlCommand::Deliver {
                    root: root.clone(),
                    title: title.clone(),
                    text: render(&ui, &token, false),
                    details: Some(
                        json!({"handle": handle, "question": token, "kind": kind_name(ui.kind)}),
                    ),
                };
                self.deliver_child_message(index, command);
            }
        }
    }
}

fn answer_result(reply: CtlReply) -> (bool, String) {
    if reply.ok {
        (true, "Answered.".into())
    } else if reply.error.as_deref() == Some("no such request") {
        (true, GONE.into())
    } else {
        (
            false,
            reply
                .error
                .unwrap_or_else(|| "The session refused the answer".into()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_refresh_waits_for_fresh_snapshot_ack_but_never_forever() {
        let now = Instant::now();
        let mut refresh = AttentionRefresh {
            rid: 1,
            until: now + Duration::from_secs(2),
            complete: false,
        };
        assert!(refresh.waiting(now));
        refresh.complete = true;
        assert!(!refresh.waiting(now));
        refresh.complete = false;
        assert!(!refresh.waiting(now + Duration::from_secs(2)));
    }

    fn ui(kind: &str) -> PendingUi {
        serde_json::from_value(json!({
            "id": 7, "kind": kind, "title": "Choose the plan",
            "message": "Which approach?", "options": [
                {"label": "Alpha", "description": "First approach"}, {"label": "Béta"}
            ],
            "questions": [{
                "id": "strategy", "question": "Which strategy?", "multi": false,
                "recommended": 1, "options": [{"label": "Alpha"}, {"label": "Béta"}]
            }],
            "approve": {"full": "Never forward this full command", "compact": "Run build?",
                "session": "Allow this session",
                "grants": [{"scope":"folder","label":"Allow folder","alwaysLabel":"Always allow"}]},
            "form": {
                "sections": [{"title":[{"text":"Plan"}], "blocks":[
                    {"kind":"text", "spans":[{"text":"Choose carefully."}]}
                ]}],
                "fields": [
                    {"kind":"choice","name":"strategy","question":"Which strategy?","other":true,
                        "options":[{"label":[{"text":"Alpha"}]},{"label":[{"text":"Béta"}]},
                            {"label":[{"text":"Disabled"}],"disabled":true}]},
                    {"kind":"text","name":"reason","question":"Why?","type":"text"}
                ]
            }
        }))
        .expect("pending dialog")
    }

    fn args(answer: Value) -> AnswerArgs {
        let mut args = json!({"session":"s-12345678","question":"q-test"});
        args.as_object_mut()
            .expect("object")
            .extend(answer.as_object().expect("answer").clone());
        serde_json::from_value(args).expect("answer args")
    }

    #[test]
    fn renders_every_kind_and_keeps_instruction_and_id_inside_byte_cap() {
        let token = "q-0123456789abcdef";
        for kind in [
            "select", "confirm", "input", "editor", "ask", "form", "approve",
        ] {
            let mut ui = ui(kind);
            let rendered = render(&ui, token, false);
            assert!(rendered.contains(&format!("kind: {kind}")));
            assert!(rendered.contains("Choose the plan"));
            assert!(rendered.contains("Which approach?"));
            assert!(rendered.contains(&format!("question: \"{token}\"")));
            assert!(!rendered.contains("question: \"7\""));
            assert!(rendered.ends_with(INSTRUCTION));
            assert!(!rendered.contains(WAIT_INSTRUCTION));
            ui.message = Some("🦀".repeat(REPLY_CAP));
            let rendered = render(&ui, token, true);
            assert!(rendered.len() <= REPLY_CAP);
            assert!(rendered.contains(&format!("question: \"{token}\"")));
            assert!(rendered.contains(INSTRUCTION));
            assert!(rendered.ends_with(WAIT_INSTRUCTION));
        }
        let ask = render(&ui("ask"), token, false);
        assert!(ask.contains("Field 1 (choice, multi: false"));
        assert!(ask.contains("2. Béta (recommended)"));
        let mut multi = ui("select");
        multi.multi = true;
        assert!(render(&multi, token, false).contains("multi: true (one toggle per answer)"));
        let form = render(&ui("form"), token, false);
        assert!(form.contains("Choose carefully."));
        assert!(form.contains("Field 2 (text, type: text"));
        assert!(form.contains("3. Disabled (disabled)"));
        let approval = render(&ui("approve"), token, false);
        assert!(approval.contains("Run build?"));
        assert!(approval.contains("allowed decisions: once, deny"));
        assert!(!approval.contains("Never forward"));
        assert!(!approval.contains("Allow this session"));
        assert!(!approval.contains("Always allow"));
    }

    fn snapshot(pid: u32, pending_ui: Vec<PendingUi>) -> LiveSnapshot {
        LiveSnapshot {
            pid,
            session_file: Some(PathBuf::from("/sandbox/child.jsonl")),
            pending_ui,
            ..Default::default()
        }
    }

    #[test]
    fn forwards_once_and_drops_stale_held_questions_at_delivery() {
        let mut queue = QuestionQueue::default();
        let pending = snapshot(101, vec![ui("confirm")]);
        queue.observe(&pending, true);
        queue.observe(&pending, true);
        let token = queue.pending_tokens(&pending).remove(0);
        assert_eq!(queue.held, std::slice::from_ref(&token));
        let (forwarded, ui) = queue.take(&pending).expect("forward once");
        assert_eq!(forwarded, token);
        assert_eq!(ui.id, 7);
        queue.observe(&pending, true);
        assert!(queue.take(&pending).is_none());
        let empty = snapshot(101, vec![]);
        queue.observe(&empty, true);
        queue.observe(&pending, true);
        assert!(
            queue.take(&pending).is_none(),
            "id is once per conversation, not per appearance"
        );
        let mut held = QuestionQueue::default();
        held.observe(&pending, true);
        assert!(
            held.take(&empty).is_none(),
            "human answered while opener was unreachable"
        );
        assert!(held.held.is_empty());
        let mut disabled = QuestionQueue::default();
        disabled.observe(&pending, false);
        assert!(disabled.held.is_empty());
        assert!(disabled.seen.is_empty());
        let token = disabled.pending_tokens(&pending).remove(0);
        assert!(token.len() <= 32);
        assert_ne!(token, "7");
        assert_eq!(
            disabled
                .resolve(&token, &pending)
                .expect("manual answer")
                .id,
            7
        );
        let mut answer = args(json!({"yes": true}));
        answer.question = token.clone();
        assert!(
            disabled
                .answer_command(&answer, &pending)
                .unwrap()
                .is_some()
        );
        disabled.observe(&pending, true);
        assert_eq!(
            disabled.held,
            [token],
            "enabling forwarding retains manual token"
        );
    }

    #[test]
    fn blocking_wait_consumes_the_same_one_shot_question_as_delivery() {
        let mut queue = QuestionQueue::default();
        let pending = snapshot(101, vec![ui("ask")]);
        queue.observe(&pending, true);
        let (token, ui) = queue.take(&pending).expect("blocking wait question");
        let text = render(&ui, &token, true);
        assert!(text.ends_with(WAIT_INSTRUCTION));
        assert!(
            queue.take(&pending).is_none(),
            "no separate message after wait consumed it"
        );
        queue.observe(&pending, true);
        assert!(queue.held.is_empty());
    }

    #[test]
    fn new_pid_reusing_ui_id_invalidates_old_tokens_and_held_questions() {
        let mut queue = QuestionQueue::default();
        let old = snapshot(101, vec![ui("confirm")]);
        queue.observe(&old, true);
        let old_token = queue.pending_tokens(&old).remove(0);
        let new = snapshot(202, old.pending_ui.clone());
        assert!(
            queue.resolve(&old_token, &new).is_none(),
            "reject before observing new PID"
        );
        assert!(
            queue.take(&new).is_none(),
            "never deliver held old-incarnation questions"
        );
        assert!(queue.pending_tokens(&new).is_empty());
        queue.observe(&new, true);
        let new_token = queue.pending_tokens(&new).remove(0);
        assert_ne!(old_token, new_token);
        assert!(queue.resolve(&old_token, &new).is_none());
        let (forwarded, ui) = queue.take(&new).expect("new incarnation is forwarded");
        assert_eq!(forwarded, new_token);
        assert_eq!(ui.id, 7);
        assert!(queue.take(&new).is_none());
    }

    #[test]
    fn same_pid_reconnect_and_reload_preserve_pending_tokens_and_dedupe() {
        let mut watch = QuestionWatch::new(None);
        let pending = snapshot(101, vec![ui("confirm")]);
        watch.queue.observe(&pending, true);
        let token = watch.queue.pending_tokens(&pending).remove(0);
        watch.snapshot = Some(Arc::new(pending.clone()));
        watch.observe(None, true); // The transport clears latest during reconnect.
        assert!(watch.pending().is_empty());
        assert!(
            watch.pending_tokens().is_empty(),
            "no stale rows while disconnected"
        );
        assert_eq!(watch.queue.held, std::slice::from_ref(&token));
        let reconnected = pending.clone(); // New Arc, same process and preserved pending UI.
        watch.queue.observe(&reconnected, true);
        assert_eq!(
            watch.queue.pending_tokens(&reconnected),
            std::slice::from_ref(&token)
        );
        assert_eq!(watch.queue.take(&reconnected).unwrap().0, token);
        watch.queue.observe(&pending.clone(), true); // Extension reload, same process.
        assert!(watch.queue.take(&pending).is_none());
        assert_eq!(watch.queue.pending_tokens(&pending), [token]);
    }

    #[test]
    fn stale_numeric_and_other_conversation_tokens_never_resolve() {
        let mut queue = QuestionQueue::default();
        let pending = snapshot(101, vec![ui("confirm")]);
        queue.observe(&pending, false);
        let token = queue.pending_tokens(&pending).remove(0);
        let mut answer = args(json!({"yes": true}));
        for stale in ["7", "q-not-issued"] {
            answer.question = stale.to_owned();
            assert!(queue.resolve(stale, &pending).is_none());
            assert!(queue.answer_command(&answer, &pending).unwrap().is_none());
        }
        answer.question = token.clone();
        assert!(
            queue
                .answer_command(&answer, &snapshot(101, vec![]))
                .unwrap()
                .is_none()
        );
        let mut other_root = pending.clone();
        other_root.session_file = Some(PathBuf::from("/sandbox/other.jsonl"));
        assert!(queue.resolve(&token, &other_root).is_none());
        queue.observe(&other_root, true);
        assert!(queue.resolve(&token, &other_root).is_none());
        assert_ne!(queue.pending_tokens(&other_root), [token]);
    }

    #[test]
    fn unknown_pid_cannot_allocate_forward_or_answer_questions() {
        let mut queue = QuestionQueue::default();
        let unknown = snapshot(0, vec![ui("confirm")]);
        queue.observe(&unknown, true);
        assert!(queue.tokens.is_empty());
        assert!(queue.take(&unknown).is_none());
        let known = snapshot(101, unknown.pending_ui.clone());
        queue.observe(&known, true);
        let token = queue.pending_tokens(&known).remove(0);
        queue.observe(&unknown, true);
        assert!(queue.pending_tokens(&unknown).is_empty());
        assert!(queue.resolve(&token, &unknown).is_none());
        queue.observe(&known, true);
        assert_eq!(queue.take(&known).unwrap().0, token);
    }

    #[test]
    fn in_flight_answer_stays_bound_to_original_pid_and_is_not_retried_on_replacement() {
        let mut queue = QuestionQueue::default();
        let old = snapshot(101, vec![ui("confirm")]);
        queue.observe(&old, true);
        let mut args = args(json!({"yes": true}));
        args.question = queue.pending_tokens(&old).remove(0);
        let (incarnation, command) = queue.answer_command(&args, &old).unwrap().unwrap();
        assert!(matches!(
            command,
            CtlCommand::AnswerUi {
                id: 7,
                pid: Some(101),
                ..
            }
        ));
        let answer = AnswerRequest {
            handle: args.session.clone(),
            args,
            until: Instant::now() + DELIVERY,
            rid: Some(42),
            incarnation: Some(incarnation),
        };
        assert!(
            answer.matches_incarnation(&old.clone()),
            "same PID reconnect keeps in-flight identity"
        );
        let mut answered = old.clone();
        answered.pending_ui.clear();
        assert!(
            answer.matches_incarnation(&answered),
            "ACK can follow removal of pending UI"
        );
        let replacement = snapshot(202, old.pending_ui.clone());
        assert!(
            !answer.matches_incarnation(&replacement),
            "never accept replacement-process ACK"
        );
        assert!(
            queue
                .answer_command(&answer.args, &replacement)
                .unwrap()
                .is_none()
        );
        queue.observe(&replacement, true);
        assert!(
            queue
                .answer_command(&answer.args, &replacement)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn ended_opener_retires_question_monitor_without_scheduling_another_wakeup() {
        let mut tools = super::super::super::AgentTools::default();
        let handle = tools
            .children
            .insert(super::super::tests::child())
            .expect("child");
        assert!(tools.children.question_poll_needed());
        let child = tools.children.entries.get_mut(&handle).expect("child");
        // The opener-end cleanup leaves the surviving child's handle intact.
        child.forward_questions = false;
        child.questions.stop();
        assert!(!child.invalidated);
        assert!(!child.monitors_questions());
        assert!(
            !tools.children.question_poll_needed(),
            "retired entries schedule no question wakeup"
        );
        tools.children.answers.insert(
            1,
            AnswerRequest {
                handle,
                args: args(json!({"yes": true})),
                until: Instant::now() + DELIVERY,
                rid: None,
                incarnation: None,
            },
        );
        assert!(
            tools.children.question_poll_needed(),
            "pending answers still receive a step"
        );
        tools.children.answers.clear();
        assert!(!tools.children.question_poll_needed());
    }

    #[test]
    fn forwarding_owns_a_ctl_link_not_a_child_controller() {
        let mut tools = super::super::super::AgentTools::default();
        let mut child = super::super::tests::child();
        child.forward_questions = true;
        let key = child.key.clone();
        let opener = child.opener.key.clone();
        let handle = tools.children.insert(child).expect("handle");
        assert!(
            !tools.controller_needed(&key),
            "a forwarding-only child is not attached to a terminal"
        );
        assert!(
            tools.controller_needed(&opener),
            "reuse the opener hold/drop ownership"
        );
        let child = tools.children.entries.get_mut(&handle).expect("child");
        child.forward_questions = false;
        child.questions.stop();
        assert!(!tools.controller_needed(&opener));
    }

    #[test]
    fn answers_all_kinds_via_shared_phone_conversion_and_maps_labels_and_numbers() {
        assert_eq!(
            args(json!({"choice":" béTA "}))
                .value(&ui("select"))
                .unwrap(),
            json!("Béta")
        );
        assert_eq!(
            args(json!({"choice":"2"})).value(&ui("select")).unwrap(),
            json!("Béta")
        );
        assert!(args(json!({"choice":"0"})).value(&ui("select")).is_err());
        let mut multi = ui("select");
        multi.multi = true;
        assert_eq!(
            args(json!({"choices":["1"]})).value(&multi).unwrap(),
            json!("Alpha")
        );
        assert!(
            args(json!({"choices":["1","2"]}))
                .value(&multi)
                .unwrap_err()
                .contains("exactly one")
        );
        assert_eq!(
            args(json!({"yes":false})).value(&ui("confirm")).unwrap(),
            json!(false)
        );
        for kind in ["input", "editor"] {
            assert_eq!(
                args(json!({"text":"a draft"})).value(&ui(kind)).unwrap(),
                json!("a draft")
            );
        }
        let ask = args(json!({"fields":[{"choices":["2"],"text":"my own detail"}]}))
            .value(&ui("ask"))
            .unwrap();
        assert_eq!(ask["results"][0]["selectedOptions"], json!(["Béta"]));
        assert_eq!(ask["results"][0]["customInput"], json!("my own detail"));
        let form =
            args(json!({"fields":[{"choices":[" béta "],"text":"another"},{"text":"reason"}]}))
                .value(&ui("form"))
                .unwrap();
        assert_eq!(
            form,
            json!({"fields":[{"checked":[1],"other":"another"},{"text":"reason"}]})
        );
        for decision in ["once", "deny"] {
            assert_eq!(
                args(json!({"decision":decision}))
                    .value(&ui("approve"))
                    .unwrap(),
                json!(decision)
            );
        }
    }

    #[test]
    fn refuses_incorrect_shapes_session_grants_and_invalid_form_choices() {
        for answer in [
            json!({"decision":"session"}),
            json!({"decision":"always"}),
            json!({"decision":"once","text":"ignore this"}),
        ] {
            let error = args(answer).value(&ui("approve")).unwrap_err();
            assert!(error.contains("decision: once or deny"));
        }
        assert!(serde_json::from_value::<AnswerArgs>(json!({
            "session":"s-12345678","question":"7","decision":"once","grant":"folder","always":true
        })).is_err());
        assert!(
            args(json!({"yes":true}))
                .value(&ui("select"))
                .unwrap_err()
                .contains("Expected choice")
        );
        assert!(
            args(json!({"fields":[{"choices":["Disabled"]},{"text":"reason"}]}))
                .value(&ui("form"))
                .unwrap_err()
                .contains("disabled")
        );
        assert!(
            args(json!({"fields":[{"choices":["1","1"]},{"text":"reason"}]}))
                .value(&ui("form"))
                .unwrap_err()
                .contains("twice")
        );
        assert!(
            args(json!({"fields":[{"choices":["1"]}]}))
                .value(&ui("form"))
                .unwrap_err()
                .contains("one answer per listed field")
        );
        let mut form = ui("form");
        if let UiFormField::Choice { other, .. } = &mut form.form.as_mut().unwrap().fields[0] {
            *other = false;
        }
        assert!(
            args(json!({"fields":[{"text":"custom"},{"text":"reason"}]}))
                .value(&form)
                .is_err()
        );
        let mut bounded = args(json!({"text":"x".repeat(16 * 1024 + 1)}));
        assert!(bounded.validate().is_err());
        bounded.text = None;
        bounded.question = "7".repeat(33);
        assert!(bounded.validate().is_err());
    }

    #[test]
    fn answer_ack_is_first_wins_and_field_errors_do_not_consume_pending_question() {
        assert_eq!(
            answer_result(CtlReply {
                ok: true,
                ..Default::default()
            }),
            (true, "Answered.".into())
        );
        assert_eq!(
            answer_result(CtlReply {
                error: Some("no such request".into()),
                ..Default::default()
            }),
            (true, GONE.into())
        );
        let mut queue = QuestionQueue::default();
        let pending = snapshot(101, vec![ui("form")]);
        queue.observe(&pending, true);
        let error = answer_result(CtlReply {
            error: Some("Field 1: required".into()),
            ..Default::default()
        });
        assert_eq!(error, (false, "Field 1: required".into()));
        assert_eq!(queue.take(&pending).expect("still pending").1.id, 7);
        assert_eq!(
            answer_result(CtlReply {
                error: Some("session changed".into()),
                ..Default::default()
            }),
            (false, "session changed".into())
        );
    }
}
