//! Task-agent definitions, found where omp looks for them (`task/discovery.ts`):
//! `<agent dir>/agents/*.md` (user) and, per project, the nearest `.omp/agents/*.md` walking up
//! from its cwd (project; replaces a user agent of the same name), plus omp's built-in agents
//! ([`Scope::Bundled`], read-only: what `omp agents unpack` writes, parsed with [`bundled`]),
//! which any user or project agent of the same name replaces. Extension/plugin packages are not
//! listed: not files the user owns.
//!
//! Reading mirrors omp's `parseAgent` + `parseFrontmatter`: CRLF normalized, leading HTML
//! comments skipped, a `---` block up to the first `\n---`, then the trimmed body. Only the keys
//! the editor shows are read (a tolerant reader of the YAML subset agent files use), and edits are
//! textual: changing the model rewrites the `model` entry alone, changing the body replaces the
//! trimmed body alone. Every other byte, comments and key order included, is kept.
//!
//! [`save`], [`create`] and [`delete`] are the only writers. `save` re-reads the file, refuses
//! when it changed since the caller read it ([`EditError::Changed`]), applies the edit and
//! replaces the file atomically (through a symlink to its target, keeping its permissions);
//! `create` never replaces a file; `delete` checks the revision too. Blocking: call from a worker.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Directory name of agent definitions inside a config root (`~/.omp/agent`, `<project>/.omp`).
pub const AGENTS_DIR: &str = "agents";
/// omp's project config directory.
pub const PROJECT_CONFIG_DIR: &str = ".omp";

/// Names omp reserves for its own sessions (`main`, `sub`): a definition using one is skipped.
const RESERVED_NAMES: [&str; 2] = ["main", "sub"];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scope {
    /// `<agent dir>/agents`: every session sees these.
    User,
    /// `<project>/.omp/agents`: sessions under the project; wins over a user agent.
    Project,
    /// omp's built-in agents: read-only, replaced by any user or project agent of the same name.
    Bundled,
}

/// The `path` prefix of built-in definitions, which are no files (`omp:bundled/scout.md`).
pub const BUNDLED_PREFIX: &str = "omp:bundled/";

/// One directory of definitions ([`Scope::Bundled`]: omp's built-ins, `dir` and `root` empty).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDir {
    pub scope: Scope,
    /// The `agents` directory itself.
    pub dir: PathBuf,
    /// The folder the scope is named after: the agent dir (user) or the folder holding `.omp`.
    pub root: PathBuf,
    /// Sorted by file name (omp's load order: the first file declaring a name wins).
    pub agents: Vec<AgentFile>,
    /// Why the directory could not be listed (it exists but is unreadable; built-ins: omp failed).
    pub error: Option<String>,
}

/// Why omp skips a definition file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Issue {
    Unreadable(String),
    /// No `---` frontmatter block.
    NoFrontmatter,
    /// Frontmatter without a string `name` / `description`.
    MissingName,
    MissingDescription,
    /// `main` / `sub`.
    ReservedName,
    /// An earlier file of the same directory declares this name.
    DuplicateOf(PathBuf),
}

impl Issue {
    /// One line for the editor ("omp skips this file: …").
    pub fn describe(&self) -> String {
        match self {
            Issue::Unreadable(e) => format!("the file cannot be read ({e})"),
            Issue::NoFrontmatter => "it has no --- frontmatter with name and description".into(),
            Issue::MissingName => "its frontmatter has no name".into(),
            Issue::MissingDescription => "its frontmatter has no description".into(),
            Issue::ReservedName => "main and sub are reserved names".into(),
            Issue::DuplicateOf(p) => format!(
                "{} declares the same name first",
                p.file_name().unwrap_or_default().to_string_lossy()
            ),
        }
    }
}

/// One `*.md` definition as last read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentFile {
    pub path: PathBuf,
    pub name: Option<String>,
    pub description: Option<String>,
    /// `model` as written: one pattern (`provider/id[:level]`, `@role`, a fuzzy name) or a
    /// comma-joined fallback list. `None`: the agent runs on the session's model.
    pub model: Option<String>,
    /// `thinkingLevel` (or `thinking`): applies when `model` carries no `:level`.
    pub thinking_level: Option<String>,
    /// A `---` block exists (a model can be set).
    pub frontmatter: bool,
    /// The system prompt omp uses: everything after the frontmatter, trimmed.
    pub body: String,
    /// Content hash at read time; [`save`] refuses when the file no longer matches it.
    pub revision: String,
    pub issue: Option<Issue>,
}

