//! The session tree of one omp session file: nodes, branches, labels, the leaf, and a tailing
//! watcher. File-based, so it works for any session (live tab or not).
//!
//! Facts and omp's own `/tree` rules (leaf computation, row text, child order):
//! `docs/notes/session-tree.md` §1. Row text mirrors `tree-selector.ts#entryParts` without the
//! `user: ` / `assistant: ` / `developer: ` role prefix.
//!
//! Parsing is a streaming pass over the file: bodies are only materialised as far as a row needs
//! (first 200 characters of text blocks; `toolResult` bodies are skipped; tool-call arguments stay
//! borrowed `RawValue`s that are rendered once). Lines above [`MAX_LINE`] are never buffered
//! beyond that size and are read by their prefix only (`type`, `id`, `parentId`, `timestamp`,
//! `role`).

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use memchr::memchr;
use serde::Deserialize;
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::platform::{self, FileId};

/// Row text cap (characters).
const TEXT_LIMIT: usize = 200;
/// Characters of message text gathered per entry; rows need 200, the `<system-*>` wrapper strip of
/// `custom_message` needs a little more.
const GATHER_LIMIT: usize = 2000;
/// Error text of an assistant row (omp: first 80 characters).
const ERROR_LIMIT: usize = 80;
/// Longest line that is parsed; longer ones are read by prefix only.
const MAX_LINE: usize = 4 * 1024 * 1024;
/// Bytes of an oversized line searched for `type` / `id` / `parentId` / `timestamp` / `role`.
const PREFIX_SCAN: usize = 2048;
/// How often the watcher looks at the file.
const POLL: Duration = Duration::from_millis(250);

// ---- model -------------------------------------------------------------------------------------

/// What an entry is; mirrors omp's entry types (session-tree.md §1 row-text table).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum NodeKind {
    // `type == "message"`, by `message.role`
    User,
    Assistant,
    ToolResult,
    Developer,
    BashExecution,
    OtherRole(String),
    // other entry types
    CustomMessage(String),
    Compaction,
    BranchSummary,
    ModelChange,
    ModelUsage,
    ThinkingChange,
    Label,
    Custom(String),
    /// `service_tier_change`, `title_change`, `mode_change`, `credential_pin`, `session_init`,
    /// `ttsr_injection`, `reset_boundary`, or any unknown entry type (its type name).
    Setting(String),
}

impl NodeKind {
    /// True for `type == "message"` entries (the only ones omp can fork at).
    pub fn is_message(&self) -> bool {
        matches!(
            self,
            NodeKind::User
                | NodeKind::Assistant
                | NodeKind::ToolResult
                | NodeKind::Developer
                | NodeKind::BashExecution
                | NodeKind::OtherRole(_)
        )
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TreeNode {
    pub id: String,
    pub parent: Option<String>,
    pub kind: NodeKind,
    /// ISO timestamp as written.
    pub timestamp: Option<String>,
    /// omp row text without the role prefix: ≤200 chars, control characters dropped,
    /// `\n`/`\t` → space, trimmed. `toolResult` → `[read: path:1-5]` etc. from the matching
    /// toolCall; an assistant without text → error text / `(aborted)` / `(no content)`.
    pub text: String,
    /// A `user` / `assistant` / `developer` message has a text block with content other than
    /// whitespace, `.` and `…` (omp's assistant visibility rule input). False for other entries.
    pub has_text: bool,
    pub stop_reason: Option<String>,
    /// `activeLeafId` when present (`Some(None)` = explicit null).
    pub active_leaf: Option<Option<String>>,
}

/// The tree of one session file.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionTree {
    /// File order; shared between successive snapshots of a watched file.
    pub nodes: Vec<Arc<TreeNode>>,
    pub index: HashMap<Arc<str>, usize>,
    /// Per node, child indices sorted by timestamp ascending (ties keep file order).
    pub children: Vec<Arc<Vec<usize>>>,
    /// Nodes whose `parentId` is null, missing from the file, or the node itself (file order).
    pub roots: Vec<usize>,
    /// Target id → latest label text; cleared labels are removed.
    pub labels: HashMap<String, String>,
    /// The leaf omp computes when loading the file (`activeLeafId` rules, session-tree.md §1).
    pub file_leaf: Option<usize>,
}

impl SessionTree {
    /// Reads and parses the whole file (blocking). A truncated last line is ignored.
    pub fn load(path: &Path) -> io::Result<SessionTree> {
        let mut b = Builder::default();
        let file = File::open(path)?;
        b.read_new(file, &AtomicBool::new(false))?;
        Ok(b.into_tree())
    }

    /// Index of the parent node, when it exists in the file and is not the node itself.
    pub fn parent_index(&self, i: usize) -> Option<usize> {
        let p = *self.index.get(self.nodes.get(i)?.parent.as_deref()?)?;
        (p != i).then_some(p)
    }

    /// Per node: lies on the root → `leaf` path (all false for `None`).
    pub fn active_path(&self, leaf: Option<usize>) -> Vec<bool> {
        let mut on = vec![false; self.nodes.len()];
        let mut cur = leaf.filter(|&l| l < on.len());
        while let Some(i) = cur {
            if on[i] {
                break; // parent cycle
            }
            on[i] = true;
            cur = self.parent_index(i);
        }
        on
    }

    /// Nearest ancestor (excluding `i`) that is a `message` entry: omp forks only at messages,
    /// while a prompt's parent is often a title/model/thinking/compaction entry.
    fn message_ancestor(&self, i: usize) -> Option<usize> {
        let mut cur = self.parent_index(i);
        for _ in 0..self.nodes.len() {
            let p = cur?;
            if self.nodes[p].kind.is_message() {
                return Some(p);
            }
            cur = self.parent_index(p);
        }
        None // parent cycle
    }

    /// Fork is offered for `message` entries only; a `user` entry forks before the prompt
    /// (omp's `/branch`), which needs a message ancestor.
    pub fn is_forkable(&self, i: usize) -> bool {
        self.fork_target(i).is_some()
    }

