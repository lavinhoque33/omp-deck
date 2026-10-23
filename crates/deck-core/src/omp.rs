//! Thin wrapper around the `omp` CLI: settings catalog + editing, update checks, session
//! move/copy argv. omp performs every write to its own store/config; OmpDeck never edits
//! `~/.omp/**` itself. Every call here blocks: run them on a worker thread.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum OmpError {
    #[error("failed to run omp: {0}")]
    Spawn(#[from] std::io::Error),
    /// Non-zero exit. `message` = trimmed stderr with a leading "Error: "/"error: " removed
    /// (falls back to stdout when stderr is empty). Never contains a secret value.
    #[error("{message}")]
    Failed { code: Option<i32>, message: String },
    #[error("unexpected omp output: {0}")]
    Parse(String),
}

/// Why [`Omp::delete_session`] did not delete.
#[derive(Debug, thiserror::Error)]
pub enum DeleteError {
    /// A live omp holds the session lease; nothing was removed.
    #[error("session is open in a running omp")]
    Live,
    /// Not an omp session file (wrong name, not a regular file, or no session header).
    #[error("not an omp session file")]
    NotASession,
    /// omp or the delete extension failed; the text is omp's own (may mean a partial delete).
    #[error("{0}")]
    Omp(String),
    #[error("delete failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("omp did not finish deleting within 20 s")]
    Timeout,
}