impl AgentFile {
    /// The name omp knows the agent by (`None` when it skips the file).
    pub fn loaded_name(&self) -> Option<&str> {
        match self.issue {
            None => self.name.as_deref(),
            Some(_) => None,
        }
    }

    /// The name to list it under: the declared one, else the file stem.
    pub fn display_name(&self) -> String {
        self.name.clone().unwrap_or_else(|| {
            self.path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
    }
}

/// What omp's config says about agents (`task.agentModelOverrides`, `task.disabledAgents`) and
/// the model roles (`modelRoles`) agents name as `@role`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Agent name → model patterns, as configured (string or list; kept whole so writing the map
    /// back changes only the entry edited). Wins over the file's `model`.
    pub model_overrides: BTreeMap<String, serde_json::Value>,
    pub disabled: Vec<String>,
    /// Role → model selector (`smol` → `provider/id:level`).
    pub roles: BTreeMap<String, String>,
}

/// The `task.agentModelOverrides` setting key.
pub const OVERRIDES_KEY: &str = "task.agentModelOverrides";
/// The `task.disabledAgents` setting key.
pub const DISABLED_KEY: &str = "task.disabledAgents";
/// The `modelRoles` setting key.
pub const ROLES_KEY: &str = "modelRoles";

impl AgentSettings {
    /// From the settings' values (`omp config get <key> --json` → `value`). A value of the
    /// wrong shape counts as unset.
    pub fn from_values(
        overrides: &serde_json::Value,
        disabled: &serde_json::Value,
        roles: &serde_json::Value,
    ) -> Self {
        AgentSettings {
            model_overrides: overrides
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
            disabled: disabled
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
            roles: roles
                .as_object()
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| {
                            let v = v.as_str()?.trim();
                            (!v.is_empty()).then(|| (k.clone(), v.to_owned()))
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// The override for `name`, comma-joined like the `model` field.
    pub fn override_for(&self, name: &str) -> Option<String> {
        self.model_overrides
            .get(name)
            .map(override_text)
            .filter(|s| !s.is_empty())
    }

    /// `task.agentModelOverrides` with `name`'s entry set to `model` (`None` removes it), every
    /// other entry as configured: the value to write.
    pub fn overrides_with(&self, name: &str, model: Option<&str>) -> serde_json::Value {
        let mut map: serde_json::Map<String, serde_json::Value> = self
            .model_overrides
            .iter()
            .filter(|(k, _)| *k != name)
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if let Some(m) = model {
            map.insert(name.to_owned(), serde_json::Value::String(m.to_owned()));
        }
        serde_json::Value::Object(map)
    }

    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled.iter().any(|d| d == name)
    }

    /// `task.disabledAgents` with `name` listed or not, the others kept in order: the value to
    /// write.
    pub fn disabled_with(&self, name: &str, disabled: bool) -> serde_json::Value {
        let mut list: Vec<&str> = self
            .disabled
            .iter()
            .map(String::as_str)
            .filter(|d| *d != name)
            .collect();
        if disabled {
            list.push(name);
        }
        serde_json::Value::from(list)
    }

    /// The model a role alias (`@smol`, legacy `pi/smol`) stands for, when configured.
    pub fn role_model(&self, alias: &str) -> Option<&str> {
        let role = alias
            .strip_prefix('@')
            .or_else(|| alias.strip_prefix("pi/"))?;
        // `@smol:high`: the level applies on top of the role's model.
        let role = role.split_once(':').map_or(role, |(r, _)| r);
        self.roles.get(role).map(String::as_str)
    }
}

fn override_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.trim().to_owned(),
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|p| p.as_str().map(str::trim).filter(|p| !p.is_empty()))
            .collect::<Vec<_>>()
            .join(", "),
        _ => String::new(),
    }
}

/// Every definition directory: the user's first (even when it does not exist yet, so the editor
/// can say where they go), then each project directory that exists, by root path, then (once the
/// caller adds it) omp's built-ins.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub dirs: Vec<AgentDir>,
    /// Project folders asked about that have no `.omp/agents` yet (where one can be created).
    pub bare_roots: Vec<PathBuf>,
    /// `Err`: omp's config could not be read (omp missing or failing); overrides are unknown.
    pub settings: Option<Result<AgentSettings, String>>,
}