    /// The `entryId` to hand to omp's fork: the nearest message ancestor for a user message
    /// ("fork before this prompt"), the node's own id for other messages ("fork after this reply").
    pub fn fork_target(&self, i: usize) -> Option<&str> {
        let n = self.nodes.get(i)?;
        match n.kind {
            NodeKind::User => self.message_ancestor(i).map(|p| self.nodes[p].id.as_str()),
            _ if n.kind.is_message() => Some(n.id.as_str()),
            _ => None,
        }
    }
}

// ---- row text ----------------------------------------------------------------------------------

fn is_control(c: char) -> bool {
    c < ' ' || ('\u{7f}'..='\u{9f}').contains(&c)
}

fn skip_escape(it: &mut std::str::Chars<'_>) {
    match it.next() {
        Some('[') => {
            for c in it.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        }
        Some(']') => {
            for c in it.by_ref() {
                if c == '\u{7}' {
                    break;
                }
            }
        }
        _ => {}
    }
}

/// omp's `sanitizeTreeField`: ANSI and control characters dropped, `\n`/`\t` → space, trimmed.
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(TEXT_LIMIT * 4));
    let mut it = s.chars();
    while let Some(c) = it.next() {
        match c {
            '\x1b' => skip_escape(&mut it),
            '\n' | '\t' => out.push(' '),
            c if is_control(c) => {}
            c => out.push(c),
        }
    }
    let t = out.trim();
    if t.len() == out.len() {
        out
    } else {
        t.to_owned()
    }
}

fn truncate_chars(s: &str, n: usize) -> &str {
    s.char_indices().nth(n).map_or(s, |(i, _)| &s[..i])
}

/// Final row text: cleaned, capped at [`TEXT_LIMIT`] characters.
fn row(s: &str) -> String {
    let c = clean(truncate_chars(s, GATHER_LIMIT));
    let t = truncate_chars(&c, TEXT_LIMIT);
    if t.len() == c.len() {
        c
    } else {
        t.trim_end().to_owned()
    }
}

fn is_blank_text(s: &str) -> bool {
    s.chars().all(|c| c == '.' || c == '…' || c.is_whitespace())
}

/// omp `stripSystemWrapperTags`; `truncated` = the content was cut at [`GATHER_LIMIT`], so only
/// the opening tag can be removed.
fn strip_system_wrapper(content: &str, truncated: bool) -> &str {
    let t = content.trim();
    let Some(rest) = t.strip_prefix('<') else {
        return content;
    };
    let name_len = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(rest.len());
    let name = &rest[..name_len];
    if name_len < 8 || !name[..7].eq_ignore_ascii_case("system-") {
        return content;
    }
    let after = &rest[name_len..];
    match after.chars().next() {
        Some('>') => {}
        Some(c) if c.is_whitespace() => {}
        _ => return content,
    }
    let mut quote: Option<char> = None;
    let mut open_end = None;
    for (i, c) in after.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '<') => return content,
            (None, '>') => {
                open_end = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(open_end) = open_end else {
        return content;
    };
    let body = &after[open_end + 1..];
    if truncated {
        return body.trim();
    }
    let closing = format!("</{name}>");
    match body.len().checked_sub(closing.len()) {
        Some(at) if body.is_char_boundary(at) && body[at..].eq_ignore_ascii_case(&closing) => {
            body[..at].trim()
        }
        _ => content,
    }
}

fn home() -> Option<&'static str> {
    static HOME: LazyLock<Option<String>> =
        LazyLock::new(|| dirs::home_dir().and_then(|h| h.to_str().map(str::to_owned)));
    HOME.as_deref()
}

/// omp `shortenPath`: a leading home directory becomes `~`.
fn shorten_path(p: &str) -> String {
    if let Some(h) = home()
        && !h.is_empty()
        && let Some(rest) = p.strip_prefix(h)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return format!("~{rest}");
    }
    p.to_owned()
}

/// JS truthiness + `String()` of an argument: `String(a || b || "")`.
fn js_str(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if n.as_f64().is_some_and(|f| f != 0.0) => Some(n.to_string()),
        Value::Bool(true) => Some("true".into()),
        Value::Array(_) | Value::Object(_) => v.map(Value::to_string),
        _ => None,
    }
}

fn path_list(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => {
            if s.starts_with('[')
                && let Ok(list) = serde_json::from_str::<Vec<String>>(s)
            {
                return list;
            }
            vec![s.clone()]
        }
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().map_or_else(|| x.to_string(), str::to_owned))
            .collect(),
        _ => Vec::new(),
    }
}

/// The argument fields omp's row renderer looks at; everything else is skipped while parsing.
#[derive(Deserialize, Default)]
struct ToolArgs {
    path: Option<Value>,
    file_path: Option<Value>,
    offset: Option<Value>,
    limit: Option<Value>,
    command: Option<Value>,
    pattern: Option<Value>,
    paths: Option<Value>,
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

fn fmt_num(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        f.to_string()
    }
}