#[derive(Clone, Debug)]
pub struct Omp {
    program: PathBuf,
    /// Extra environment for every command except the defaults probe (tests / sandboxes).
    env: Vec<(OsString, OsString)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateKind {
    Omp,
    Plugins,
    Canary,
    Stable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateStatus {
    pub current: String,
    /// `None` = up to date.
    pub latest: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MoveOutcome {
    pub new_path: PathBuf,
    pub kind: MoveKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveKind {
    Created,
    Replaced,
    /// The local copy diverged: omp unpacked a fork with a new id and left the original.
    Forked {
        new_id: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsCatalog {
    pub tabs: Vec<SettingsTab>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsTab {
    pub id: String,
    pub label: String,
    pub settings: Vec<Setting>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    pub key: String,
    pub kind: SettingKind,
    /// Effective value. `None` = unset. For a secret that is set this is the placeholder
    /// string `"********"`, never the real value.
    pub value: Option<Value>,
    pub default: Option<Value>,
    pub description: String,
    pub secret: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SettingKind {
    Bool,
    Number,
    Enum(Vec<String>),
    String,
    Array,
    Record,
}

/// Placeholder stored in [`Setting::value`] for a secret that is set.
pub const SECRET_PLACEHOLDER: &str = "********";

/// Credential keys of omp 18.12.3 (`credential: true` / `secret: true`); omp's JSON only reveals
/// redaction once a value is set.
const KNOWN_SECRET_KEYS: &[&str] = &[
    "auth.broker.token",
    "dev.autoqaPush.token",
    "mnemopi.embeddingApiKey",
    "mnemopi.llmApiKey",
    "hindsight.apiToken",
    "searxng.token",
    "searxng.basicPassword",
    "images.urls.credentials",
];

/// omp's `/settings` tab order, then anything unknown, then `internal`.
const TAB_ORDER: &[(&str, &str)] = &[
    ("appearance", "Appearance"),
    ("model", "Model"),
    ("interaction", "Interaction"),
    ("context", "Context"),
    ("memory", "Memory"),
    ("files", "Files"),
    ("shell", "Shell"),
    ("tools", "Tools"),
    ("tasks", "Tasks"),
    ("providers", "Providers"),
];

impl Setting {
    /// Last key segment humanized: `compactThinkingLevel` → "Compact thinking level".
    pub fn label(&self) -> String {
        let seg = self.key.rsplit('.').next().unwrap_or(&self.key);
        humanize(seg)
    }

    /// Key up to the last '.', "" if none (`statusLine.preset` → `statusLine`).
    pub fn section(&self) -> &str {
        self.key.rsplit_once('.').map_or("", |(s, _)| s)
    }

    /// Effective value differs from the default (secret: a value is set).
    pub fn is_modified(&self) -> bool {
        if self.secret {
            return self.value.is_some();
        }
        match (&self.value, &self.default) {
            (None, _) => false,
            (Some(v), Some(d)) => v != d,
            (Some(_), None) => true,
        }
    }
}

fn humanize(seg: &str) -> String {
    let chars: Vec<char> = seg.chars().collect();
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == '_' || c == '-' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            continue;
        }
        let prev = i.checked_sub(1).map(|p| chars[p]);
        let next = chars.get(i + 1).copied();
        let boundary = c.is_uppercase()
            && prev.is_some_and(|p| {
                p.is_lowercase()
                    || p.is_ascii_digit()
                    || (p.is_uppercase() && next.is_some_and(char::is_lowercase))
            });
        if boundary && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let mut out = String::with_capacity(seg.len() + 4);
    for (i, w) in words.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let acronym = w.chars().count() > 1 && w.chars().all(|c| !c.is_lowercase());
        if acronym {
            out.push_str(w);
        } else if i == 0 {
            let mut it = w.chars();
            if let Some(f) = it.next() {
                out.extend(f.to_uppercase());
                out.push_str(&it.as_str().to_lowercase());
            }
        } else {
            out.push_str(&w.to_lowercase());
        }
    }
    out
}

/// argv for a terminal tab (same form as `omp --resume`): `["omp","--fork",<src>,"--cwd",<dest>]`.
pub fn fork_argv(src: &Path, dest_cwd: &Path) -> Vec<String> {
    vec![
        "omp".into(),
        "--fork".into(),
        src.to_string_lossy().into_owned(),
        "--cwd".into(),
        dest_cwd.to_string_lossy().into_owned(),
    ]
}

/// One entry of the `omp models --json` catalog.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    /// `provider/id`, what `/switch` takes.
    pub selector: String,
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    /// Supported effort levels in omp's order; empty = the model has no thinking.
    pub thinking: Vec<String>,
    /// $ per million tokens.
    pub cost_input: Option<f64>,
    pub cost_output: Option<f64>,
}

/// Parses `omp models --json` (`{"models":[{provider,id,selector,name,contextWindow,thinking,
/// cost:{input,output}}…]}`). Entries without provider or id are skipped; a missing selector
/// becomes `provider/id`, a missing name the id; `thinking: null` → empty.
pub fn parse_models(stdout: &str) -> Result<Vec<ModelInfo>, OmpError> {
    let parse_err = |e: serde_json::Error| OmpError::Parse(format!("models: {e}"));
    let start = stdout.find('{').unwrap_or(0);
    let root: Value = serde_json::from_str(&stdout[start..]).map_err(parse_err)?;
    let list = root
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| OmpError::Parse("models: no \"models\" array".into()))?;
    let text = |m: &Value, k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    Ok(list
        .iter()
        .filter_map(|m| {
            let provider = text(m, "provider")?;
            let id = text(m, "id")?;
            let cost = |k: &str| m.get("cost").and_then(|c| c.get(k)).and_then(Value::as_f64);
            Some(ModelInfo {
                selector: text(m, "selector").unwrap_or_else(|| format!("{provider}/{id}")),
                name: text(m, "name").unwrap_or_else(|| id.clone()),
                context_window: m.get("contextWindow").and_then(Value::as_u64),
                thinking: m
                    .get("thinking")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|l| l.as_str().map(str::to_owned))
                            .collect()
                    })
                    .unwrap_or_default(),
                cost_input: cost("input"),
                cost_output: cost("output"),
                provider,
                id,
            })
        })
        .collect())
}

/// argv for an update tab.
pub fn update_argv(kind: UpdateKind) -> Vec<String> {
    let mut v = vec!["omp".to_string(), "update".to_string()];
    match kind {
        UpdateKind::Omp => {}
        UpdateKind::Plugins => v.push("--plugins".into()),
        UpdateKind::Canary => v.push("--canary".into()),
        UpdateKind::Stable => v.push("--stable".into()),
    }
    v
}

/// omp's official installer script.
pub const INSTALL_URL: &str = "https://omp.sh/install";

/// argv for an "Install omp" tab: fetch [`INSTALL_URL`] with curl (else wget) and run it with
/// `--binary`, which installs a prebuilt omp to `~/.local/bin` without bun or root.
/// A failed download fails the whole command instead of running an empty script.
pub fn install_argv() -> Vec<String> {
    let script = format!(
        "set -e\n\
         if command -v curl >/dev/null 2>&1; then s=$(curl -fsSL {INSTALL_URL})\n\
         elif command -v wget >/dev/null 2>&1; then s=$(wget -qO- {INSTALL_URL})\n\
         else echo 'Installing omp needs curl or wget.' >&2; exit 1\n\
         fi\n\
         printf '%s\\n' \"$s\" | sh -s -- --binary\n"
    );
    vec!["sh".into(), "-c".into(), script]
}

fn failed(message: impl Into<String>) -> OmpError {
    OmpError::Failed {
        code: None,
        message: message.into(),
    }
}

/// Value → the single CLI argument: bool "true"/"false", number as JSON number text, enum/string
/// raw text (JSON-quoted when omp would otherwise alter it: leading/trailing whitespace, a leading
/// quote, line breaks), array/record compact JSON.
pub fn encode_value(kind: &SettingKind, value: &Value) -> Result<String, OmpError> {
    match (kind, value) {
        (SettingKind::Bool, Value::Bool(b)) => Ok(b.to_string()),
        (SettingKind::Number, Value::Number(n)) => Ok(n.to_string()),
        (SettingKind::Enum(choices), Value::String(s)) => {
            if !choices.is_empty() && !choices.iter().any(|c| c == s) {
                return Err(failed(format!(
                    "Invalid value: {s}. Valid values: {}",
                    choices.join(", ")
                )));
            }
            Ok(s.clone())
        }
        (SettingKind::String, Value::String(s)) => {
            let needs_quote = s.trim() != s || s.starts_with('"') || s.contains(['\n', '\r']);
            if needs_quote {
                Ok(Value::String(s.clone()).to_string())
            } else {
                Ok(s.clone())
            }
        }
        (SettingKind::Array, Value::Array(_)) | (SettingKind::Record, Value::Object(_)) => {
            Ok(value.to_string())
        }
        (kind, _) => Err(failed(format!(
            "Expected a {} value",
            match kind {
                SettingKind::Bool => "boolean",
                SettingKind::Number => "number",
                SettingKind::Enum(_) | SettingKind::String => "string",
                SettingKind::Array => "JSON array",
                SettingKind::Record => "JSON object",
            }
        ))),
    }
}

fn strip_error_prefix(s: &str) -> &str {
    let s = s.trim();
    s.strip_prefix("Error: ")
        .or_else(|| s.strip_prefix("error: "))
        .unwrap_or(s)
}

/// Builds the error for a non-zero exit.
fn failure(code: Option<i32>, stdout: &str, stderr: &str) -> OmpError {
    let src = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    OmpError::Failed {
        code,
        message: strip_error_prefix(src).to_string(),
    }
}

/// Parses `omp update --check` stdout.
pub fn parse_update_check(stdout: &str) -> Result<UpdateStatus, OmpError> {
    let mut current = None;
    let mut latest = None;
    let mut up_to_date = false;
    for line in stdout.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("Current version:") {
            current = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("New version available:") {
            latest = Some(v.trim().to_string());
        } else if line.ends_with("Already up to date") {
            up_to_date = true;
        }
    }
    match (current, latest, up_to_date) {
        (Some(current), Some(latest), _) if !latest.is_empty() => Ok(UpdateStatus {
            current,
            latest: Some(latest),
        }),
        (Some(current), None, true) => Ok(UpdateStatus {
            current,
            latest: None,
        }),
        _ => Err(OmpError::Parse(stdout.trim().to_string())),
    }
}

/// Parses a successful `omp session unpack`: stdout = new path, stderr = mode line.
pub fn parse_unpack(stdout: &str, stderr: &str) -> Result<MoveOutcome, OmpError> {
    let path = stdout
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .ok_or_else(|| OmpError::Parse("unpack printed no session path".into()))?;
    let mut kind = None;
    for line in stderr.lines().map(str::trim) {
        if line.starts_with("Replaced the local copy of this session") {
            kind = Some(MoveKind::Replaced);
        } else if let Some(rest) = line.strip_prefix("The local copy of this session diverged;") {
            let id = rest
                .rsplit_once('(')
                .and_then(|(_, r)| r.split_once(')'))
                .map(|(id, _)| id.to_string())
                .ok_or_else(|| OmpError::Parse(line.to_string()))?;
            kind = Some(MoveKind::Forked { new_id: id });
        } else if line.starts_with("Unpacked session") {
            kind = Some(MoveKind::Created);
        }
    }
    let kind = kind
        .ok_or_else(|| OmpError::Parse(format!("unpack mode not recognized: {}", stderr.trim())))?;
    Ok(MoveOutcome {
        new_path: PathBuf::from(path),
        kind,
    })
}

struct TextRow<'a> {
    key: &'a str,
    group: &'a str,
}