/// The `agents` directory omp uses for a session in `cwd`: `<dir>/.omp/agents` of the nearest
/// ancestor (`cwd` included) where it is a directory.
pub fn project_agents_dir(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .map(|a| a.join(PROJECT_CONFIG_DIR).join(AGENTS_DIR))
        .find(|d| d.is_dir())
}

/// Lists the user directory under `agent_dir` and the project directories of `project_cwds`
/// (each resolved like omp does; several cwds sharing one directory list it once). A cwd no
/// `.omp/agents` applies to is a [`Catalog::bare_roots`] entry. `settings` is left unset.
pub fn discover(agent_dir: &Path, project_cwds: &[PathBuf]) -> Catalog {
    let user = agent_dir.join(AGENTS_DIR);
    let mut dirs = vec![read_dir(Scope::User, &user, agent_dir)];
    let mut projects = Vec::new();
    let mut bare_roots = Vec::new();
    // omp resolves from a session's cwd, which exists; a vanished folder has no sessions.
    for cwd in project_cwds.iter().filter(|c| c.is_dir()) {
        match project_agents_dir(cwd) {
            Some(d) if same_dir(&d, &user) => {}
            Some(d) => projects.push(d),
            None => bare_roots.push(cwd.clone()),
        }
    }
    projects.sort();
    projects.dedup();
    bare_roots.sort();
    bare_roots.dedup();
    for dir in projects {
        let root = dir
            .parent()
            .and_then(Path::parent)
            .map_or_else(|| dir.clone(), Path::to_path_buf);
        dirs.push(read_dir(Scope::Project, &dir, &root));
    }
    Catalog {
        dirs,
        bare_roots,
        settings: None,
    }
}

/// The built-in definitions omp ships, from `(file name, content)` pairs (what `omp agents
/// unpack` writes): parsed like files, each with a `path` of [`BUNDLED_PREFIX`] + file name.
pub fn bundled(sources: &[(String, String)]) -> AgentDir {
    let mut sources: Vec<&(String, String)> = sources.iter().collect();
    sources.sort();
    AgentDir {
        scope: Scope::Bundled,
        dir: PathBuf::new(),
        root: PathBuf::new(),
        agents: sources
            .into_iter()
            .map(|(file, content)| {
                parse(
                    &PathBuf::from(format!("{BUNDLED_PREFIX}{file}")),
                    content.as_bytes(),
                )
            })
            .collect(),
        error: None,
    }
}

/// Whether `path` names a built-in definition (see [`bundled`]).
pub fn is_bundled(path: &Path) -> bool {
    path.to_str().is_some_and(|p| p.starts_with(BUNDLED_PREFIX))
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || matches!((fs::canonicalize(a), fs::canonicalize(b)), (Ok(x), Ok(y)) if x == y)
}

fn read_dir(scope: Scope, dir: &Path, root: &Path) -> AgentDir {
    let mut out = AgentDir {
        scope,
        dir: dir.to_path_buf(),
        root: root.to_path_buf(),
        agents: Vec::new(),
        error: None,
    };
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return out,
        Err(e) => {
            out.error = Some(e.to_string());
            return out;
        }
    };
    let mut files: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file() || t.is_symlink()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.ends_with(".md").then(|| (name, e.path()))
        })
        .collect();
    files.sort();
    let mut first: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (_, path) in files {
        let mut agent = read_file(&path);
        if let Some(name) = agent.loaded_name() {
            match first.get(name) {
                Some(winner) => agent.issue = Some(Issue::DuplicateOf(winner.clone())),
                None => {
                    first.insert(name.to_owned(), path.clone());
                }
            }
        }
        out.agents.push(agent);
    }
    out
}

/// Reads and parses one definition (an unreadable file comes back with [`Issue::Unreadable`]).
pub fn read_file(path: &Path) -> AgentFile {
    match fs::read(path) {
        Ok(bytes) => parse(path, &bytes),
        Err(e) => AgentFile {
            path: path.to_path_buf(),
            name: None,
            description: None,
            model: None,
            thinking_level: None,
            frontmatter: false,
            body: String::new(),
            revision: String::new(),
            issue: Some(Issue::Unreadable(e.to_string())),
        },
    }
}