/// omp `#formatToolCall`.
fn format_tool_call(name: &str, raw: Option<&RawValue>) -> String {
    let args: ToolArgs = raw
        .and_then(|r| serde_json::from_str(r.get()).ok())
        .unwrap_or_default();
    let path_arg = || {
        js_str(args.path.as_ref())
            .or_else(|| js_str(args.file_path.as_ref()))
            .unwrap_or_default()
    };
    match name {
        "read" => {
            let mut d = shorten_path(&path_arg());
            let (offset, limit) = (num(args.offset.as_ref()), num(args.limit.as_ref()));
            if offset.is_some() || limit.is_some() {
                let start = offset.unwrap_or(1.0);
                d.push(':');
                d.push_str(&fmt_num(start));
                if let Some(l) = limit {
                    let end = start + l - 1.0;
                    if end != 0.0 {
                        d.push('-');
                        d.push_str(&fmt_num(end));
                    }
                }
            }
            format!("[read: {d}]")
        }
        "write" => format!("[write: {}]", shorten_path(&path_arg())),
        "edit" => format!("[edit: {}]", shorten_path(&path_arg())),
        "bash" => {
            let cmd = js_str(args.command.as_ref()).unwrap_or_default();
            let short: String = cmd
                .chars()
                .take(50)
                .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
                .collect();
            let more = if cmd.chars().nth(50).is_some() {
                "..."
            } else {
                ""
            };
            format!("[bash: {}{more}]", short.trim())
        }
        "grep" => {
            let pattern = js_str(args.pattern.as_ref()).unwrap_or_default();
            let paths = match args.paths.as_ref() {
                Some(v @ (Value::String(_) | Value::Array(_))) => path_list(Some(v)),
                _ => match args.path.as_ref() {
                    Some(v @ Value::String(_)) => path_list(Some(v)),
                    _ => Vec::new(),
                },
            };
            let scope = if paths.is_empty() {
                ".".to_owned()
            } else {
                paths.join(", ")
            };
            format!("[grep: /{pattern}/ in {}]", shorten_path(&scope))
        }
        "glob" => {
            let list = match args.path.as_ref() {
                Some(v @ Value::String(_)) => path_list(Some(v)),
                _ => match args.paths.as_ref() {
                    Some(v @ (Value::String(_) | Value::Array(_))) => path_list(Some(v)),
                    _ => Vec::new(),
                },
            };
            let scope = if list.is_empty() {
                ".".to_owned()
            } else {
                list.join(", ")
            };
            format!("[glob: {}]", shorten_path(&scope))
        }
        "ls" => format!(
            "[ls: {}]",
            shorten_path(&js_str(args.path.as_ref()).unwrap_or_else(|| ".".into()))
        ),
        _ => {
            let json = raw.map_or("{}", |r| r.get());
            let short = truncate_chars(json, 40);
            let more = if short.len() < json.len() { "..." } else { "" };
            format!("[{name}: {short}{more}]")
        }
    }
}

// ---- line parsing ------------------------------------------------------------------------------

#[derive(Deserialize)]
struct Key<'a>(#[serde(borrow)] Cow<'a, str>);

/// Text blocks of a message / custom_message `content`, gathered only as far as rows need.
#[derive(Default)]
struct Content {
    text: String,
    chars: usize,
    truncated: bool,
    has_text: bool,
    /// (toolCall id, rendered row) of the assistant's tool calls.
    calls: Vec<(String, String)>,
}

impl Content {
    fn add_text(&mut self, s: &str) {
        if !self.has_text && !is_blank_text(s) {
            self.has_text = true;
        }
        let room = GATHER_LIMIT - self.chars;
        if room == 0 {
            self.truncated |= !s.is_empty();
            return;
        }
        let t = truncate_chars(s, room);
        self.truncated |= t.len() < s.len();
        self.chars += t.chars().count();
        self.text.push_str(t);
    }
}

#[derive(Deserialize)]
struct Block<'a> {
    #[serde(default, rename = "type", borrow)]
    ty: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    text: Option<Cow<'a, str>>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, borrow)]
    name: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    arguments: Option<&'a RawValue>,
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Content;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("message content")
            }
            fn visit_str<E>(self, s: &str) -> Result<Content, E> {
                let mut c = Content::default();
                c.add_text(s);
                Ok(c)
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Content, A::Error> {
                let mut c = Content::default();
                while let Some(b) = seq.next_element::<Block<'de>>()? {
                    match b.ty.as_deref() {
                        Some("text") => {
                            if let Some(t) = &b.text {
                                c.add_text(t);
                            }
                        }
                        Some("toolCall") => {
                            if let Some(id) = b.id {
                                let name = b.name.as_deref().unwrap_or("tool");
                                c.calls.push((id, format_tool_call(name, b.arguments)));
                            }
                        }
                        _ => {}
                    }
                }
                Ok(c)
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Content, A::Error> {
                while m.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Content::default())
            }
            fn visit_unit<E>(self) -> Result<Content, E> {
                Ok(Content::default())
            }
            fn visit_bool<E>(self, _: bool) -> Result<Content, E> {
                Ok(Content::default())
            }
            fn visit_i64<E>(self, _: i64) -> Result<Content, E> {
                Ok(Content::default())
            }
            fn visit_u64<E>(self, _: u64) -> Result<Content, E> {
                Ok(Content::default())
            }
            fn visit_f64<E>(self, _: f64) -> Result<Content, E> {
                Ok(Content::default())
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Deserialize, Default)]
struct Recovery {
    status: Option<String>,
    note: Option<String>,
}

#[derive(Default)]
struct Msg {
    role: Option<String>,
    content: Option<Content>,
    stop_reason: Option<String>,
    error_message: Option<String>,
    tool_call_id: Option<String>,
    tool_name: Option<String>,
    command: Option<String>,
    recovery: Option<Recovery>,
}

impl<'de> Deserialize<'de> for Msg {
    fn deserialize<D: de::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Msg;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a message object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Msg, A::Error> {
                let mut msg = Msg::default();
                while let Some(Key(key)) = m.next_key::<Key<'de>>()? {
                    match &*key {
                        "role" => msg.role = m.next_value()?,
                        "content" => match msg.role.as_deref() {
                            // Bodies of tool output are never shown: skip without allocating.
                            Some("toolResult" | "bashExecution") => {
                                m.next_value::<IgnoredAny>()?;
                            }
                            _ => msg.content = Some(m.next_value()?),
                        },
                        "stopReason" => msg.stop_reason = m.next_value()?,
                        "errorMessage" => msg.error_message = m.next_value()?,
                        "toolCallId" => msg.tool_call_id = m.next_value()?,
                        "toolName" => msg.tool_name = m.next_value()?,
                        "command" => msg.command = m.next_value()?,
                        "retryRecovery" => msg.recovery = m.next_value()?,
                        _ => {
                            m.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(msg)
            }
        }
        d.deserialize_map(V)
    }
}