/// `  key = value (group)` → (key, group); the group is the text after the LAST " (".
fn parse_text_row(line: &str) -> Option<TextRow<'_>> {
    let rest = line.strip_prefix("  ")?;
    let (key, tail) = rest.split_once(" = ")?;
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    let body = tail.strip_suffix(')')?;
    let (_, group) = body.rsplit_once(" (")?;
    Some(TextRow { key, group })
}

fn tab_header(line: &str) -> Option<&str> {
    let id = line.strip_prefix('[')?.strip_suffix(']')?;
    (!id.is_empty() && id.chars().all(|c| c.is_alphanumeric() || c == '_')).then_some(id)
}

fn kind_from(type_name: Option<&str>, group: Option<&str>) -> Result<SettingKind, OmpError> {
    let choices = |g: Option<&str>| match g {
        Some(g) if !matches!(g, "string" | "number" | "boolean" | "array" | "record") => {
            g.split('|').map(str::to_string).collect()
        }
        _ => Vec::new(),
    };
    match type_name.or(group) {
        Some("boolean") => Ok(SettingKind::Bool),
        Some("number") => Ok(SettingKind::Number),
        Some("string") => Ok(SettingKind::String),
        Some("array") => Ok(SettingKind::Array),
        Some("record") => Ok(SettingKind::Record),
        Some("enum") => Ok(SettingKind::Enum(choices(group))),
        Some(_) if type_name.is_none() => Ok(SettingKind::Enum(choices(group))),
        other => Err(OmpError::Parse(format!("unknown setting type {other:?}"))),
    }
}

fn json_map(text: &str, what: &str) -> Result<Map<String, Value>, OmpError> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err(OmpError::Parse(format!("{what}: expected a JSON object"))),
        Err(e) => Err(OmpError::Parse(format!("{what}: {e}"))),
    }
}

fn tab_label(id: &str) -> String {
    if id == "internal" {
        return "Advanced".into();
    }
    if let Some((_, l)) = TAB_ORDER.iter().find(|(i, _)| *i == id) {
        return (*l).into();
    }
    let mut c = id.chars();
    c.next().map_or_else(String::new, |f| {
        f.to_uppercase().chain(c).collect::<String>()
    })
}