/// Parses file bytes (lossy UTF-8, like omp's `readFile(…, "utf-8")`).
pub fn parse(path: &Path, bytes: &[u8]) -> AgentFile {
    let text = normalize(&String::from_utf8_lossy(bytes));
    let layout = Layout::of(&text);
    let entries = layout
        .meta
        .clone()
        .map(|m| top_level_entries(&text, m))
        .unwrap_or_default();
    let get = |key: &str| entries.iter().find(|e| e.key == key);
    let scalar = |key: &str| {
        get(key)
            .and_then(|e| value_of(&text, e))
            .and_then(Value::into_string)
    };
    let name = scalar("name").filter(|n| !n.is_empty());
    let description = scalar("description").filter(|d| !d.is_empty());
    let model = get("model")
        .and_then(|e| value_of(&text, e))
        .map(Value::joined)
        .filter(|m| !m.is_empty());
    let thinking_level = scalar("thinkingLevel")
        .or_else(|| scalar("thinking"))
        .filter(|l| !l.is_empty());
    let issue = if layout.meta.is_none() {
        Some(Issue::NoFrontmatter)
    } else if name.is_none() {
        Some(Issue::MissingName)
    } else if description.is_none() {
        Some(Issue::MissingDescription)
    } else if name
        .as_deref()
        .is_some_and(|n| RESERVED_NAMES.contains(&n.trim().to_lowercase().as_str()))
    {
        Some(Issue::ReservedName)
    } else {
        None
    };
    AgentFile {
        path: path.to_path_buf(),
        name,
        description,
        model,
        thinking_level,
        frontmatter: layout.meta.is_some(),
        body: text[layout.body.clone()].to_owned(),
        revision: revision(bytes),
        issue,
    }
}

/// Hex SHA-256 of the file bytes (first 128 bits).
pub fn revision(bytes: &[u8]) -> String {
    Sha256::digest(bytes)[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

// ------------------------------------------------------------------ layout

/// Byte ranges of a normalized file.
struct Layout {
    /// Between `---\n` and the closing `\n---`; `None` without frontmatter.
    meta: Option<Range<usize>>,
    /// End of the closing `---` (frontmatter only).
    close_end: usize,
    /// The trimmed body.
    body: Range<usize>,
}

impl Layout {
    fn of(text: &str) -> Layout {
        let start = skip_leading_comments(text);
        let rest = &text[start..];
        let close = rest
            .starts_with("---")
            .then(|| rest.get(3..).and_then(|r| r.find("\n---")))
            .flatten()
            .map(|i| start + 3 + i);
        match close {
            Some(close) => {
                let meta_start = (start + 4).min(close);
                let close_end = close + 4;
                Layout {
                    meta: Some(meta_start..close),
                    close_end,
                    body: trimmed(text, close_end..text.len()),
                }
            }
            None => Layout {
                meta: None,
                close_end: 0,
                body: trimmed(text, 0..text.len()),
            },
        }
    }
}

/// omp drops HTML comments (and the whitespace around them) ahead of an opening `---`; a
/// document whose comments are not followed by `---` is left alone.
fn skip_leading_comments(text: &str) -> usize {
    let ws = |s: &str| s.len() - s.trim_start().len();
    let mut at = 0;
    while let Some(end) = text[at..]
        .trim_start()
        .strip_prefix("<!--")
        .and_then(|r| r.find("-->"))
    {
        at += ws(&text[at..]) + 4 + end + 3;
    }
    if at == 0 {
        return 0;
    }
    let start = at + ws(&text[at..]);
    if text[start..].starts_with("---") {
        start
    } else {
        0
    }
}

fn trimmed(text: &str, r: Range<usize>) -> Range<usize> {
    let s = &text[r.clone()];
    let lead = s.len() - s.trim_start().len();
    let t = s.trim();
    r.start + lead..r.start + lead + t.len()
}

// ------------------------------------------------------------------ frontmatter reader

/// A top-level `key: …` entry: its key line and continuation lines.
#[derive(Debug)]
struct Entry {
    /// camelCase like omp's `normalizeFrontmatterKeys` (`thinking-level` → `thinkingLevel`).
    key: String,
    /// Key line start .. end of its last non-blank line (no trailing newline).
    span: Range<usize>,
    /// The text after `key:` on the key line.
    inline: Range<usize>,
    /// Continuation lines (start of the first .. `span.end`); empty when none.
    more: Range<usize>,
}

fn lines(text: &str, r: Range<usize>) -> impl Iterator<Item = Range<usize>> + '_ {
    let base = r.start;
    text[r].split('\n').scan(base, |at, line| {
        let start = *at;
        *at += line.len() + 1;
        Some(start..start + line.len())
    })
}

fn top_level_entries(text: &str, meta: Range<usize>) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for line in lines(text, meta) {
        let s = &text[line.clone()];
        let continues =
            s.trim().is_empty() || s.starts_with([' ', '\t']) || s == "-" || s.starts_with("- ");
        if continues {
            if let Some(e) = out.last_mut()
                && !s.trim().is_empty()
            {
                if e.more.is_empty() {
                    e.more = line.start..line.end;
                }
                e.span.end = line.end;
                e.more.end = line.end;
            }
            continue;
        }
        // A column-0 comment (or a line that is no `key:`) ends the entry before it; a keyless
        // placeholder keeps the lines under it from joining that entry.
        let Some(colon) = key_colon(s).filter(|_| !s.starts_with('#')) else {
            out.push(Entry {
                key: String::new(),
                span: line.clone(),
                inline: line.end..line.end,
                more: line.end..line.end,
            });
            continue;
        };
        let raw = s[..colon].trim();
        let key = unquote_key(raw);
        out.push(Entry {
            key: camel(&key),
            span: line.clone(),
            inline: line.start + colon + 1..line.end,
            more: line.end..line.end,
        });
    }
    out.retain(|e| !e.key.is_empty());
    out
}