/// `Some(v)` for any present value, `Some(None)` for explicit `null`.
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: de::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(rename = "type", borrow)]
    ty: Cow<'a, str>,
    #[serde(default, borrow)]
    id: Option<Cow<'a, str>>,
    #[serde(default, rename = "parentId", borrow)]
    parent_id: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    timestamp: Option<Cow<'a, str>>,
    #[serde(default, rename = "activeLeafId", deserialize_with = "present")]
    active_leaf: Option<Option<String>>,
    #[serde(default)]
    message: Option<Msg>,
    #[serde(default, rename = "customType", borrow)]
    custom_type: Option<Cow<'a, str>>,
    /// `custom_message` content.
    #[serde(default)]
    content: Option<Content>,
    #[serde(default, borrow)]
    details: Option<&'a RawValue>,
    #[serde(default, borrow)]
    summary: Option<Cow<'a, str>>,
    #[serde(default, rename = "tokensBefore")]
    tokens_before: Option<f64>,
    #[serde(default, borrow)]
    model: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    purpose: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    role: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    provider: Option<Cow<'a, str>>,
    #[serde(default, rename = "thinkingLevel", deserialize_with = "present")]
    thinking_level: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    label: Option<Option<String>>,
    #[serde(default, rename = "targetId", borrow)]
    target_id: Option<Cow<'a, str>>,
    #[serde(default, rename = "serviceTier")]
    service_tier: Option<serde_json::Map<String, Value>>,
    #[serde(default, borrow)]
    title: Option<Cow<'a, str>>,
    #[serde(default, borrow)]
    mode: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct AdvisorDetails {
    #[serde(default)]
    notes: Vec<AdvisorNote>,
}

#[derive(Deserialize)]
struct AdvisorNote {
    note: Option<String>,
    advisor: Option<String>,
    severity: Option<String>,
}

/// omp `advisorTreeDisplay` + row label.
fn advisor_text(details: Option<&RawValue>) -> String {
    let notes = details
        .and_then(|d| serde_json::from_str::<AdvisorDetails>(d.get()).ok())
        .map(|d| d.notes)
        .unwrap_or_default();
    let mut texts = Vec::new();
    let mut advisors: Vec<String> = Vec::new();
    let mut severities: Vec<String> = Vec::new();
    for n in notes {
        if let Some(t) = n.note {
            texts.push(t);
        }
        if let Some(a) = n.advisor.map(|a| clean(&a))
            && !a.is_empty()
            && a != "default"
            && !advisors.contains(&a)
        {
            advisors.push(a);
        }
        if let Some(s) = n.severity.map(|s| clean(&s))
            && !s.is_empty()
            && !severities.contains(&s)
        {
            severities.push(s);
        }
    }
    let qualifier = advisors
        .into_iter()
        .chain(severities)
        .collect::<Vec<_>>()
        .join(", ");
    let label = if qualifier.is_empty() {
        "advisor: ".to_owned()
    } else {
        format!("advisor ({qualifier}): ")
    };
    format!("{label}{}", clean(&texts.join(" ")))
}

fn kind_of_role(role: &str) -> NodeKind {
    match role {
        "user" => NodeKind::User,
        "assistant" => NodeKind::Assistant,
        "toolResult" => NodeKind::ToolResult,
        "developer" => NodeKind::Developer,
        "bashExecution" => NodeKind::BashExecution,
        other => NodeKind::OtherRole(other.to_owned()),
    }
}

/// Fallback row for entry types without a dedicated format: `[<type with _ → space>]`.
fn type_row(ty: &str) -> String {
    format!("[{}]", ty.replace('_', " "))
}

/// One parsed line, ready for the builder.
struct Parsed {
    node: TreeNode,
    /// `Label` entries: (target id, label; `None` = cleared).
    label: Option<(String, Option<String>)>,
    /// toolCall id → rendered row, from an assistant message.
    calls: Vec<(String, String)>,
}