/// Merges the human list (tab + enum choices + key order), `list --json` (type/value/description/
/// redacted) and the defaults JSON into a catalog.
pub fn parse_settings(
    list_text: &str,
    list_json: &str,
    defaults_json: &str,
) -> Result<SettingsCatalog, OmpError> {
    let json = json_map(list_json, "config list --json")?;
    let defaults = json_map(defaults_json, "defaults config list --json")?;

    // (tab id, key, group) in text order.
    let mut rows: Vec<(&str, &str, &str)> = Vec::new();
    let mut tab: Option<&str> = None;
    for line in list_text.lines() {
        if let Some(id) = tab_header(line) {
            tab = Some(id);
        } else if let (Some(t), Some(r)) = (tab, parse_text_row(line)) {
            rows.push((t, r.key, r.group));
        }
    }
    if rows.is_empty() {
        return Err(OmpError::Parse("config list printed no settings".into()));
    }

    let mut by_tab: Vec<(String, Vec<Setting>)> = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut push = |tab: &str, s: Setting| {
        match by_tab.iter_mut().find(|(id, _)| id == tab) {
            Some((_, v)) => v.push(s),
            None => by_tab.push((tab.to_string(), vec![s])),
        };
    };

    let build = |key: &str, group: Option<&str>| -> Result<Setting, OmpError> {
        let entry = json.get(key).and_then(Value::as_object);
        let type_name = entry.and_then(|e| e.get("type")).and_then(Value::as_str);
        let kind = kind_from(type_name, group)?;
        let redacted = entry
            .and_then(|e| e.get("redacted"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let secret = redacted || KNOWN_SECRET_KEYS.contains(&key);
        let value = if secret {
            redacted.then(|| Value::String(SECRET_PLACEHOLDER.into()))
        } else {
            entry.and_then(|e| e.get("value")).cloned()
        };
        let default = if secret {
            None
        } else {
            defaults
                .get(key)
                .and_then(Value::as_object)
                .and_then(|e| e.get("value"))
                .cloned()
        };
        let description = entry
            .and_then(|e| e.get("description"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(Setting {
            key: key.to_string(),
            kind,
            value,
            default,
            description,
            secret,
        })
    };

    for &(t, key, group) in &rows {
        if seen.insert(key) {
            push(t, build(key, Some(group))?);
        }
    }
    for key in json.keys() {
        if !seen.contains(key.as_str()) {
            push("internal", build(key, None)?);
        }
    }

    let mut tabs: Vec<SettingsTab> = Vec::with_capacity(by_tab.len());
    let mut take = |id: &str, by_tab: &mut Vec<(String, Vec<Setting>)>| {
        if let Some(pos) = by_tab.iter().position(|(i, _)| i == id) {
            let (id, settings) = by_tab.remove(pos);
            tabs.push(SettingsTab {
                label: tab_label(&id),
                id,
                settings,
            });
        }
    };
    for (id, _) in TAB_ORDER {
        take(id, &mut by_tab);
    }
    let internal = by_tab
        .iter()
        .position(|(i, _)| i == "internal")
        .map(|p| by_tab.remove(p));
    for (id, settings) in by_tab {
        tabs.push(SettingsTab {
            label: tab_label(&id),
            id,
            settings,
        });
    }
    if let Some((id, settings)) = internal {
        tabs.push(SettingsTab {
            label: tab_label(&id),
            id,
            settings,
        });
    }
    Ok(SettingsCatalog { tabs })
}

struct Captured {
    stdout: String,
    stderr: String,
}

impl Omp {
    /// First executable `omp` on PATH, else `~/.local/bin/omp`.
    pub fn detect() -> Option<Omp> {
        let on_path = std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|d| d.join("omp"))
                .find(|p| is_executable(p))
        });
        on_path
            .or_else(|| {
                dirs::home_dir()
                    .map(|h| h.join(".local/bin/omp"))
                    .filter(|p| is_executable(p))
            })
            .map(Omp::at)
    }

    pub fn at(program: impl Into<PathBuf>) -> Omp {
        Omp {
            program: program.into(),
            env: Vec::new(),
        }
    }

    /// Adds an environment variable to every command except the defaults probe (sandboxing in
    /// tests: `PI_CODING_AGENT_DIR`).
    #[must_use]
    pub fn with_env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Omp {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub(crate) fn command(&self, args: &[&std::ffi::OsStr]) -> Command {
        let mut c = Command::new(&self.program);
        c.args(args)
            .current_dir("/")
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.env {
            c.env(k, v);
        }
        c
    }

    /// Runs `omp <args>`; non-zero exit → [`OmpError::Failed`]. `redact` text is masked in the
    /// failure message (secret values).
    fn run(&self, args: &[&str], redact: Option<&str>) -> Result<Captured, OmpError> {
        let os: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
        self.finish(self.command(&os), redact)
    }

    fn finish(&self, mut cmd: Command, redact: Option<&str>) -> Result<Captured, OmpError> {
        let out = cmd.output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if out.status.success() {
            return Ok(Captured { stdout, stderr });
        }
        let mut err = failure(out.status.code(), &stdout, &stderr);
        if let (Some(secret), OmpError::Failed { message, .. }) = (redact, &mut err)
            && !secret.is_empty()
        {
            *message = message.replace(secret, SECRET_PLACEHOLDER);
        }
        Err(err)
    }

    /// `omp --version` → `omp/18.12.3` → "18.12.3".
    pub fn version(&self) -> Result<String, OmpError> {
        let out = self.run(&["--version"], None)?;
        let line = out.stdout.trim();
        Ok(line.strip_prefix("omp/").unwrap_or(line).to_string())
    }

    /// `omp update --check`.
    pub fn check_update(&self) -> Result<UpdateStatus, OmpError> {
        parse_update_check(&self.run(&["update", "--check"], None)?.stdout)
    }

    /// `omp models --json`: the model catalog (can take >1 s; call from a worker).
    pub fn models(&self) -> Result<Vec<ModelInfo>, OmpError> {
        parse_models(&self.run(&["models", "--json"], None)?.stdout)
    }

    /// Merges `config list`, `config list --json` and the defaults (the same command in an empty
    /// sandbox agent dir/cwd with every other `PI_*` variable removed). Runs the three commands
    /// concurrently.
    pub fn load_settings(&self) -> Result<SettingsCatalog, OmpError> {
        let agent = tempfile::tempdir()?;
        let cwd = tempfile::tempdir()?;
        let (text, json, defaults) = std::thread::scope(|s| {
            let text = s.spawn(|| self.run(&["config", "list"], None));
            let json = s.spawn(|| self.run(&["config", "list", "--json"], None));
            let defaults = s.spawn(|| {
                let mut c = Command::new(&self.program);
                c.args(["config", "list", "--json"])
                    .current_dir(cwd.path())
                    .env("NO_COLOR", "1")
                    .env("PI_CODING_AGENT_DIR", agent.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                for (k, _) in std::env::vars_os() {
                    if k.to_string_lossy().starts_with("PI_") && k != "PI_CODING_AGENT_DIR" {
                        c.env_remove(k);
                    }
                }
                self.finish(c, None)
            });
            (text.join(), json.join(), defaults.join())
        });
        let join = |r: std::thread::Result<Result<Captured, OmpError>>| {
            r.unwrap_or_else(|_| Err(failed("settings worker panicked")))
        };
        let (text, json, defaults) = (join(text)?, join(json)?, join(defaults)?);
        parse_settings(&text.stdout, &json.stdout, &defaults.stdout)
    }

    /// `omp config set <key> --json -- <encoded>`. `--json` has to come BEFORE `--` (everything
    /// after `--` is the value); `--` keeps values starting with '-' from being read as flags.
    /// Returns the saved value (secrets: `Value::Null`; their stdout is never returned or logged).
    pub fn set_setting(&self, setting: &Setting, value: &Value) -> Result<Value, OmpError> {
        let encoded = encode_value(&setting.kind, value)?;
        if setting.key.is_empty() || setting.key.starts_with('-') {
            return Err(failed(format!("Invalid setting key: {}", setting.key)));
        }
        if setting.secret {
            log::debug!("omp config set {} (secret, value not logged)", setting.key);
        } else {
            log::debug!("omp config set {} = {encoded}", setting.key);
        }
        let out = self.run(
            &["config", "set", &setting.key, "--json", "--", &encoded],
            setting.secret.then_some(encoded.as_str()),
        )?;
        if setting.secret {
            return Ok(Value::Null);
        }
        match serde_json::from_str::<Value>(out.stdout.trim()) {
            Ok(Value::Object(m)) => Ok(m.get("value").cloned().unwrap_or(Value::Null)),
            _ => Err(OmpError::Parse(out.stdout.trim().to_string())),
        }
    }

    /// `omp config reset <key> --json`.
    pub fn reset_setting(&self, key: &str) -> Result<(), OmpError> {
        if key.is_empty() || key.starts_with('-') {
            return Err(failed(format!("Invalid setting key: {key}")));
        }
        self.run(&["config", "reset", key, "--json"], None)?;
        Ok(())
    }

    /// Forks the session file at `entry_id` into a new file via a headless
    /// `omp --mode rpc --resume <file>` (cwd = the header's cwd, else `$HOME`): sends
    /// `{"type":"fork","entryId"}` then `get_state` for the new `sessionFile`, closes stdin and
    /// reaps the process. omp cuts a tool batch after its last result, like `/fork`; the source
    /// file is untouched. Fails with omp's own text when a live omp holds the session lease. 30 s
    /// timeout. Blocking: run on a worker.
    pub fn fork_at(&self, session_file: &Path, entry_id: &str) -> Result<PathBuf, OmpError> {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::sync::mpsc::{RecvTimeoutError, channel};
        use std::time::{Duration, Instant};

        const TIMEOUT: Duration = Duration::from_secs(30);
        let file = std::path::absolute(session_file)?;
        let cwd = crate::sessions::header_cwd(&file)
            .filter(|d| d.is_dir())
            .or_else(dirs::home_dir)
            .unwrap_or_else(|| PathBuf::from("/"));
        let mut cmd = Command::new(&self.program);
        cmd.args(["--mode", "rpc", "--resume"])
            .arg(&file)
            .current_dir(cwd)
            .env("NO_COLOR", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let mut child = Reaper(cmd.spawn()?);
        let (Some(mut stdin), Some(stdout), Some(mut stderr)) = (
            child.0.stdin.take(),
            child.0.stdout.take(),
            child.0.stderr.take(),
        ) else {
            return Err(failed("omp rpc pipes unavailable"));
        };
        let (tx, rx) = channel::<String>();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        // stderr is only needed for the error text; bound it.
        let err_reader = std::thread::spawn(move || {
            let mut text = String::new();
            let _ = (&mut stderr).take(64 * 1024).read_to_string(&mut text);
            text
        });
        let requests = [
            serde_json::json!({"id": "1", "type": "fork", "entryId": entry_id}),
            serde_json::json!({"id": "2", "type": "get_state"}),
        ];
        for r in &requests {
            writeln!(stdin, "{r}")?;
        }
        stdin.flush()?;

        let deadline = Instant::now() + TIMEOUT;
        let mut forked = false;
        let outcome: Result<PathBuf, OmpError> = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = match rx.recv_timeout(left) {
                Ok(l) => l,
                Err(RecvTimeoutError::Timeout) => break Err(failed("omp fork timed out")),
                Err(RecvTimeoutError::Disconnected) => {
                    // omp exited before answering: its stderr says why.
                    drop(stdin);
                    let status = child.0.wait()?;
                    let err = err_reader.join().unwrap_or_default();
                    return Err(failure(status.code(), "", &err));
                }
            };
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("response") {
                continue;
            }
            let id = v.get("id").and_then(Value::as_str);
            if v.get("success").and_then(Value::as_bool) != Some(true) {
                let msg = v
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("request failed");
                break Err(failed(strip_error_prefix(msg).to_string()));
            }
            match id {
                Some("1") => {
                    if v.pointer("/data/cancelled").and_then(Value::as_bool) == Some(true) {
                        break Err(failed("omp cancelled the fork"));
                    }
                    forked = true;
                }
                Some("2") if forked => {
                    break match v.pointer("/data/sessionFile").and_then(Value::as_str) {
                        Some(f) => Ok(PathBuf::from(f)),
                        None => Err(OmpError::Parse("get_state without sessionFile".into())),
                    };
                }
                _ => {}
            }
        };
        // Closing stdin ends rpc mode; omp flushes and exits. A stuck process is killed.
        drop(stdin);
        let end = Instant::now() + Duration::from_secs(5);
        while child.0.try_wait()?.is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(child);
        let _ = reader.join();
        let _ = err_reader.join();
        outcome
    }

    /// Deletes a closed session the way omp's own picker does: a headless
    /// `omp --mode rpc --no-session --no-extensions -e <ompdeck-del.ts>` (cwd `$HOME`, stdin
    /// null) takes the session lease (held by a live omp → [`DeleteError::Live`], nothing
    /// removed) and runs `FileSessionStorage.deleteSessionWithArtifacts`: the `.jsonl`, its
    /// sibling `<stem>/` directory and stale `.jsonl.<n>.bak` files. Pins, blobs and breadcrumbs
    /// are untouched. The extension is written to a private temp dir per call; a missing result
    /// line is a failure. 20 s timeout, child killed on timeout/drop. Blocking: run on a worker.
    pub fn delete_session(&self, file: &Path) -> Result<(), DeleteError> {
        use std::time::{Duration, Instant};

        const TIMEOUT: Duration = Duration::from_secs(20);
        let file = std::path::absolute(file)?;
        if file.extension().is_none_or(|e| e != "jsonl") || !std::fs::metadata(&file)?.is_file() {
            return Err(DeleteError::NotASession);
        }
        let work = tempfile::Builder::new().prefix("ompdeck-del").tempdir()?;
        let ext = crate::ctl::install_delete_extension(work.path())?;
        let out = work.path().join("result");
        let err_path = work.path().join("stderr");
        let cwd = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let mut cmd = Command::new(&self.program);
        cmd.args(["--mode", "rpc", "--no-session", "--no-extensions", "-e"])
            .arg(&ext)
            .current_dir(cwd)
            .env("NO_COLOR", "1")
            .env("OMPDECK_DEL_TARGET", &file)
            .env("OMPDECK_DEL_OUT", &out)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&err_path)?);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let mut child = Reaper(cmd.spawn()?);
        let deadline = Instant::now() + TIMEOUT;
        while child.0.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                return Err(DeleteError::Timeout);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The exit code is not trusted (omp exits 1 without a default model after the extension
        // already ran); the result line is.
        let result = std::fs::read_to_string(&out).unwrap_or_default();
        let Some(line) = result.lines().find(|l| !l.trim().is_empty()) else {
            let stderr = std::fs::read(&err_path).unwrap_or_default();
            let text = String::from_utf8_lossy(&stderr[..stderr.len().min(64 * 1024)]);
            let text = text.trim();
            return Err(DeleteError::Omp(if text.is_empty() {
                "omp produced no delete result".into()
            } else {
                strip_error_prefix(text).to_string()
            }));
        };
        let v: Value = serde_json::from_str(line)
            .map_err(|e| DeleteError::Omp(format!("unexpected delete result: {e}")))?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(());
        }
        match v.get("error").and_then(Value::as_str) {
            Some("live") => Err(DeleteError::Live),
            Some("no session header") => Err(DeleteError::NotASession),
            Some(m) => Err(DeleteError::Omp(m.to_string())),
            None => Err(DeleteError::Omp("delete failed".into())),
        }
    }

    /// Omp-native move: `omp session pack <src> <tmp.tar>` then `omp session unpack <tmp.tar>
    /// --cwd <dest>`; the tar is always removed. omp deletes the original when it is a prefix of
    /// the archive (`Replaced`). Fails with nothing changed while a live omp writes the session.
    pub fn move_session(&self, src: &Path, dest_cwd: &Path) -> Result<MoveOutcome, OmpError> {
        // Commands run with cwd "/", so relative paths must be made absolute first.
        let src = std::path::absolute(src)?;
        let dest = std::path::absolute(dest_cwd)?;
        let tmp = tempfile::tempdir()?;
        let tar = tmp.path().join("session.tar");
        let pack = self.command(&[
            "session".as_ref(),
            "pack".as_ref(),
            src.as_os_str(),
            tar.as_os_str(),
        ]);
        self.finish(pack, None)?;
        let unpack = self.command(&[
            "session".as_ref(),
            "unpack".as_ref(),
            tar.as_os_str(),
            "--cwd".as_ref(),
            dest.as_os_str(),
        ]);
        let out = self.finish(unpack, None)?;
        parse_unpack(&out.stdout, &out.stderr)
    }

    /// `omp session unpack <tar> --cwd <cwd>` for a hand-built pack-format archive (project
    /// restore). `cwd` must exist. stdin is null and the terminal-identifying env vars are
    /// removed so omp writes no `terminal-sessions` breadcrumb; 120 s timeout, the process group
    /// is killed on timeout. Blocking: run on a worker.
    pub fn unpack_session(&self, tar: &Path, cwd: &Path) -> Result<MoveOutcome, OmpError> {
        use crate::usage::{UsageError, run_bounded};
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
        /// What omp keys its terminal breadcrumb on (`docs/notes/project-sync.md`).
        const TERMINAL_VARS: [&str; 8] = [
            "TMUX",
            "TMUX_PANE",
            "ZELLIJ_PANE_ID",
            "CMUX_SURFACE_ID",
            "KITTY_WINDOW_ID",
            "WEZTERM_PANE",
            "TERM_SESSION_ID",
            "WT_SESSION",
        ];
        let tar = std::path::absolute(tar)?;
        let cwd = std::path::absolute(cwd)?;
        let mut cmd = self.command(&[
            "session".as_ref(),
            "unpack".as_ref(),
            tar.as_os_str(),
            "--cwd".as_ref(),
            cwd.as_os_str(),
        ]);
        for var in TERMINAL_VARS {
            cmd.env_remove(var);
        }
        let (code, stdout, stderr) = run_bounded(cmd, TIMEOUT).map_err(|e| match e {
            UsageError::Spawn(e) => OmpError::Spawn(e),
            _ => OmpError::Failed {
                code: None,
                message: format!(
                    "omp did not finish unpacking within {} s",
                    TIMEOUT.as_secs()
                ),
            },
        })?;
        if code != Some(0) {
            return Err(failure(code, &stdout, &stderr));
        }
        parse_unpack(&stdout, &stderr)
    }
}

/// Kills and reaps the child on every exit path.
struct Reaper(std::process::Child);

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LIST_TXT: &str = include_str!("../tests/fixtures/omp/list.txt");
    const LIST_JSON: &str = include_str!("../tests/fixtures/omp/list.json");
    const DEFAULTS_JSON: &str = include_str!("../tests/fixtures/omp/defaults.json");

    fn catalog() -> SettingsCatalog {
        parse_settings(LIST_TXT, LIST_JSON, DEFAULTS_JSON).unwrap()
    }

    fn find<'a>(c: &'a SettingsCatalog, key: &str) -> (&'a SettingsTab, &'a Setting) {
        c.tabs
            .iter()
            .find_map(|t| t.settings.iter().find(|s| s.key == key).map(|s| (t, s)))
            .unwrap_or_else(|| panic!("missing {key}"))
    }

    #[test]
    fn catalog_counts_match_real_omp_output() {
        let c = catalog();
        let ids: Vec<_> = c.tabs.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "appearance",
                "model",
                "interaction",
                "context",
                "memory",
                "files",
                "shell",
                "tools",
                "tasks",
                "providers",
                "internal"
            ]
        );
        assert_eq!(c.tabs.last().unwrap().label, "Advanced");
        let all: Vec<_> = c.tabs.iter().flat_map(|t| &t.settings).collect();
        assert_eq!(all.len(), 559);
        assert_eq!(
            all.iter()
                .filter(|s| matches!(s.kind, SettingKind::Enum(_)))
                .count(),
            98
        );
        assert_eq!(
            c.tabs
                .iter()
                .find(|t| t.id == "internal")
                .unwrap()
                .settings
                .len(),
            140
        );
    }

    #[test]
    fn values_defaults_and_choices() {
        let c = catalog();
        let (tab, s) = find(&c, "symbolPreset");
        assert_eq!(tab.id, "appearance");
        assert_eq!(
            s.kind,
            SettingKind::Enum(vec!["unicode".into(), "nerd".into(), "ascii".into()])
        );
        assert_eq!(s.value, Some(json!("nerd")));
        assert_eq!(s.default, Some(json!("unicode")));
        assert!(s.is_modified());

        let (_, s) = find(&c, "autocompleteMaxVisible");
        assert_eq!(s.kind, SettingKind::Number);
        assert_eq!(s.value, Some(json!(12)));
        assert!(s.is_modified());

        let (_, s) = find(&c, "compaction.enabled");
        assert_eq!(s.value, Some(json!(false)));
        assert_eq!(s.default, Some(json!(true)));

        let (_, s) = find(&c, "enabledModels");
        assert_eq!(s.kind, SettingKind::Array);
        assert_eq!(s.value, Some(json!(["a/b", "c"])));

        let (_, s) = find(&c, "modelRoles");
        assert_eq!(s.kind, SettingKind::Record);
        assert_eq!(s.value, Some(json!({"default": "x/y"})));

        let (_, s) = find(&c, "theme.light");
        assert!(!s.is_modified());
        assert_eq!(s.section(), "theme");
        assert_eq!(s.label(), "Light");
    }

    #[test]
    fn unset_optional_has_no_value_or_default() {
        let c = catalog();
        let (_, s) = find(&c, "compaction.reserveTokens");
        assert_eq!(s.value, None);
        assert_eq!(s.default, None);
        assert!(!s.is_modified());
    }

    #[test]
    fn secrets_never_carry_a_value() {
        let c = catalog();
        let (_, set) = find(&c, "searxng.token");
        assert!(set.secret);
        assert_eq!(set.value, Some(json!(SECRET_PLACEHOLDER)));
        assert!(set.is_modified());
        let (_, unset) = find(&c, "auth.broker.token");
        assert!(unset.secret, "known credential key without redaction");
        assert_eq!(unset.value, None);
        assert!(!unset.is_modified());
        assert!(!LIST_TXT.contains("fixture-not-a-secret"));
    }

    #[test]
    fn keys_missing_from_text_go_to_internal() {
        let txt = "Settings:\n\n[model]\n  a.b = 1 (number)\n";
        let js = r#"{"a.b":{"value":1,"type":"number","description":"d"},
                     "z":{"value":true,"type":"boolean","description":""}}"#;
        let c = parse_settings(txt, js, "{}").unwrap();
        assert_eq!(c.tabs.len(), 2);
        assert_eq!(c.tabs[1].id, "internal");
        assert_eq!(c.tabs[1].settings[0].key, "z");
        assert_eq!(c.tabs[0].settings[0].description, "d");
    }

    #[test]
    fn row_value_may_contain_parentheses() {
        let txt = "[tools]\n  k.x = see (this) thing (string)\n";
        let js = r#"{"k.x":{"value":"see (this) thing","type":"string","description":""}}"#;
        let c = parse_settings(txt, js, "{}").unwrap();
        assert_eq!(c.tabs[0].settings[0].kind, SettingKind::String);
    }

    #[test]
    fn labels_are_humanized() {
        let mk = |key: &str| Setting {
            key: key.into(),
            kind: SettingKind::Bool,
            value: None,
            default: None,
            description: String::new(),
            secret: false,
        };
        assert_eq!(
            mk("statusLine.compactThinkingLevel").label(),
            "Compact thinking level"
        );
        assert_eq!(mk("simple").label(), "Simple");
        assert_eq!(mk("a.apiURLPath").label(), "Api URL path");
        assert_eq!(mk("noDot").section(), "");
    }

    #[test]
    fn encode_value_boundaries() {
        let s = SettingKind::String;
        assert_eq!(
            encode_value(&s, &json!("plain text")).unwrap(),
            "plain text"
        );
        assert_eq!(encode_value(&s, &json!("  pad ")).unwrap(), "\"  pad \"");
        assert_eq!(encode_value(&s, &json!("\"q\"")).unwrap(), r#""\"q\"""#);
        assert_eq!(encode_value(&s, &json!("a\nb")).unwrap(), "\"a\\nb\"");
        assert_eq!(encode_value(&s, &json!("")).unwrap(), "");
        assert_eq!(encode_value(&s, &json!("-dash")).unwrap(), "-dash");
        let n = SettingKind::Number;
        assert_eq!(encode_value(&n, &json!(-5)).unwrap(), "-5");
        assert_eq!(encode_value(&n, &json!(12.5)).unwrap(), "12.5");
        assert_eq!(
            encode_value(&SettingKind::Bool, &json!(false)).unwrap(),
            "false"
        );
        let nested = json!({"a": [1, {"b": "c d"}], "e": null});
        assert_eq!(
            encode_value(&SettingKind::Record, &nested).unwrap(),
            r#"{"a":[1,{"b":"c d"}],"e":null}"#
        );
        assert_eq!(
            encode_value(&SettingKind::Array, &json!(["x", " y"])).unwrap(),
            r#"["x"," y"]"#
        );
    }

    #[test]
    fn encode_value_rejects_wrong_shapes_and_enum_members() {
        let e = SettingKind::Enum(vec!["a".into(), "b".into()]);
        assert_eq!(encode_value(&e, &json!("a")).unwrap(), "a");
        let err = encode_value(&e, &json!("c")).unwrap_err().to_string();
        assert_eq!(err, "Invalid value: c. Valid values: a, b");
        assert!(encode_value(&SettingKind::Number, &json!("5")).is_err());
        assert!(encode_value(&SettingKind::Bool, &json!("true")).is_err());
        assert!(encode_value(&SettingKind::Array, &json!({"a": 1})).is_err());
        assert!(encode_value(&SettingKind::Record, &json!([1])).is_err());
    }

    #[test]
    fn update_check_variants() {
        assert_eq!(
            parse_update_check("Current version: 18.12.3\nNew version available: 18.12.5\n")
                .unwrap(),
            UpdateStatus {
                current: "18.12.3".into(),
                latest: Some("18.12.5".into())
            }
        );
        assert_eq!(
            parse_update_check("Current version: 18.12.3\n✔ Already up to date\n").unwrap(),
            UpdateStatus {
                current: "18.12.3".into(),
                latest: None
            }
        );
        assert_eq!(
            parse_update_check(
                "Current version: 18.12.3\nCurrent channel: canary\nNew version available: 18.12.4-canary.2\n"
            )
            .unwrap()
            .latest
            .as_deref(),
            Some("18.12.4-canary.2")
        );
        assert!(matches!(
            parse_update_check(
                "/x/omp is installed and kept up to date by brew; update it from brew.\n"
            ),
            Err(OmpError::Parse(_))
        ));
        assert!(parse_update_check("").is_err());
    }

    #[test]
    fn unpack_modes() {
        let p = "/home/u/.omp/agent/sessions/-x/2026_abc.jsonl\n";
        let o = parse_unpack(p, "Unpacked session (abc).\n").unwrap();
        assert_eq!(o.kind, MoveKind::Created);
        assert_eq!(o.new_path, PathBuf::from(p.trim()));
        assert_eq!(
            parse_unpack(p, "Replaced the local copy of this session (abc).\n")
                .unwrap()
                .kind,
            MoveKind::Replaced
        );
        assert_eq!(
            parse_unpack(
                p,
                "The local copy of this session diverged; unpacked as a fork (def456).\n"
            )
            .unwrap()
            .kind,
            MoveKind::Forked {
                new_id: "def456".into()
            }
        );
        assert!(matches!(
            parse_unpack("", "Unpacked session (a).\n"),
            Err(OmpError::Parse(_))
        ));
        assert!(matches!(
            parse_unpack(p, "something else\n"),
            Err(OmpError::Parse(_))
        ));
    }

    #[test]
    fn failure_messages_strip_prefix_and_fall_back_to_stdout() {
        let e = failure(Some(1), "", "Error: Invalid number: abc\n");
        assert!(
            matches!(&e, OmpError::Failed { code: Some(1), message } if message == "Invalid number: abc")
        );
        let e = failure(Some(1), "", "error: Session \"x\" not found.\n");
        assert_eq!(e.to_string(), "Session \"x\" not found.");
        let e = failure(Some(1), "Current version: 1\n", "");
        assert_eq!(e.to_string(), "Current version: 1");
    }

    #[test]
    fn argv_forms() {
        assert_eq!(
            fork_argv(Path::new("/s/a.jsonl"), Path::new("/p/d")),
            ["omp", "--fork", "/s/a.jsonl", "--cwd", "/p/d"]
        );
        assert_eq!(update_argv(UpdateKind::Omp), ["omp", "update"]);
        assert_eq!(
            update_argv(UpdateKind::Plugins),
            ["omp", "update", "--plugins"]
        );
        assert_eq!(
            update_argv(UpdateKind::Canary),
            ["omp", "update", "--canary"]
        );
        assert_eq!(
            update_argv(UpdateKind::Stable),
            ["omp", "update", "--stable"]
        );
    }

    /// Runs `install_argv()` with a fake `curl` (body `curl_body`) first on PATH.
    fn run_installer(curl_body: &str) -> (bool, Option<String>) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let curl = dir.path().join("curl");
        std::fs::write(&curl, format!("#!/bin/sh\n{curl_body}\n")).unwrap();
        std::fs::set_permissions(&curl, std::fs::Permissions::from_mode(0o755)).unwrap();
        let out = dir.path().join("ran");
        let argv = install_argv();
        let ok = Command::new(&argv[0])
            .args(&argv[1..])
            .env("PATH", format!("{}:/usr/bin:/bin", dir.path().display()))
            .env("OUT", &out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        (ok, std::fs::read_to_string(&out).ok())
    }

    #[test]
    fn installer_runs_the_downloaded_script_with_binary_flag() {
        let (ok, ran) = run_installer(r#"echo 'echo "args:$*" > "$OUT"'"#);
        assert!(ok);
        assert_eq!(ran.as_deref(), Some("args:--binary\n"));
    }

    #[test]
    fn installer_fails_without_running_anything_when_the_download_fails() {
        let (ok, ran) = run_installer("echo 'echo partial > \"$OUT\"'; exit 22");
        assert!(!ok);
        assert_eq!(ran, None);
    }
}

#[cfg(test)]
mod models_tests {
    use super::*;

    const MODELS: &str = include_str!("../tests/fixtures/status/models.json");

    #[test]
    fn catalog_fixture_maps_thinking_window_and_cost() {
        let m = parse_models(MODELS).unwrap();
        assert_eq!(m.len(), 4);
        let fable = m
            .iter()
            .find(|m| m.selector == "anthropic/claude-fable-5")
            .unwrap();
        assert_eq!(fable.provider, "anthropic");
        assert_eq!(fable.id, "claude-fable-5");
        assert_eq!(fable.name, "Claude Fable 5");
        assert_eq!(fable.context_window, Some(1_000_000));
        assert_eq!(fable.thinking, ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(
            (fable.cost_input, fable.cost_output),
            (Some(10.0), Some(50.0))
        );
        // `thinking: null` = no thinking support.
        let old = m
            .iter()
            .find(|m| m.selector == "anthropic/claude-3-5-sonnet-20240620")
            .unwrap();
        assert!(old.thinking.is_empty());
        assert_eq!(old.context_window, Some(200_000));
    }

    #[test]
    fn sparse_entries_and_noise() {
        let out = r#"warning: something
{"models":[{"provider":"p","id":"i"},{"provider":"","id":"x"},{"id":"y"},{"provider":"q","id":"j","selector":"q/custom","name":"J","thinking":["low"]}]}"#;
        let m = parse_models(out).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].selector, "p/i");
        assert_eq!(m[0].name, "i");
        assert_eq!(m[0].context_window, None);
        assert_eq!(m[0].cost_input, None);
        assert_eq!(m[1].selector, "q/custom");
        assert_eq!(m[1].thinking, ["low"]);
        assert!(matches!(parse_models("[]"), Err(OmpError::Parse(_))));
        assert!(matches!(parse_models("nope"), Err(OmpError::Parse(_))));
    }
}