/// The `:` ending a mapping key: followed by whitespace or the line end, outside quotes.
fn key_colon(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let quote = matches!(bytes.first(), Some(b'"' | b'\''));
    let from = if quote {
        let q = bytes[0];
        1 + s[1..].find(q as char)? + 1
    } else {
        0
    };
    s[from..]
        .char_indices()
        .find(|&(i, c)| {
            c == ':'
                && s[from + i + 1..]
                    .chars()
                    .next()
                    .is_none_or(char::is_whitespace)
        })
        .map(|(i, _)| from + i)
}

fn unquote_key(raw: &str) -> String {
    let b = raw.as_bytes();
    if b.len() >= 2 && (b[0] == b'"' || b[0] == b'\'') && b[b.len() - 1] == b[0] {
        raw[1..raw.len() - 1].to_owned()
    } else {
        raw.to_owned()
    }
}

/// omp's `kebabToCamel`: `-` + a lowercase ASCII letter becomes the uppercase letter.
fn camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut chars = key.chars().peekable();
    while let Some(c) = chars.next() {
        match chars.peek() {
            Some(&n) if c == '-' && n.is_ascii_lowercase() => {
                out.push(n.to_ascii_uppercase());
                chars.next();
            }
            _ => out.push(c),
        }
    }
    out
}

#[derive(Debug, PartialEq)]
enum Value {
    Str(String),
    List(Vec<String>),
}

impl Value {
    fn into_string(self) -> Option<String> {
        match self {
            Value::Str(s) => Some(s),
            Value::List(_) => None,
        }
    }

    fn joined(self) -> String {
        match self {
            Value::Str(s) => s,
            Value::List(items) => items.join(", "),
        }
    }
}

fn value_of(text: &str, e: &Entry) -> Option<Value> {
    let inline = strip_comment(text[e.inline.clone()].trim());
    let more = &text[e.more.clone()];
    let more_lines = || {
        more.split('\n')
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
    };
    if inline.is_empty() {
        if more.trim().is_empty() {
            return None;
        }
        let items: Vec<&str> = more_lines().collect();
        if items.iter().all(|l| *l == "-" || l.starts_with("- ")) {
            return Some(Value::List(
                items
                    .iter()
                    .map(|l| scalar(strip_comment(l.trim_start_matches('-').trim())))
                    .filter(|s| !s.is_empty())
                    .collect(),
            ));
        }
        return Some(Value::Str(scalar(&items.join(" "))));
    }
    if let Some(style) = inline.chars().next().filter(|c| *c == '|' || *c == '>') {
        return Some(Value::Str(block_scalar(more, style == '|')));
    }
    if inline.starts_with('[') {
        let all = format!("{inline} {}", more_lines().collect::<Vec<_>>().join(" "));
        let inner = all
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .trim();
        return Some(Value::List(
            split_flow(inner)
                .into_iter()
                .map(|p| scalar(p.trim()))
                .filter(|s| !s.is_empty())
                .collect(),
        ));
    }
    if inline.starts_with(['"', '\'']) {
        // A quoted scalar may continue on the next lines (folded with spaces).
        let joined = std::iter::once(inline)
            .chain(more.split('\n').map(str::trim))
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        return Some(Value::Str(scalar(&joined)));
    }
    let folded = std::iter::once(inline)
        .chain(more_lines())
        .collect::<Vec<_>>()
        .join(" ");
    Some(Value::Str(scalar(&folded)))
}