fn parse_line(bytes: &[u8], tools: &HashMap<String, String>) -> Result<Option<Parsed>, String> {
    let line: Line<'_> = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if matches!(line.ty.as_ref(), "title" | "session") {
        return Ok(None); // title slot and session header are not entries (the header has an id)
    }
    let Some(id) = line.id.as_deref().filter(|i| !i.is_empty()) else {
        return Ok(None); // title slot, session header, anything without an id
    };
    let str_or =
        |s: &Option<Cow<'_, str>>, d: &str| s.as_deref().map_or_else(|| d.to_owned(), clean);
    let mut label = None;
    let mut calls = Vec::new();
    let mut has_text = false;
    let mut stop_reason = None;
    let (kind, text) = match line.ty.as_ref() {
        "message" => {
            let mut msg = line.message.unwrap_or_default();
            let role = msg.role.take().unwrap_or_else(|| "message".to_owned());
            let kind = kind_of_role(&role);
            stop_reason = msg.stop_reason.take();
            let content = msg.content.take().unwrap_or_default();
            has_text = content.has_text;
            let text = match kind {
                NodeKind::User | NodeKind::Developer => row(&content.text),
                NodeKind::Assistant => {
                    let note = msg
                        .recovery
                        .as_ref()
                        .filter(|r| r.status.as_deref() == Some("recovered"))
                        .and_then(|r| r.note.as_deref());
                    let t = row(&content.text);
                    if let Some(note) = note {
                        row(note)
                    } else if !t.is_empty() {
                        t
                    } else if stop_reason.as_deref() == Some("error") {
                        let e = msg.error_message.as_deref().filter(|e| !e.is_empty());
                        truncate_chars(&clean(e.unwrap_or("Error")), ERROR_LIMIT).to_owned()
                    } else if stop_reason.as_deref() == Some("aborted") {
                        "(aborted)".to_owned()
                    } else {
                        "(no content)".to_owned()
                    }
                }
                NodeKind::ToolResult => msg
                    .tool_call_id
                    .as_deref()
                    .and_then(|i| tools.get(i))
                    .cloned()
                    .unwrap_or_else(|| format!("[{}]", msg.tool_name.as_deref().unwrap_or("tool"))),
                NodeKind::BashExecution => row(&format!(
                    "[bash]: {}",
                    clean(msg.command.as_deref().unwrap_or(""))
                )),
                _ => format!("[{role}]"),
            };
            calls = content.calls;
            (kind, text)
        }
        "custom_message" => {
            let ct = line.custom_type.as_deref().unwrap_or("custom");
            let text = if ct == "advisor" {
                row(&advisor_text(line.details))
            } else {
                let c = line.content.unwrap_or_default();
                let body = strip_system_wrapper(&c.text, c.truncated);
                row(&format!("[{ct}]: {}", clean(body)))
            };
            (NodeKind::CustomMessage(ct.to_owned()), text)
        }
        "compaction" => {
            let k = ((line.tokens_before.unwrap_or(0.0) / 1000.0) + 0.5).floor();
            (
                NodeKind::Compaction,
                format!("[compaction: {}k tokens]", fmt_num(k)),
            )
        }
        "branch_summary" => (
            NodeKind::BranchSummary,
            row(&format!(
                "[branch summary]: {}",
                clean(truncate_chars(
                    line.summary.as_deref().unwrap_or(""),
                    GATHER_LIMIT
                ))
            )),
        ),
        "model_change" => (
            NodeKind::ModelChange,
            format!("[model: {}]", str_or(&line.model, "")),
        ),
        "model_usage" => {
            let role = line.role.as_deref().map(clean).filter(|r| !r.is_empty());
            (
                NodeKind::ModelUsage,
                format!(
                    "[model usage: {} {}{}/{}]",
                    str_or(&line.purpose, ""),
                    role.map_or_else(String::new, |r| format!("{r} ")),
                    str_or(&line.provider, ""),
                    str_or(&line.model, "")
                ),
            )
        }
        "thinking_level_change" => {
            let level = line
                .thinking_level
                .flatten()
                .unwrap_or_else(|| "off".to_owned());
            (NodeKind::ThinkingChange, format!("[thinking: {level}]"))
        }
        "custom" => {
            let ct = line.custom_type.as_deref().unwrap_or("");
            (
                NodeKind::Custom(ct.to_owned()),
                format!("[custom: {}]", clean(ct)),
            )
        }
        "label" => {
            let value = line.label.flatten();
            let shown = value.clone().unwrap_or_else(|| "(cleared)".to_owned());
            if let Some(target) = line.target_id.as_deref() {
                label = Some((target.to_owned(), value.filter(|l| !l.is_empty())));
            }
            (NodeKind::Label, row(&format!("[label: {shown}]")))
        }
        "service_tier_change" => {
            let tiers = match &line.service_tier {
                Some(m) => m
                    .iter()
                    .map(|(f, t)| {
                        format!(
                            "{f}:{}",
                            t.as_str().map_or_else(|| t.to_string(), str::to_owned)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
                None => "(default)".to_owned(),
            };
            (
                NodeKind::Setting("service_tier_change".into()),
                row(&format!("[service tier: {tiers}]")),
            )
        }
        "title_change" => (
            NodeKind::Setting("title_change".into()),
            row(&format!("[title: {}]", str_or(&line.title, ""))),
        ),
        "mode_change" => (
            NodeKind::Setting("mode_change".into()),
            format!("[mode: {}]", str_or(&line.mode, "")),
        ),
        "credential_pin" => (
            NodeKind::Setting("credential_pin".into()),
            format!("[credential pin: {}]", str_or(&line.provider, "")),
        ),
        other => (NodeKind::Setting(other.to_owned()), type_row(&clean(other))),
    };
    Ok(Some(Parsed {
        node: TreeNode {
            id: id.to_owned(),
            parent: line.parent_id.as_deref().map(str::to_owned),
            kind,
            timestamp: line.timestamp.as_deref().map(str::to_owned),
            text,
            has_text,
            stop_reason,
            active_leaf: line.active_leaf,
        },
        label,
        calls,
    }))
}

/// `"key":"value"` (or `"key":null`) within the head of a line; no unescaping (ids/types/ISO
/// timestamps never need it). `Some(None)` = null.
fn head_field<'a>(head: &'a [u8], key: &str) -> Option<Option<&'a str>> {
    let pat = format!("\"{key}\":");
    let at = memchr::memmem::find(head, pat.as_bytes())? + pat.len();
    let rest = &head[at..];
    if rest.starts_with(b"null") {
        return Some(None);
    }
    let rest = rest.strip_prefix(b"\"")?;
    let end = memchr(b'"', rest)?;
    std::str::from_utf8(&rest[..end]).ok().map(Some)
}

/// Node for a line too large to parse: type, ids and role from its prefix only.
fn parse_oversize(head: &[u8], total: usize, leaf: Option<Option<String>>) -> Option<Parsed> {
    let head = &head[..head.len().min(PREFIX_SCAN)];
    let ty = head_field(head, "type")??;
    let id = head_field(head, "id")??;
    let parent = head_field(head, "parentId").flatten();
    let timestamp = head_field(head, "timestamp").flatten();
    let kind = match ty {
        "message" => head_field(head, "role")
            .flatten()
            .map_or_else(|| NodeKind::OtherRole("message".into()), kind_of_role),
        "custom_message" => NodeKind::CustomMessage("custom_message".into()),
        "compaction" => NodeKind::Compaction,
        "branch_summary" => NodeKind::BranchSummary,
        "model_change" => NodeKind::ModelChange,
        "model_usage" => NodeKind::ModelUsage,
        "thinking_level_change" => NodeKind::ThinkingChange,
        "label" => NodeKind::Label,
        "custom" => NodeKind::Custom(String::new()),
        other => NodeKind::Setting(other.to_owned()),
    };
    let mb = total as f64 / (1024.0 * 1024.0);
    let shown = matches!(
        kind,
        NodeKind::User | NodeKind::Assistant | NodeKind::Developer
    );
    Some(Parsed {
        node: TreeNode {
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
            has_text: shown,
            text: format!("[large entry: {mb:.1} MB]"),
            kind,
            timestamp: timestamp.map(str::to_owned),
            stop_reason: None,
            active_leaf: leaf,
        },
        label: None,
        calls: Vec::new(),
    })
}