/// Drops a ` # comment` from a plain (unquoted) value.
fn strip_comment(s: &str) -> &str {
    if s.starts_with(['"', '\'']) {
        return s;
    }
    if s.starts_with('#') {
        return "";
    }
    match s.find(" #").or_else(|| s.find("\t#")) {
        Some(i) => s[..i].trim_end(),
        None => s,
    }
}

/// A scalar's text: double quotes with JSON-compatible escapes, single quotes with `''`, else
/// the plain text.
fn scalar(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2
        && s.starts_with('"')
        && let Some(end) = closing_double_quote(s)
    {
        let quoted = &s[..=end];
        return serde_json::from_str::<String>(quoted)
            .unwrap_or_else(|_| quoted[1..quoted.len() - 1].to_owned());
    }
    if s.len() >= 2 && s.starts_with('\'') {
        let inner = &s[1..];
        let mut out = String::new();
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                } else {
                    break;
                }
            } else {
                out.push(c);
            }
        }
        return out;
    }
    s.to_owned()
}

fn closing_double_quote(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, c) in s.char_indices().skip(1) {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(i),
            _ => {}
        }
    }
    None
}

/// Splits a flow sequence's inside at top-level commas (not inside quotes).
fn split_flow(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut quote: Option<char> = None;
    let mut start = 0;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(c),
            (None, ',') => {
                out.push(&s[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// `|` keeps the lines (dedented), `>` folds them with spaces; trailing newlines are clipped.
fn block_scalar(more: &str, literal: bool) -> String {
    let ls: Vec<&str> = more.split('\n').collect();
    let indent = ls
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let body: Vec<&str> = ls
        .iter()
        .map(|l| {
            if l.len() >= indent {
                &l[indent..]
            } else {
                l.trim_start()
            }
        })
        .collect();
    let out = if literal {
        body.join("\n")
    } else {
        body.iter()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join(" ")
    };
    out.trim_end().to_owned()
}

// ------------------------------------------------------------------ edits

/// One change to a definition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Edit {
    /// Set the `model` entry (`None` removes it: the agent then runs on the session's model).
    Model(Option<String>),
    /// Replace the system prompt (stored trimmed, like omp reads it).
    Body(String),
}

/// [`EditError::Changed`]'s text: clients that only receive the message (gateway replies) test for
/// it with [`is_changed_error`].
pub const CHANGED_ON_DISK: &str = "the file changed on disk since it was loaded";

/// Whether an edit failed because the file changed since it was read (`message` may carry a
/// prefix added on the way).
pub fn is_changed_error(message: &str) -> bool {
    message.ends_with(CHANGED_ON_DISK)
}

#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("not an agent definition omp reads: {0}")]
    NotAgentFile(PathBuf),
    #[error("not a directory omp reads agents from: {0}")]
    NotAgentsDir(PathBuf),
    #[error("{}", CHANGED_ON_DISK)]
    Changed,
    #[error("the file has no frontmatter to hold a model")]
    NoFrontmatter,
    #[error("a model pattern cannot be empty or span lines")]
    BadModel,
    #[error("{0}")]
    BadName(String),
    #[error("a description is required: the parent model reads it to pick the agent")]
    NoDescription,
    #[error("{} already exists", .0.display())]
    Exists(PathBuf),
    #[error("{0}")]
    Io(#[from] io::Error),
}

/// `content` with `edit` applied (line endings follow the original: CRLF stays CRLF).
pub fn apply(content: &str, edit: &Edit) -> Result<String, EditError> {
    let crlf = content.contains("\r\n");
    let text = normalize(content);
    let layout = Layout::of(&text);
    let out = match edit {
        Edit::Body(body) => replace_body(&text, &layout, body.trim()),
        Edit::Model(model) => {
            let line = match model.as_deref().map(str::trim) {
                Some(m) if m.is_empty() || m.contains('\n') => return Err(EditError::BadModel),
                Some(m) => Some(format!(
                    "model: {}",
                    serde_json::to_string(m).map_err(|_| EditError::BadModel)?
                )),
                None => None,
            };
            let meta = layout.meta.clone().ok_or(EditError::NoFrontmatter)?;
            set_model_line(&text, &layout, meta, line.as_deref())
        }
    };
    Ok(if crlf { out.replace('\n', "\r\n") } else { out })
}

fn replace_body(text: &str, layout: &Layout, body: &str) -> String {
    let r = &layout.body;
    if !r.is_empty() {
        return format!("{}{body}{}", &text[..r.start], &text[r.end..]);
    }
    // No body yet: one blank line after the frontmatter, a newline at the end.
    let head = if layout.meta.is_some() {
        text[..layout.close_end].to_owned()
    } else {
        String::new()
    };
    match (head.is_empty(), body.is_empty()) {
        (_, true) if head.is_empty() => String::new(),
        (_, true) => format!("{head}\n"),
        (true, false) => format!("{body}\n"),
        (false, false) => format!("{head}\n\n{body}\n"),
    }
}

fn set_model_line(text: &str, layout: &Layout, meta: Range<usize>, line: Option<&str>) -> String {
    let entries = top_level_entries(text, meta.clone());
    let splice =
        |r: Range<usize>, with: &str| format!("{}{with}{}", &text[..r.start], &text[r.end..]);
    match (entries.iter().find(|e| e.key == "model"), line) {
        (Some(e), Some(line)) => splice(e.span.clone(), line),
        (Some(e), None) => {
            // Remove the entry with one adjacent newline.
            let r = if e.span.end < meta.end {
                e.span.start..e.span.end + 1
            } else if e.span.start > meta.start {
                e.span.start - 1..e.span.end
            } else {
                e.span.clone()
            };
            splice(r, "")
        }
        (None, None) => text.to_owned(),
        (None, Some(line)) => {
            if meta.is_empty() {
                // `---\n---` / `---\n\n---`: rebuild the block around the one entry.
                return format!("---\n{line}\n---{}", &text[layout.close_end..]);
            }
            let after = entries
                .iter()
                .find(|e| e.key == "description")
                .or(entries.last())
                .map_or(meta.end, |e| e.span.end);
            splice(after..after, &format!("\n{line}"))
        }
    }
}

/// Whether `dir` is a directory omp reads definitions from: `<agent_dir>/agents` or some
/// `.omp/agents` (which need not exist yet). Guards [`create`].
pub fn is_agents_dir(dir: &Path, agent_dir: &Path) -> bool {
    same_dir(dir, &agent_dir.join(AGENTS_DIR))
        || (dir.is_absolute()
            && dir.file_name().is_some_and(|n| n == AGENTS_DIR)
            && dir
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|n| n == PROJECT_CONFIG_DIR))
}

/// Whether `path` is a definition file omp reads: an existing `*.md` directly in a directory
/// [`is_agents_dir`] accepts. Guards [`save`] and [`delete`] against arbitrary paths.
pub fn is_agent_file(path: &Path, agent_dir: &Path) -> bool {
    let named_md = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.ends_with(".md") && n.len() > 3);
    named_md && path.parent().is_some_and(|p| is_agents_dir(p, agent_dir)) && path.is_file()
}

/// Longest agent name [`check_name`] accepts.
pub const NAME_MAX: usize = 64;

/// A name for a new agent (also its file name, `<name>.md`): lowercase letters, digits and
/// inner hyphens, like omp's own agents (`security-reviewer`); not `main` / `sub`.
pub fn check_name(name: &str) -> Result<(), EditError> {
    let bad = |why: &str| Err(EditError::BadName(why.to_owned()));
    if name.is_empty() {
        return bad("a name is required");
    }
    if name.len() > NAME_MAX {
        return bad("names are at most 64 characters");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return bad("use lowercase letters, digits and hyphens (e.g. api-reviewer)");
    }
    if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        return bad("hyphens go between words");
    }
    if RESERVED_NAMES.contains(&name) {
        return bad("main and sub are reserved names");
    }
    Ok(())
}

/// What a new definition says (see [`NewAgent::content`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewAgent {
    pub name: String,
    /// What the parent model reads to choose the agent.
    pub description: String,
    /// `None`: the agent runs on the session's model.
    pub model: Option<String>,
    /// The system prompt.
    pub body: String,
}

impl NewAgent {
    /// The file omp's `/agents` writes for a new agent: `name` and `description` (and `model`)
    /// frontmatter, a blank line, the trimmed prompt. Strings are JSON-quoted (valid YAML).
    pub fn content(&self) -> Result<String, EditError> {
        check_name(&self.name)?;
        let description = self.description.trim();
        if description.is_empty() {
            return Err(EditError::NoDescription);
        }
        let quote = |s: &str| serde_json::to_string(s).map_err(|e| io::Error::other(e.to_string()));
        let mut out = format!(
            "---\nname: {}\ndescription: {}\n",
            self.name,
            quote(description)?
        );
        if let Some(m) = self.model.as_deref().map(str::trim) {
            if m.is_empty() || m.contains('\n') {
                return Err(EditError::BadModel);
            }
            out.push_str(&format!("model: {}\n", quote(m)?));
        }
        out.push_str("---\n");
        let body = self.body.trim();
        if !body.is_empty() {
            out.push_str(&format!("\n{body}\n"));
        }
        Ok(out)
    }
}