// ---- builder -----------------------------------------------------------------------------------

#[derive(Default)]
struct Builder {
    nodes: Vec<Arc<TreeNode>>,
    index: HashMap<Arc<str>, usize>,
    children: Vec<Arc<Vec<usize>>>,
    roots: Vec<usize>,
    /// Parent ids named by roots that have not appeared (yet).
    orphans: HashSet<String>,
    /// A node arrived after children that named it: `children`/`roots` need a full rebuild.
    rederive: bool,
    labels: HashMap<String, String>,
    /// toolCall id → rendered row.
    tools: HashMap<String, String>,
    /// Last node whose `activeLeafId` is absent or its own id (omp `vee`).
    last_self: Option<usize>,
    /// Bytes of the file consumed: complete lines (or a valid last line).
    offset: u64,
    /// Identity of the file the offset belongs to.
    ident: Option<FileId>,
    /// Something changed since the last snapshot.
    dirty: bool,
}

enum LineRead {
    Eof,
    /// `bytes` includes the newline; `over` = more than the cap, only the first `cap` kept and
    /// `leaf` is the top-level `activeLeafId` found while skipping the rest.
    Line {
        complete: bool,
        bytes: usize,
        over: bool,
        leaf: Option<Option<String>>,
    },
}

/// Streaming scan of a (huge) JSON object line for its top-level `"activeLeafId"` value without
/// materialising the line: tracks nesting and strings only.
#[derive(Default)]
struct LeafScan {
    depth: u32,
    in_str: bool,
    esc: bool,
    /// What the string being read is: a depth-1 candidate key, or the wanted value.
    cap: Cap,
    buf: Vec<u8>,
    buf_over: bool,
    /// The last closed depth-1 string was `activeLeafId` and a `:` is next.
    key_seen: bool,
    /// After the key's colon: the value is next.
    want_value: bool,
    result: Option<Option<String>>,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum Cap {
    #[default]
    No,
    Key,
    Value,
}

impl LeafScan {
    const KEY: &'static [u8] = b"activeLeafId";
    const CAP: usize = 512;

    fn feed(&mut self, bytes: &[u8]) {
        if self.result.is_some() {
            return;
        }
        for &b in bytes {
            self.byte(b);
            if self.result.is_some() {
                return;
            }
        }
    }

    fn byte(&mut self, b: u8) {
        if self.in_str {
            if self.esc {
                self.esc = false;
                self.push(b);
            } else if b == b'\\' {
                self.esc = true;
                self.push(b);
            } else if b == b'"' {
                self.in_str = false;
                match self.cap {
                    Cap::Key => self.key_seen = !self.buf_over && self.buf == Self::KEY,
                    Cap::Value => {
                        self.result = Some(Some(String::from_utf8_lossy(&self.buf).into_owned()));
                    }
                    Cap::No => {}
                }
                self.cap = Cap::No;
            } else {
                self.push(b);
            }
            return;
        }
        if b.is_ascii_whitespace() {
            return;
        }
        match b {
            b'"' => {
                self.in_str = true;
                self.buf.clear();
                self.buf_over = false;
                self.cap = if self.want_value {
                    self.want_value = false;
                    Cap::Value
                } else if self.depth == 1 {
                    Cap::Key
                } else {
                    Cap::No
                };
                self.key_seen = false;
                return;
            }
            b':' if self.depth == 1 && self.key_seen => {
                self.want_value = true;
                self.key_seen = false;
                return;
            }
            b'n' if self.want_value => self.result = Some(None),
            b'{' | b'[' => self.depth += 1,
            b'}' | b']' => self.depth = self.depth.saturating_sub(1),
            _ => {}
        }
        self.key_seen = false;
        self.want_value = false;
    }

    fn push(&mut self, b: u8) {
        if self.cap == Cap::No {
            return;
        }
        if self.buf.len() < Self::CAP {
            self.buf.push(b);
        } else {
            self.buf_over = true;
        }
    }
}

/// Reads one line (without its `\n`) into `buf`, keeping at most `cap` bytes of it.
fn read_line<R: BufRead>(r: &mut R, buf: &mut Vec<u8>, cap: usize) -> io::Result<LineRead> {
    buf.clear();
    let mut total = 0;
    let mut over = false;
    let mut scan = LeafScan::default();
    loop {
        let chunk = r.fill_buf()?;
        if chunk.is_empty() {
            return Ok(if total == 0 {
                LineRead::Eof
            } else {
                LineRead::Line {
                    complete: false,
                    bytes: total,
                    over,
                    leaf: scan.result,
                }
            });
        }
        let (piece, used, done) = match memchr(b'\n', chunk) {
            Some(p) => (&chunk[..p], p + 1, true),
            None => (chunk, chunk.len(), false),
        };
        if over {
            scan.feed(piece);
        } else {
            let room = cap - buf.len();
            if piece.len() <= room {
                buf.extend_from_slice(piece);
            } else {
                buf.extend_from_slice(&piece[..room]);
                over = true;
                scan.feed(buf);
                scan.feed(&piece[room..]);
            }
        }
        total += used;
        r.consume(used);
        if done {
            return Ok(LineRead::Line {
                complete: true,
                bytes: total,
                over,
                leaf: scan.result,
            });
        }
    }
}

impl Builder {
    fn reset(&mut self) {
        *self = Builder {
            dirty: true,
            ..Builder::default()
        };
    }

    /// Adds one parsed line. Returns false for a malformed line (skipped).
    fn ingest(&mut self, bytes: &[u8], over: Option<(usize, Option<Option<String>>)>) -> bool {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return true;
        }
        let parsed = match over {
            Some((total, leaf)) => Ok(parse_oversize(bytes, total, leaf)),
            None => parse_line(bytes, &self.tools),
        };
        match parsed {
            Ok(Some(p)) => {
                self.add(p);
                true
            }
            Ok(None) => true,
            Err(e) => {
                log::debug!("tree: skipped malformed line: {e}");
                false
            }
        }
    }