/// Writes `<dir>/<name>.md` with `content`, creating `dir` when missing; never replaces a file,
/// and refuses a name another file of `dir` already declares (omp would load only one).
/// Returns the new file as read back.
pub fn create(
    dir: &Path,
    agent_dir: &Path,
    name: &str,
    content: &str,
) -> Result<AgentFile, EditError> {
    let user = same_dir(dir, &agent_dir.join(AGENTS_DIR));
    // A project's `.omp/agents` is created inside an existing project, never with the project.
    let in_project = || {
        dir.parent()
            .and_then(Path::parent)
            .is_some_and(Path::is_dir)
    };
    if !is_agents_dir(dir, agent_dir) || !(user || in_project()) {
        return Err(EditError::NotAgentsDir(dir.to_path_buf()));
    }
    check_name(name)?;
    if let Some(same) = read_dir(Scope::User, dir, dir)
        .agents
        .into_iter()
        .find(|a| a.loaded_name() == Some(name))
    {
        return Err(EditError::Exists(same.path));
    }
    fs::create_dir_all(dir)?;
    let path = dir.join(format!("{name}.md"));
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(EditError::Exists(path)),
        Err(e) => return Err(e.into()),
    };
    file.write_all(content.as_bytes())?;
    file.sync_all()?;
    Ok(parse(&path, content.as_bytes()))
}

/// Deletes the definition at `path` if it still has `revision`.
pub fn delete(path: &Path, agent_dir: &Path, revision_seen: &str) -> Result<(), EditError> {
    if !is_agent_file(path, agent_dir) {
        return Err(EditError::NotAgentFile(path.to_path_buf()));
    }
    if revision(&fs::read(path)?) != revision_seen {
        return Err(EditError::Changed);
    }
    fs::remove_file(path)?;
    Ok(())
}

/// Applies `edit` to the definition at `path` if it still has `revision`, writing atomically;
/// returns the file as now on disk.
pub fn save(
    path: &Path,
    agent_dir: &Path,
    revision_seen: &str,
    edit: &Edit,
) -> Result<AgentFile, EditError> {
    if !is_agent_file(path, agent_dir) {
        return Err(EditError::NotAgentFile(path.to_path_buf()));
    }
    let bytes = fs::read(path)?;
    if revision(&bytes) != revision_seen {
        return Err(EditError::Changed);
    }
    let content = String::from_utf8(bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("not UTF-8 text: {e}")))?;
    let updated = apply(&content, edit)?;
    if updated != content {
        write_atomic(path, updated.as_bytes())?;
    }
    Ok(parse(path, updated.as_bytes()))
}

/// Replaces the file behind `path` (a symlink's target) with `bytes` via a synced temp file in
/// the same directory, keeping the permissions.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = fs::canonicalize(path)?;
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no directory"))?;
    let perms = fs::metadata(&target)?.permissions();
    let mut tmp = tempfile::Builder::new()
        .prefix(".omp-deck-")
        .suffix(".md.tmp")
        .tempfile_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().set_permissions(perms)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&target).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests;