    fn add(&mut self, p: Parsed) {
        if self.index.contains_key(p.node.id.as_str()) {
            log::debug!("tree: duplicate entry id {}", p.node.id);
            return;
        }
        for (id, rendered) in p.calls {
            self.tools.insert(id, rendered);
        }
        if let Some((target, label)) = p.label {
            match label {
                Some(l) => {
                    self.labels.insert(target, l);
                }
                None => {
                    self.labels.remove(&target);
                }
            }
        }
        let i = self.nodes.len();
        let own = match &p.node.active_leaf {
            None => true,
            Some(a) => a.as_deref() == Some(p.node.id.as_str()),
        };
        if own {
            self.last_self = Some(i);
        }
        // Parent resolution happens before the node's own id enters the index (self parent = root).
        let parent = p
            .node
            .parent
            .as_deref()
            .and_then(|pid| self.index.get(pid).copied());
        match parent {
            Some(pi) => {
                let ts = &p.node.timestamp;
                let kids = Arc::make_mut(&mut self.children[pi]);
                let at = kids.partition_point(|&c| self.nodes[c].timestamp <= *ts);
                kids.insert(at, i);
            }
            None => {
                self.roots.push(i);
                if let Some(pid) = p.node.parent.as_deref()
                    && pid != p.node.id
                {
                    self.orphans.insert(pid.to_owned());
                }
            }
        }
        if self.orphans.remove(p.node.id.as_str()) {
            self.rederive = true; // earlier nodes named this one as parent
        }
        self.index.insert(Arc::from(p.node.id.as_str()), i);
        self.children.push(Arc::default());
        self.nodes.push(Arc::new(p.node));
        self.dirty = true;
    }

    /// Consumes the complete lines after `self.offset` (plus a last line that already parses).
    fn read_new(&mut self, mut file: File, stop: &AtomicBool) -> io::Result<()> {
        let meta = file.metadata()?;
        let ident = platform::file_identity(&file)?;
        if self.ident.is_some_and(|i| i != ident) || meta.len() < self.offset {
            self.reset();
        }
        self.ident = Some(ident);
        file.seek(SeekFrom::Start(self.offset))?;
        let mut rd = BufReader::with_capacity(1 << 20, file);
        let mut buf = Vec::new();
        while !stop.load(Ordering::Relaxed) {
            match read_line(&mut rd, &mut buf, MAX_LINE)? {
                LineRead::Eof => break,
                LineRead::Line {
                    complete,
                    bytes,
                    over,
                    leaf,
                } => {
                    let over = over.then_some((bytes, leaf));
                    if complete {
                        self.ingest(&buf, over);
                    } else if over.is_none() && self.ingest(&buf, None) {
                        // a valid last line without its newline yet
                    } else {
                        break; // truncated: wait for the rest
                    }
                    self.offset += bytes as u64;
                    if !complete {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    fn leaf(&self) -> Option<usize> {
        let last = self.nodes.last()?;
        let a = match &last.active_leaf {
            None => return Some(self.nodes.len() - 1),
            Some(None) => return None,
            Some(Some(a)) => a,
        };
        if *a == last.id {
            return Some(self.nodes.len() - 1);
        }
        self.index.get(a.as_str()).copied().or(self.last_self)
    }

    /// Children (timestamp-sorted) and roots from scratch.
    fn derive(
        nodes: &[Arc<TreeNode>],
        index: &HashMap<Arc<str>, usize>,
    ) -> (Vec<Arc<Vec<usize>>>, Vec<usize>) {
        let mut children = vec![Vec::new(); nodes.len()];
        let mut roots = Vec::new();
        for (i, n) in nodes.iter().enumerate() {
            match n.parent.as_deref().and_then(|p| index.get(p)) {
                Some(&p) if p != i => children[p].push(i),
                _ => roots.push(i),
            }
        }
        for c in &mut children {
            if c.len() > 1 {
                c.sort_by(|&a, &b| nodes[a].timestamp.cmp(&nodes[b].timestamp));
            }
        }
        (children.into_iter().map(Arc::new).collect(), roots)
    }

    /// Re-derives the parent links when a node arrived after its children (rare).
    fn settle(&mut self) {
        if self.rederive {
            (self.children, self.roots) = Self::derive(&self.nodes, &self.index);
            self.rederive = false;
        }
    }

    /// Publishes the current state; nodes, id keys and child lists are shared, not copied.
    fn snapshot(&mut self) -> SessionTree {
        self.settle();
        SessionTree {
            nodes: self.nodes.clone(),
            index: self.index.clone(),
            children: self.children.clone(),
            roots: self.roots.clone(),
            labels: self.labels.clone(),
            file_leaf: self.leaf(),
        }
    }

    fn into_tree(mut self) -> SessionTree {
        self.settle();
        let file_leaf = self.leaf();
        SessionTree {
            nodes: self.nodes,
            index: self.index,
            children: self.children,
            roots: self.roots,
            labels: self.labels,
            file_leaf,
        }
    }
}

// ---- watcher -----------------------------------------------------------------------------------

#[derive(Default)]
struct Shared {
    latest: Mutex<Option<Arc<SessionTree>>>,
    error: Mutex<Option<String>>,
    revision: AtomicU64,
    shutdown: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Streams the file on its own thread, tails appends (every 250 ms) and rebuilds; a replaced
/// (different inode) or shrunken file is reloaded in full. A truncated last line is ignored until
/// it completes. Dropping stops and joins the thread.
pub struct TreeWatcher {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl TreeWatcher {
    /// `on_update` runs on the watcher thread after every new tree / error (repaint request).
    pub fn spawn(path: PathBuf, on_update: impl Fn() + Send + 'static) -> TreeWatcher {
        let shared = Arc::new(Shared::default());
        let worker = Arc::clone(&shared);
        let thread = std::thread::Builder::new()
            .name("tree-watcher".into())
            .spawn(move || run(&path, &worker, &on_update))
            .map_err(|e| {
                log::error!("cannot spawn tree watcher: {e}");
                *lock(&shared.error) = Some(format!("cannot start tree reader: {e}"));
            })
            .ok();
        TreeWatcher { shared, thread }
    }

    /// The newest tree; `None` until the first load finished.
    pub fn latest(&self) -> Option<Arc<SessionTree>> {
        lock(&self.shared.latest).clone()
    }

    /// Bumped whenever `latest` or `error` changes.
    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    /// Why the file cannot be read right now (missing, unreadable); cleared on success.
    pub fn error(&self) -> Option<String> {
        lock(&self.shared.error).clone()
    }
}

impl Drop for TreeWatcher {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            t.thread().unpark();
            let _ = t.join();
        }
    }
}

fn run(path: &Path, shared: &Shared, on_update: &dyn Fn()) {
    let mut b = Builder::default();
    let mut published = false;
    while !shared.shutdown.load(Ordering::Acquire) {
        let step = File::open(path).and_then(|file| {
            let meta = file.metadata()?;
            let ident = platform::file_identity(&file)?;
            let stale = b.ident.is_some_and(|i| i != ident) || meta.len() < b.offset;
            if published && !stale && meta.len() == b.offset {
                return Ok(());
            }
            b.read_new(file, &shared.shutdown)
        });
        match step {
            Ok(()) => {
                let cleared = lock(&shared.error).take().is_some();
                if b.dirty || !published {
                    let tree = Arc::new(b.snapshot());
                    b.dirty = false;
                    published = true;
                    *lock(&shared.latest) = Some(tree);
                } else if !cleared {
                    park(shared);
                    continue;
                }
                shared.revision.fetch_add(1, Ordering::Release);
                on_update();
            }
            Err(e) => {
                let msg = format!("{}: {e}", path.display());
                let changed = lock(&shared.error).replace(msg.clone()).as_deref() != Some(&msg);
                if changed {
                    shared.revision.fetch_add(1, Ordering::Release);
                    on_update();
                }
            }
        }
        park(shared);
    }
}

fn park(shared: &Shared) {
    if !shared.shutdown.load(Ordering::Acquire) {
        std::thread::park_timeout(POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_strips_controls_ansi_and_folds_whitespace() {
        assert_eq!(
            clean("  a\n\tb\r\x07c \x1b[31mred\x1b[0m\x1b]0;t\x07 "),
            "a  bc red"
        );
        assert_eq!(truncate_chars("héllo", 2), "hé");
    }

    #[test]
    fn system_wrapper_is_stripped_once() {
        let s = "<system-notice>\nJob <system-x>y</system-x> done\n</system-notice>";
        assert_eq!(
            strip_system_wrapper(s, false),
            "Job <system-x>y</system-x> done"
        );
        assert_eq!(strip_system_wrapper("<b>x</b>", false), "<b>x</b>");
        assert_eq!(strip_system_wrapper("<system-n>cut off", true), "cut off");
        assert_eq!(
            strip_system_wrapper("<system-n>no close", false),
            "<system-n>no close"
        );
    }

    fn call(name: &str, args: &str) -> String {
        let raw = RawValue::from_string(args.to_owned()).unwrap();
        format_tool_call(name, Some(&raw))
    }

    #[test]
    fn tool_call_rows_follow_omp() {
        assert_eq!(call("read", r#"{"path":"/a/b.rs"}"#), "[read: /a/b.rs]");
        assert_eq!(
            call("read", r#"{"path":"/a","offset":10,"limit":5}"#),
            "[read: /a:10-14]"
        );
        assert_eq!(
            call("read", r#"{"file_path":"/a","limit":3}"#),
            "[read: /a:1-3]"
        );
        assert_eq!(
            call("write", r#"{"path":"/w","content":"zzz"}"#),
            "[write: /w]"
        );
        assert_eq!(call("bash", r#"{"command":"ls\n-la"}"#), "[bash: ls -la]");
        let long = "x".repeat(60);
        assert_eq!(
            call("bash", &format!(r#"{{"command":"{long}"}}"#)),
            format!("[bash: {}...]", "x".repeat(50))
        );
        assert_eq!(
            call("grep", r#"{"pattern":"fn main","paths":["src","tests"]}"#),
            "[grep: /fn main/ in src, tests]"
        );
        assert_eq!(call("grep", r#"{"pattern":"p"}"#), "[grep: /p/ in .]");
        assert_eq!(call("glob", r#"{"path":"**/*.rs"}"#), "[glob: **/*.rs]");
        assert_eq!(call("ls", "{}"), "[ls: .]");
        assert_eq!(
            call("fetch", r#"{"url":"https://x.y"}"#),
            r#"[fetch: {"url":"https://x.y"}]"#
        );
        assert_eq!(
            call("fetch", &format!(r#"{{"u":"{}"}}"#, "z".repeat(60))),
            format!(r#"[fetch: {{"u":"{}...]"#, "z".repeat(34))
        );
    }

    #[test]
    fn oversize_prefix_scan_reads_ids_and_role() {
        let head = br#"{"type":"message","id":"ab12","parentId":"cd34","timestamp":"2026-01-01T00:00:00Z","message":{"role":"toolResult","content":[{"type":"text","text":"xxx"#;
        let p = parse_oversize(head, 9 * 1024 * 1024, None).unwrap();
        assert_eq!(p.node.id, "ab12");
        assert_eq!(p.node.parent.as_deref(), Some("cd34"));
        assert_eq!(p.node.kind, NodeKind::ToolResult);
        assert_eq!(p.node.text, "[large entry: 9.0 MB]");
        let root = br#"{"type":"model_change","id":"r","parentId":null,"timestamp":"t""#;
        assert_eq!(parse_oversize(root, 1, None).unwrap().node.parent, None);
    }
}
