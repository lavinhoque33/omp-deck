//! Native model of omp's `/usage` dashboard: the `omp usage --json` snapshot, the provider cards
//! (a port of omp 18.12.6 `buildProviderCards`), the activity numbers from omp's `stats.db`
//! (opened strictly read-only) and the contribution heatmap. Everything that touches omp or
//! SQLite blocks: run it on a worker thread. Facts: `docs/notes/omp-usage-context.md`.

use std::cmp::Ordering;
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags};
use serde::de::{DeserializeOwned, IgnoredAny};
use serde::{Deserialize, Deserializer};

use crate::omp::Omp;
use crate::platform;

/// Default wall-clock bound for [`fetch_usage`] (providers answer in up to 10 s each).
pub const DEFAULT_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest stdout/stderr we keep from `omp usage`.
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
/// Windows shown per provider before the rest fold into "N more" (omp's `B`).
const TOP_WINDOWS: usize = 4;
/// A window under this used fraction counts as untouched (omp's `fe`).
const IDLE_FRACTION: f64 = 0.005;
const MS_DAY: i64 = 86_400_000;
/// omp's activity window: the last 371 days (53 weeks).
pub const ACTIVITY_DAYS: u32 = 371;

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("could not run omp: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("omp usage timed out after {0:?}")]
    Timeout(Duration),
    #[error("omp usage failed ({code}): {stderr_tail}", code = code.map_or("signal".to_string(), |c| format!("exit {c}")))]
    Exit {
        code: Option<i32>,
        stderr_tail: String,
    },
    #[error("unexpected omp usage output: {0}")]
    Parse(String),
    #[error("stats database: {0}")]
    Db(String),
}

// ---- serde mirror of `omp usage --json` ---------------------------------------------------------

/// A field that fails to parse (or is `null`) takes its default instead of failing the snapshot.
fn tolerant<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(T::deserialize(v).unwrap_or_default())
}

/// Elements that fail to parse are skipped; `null` is an empty list.
fn lenient_vec<'de, D, T>(d: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Item<T> {
        Ok(T),
        Bad(IgnoredAny),
    }
    let items: Option<Vec<Item<T>>> = Option::deserialize(d)?;
    Ok(items
        .unwrap_or_default()
        .into_iter()
        .filter_map(|i| match i {
            Item::Ok(t) => Some(t),
            Item::Bad(_) => None,
        })
        .collect())
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    #[serde(default, rename = "generatedAt", deserialize_with = "tolerant")]
    pub generated_at_ms: i64,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub reports: Vec<UsageReport>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub accounts_without_usage: Vec<UnavailableAccount>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub disabled_credentials: Vec<UnavailableAccount>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    #[serde(default, deserialize_with = "tolerant")]
    pub provider: String,
    #[serde(default, rename = "fetchedAt", deserialize_with = "tolerant")]
    pub fetched_at_ms: i64,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub limits: Vec<UsageLimit>,
    #[serde(default, deserialize_with = "tolerant")]
    pub reset_credits: Option<ResetCredits>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub notes: Vec<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub metadata: UsageMetadata,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default, deserialize_with = "tolerant")]
    pub email: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub account_id: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub org_name: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub plan_type: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub tier: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub project_id: Option<String>,
    /// omp's `metadata.daybreak` flag (Codex accounts on the daybreak program).
    #[serde(default, deserialize_with = "tolerant")]
    pub daybreak: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageLimit {
    #[serde(default, deserialize_with = "tolerant")]
    pub id: String,
    #[serde(default, deserialize_with = "tolerant")]
    pub label: String,
    #[serde(default, deserialize_with = "tolerant")]
    pub scope: UsageScope,
    #[serde(default, deserialize_with = "tolerant")]
    pub window: Option<UsageWindow>,
    #[serde(default, deserialize_with = "tolerant")]
    pub amount: UsageAmount,
    #[serde(default, deserialize_with = "tolerant")]
    pub status: LimitStatus,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageScope {
    #[serde(default, deserialize_with = "tolerant")]
    pub account_id: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub model_id: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub tier: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub window_id: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub shared: bool,
    /// Limits of one report sharing a group are one quota (omp keeps the first).
    #[serde(default, deserialize_with = "tolerant")]
    pub shared_group: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    #[serde(default, deserialize_with = "tolerant")]
    pub id: String,
    #[serde(default, deserialize_with = "tolerant")]
    pub label: String,
    #[serde(default, deserialize_with = "tolerant")]
    pub duration_ms: Option<i64>,
    #[serde(default, rename = "resetsAt", deserialize_with = "tolerant")]
    pub resets_at_ms: Option<i64>,
    #[serde(default, deserialize_with = "tolerant")]
    pub reset_label: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageAmount {
    #[serde(default, deserialize_with = "tolerant")]
    pub used: Option<f64>,
    #[serde(default, deserialize_with = "tolerant")]
    pub limit: Option<f64>,
    #[serde(default, deserialize_with = "tolerant")]
    pub remaining: Option<f64>,
    #[serde(default, deserialize_with = "tolerant")]
    pub used_fraction: Option<f64>,
    #[serde(default, deserialize_with = "tolerant")]
    pub remaining_fraction: Option<f64>,
    /// `percent`, `tokens`, `requests`, `credits`, `usd`, `minutes`, `bytes` or `unknown`.
    #[serde(default = "unknown_unit", deserialize_with = "tolerant_unit")]
    pub unit: String,
}

fn unknown_unit() -> String {
    "unknown".to_string()
}

fn tolerant_unit<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    let s: String = tolerant(d)?;
    Ok(if s.is_empty() { unknown_unit() } else { s })
}

impl Default for UsageAmount {
    fn default() -> Self {
        UsageAmount {
            used: None,
            limit: None,
            remaining: None,
            used_fraction: None,
            remaining_fraction: None,
            unit: unknown_unit(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitStatus {
    Ok,
    Warning,
    Exhausted,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCredits {
    #[serde(default, deserialize_with = "tolerant")]
    pub available_count: i64,
    #[serde(default, deserialize_with = "tolerant")]
    pub redeemable_count: Option<i64>,
    #[serde(default, deserialize_with = "tolerant")]
    pub next_credit_id: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub eligible: Option<bool>,
    #[serde(default, deserialize_with = "tolerant")]
    pub reason: Option<String>,
    /// ISO-8601 timestamp.
    #[serde(default, deserialize_with = "tolerant")]
    pub cooldown_until: Option<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub credits: Vec<ResetCredit>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetCredit {
    #[serde(default, deserialize_with = "tolerant")]
    pub id: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub title: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub remaining_count: Option<i64>,
    #[serde(default, deserialize_with = "tolerant")]
    pub usable: Option<bool>,
    /// ISO-8601 timestamp.
    #[serde(default, deserialize_with = "tolerant")]
    pub expires_at: Option<String>,
    #[serde(default, deserialize_with = "tolerant")]
    pub status: Option<String>,
    #[serde(default, deserialize_with = "lenient_vec")]
    pub blocking: Vec<String>,
}

/// A credential that produced no usage report (`accountsWithoutUsage`, `disabledCredentials`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct UnavailableAccount {
    #[serde(default, deserialize_with = "tolerant")]
    pub provider: String,
    #[serde(default, deserialize_with = "tolerant")]
    pub label: Option<String>,
    /// Identity omp attaches to credential entries (`accountsWithoutUsage`, `disabledCredentials`).
    #[serde(default, deserialize_with = "tolerant")]
    pub email: Option<String>,
    #[serde(default, rename = "accountId", deserialize_with = "tolerant")]
    pub account_id: Option<String>,
    #[serde(
        default,
        alias = "cause",
        alias = "error",
        deserialize_with = "tolerant"
    )]
    pub reason: Option<String>,
}

impl UnavailableAccount {
    /// Display label: `label`, else `email`, else `accountId`, else `"account"`.
    pub fn display_label(&self) -> &str {
        [&self.label, &self.email, &self.account_id]
            .into_iter()
            .filter_map(|o| o.as_deref())
            .find(|s| !s.is_empty())
            .unwrap_or("account")
    }
}

impl UsageSnapshot {
    pub fn parse(json: &str) -> Result<UsageSnapshot, UsageError> {
        serde_json::from_str(json).map_err(|e| UsageError::Parse(e.to_string()))
    }
}

// ---- fetching -----------------------------------------------------------------------------------

/// Reads `r` (≤ [`MAX_OUTPUT`]) on a thread; the bytes arrive on the returned channel.
fn spawn_reader<R: Read + Send + 'static>(r: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(r) = r {
            let _ = r.take(MAX_OUTPUT).read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

/// Runs `cmd` as a killable process tree (see [`platform::KillableCommand`]) to completion within
/// `timeout`. On timeout, error, or after the child exits, the whole tree is killed; the deadline
/// also bounds waiting for the output readers (they only block past it if a descendant escaped).
pub(crate) fn run_bounded(
    cmd: Command,
    timeout: Duration,
) -> Result<(Option<i32>, String, String), UsageError> {
    let mut child = platform::spawn_killable(cmd).map_err(UsageError::Spawn)?;
    let out = spawn_reader(child.stdout.take());
    let err = spawn_reader(child.stderr.take());
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait().map_err(UsageError::Spawn)? {
            Some(s) => break s,
            None if Instant::now() >= deadline => return Err(UsageError::Timeout(timeout)),
            None => std::thread::sleep(Duration::from_millis(15)),
        }
    };
    // Descendants left behind (`cmd & exit`) hold the pipes open: close them by killing the tree.
    child.kill_tree();
    let collect = |rx: &mpsc::Receiver<Vec<u8>>| {
        let left = deadline.saturating_duration_since(Instant::now());
        rx.recv_timeout(left.max(Duration::from_millis(250)))
            .map_err(|_| UsageError::Timeout(timeout))
    };
    let stdout = collect(&out)?;
    let stderr = collect(&err)?;
    Ok((
        status.code(),
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    ))
}

/// Last few non-empty lines (≤ 600 chars) of `stderr`, falling back to `stdout`.
fn tail(stderr: &str, stdout: &str) -> String {
    let text = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let joined = lines[lines.len().saturating_sub(6)..].join("\n");
    if joined.chars().count() <= 600 {
        return joined;
    }
    let skip = joined.chars().count() - 600;
    joined.chars().skip(skip).collect()
}

fn run_omp(omp: &Omp, args: &[&str], timeout: Duration) -> Result<String, UsageError> {
    let os: Vec<&std::ffi::OsStr> = args.iter().map(std::ffi::OsStr::new).collect();
    let (code, stdout, stderr) = run_bounded(omp.command(&os), timeout)?;
    if code == Some(0) {
        Ok(stdout)
    } else {
        Err(UsageError::Exit {
            code,
            stderr_tail: tail(&stderr, &stdout),
        })
    }
}

/// `omp usage --json --no-extensions`, preceded by `omp usage invalidate` when `invalidate` (drops
/// omp's 5-minute report cache). Never runs `usage reset`. The same `timeout` bounds each command.
pub fn fetch_usage(
    omp: &Omp,
    invalidate: bool,
    timeout: Duration,
) -> Result<UsageSnapshot, UsageError> {
    if invalidate {
        run_omp(omp, &["usage", "invalidate"], timeout)?;
    }
    let stdout = run_omp(omp, &["usage", "--json", "--no-extensions"], timeout)?;
    UsageSnapshot::parse(&stdout)
}

// ---- provider cards -----------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct CardWindow {
    /// omp's limit label (plus the tier when the label does not name it).
    pub label: String,
    /// `5h`, `7d`, `mo`… only when another window of the card has the same label.
    pub window_tag: Option<String>,
    /// Used fraction, mean across accounts; `None` when no limit reports one.
    pub fraction: Option<f64>,
    pub status: LimitStatus,
    /// Time until the fullest account's window resets.
    pub resets_in_ms: Option<i64>,
    /// Fallback text when there is no fraction (`12.3 usd used`, `5 credits left`).
    pub used_text: Option<String>,
}

impl CardWindow {
    /// Remaining share 0..=1 (the bar), if a fraction is known.
    pub fn remaining(&self) -> Option<f64> {
        self.fraction.map(|f| (1.0 - f).clamp(0.0, 1.0))
    }
}

/// Banked "reset" credits of a provider (Codex, Claude).
#[derive(Clone, Debug, PartialEq)]
pub struct ResetSummary {
    pub banked: i64,
    pub redeemable: i64,
    /// Soonest expiry relative to `now_ms` (negative when already expired).
    pub soonest_expiry_in_ms: Option<i64>,
    pub unavailable_reasons: Vec<String>,
}

impl ResetSummary {
    /// `3 resets banked · 2 usable · expires in 3d4h · not usable right now`.
    pub fn text(&self) -> String {
        let mut s = format!(
            "{} reset{} banked",
            self.banked,
            if self.banked == 1 { "" } else { "s" }
        );
        s.push_str(&format!(" · {} usable", self.redeemable));
        match self.soonest_expiry_in_ms {
            Some(ms) if ms > 0 => s.push_str(&format!(" · expires in {}", format_remaining(ms))),
            Some(_) => s.push_str(" · expired"),
            None => {}
        }
        for r in &self.unavailable_reasons {
            s.push_str(" · ");
            s.push_str(r);
        }
        s
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderCard {
    /// omp's provider id (`anthropic`, `openai-codex`).
    pub provider: String,
    /// Display name (`Anthropic`, `Codex`).
    pub name: String,
    /// Reports + accounts without usage; disabled credentials are listed but not counted (as in omp's TUI).
    pub accounts: usize,
    /// Labels of credentials with no report (`user@example.com`, or `account`).
    pub unavailable_accounts: Vec<String>,
    pub windows: Vec<CardWindow>,
    /// No windows and no unavailable accounts.
    pub unlimited: bool,
    pub notes: Vec<String>,
    /// Every window under 0.5 % used, nothing banked, no notes or unavailable accounts.
    pub idle: bool,
    pub reset_credits: Option<ResetSummary>,
    pub daybreak_accounts: Vec<String>,
}

impl ProviderCard {
    /// Worst window status (`Exhausted` > `Warning` > `Ok`); `Unknown` without any known window.
    pub fn status(&self) -> LimitStatus {
        self.windows
            .iter()
            .map(|w| w.status)
            .max_by_key(|s| status_rank(*s))
            .unwrap_or(LimitStatus::Unknown)
    }

    /// Highest used fraction over the windows.
    pub fn max_fraction(&self) -> Option<f64> {
        self.windows
            .iter()
            .filter_map(|w| w.fraction)
            .reduce(f64::max)
    }
}

fn status_rank(s: LimitStatus) -> u8 {
    match s {
        LimitStatus::Exhausted => 3,
        LimitStatus::Warning => 2,
        LimitStatus::Ok => 1,
        LimitStatus::Unknown => 0,
    }
}

/// omp's `Cr`: used fraction of one limit.
pub fn limit_fraction(l: &UsageLimit) -> Option<f64> {
    let a = &l.amount;
    if let Some(f) = a.used_fraction {
        return Some(f);
    }
    if let (Some(u), Some(lim)) = (a.used, a.limit)
        && lim > 0.0
    {
        return Some(u / lim);
    }
    if a.unit == "percent"
        && let Some(u) = a.used
    {
        return Some(u / 100.0);
    }
    a.remaining_fraction.map(|r| (1.0 - r).max(0.0))
}

/// Explicit status, else derived from the fraction (≥ 1 exhausted, ≥ 0.9 warning, else ok).
fn effective_status(l: &UsageLimit) -> LimitStatus {
    if l.status != LimitStatus::Unknown {
        return l.status;
    }
    match limit_fraction(l) {
        Some(f) if f >= 1.0 => LimitStatus::Exhausted,
        Some(f) if f >= 0.9 => LimitStatus::Warning,
        Some(_) => LimitStatus::Ok,
        None => LimitStatus::Unknown,
    }
}

/// omp's `V`: any `ok` wins unless something is also under pressure.
fn combine_status(statuses: &[LimitStatus]) -> LimitStatus {
    let has = |s| statuses.contains(&s);
    if has(LimitStatus::Ok) {
        return if has(LimitStatus::Warning) || has(LimitStatus::Exhausted) {
            LimitStatus::Warning
        } else {
            LimitStatus::Ok
        };
    }
    if has(LimitStatus::Warning) {
        LimitStatus::Warning
    } else if has(LimitStatus::Exhausted) {
        LimitStatus::Exhausted
    } else {
        LimitStatus::Unknown
    }
}

/// `Anthropic`, `Codex`, `Copilot`…; anything else: omp's `Title Case` of the id.
pub fn provider_display_name(id: &str) -> String {
    match id {
        "anthropic" => return "Anthropic".into(),
        "openai-codex" => return "Codex".into(),
        "github-copilot" => return "Copilot".into(),
        "google-antigravity" => return "Antigravity".into(),
        "gemini" | "google-gemini-cli" => return "Gemini".into(),
        "zai" => return "Z.ai".into(),
        "xai-oauth" => return "xAI".into(),
        _ => {}
    }
    id.split(['-', '_'])
        .map(|seg| {
            let mut c = seg.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn limit_title(l: &UsageLimit) -> String {
    match &l.scope.tier {
        Some(t) if !l.label.to_lowercase().contains(&t.to_lowercase()) => {
            format!("{} ({t})", l.label)
        }
        _ => l.label.clone(),
    }
}

fn window_tag(w: &UsageWindow) -> String {
    if let Some(ms) = w.duration_ms.filter(|d| *d != 0) {
        let hours = ms as f64 / 3_600_000.0;
        if hours >= 28.0 * 24.0 {
            return "mo".into();
        }
        if hours >= 24.0 {
            return format!("{}d", (hours / 24.0).round() as i64);
        }
        return format!("{}h", hours.round() as i64);
    }
    let id = w.id.to_lowercase();
    if id.chars().count() <= 3 {
        id
    } else {
        id.chars().take(1).collect()
    }
}

/// `12.3 usd used` / `5 credits left` for windows without a fraction (omp's `C0e`, simplified).
fn used_text(limits: &[&UsageLimit]) -> Option<String> {
    let first = limits.first()?;
    let unit = &first.amount.unit;
    if !limits.iter().all(|l| &l.amount.unit == unit) {
        return None;
    }
    let num = |v: f64| {
        if v.fract().abs() < 1e-9 {
            format!("{v:.0}")
        } else {
            format!("{v:.2}")
        }
    };
    let used = limits.iter().filter_map(|l| l.amount.used).reduce(f64::max);
    if let Some(u) = used {
        return Some(format!("{} {unit} used", num(u)));
    }
    let left = limits
        .iter()
        .filter_map(|l| l.amount.remaining)
        .reduce(f64::max)?;
    Some(format!("{} {unit} left", num(left)))
}

struct Group<'a> {
    label: String,
    limits: Vec<&'a UsageLimit>,
}

/// Port of omp's `buildProviderCards(reports, now, unavailableAccounts)`.
pub fn provider_cards(snap: &UsageSnapshot, now_ms: i64) -> Vec<ProviderCard> {
    let unavailable: Vec<&UnavailableAccount> = snap
        .accounts_without_usage
        .iter()
        .chain(&snap.disabled_credentials)
        .collect();
    let mut providers: Vec<(&str, Vec<&UsageReport>)> = Vec::new();
    for r in snap.reports.iter().filter(|r| !r.provider.is_empty()) {
        match providers.iter_mut().find(|(p, _)| *p == r.provider) {
            Some((_, v)) => v.push(r),
            None => providers.push((&r.provider, vec![r])),
        }
    }
    for u in &unavailable {
        if !u.provider.is_empty() && !providers.iter().any(|(p, _)| *p == u.provider) {
            providers.push((&u.provider, Vec::new()));
        }
    }

    let mut cards: Vec<ProviderCard> = providers
        .into_iter()
        .map(|(provider, reports)| {
            let without_usage = snap
                .accounts_without_usage
                .iter()
                .filter(|u| u.provider == provider)
                .count();
            build_card(provider, &reports, &unavailable, without_usage, now_ms)
        })
        .collect();
    cards.sort_by(|a, b| {
        a.idle.cmp(&b.idle).then_with(|| {
            let fa = a.max_fraction().unwrap_or(-1.0);
            let fb = b.max_fraction().unwrap_or(-1.0);
            fb.partial_cmp(&fa)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        })
    });
    cards
}

fn build_card(
    provider: &str,
    reports: &[&UsageReport],
    unavailable: &[&UnavailableAccount],
    without_usage: usize,
    now_ms: i64,
) -> ProviderCard {
    let unavailable_accounts: Vec<String> = unavailable
        .iter()
        .filter(|u| u.provider == provider)
        .map(|u| u.display_label().to_string())
        .collect();

    // Group by `label|window id`, in first-seen order; a report's shared groups count once.
    let mut groups: Vec<(String, Group)> = Vec::new();
    for r in reports {
        let mut seen_groups: Vec<&str> = Vec::new();
        for l in &r.limits {
            if let Some(g) = l.scope.shared_group.as_deref() {
                if seen_groups.contains(&g) {
                    continue;
                }
                seen_groups.push(g);
            }
            let label = limit_title(l);
            let win = l
                .window
                .as_ref()
                .map(|w| w.id.as_str())
                .filter(|s| !s.is_empty())
                .or(l.scope.window_id.as_deref())
                .unwrap_or("default");
            let key = format!("{label}|{win}");
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, g)) => g.limits.push(l),
                None => groups.push((
                    key,
                    Group {
                        label,
                        limits: vec![l],
                    },
                )),
            }
        }
    }

    let windows: Vec<CardWindow> = groups
        .into_iter()
        .map(|(_, g)| {
            let fracs: Vec<f64> = g.limits.iter().filter_map(|l| limit_fraction(l)).collect();
            let fraction =
                (!fracs.is_empty()).then(|| fracs.iter().sum::<f64>() / fracs.len() as f64);
            let fullest = g.limits.iter().copied().reduce(|best, cur| {
                if limit_fraction(cur).unwrap_or(-1.0) > limit_fraction(best).unwrap_or(-1.0) {
                    cur
                } else {
                    best
                }
            });
            let resets_in_ms = fullest
                .and_then(|l| l.window.as_ref())
                .and_then(|w| w.resets_at_ms)
                .filter(|t| *t > now_ms)
                .map(|t| t - now_ms);
            let statuses: Vec<LimitStatus> = g.limits.iter().map(|l| effective_status(l)).collect();
            CardWindow {
                window_tag: fullest.and_then(|l| l.window.as_ref()).map(window_tag),
                fraction,
                status: combine_status(&statuses),
                resets_in_ms,
                used_text: if fraction.is_none() {
                    used_text(&g.limits)
                } else {
                    None
                },
                label: g.label,
            }
        })
        .collect();

    // The `TOP_WINDOWS` fullest windows first (original order), the rest after.
    let mut by_fraction: Vec<usize> = (0..windows.len()).collect();
    by_fraction.sort_by(|&a, &b| {
        let fa = windows[a].fraction.unwrap_or(-1.0);
        let fb = windows[b].fraction.unwrap_or(-1.0);
        fb.partial_cmp(&fa).unwrap_or(Ordering::Equal)
    });
    let top: Vec<usize> = by_fraction.into_iter().take(TOP_WINDOWS).collect();
    let order: Vec<usize> = (0..windows.len())
        .filter(|i| top.contains(i))
        .chain((0..windows.len()).filter(|i| !top.contains(i)))
        .collect();
    let mut windows: Vec<CardWindow> = {
        let mut slots: Vec<Option<CardWindow>> = windows.into_iter().map(Some).collect();
        order.into_iter().filter_map(|i| slots[i].take()).collect()
    };
    let labels: Vec<String> = windows.iter().map(|w| w.label.clone()).collect();
    for (i, w) in windows.iter_mut().enumerate() {
        let collides = labels
            .iter()
            .enumerate()
            .any(|(j, l)| j != i && *l == w.label);
        if !collides {
            w.window_tag = None;
        }
    }

    let reset_credits = summarize_resets(reports, now_ms);
    let daybreak_accounts: Vec<String> = reports
        .iter()
        .enumerate()
        .filter(|(_, r)| r.metadata.daybreak)
        .map(|(i, r)| {
            r.metadata
                .email
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| r.metadata.account_id.clone().filter(|s| !s.is_empty()))
                .unwrap_or_else(|| format!("account {}", i + 1))
        })
        .collect();
    let mut notes: Vec<String> = Vec::new();
    for n in reports.iter().flat_map(|r| &r.notes) {
        if !notes.contains(n) {
            notes.push(n.clone());
        }
    }
    let idle = unavailable_accounts.is_empty()
        && reset_credits.is_none()
        && daybreak_accounts.is_empty()
        && notes.is_empty()
        && !windows.is_empty()
        && windows
            .iter()
            .all(|w| w.fraction.is_some_and(|f| f < IDLE_FRACTION));
    ProviderCard {
        provider: provider.to_string(),
        name: provider_display_name(provider),
        accounts: reports.len() + without_usage,
        unlimited: windows.is_empty() && unavailable_accounts.is_empty(),
        unavailable_accounts,
        windows,
        notes,
        idle,
        reset_credits,
        daybreak_accounts,
    }
}

fn claude_window_name(id: &str) -> &str {
    match id {
        "anthropic:5h" => "Claude 5h",
        "anthropic:7d" => "Claude weekly",
        "anthropic:7d:opus" => "Claude Opus weekly",
        "anthropic:7d:sonnet" => "Claude Sonnet weekly",
        other => other,
    }
}

/// omp's `eu` + the aggregation in `buildProviderCards`.
fn summarize_resets(reports: &[&UsageReport], now_ms: i64) -> Option<ResetSummary> {
    struct One {
        banked: i64,
        redeemable: i64,
        soonest: Option<i64>,
        reason: Option<String>,
    }
    let ones: Vec<One> = reports
        .iter()
        .filter_map(|r| r.reset_credits.as_ref())
        .map(|rc| {
            let banked = rc.available_count.max(0);
            let redeemable = rc.redeemable_count.unwrap_or(rc.available_count).max(0);
            let mut next: Option<i64> = None;
            let mut last_past: Option<i64> = None;
            for c in &rc.credits {
                let Some(t) = c
                    .expires_at
                    .as_deref()
                    .filter(|_| {
                        c.remaining_count != Some(0) && c.status.as_deref() != Some("redeemed")
                    })
                    .and_then(parse_iso_ms)
                else {
                    continue;
                };
                if t > now_ms {
                    next = Some(next.map_or(t, |n| n.min(t)));
                } else {
                    last_past = Some(last_past.map_or(t, |n| n.max(t)));
                }
            }
            let picked = match &rc.next_credit_id {
                Some(id) => rc.credits.iter().find(|c| c.id.as_ref() == Some(id)),
                None => rc
                    .credits
                    .iter()
                    .find(|c| c.usable != Some(false))
                    .or(rc.credits.first()),
            };
            let reason = rc
                .reason
                .clone()
                .or_else(|| {
                    rc.cooldown_until
                        .as_ref()
                        .map(|c| format!("cooldown until {c}"))
                })
                .or_else(|| {
                    picked.filter(|c| !c.blocking.is_empty()).map(|c| {
                        format!(
                            "blocked by {}",
                            c.blocking
                                .iter()
                                .map(|b| claude_window_name(b))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    })
                })
                .or_else(|| {
                    picked
                        .and_then(|c| c.status.clone())
                        .filter(|s| s != "available")
                })
                .or_else(|| (rc.eligible == Some(false)).then(|| "not eligible".to_string()))
                .or_else(|| {
                    (banked > 0 && redeemable == 0).then(|| "not usable right now".to_string())
                });
            One {
                banked,
                redeemable,
                soonest: next.or(last_past),
                reason,
            }
        })
        .filter(|o| o.banked > 0)
        .collect();
    if ones.is_empty() {
        return None;
    }
    let mut expiries: Vec<i64> = ones.iter().filter_map(|o| o.soonest).collect();
    expiries.sort_unstable();
    let soonest = expiries
        .iter()
        .copied()
        .find(|t| *t > now_ms)
        .or_else(|| expiries.last().copied());
    let mut reasons: Vec<String> = Vec::new();
    for r in ones.iter().filter_map(|o| o.reason.as_ref()) {
        if !reasons.contains(r) {
            reasons.push(r.clone());
        }
    }
    Some(ResetSummary {
        banked: ones.iter().map(|o| o.banked).sum(),
        redeemable: ones.iter().map(|o| o.redeemable).sum(),
        soonest_expiry_in_ms: soonest.map(|t| t - now_ms),
        unavailable_reasons: reasons,
    })
}

/// The window with the highest used fraction over all cards: (provider name, window label, used
/// fraction, status). Ties keep the first card/window.
pub fn tightest(cards: &[ProviderCard]) -> Option<(&str, &str, f64, LimitStatus)> {
    let mut best: Option<(&str, &str, f64, LimitStatus)> = None;
    for c in cards {
        for w in &c.windows {
            let Some(f) = w.fraction else { continue };
            if best.is_none_or(|(_, _, b, _)| f > b) {
                best = Some((&c.name, &w.label, f, w.status));
            }
        }
    }
    best
}

/// `2h11m`, `3d4h`, `45m`, `<1m`: time until a reset.
pub fn format_remaining(ms: i64) -> String {
    let mins = ms.max(0) / 60_000;
    if mins < 1 {
        return "<1m".into();
    }
    let (d, h, m) = (mins / 1440, mins % 1440 / 60, mins % 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

/// `2026-06-01T12:30:00Z`, `…+02:00`, optional fraction → epoch ms.
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let date = Date::new(num(0, 4)? as i32, num(5, 7)? as u32, num(8, 10)? as u32)?;
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut idx = 19;
    let mut ms = 0;
    if b.get(idx) == Some(&b'.') {
        let start = idx + 1;
        idx = start;
        while b.get(idx).is_some_and(u8::is_ascii_digit) {
            idx += 1;
        }
        let frac = &s[start..idx.min(start + 3)];
        ms = frac.parse::<i64>().ok()? * 10_i64.pow(3 - frac.len() as u32);
    }
    let offset_min = match s.get(idx..)? {
        "" | "Z" | "z" => 0,
        tz if tz.len() == 6 && matches!(tz.as_bytes()[0], b'+' | b'-') => {
            let v = tz.get(1..3)?.parse::<i64>().ok()? * 60 + tz.get(4..6)?.parse::<i64>().ok()?;
            if tz.starts_with('-') { -v } else { v }
        }
        _ => return None,
    };
    Some((date.days() * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60) * 1000 + ms)
}

// ---- stats.db: activity -------------------------------------------------------------------------

/// `~/$PI_CONFIG_DIR/stats.db` (`PI_CONFIG_DIR` defaults to `.omp`).
pub fn stats_db_path() -> PathBuf {
    let cfg = std::env::var_os("PI_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| ".omp".into());
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(cfg)
        .join("stats.db")
}

/// A calendar date (local time), ordered chronologically.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date {
    pub year: i32,
    pub month: u32,
    pub day: u32,
}

impl fmt::Display for Date {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl Date {
    /// `None` for an impossible date.
    pub fn new(year: i32, month: u32, day: u32) -> Option<Date> {
        let d = Date { year, month, day };
        // Round-tripping through the day count rejects Feb 30 and friends.
        ((1..=12).contains(&month) && day >= 1 && Date::from_days(d.days()) == d).then_some(d)
    }

    /// `YYYY-MM-DD`.
    pub fn parse(s: &str) -> Option<Date> {
        let b = s.as_bytes();
        if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
            return None;
        }
        Date::new(
            s[0..4].parse().ok()?,
            s[5..7].parse().ok()?,
            s[8..10].parse().ok()?,
        )
    }

    /// Days since 1970-01-01.
    pub fn days(&self) -> i64 {
        let y = i64::from(self.year) - i64::from(self.month <= 2);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let m = i64::from(self.month);
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    pub fn from_days(z: i64) -> Date {
        let z = z + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = (yoe + era * 400 + i64::from(month <= 2)) as i32;
        Date { year, month, day }
    }

    pub fn add_days(&self, n: i64) -> Date {
        Date::from_days(self.days() + n)
    }

    /// Monday = 0 … Sunday = 6.
    pub fn weekday_mon0(&self) -> u32 {
        (self.days() + 3).rem_euclid(7) as u32
    }
}

/// Today's date in the local time zone (SQLite's `localtime`, the same clock omp's query uses).
pub fn today_local() -> Date {
    Connection::open_in_memory()
        .and_then(|c| {
            c.query_row("SELECT date('now','localtime')", [], |r| {
                r.get::<_, String>(0)
            })
        })
        .ok()
        .and_then(|s| Date::parse(&s))
        .unwrap_or_else(|| Date::from_days(now_ms() / MS_DAY))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// Totals of one local day.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DayTotals {
    pub cost: f64,
    pub requests: u64,
    pub tokens: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DayActivity {
    pub day: Date,
    pub totals: DayTotals,
}

/// One model's usage over the last 30 days.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelActivity {
    pub provider: String,
    pub model: String,
    pub cost: f64,
    pub requests: u64,
    pub tokens: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Activity {
    /// Per local day (ascending) over the requested window; days without messages are absent.
    pub days: Vec<DayActivity>,
    /// Last 30 days, by cost then requests, descending.
    pub models: Vec<ModelActivity>,
}

fn db_err(e: rusqlite::Error) -> UsageError {
    UsageError::Db(e.to_string())
}

/// Reads omp's `stats.db` without creating or modifying any file next to it (no `-wal`, `-shm`,
/// `-journal`, no shm read-marks). WAL content is visible whenever the `-wal`/`-shm` sidecars exist
/// (see [`open_stats_readonly`]); 2 s busy timeout. A missing database (or one without a `messages`
/// table) yields an empty [`Activity`].
pub fn activity(db: &Path, days: u32) -> Result<Activity, UsageError> {
    activity_at(db, days, now_ms())
}

/// `file:` URI for `path` with every byte outside `[A-Za-z0-9/._~-]` percent-encoded.
fn sqlite_uri(path: &Path, query: &str) -> String {
    encode_sqlite_uri(platform::path_bytes(path), query, cfg!(windows))
}

/// [`sqlite_uri`] over raw path bytes. `windows`: a `C:\dir\db` path becomes `file:///C:/dir/db`
/// (the drive colon stays literal, backslashes turn into `/`).
fn encode_sqlite_uri(path: &[u8], query: &str, windows: bool) -> String {
    let drive = windows && path.len() > 1 && path[0].is_ascii_alphabetic() && path[1] == b':';
    let mut uri = String::from(if drive { "file:///" } else { "file:" });
    for (i, &b) in path.iter().enumerate() {
        let b = if windows && b == b'\\' { b'/' } else { b };
        if b.is_ascii_alphanumeric()
            || matches!(b, b'/' | b'.' | b'_' | b'~' | b'-')
            || (drive && i == 1)
        {
            uri.push(char::from(b));
        } else {
            uri.push_str(&format!("%{b:02X}"));
        }
    }
    uri.push('?');
    uri.push_str(query);
    uri
}

fn sidecar(db: &Path, suffix: &str) -> PathBuf {
    let mut p = db.as_os_str().to_owned();
    p.push(suffix);
    PathBuf::from(p)
}

/// Whether the database header (file-format versions at bytes 18/19) says WAL mode.
fn is_wal_mode(db: &Path) -> bool {
    let mut head = [0u8; 20];
    std::fs::File::open(db)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && head[18] == 2
        && head[19] == 2
}

/// Opens `db` so that SQLite cannot create or write any file:
/// - WAL database with `-wal` and `-shm` present: `mode=ro&readonly_shm=1` — the `-shm` is opened
///   `O_RDONLY` (never created, no read-mark writes; when no process holds it SQLite falls back to a
///   private heap wal-index built from the `-wal`), so WAL-only rows are visible;
/// - WAL database whose sidecars are missing: `immutable=1` (no locking, no `-wal`/`-shm` creation;
///   there is nothing beyond the main file to see);
/// - rollback-journal database: `mode=ro` (a hot journal is never rolled back or deleted).
fn open_stats_readonly(db: &Path) -> rusqlite::Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let query = if !is_wal_mode(db) {
        "mode=ro"
    } else if sidecar(db, "-wal").exists() && sidecar(db, "-shm").exists() {
        "mode=ro&readonly_shm=1"
    } else {
        "immutable=1"
    };
    Connection::open_with_flags(sqlite_uri(db, query), flags)
}

/// [`activity`] with an explicit clock.
pub fn activity_at(db: &Path, days: u32, now_ms: i64) -> Result<Activity, UsageError> {
    if !db.exists() {
        return Ok(Activity::default());
    }
    let conn = open_stats_readonly(db).map_err(db_err)?;
    conn.busy_timeout(Duration::from_secs(2)).map_err(db_err)?;
    let has_messages: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages')",
            [],
            |r| r.get(0),
        )
        .map_err(db_err)?;
    if !has_messages {
        return Ok(Activity::default());
    }

    let since = now_ms - i64::from(days) * MS_DAY;
    let mut stmt = conn
        .prepare(
            "SELECT date(timestamp/1000,'unixepoch','localtime') AS day, \
             SUM(cost_total), COUNT(*), SUM(total_tokens) \
             FROM messages WHERE timestamp >= ?1 GROUP BY day ORDER BY day",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([since], |r| {
            Ok((
                r.get::<_, Option<String>>(0)?,
                r.get::<_, Option<f64>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<i64>>(3)?,
            ))
        })
        .map_err(db_err)?;
    let mut out = Activity::default();
    for row in rows {
        let (day, cost, requests, tokens) = row.map_err(db_err)?;
        let Some(day) = day.as_deref().and_then(Date::parse) else {
            log::debug!("usage: skipped activity row with unparseable day");
            continue;
        };
        out.days.push(DayActivity {
            day,
            totals: DayTotals {
                cost: cost.unwrap_or(0.0),
                requests: requests.max(0) as u64,
                tokens: tokens.unwrap_or(0).max(0) as u64,
            },
        });
    }

    let mut stmt = conn
        .prepare(
            "SELECT COALESCE(provider,''), COALESCE(model,''), SUM(cost_total), COUNT(*), \
             SUM(total_tokens) FROM messages WHERE timestamp >= ?1 GROUP BY provider, model \
             ORDER BY SUM(cost_total) DESC, COUNT(*) DESC",
        )
        .map_err(db_err)?;
    let rows = stmt
        .query_map([now_ms - 30 * MS_DAY], |r| {
            Ok(ModelActivity {
                provider: r.get(0)?,
                model: r.get(1)?,
                cost: r.get::<_, Option<f64>>(2)?.unwrap_or(0.0),
                requests: r.get::<_, i64>(3)?.max(0) as u64,
                tokens: r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u64,
            })
        })
        .map_err(db_err)?;
    for row in rows {
        out.models.push(row.map_err(db_err)?);
    }
    Ok(out)
}

// ---- heatmap ------------------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

#[derive(Clone, Debug, PartialEq)]
pub struct Last30 {
    pub cost: f64,
    pub requests: u64,
    pub tokens: u64,
    /// The cost series (requests when nothing has a cost), oldest → today, exactly 30 values.
    pub daily: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Heatmap {
    pub weeks: usize,
    /// `cells[row][week]`, row 0 = Monday … 6 = Sunday; `None` after today, else level 0–4.
    pub cells: Vec<Vec<Option<u8>>>,
    /// Same shape as `cells`: the day's totals (zero for quiet days).
    pub totals: Vec<Vec<DayTotals>>,
    /// Per week: the month's abbreviation where it differs from the previous week's.
    pub month_labels: Vec<Option<&'static str>>,
    /// The Monday of the first week.
    pub start: Date,
    pub today: Date,
    /// True when levels/series are by cost, false when by request count.
    pub by_cost: bool,
    /// Past-year totals (the whole grid).
    pub total_cost: f64,
    pub total_requests: u64,
    pub total_tokens: u64,
    pub last30: Last30,
    pub busiest: Option<DayActivity>,
    /// Days with any cost or request in the grid.
    pub active_days: u32,
    /// Longest run of consecutive active days.
    pub longest_streak: u32,
}

impl Heatmap {
    /// omp's `buildHeatmapLayout`: 7 × `weeks` cells ending in today's week, intensity
    /// `ceil(sqrt(v / max) · 4)` with `v` = cost if any day has a cost, else requests.
    pub fn build(activity: &Activity, today: Date, weeks: usize) -> Heatmap {
        let weeks = weeks.max(1);
        let start = today
            .add_days(-i64::from(today.weekday_mon0()))
            .add_days(-(weeks as i64 - 1) * 7);
        let mut in_grid: Vec<&DayActivity> = activity
            .days
            .iter()
            .filter(|d| d.day >= start && d.day <= today)
            .collect();
        in_grid.sort_by_key(|d| d.day);
        let by_cost = in_grid.iter().any(|d| d.totals.cost > 0.0);
        let value = |t: &DayTotals| if by_cost { t.cost } else { t.requests as f64 };
        let max = in_grid.iter().map(|d| value(&d.totals)).fold(0.0, f64::max);
        let level = |v: f64| {
            if v <= 0.0 || max <= 0.0 {
                0
            } else {
                ((v / max).sqrt() * 4.0).ceil().clamp(1.0, 4.0) as u8
            }
        };
        let start_days = start.days();
        let mut cells = vec![vec![None; weeks]; 7];
        let mut totals = vec![vec![DayTotals::default(); weeks]; 7];
        let mut month_labels = Vec::with_capacity(weeks);
        let mut prev_month = 0;
        for w in 0..weeks {
            let monday = Date::from_days(start_days + w as i64 * 7);
            month_labels
                .push((monday.month != prev_month).then(|| MONTHS[monday.month as usize - 1]));
            prev_month = monday.month;
            for row in 0..7 {
                let day = Date::from_days(start_days + w as i64 * 7 + row as i64);
                if day > today {
                    continue;
                }
                let t = in_grid
                    .binary_search_by(|d| d.day.cmp(&day))
                    .map_or(DayTotals::default(), |i| in_grid[i].totals);
                cells[row][w] = Some(level(value(&t)));
                totals[row][w] = t;
            }
        }

        let active = |t: &DayTotals| t.cost > 0.0 || t.requests > 0;
        let mut active_days = 0;
        let (mut streak, mut longest, mut prev_day) = (0u32, 0u32, None::<i64>);
        for d in in_grid.iter().filter(|d| active(&d.totals)) {
            active_days += 1;
            streak = if prev_day == Some(d.day.days() - 1) {
                streak + 1
            } else {
                1
            };
            longest = longest.max(streak);
            prev_day = Some(d.day.days());
        }
        let busiest = in_grid
            .iter()
            .filter(|d| value(&d.totals) > 0.0)
            .fold(None::<&&DayActivity>, |best, d| match best {
                Some(b) if value(&b.totals) >= value(&d.totals) => Some(b),
                _ => Some(d),
            })
            .map(|d| **d);

        let last30_from = today.add_days(-29);
        let mut daily = vec![0.0; 30];
        let mut last30 = Last30 {
            cost: 0.0,
            requests: 0,
            tokens: 0,
            daily: Vec::new(),
        };
        for d in in_grid.iter().filter(|d| d.day >= last30_from) {
            last30.cost += d.totals.cost;
            last30.requests += d.totals.requests;
            last30.tokens += d.totals.tokens;
            daily[(d.day.days() - last30_from.days()) as usize] = value(&d.totals);
        }
        last30.daily = daily;

        Heatmap {
            weeks,
            cells,
            totals,
            month_labels,
            start,
            today,
            by_cost,
            total_cost: in_grid.iter().map(|d| d.totals.cost).sum(),
            total_requests: in_grid.iter().map(|d| d.totals.requests).sum(),
            total_tokens: in_grid.iter().map(|d| d.totals.tokens).sum(),
            last30,
            busiest,
            active_days,
            longest_streak: longest,
        }
    }

    /// The date of a cell.
    pub fn date_at(&self, row: usize, week: usize) -> Date {
        self.start.add_days(week as i64 * 7 + row as i64)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sqlite_uris_escape_the_path_per_os() {
        assert_eq!(
            encode_sqlite_uri(b"/home/a b/db.sqlite", "mode=ro", false),
            "file:/home/a%20b/db.sqlite?mode=ro"
        );
        assert_eq!(
            encode_sqlite_uri(b"/x/\xc3\xbc?#%", "q", false),
            "file:/x/%C3%BC%3F%23%25?q"
        );
        // A Windows path becomes an absolute file URI: literal drive colon, slashes for backslashes.
        assert_eq!(
            encode_sqlite_uri(br"C:\Users\a b\omp.db", "immutable=1", true),
            "file:///C:/Users/a%20b/omp.db?immutable=1"
        );
        // On Unix a backslash is an ordinary (escaped) file name character.
        assert_eq!(encode_sqlite_uri(br"/a\b", "q", false), "file:/a%5Cb?q");
    }

    use super::*;
    use rusqlite::params;

    const SAMPLE: &str = r#"{
      "generatedAt": 1792647505321,
      "reports": [{
        "provider": "anthropic", "fetchedAt": 1792647505316,
        "limits": [
          { "id": "anthropic:5h", "label": "Claude 5 Hour",
            "scope": { "provider": "anthropic", "windowId": "5h", "shared": true },
            "window": { "id": "5h", "label": "5 Hour", "durationMs": 18000000, "resetsAt": 1792655545000 },
            "amount": { "used": 37, "limit": 100, "remaining": 63, "usedFraction": 0.37, "remainingFraction": 0.63, "unit": "percent" },
            "status": "ok" },
          { "id": "anthropic:7d", "label": "Claude 7 Day",
            "scope": {"provider":"anthropic","windowId":"7d","shared":true},
            "window": {"id":"7d","label":"7 Day","durationMs":604800000,"resetsAt":1792924705000},
            "amount": {"used":81.5,"limit":100,"remaining":18.5,"usedFraction":0.815,"remainingFraction":0.185,"unit":"percent"}, "status":"ok" },
          { "id": "anthropic:7d:sonnet", "label": "Claude 7 Day (Sonnet)",
            "scope": {"provider":"anthropic","windowId":"7d","tier":"sonnet"},
            "window": {"id":"7d","label":"7 Day","durationMs":604800000,"resetsAt":1792924705000},
            "amount": {"used":12,"limit":100,"remaining":88,"usedFraction":0.12,"remainingFraction":0.88,"unit":"percent"}, "status":"ok" },
          { "id": "anthropic:extra", "label": "Claude Extra Usage",
            "scope": {"provider":"anthropic","windowId":"extra"},
            "amount": {"used":12.34,"unit":"usd","limit":50,"remaining":37.66,"usedFraction":0.2468,"remainingFraction":0.7532}, "status":"ok" }
        ],
        "metadata": { "endpoint": "http://127.0.0.1:18955/api/oauth/usage", "accountId": "acc-uuid-1", "email": "sandbox@example.test" },
        "resetCredits": { "availableCount": 0, "redeemableCount": 0, "eligible": false, "credits": [] },
        "raw": {"ignored": [1,2,3]}
      }],
      "accountsWithoutUsage": [],
      "disabledCredentials": [],
      "capacity": {}
    }"#;

    const NOW: i64 = 1_792_647_505_321;

    fn limit(
        label: &str,
        win: &str,
        frac: f64,
        status: LimitStatus,
        resets: Option<i64>,
    ) -> UsageLimit {
        UsageLimit {
            id: format!("x:{win}"),
            label: label.into(),
            window: Some(UsageWindow {
                id: win.into(),
                label: win.into(),
                duration_ms: Some(match win {
                    "5h" => 5 * 3_600_000,
                    "7d" => 7 * MS_DAY,
                    _ => 30 * MS_DAY,
                }),
                resets_at_ms: resets,
                reset_label: None,
            }),
            amount: UsageAmount {
                used_fraction: Some(frac),
                ..UsageAmount::default()
            },
            status,
            ..UsageLimit::default()
        }
    }

    fn report(provider: &str, limits: Vec<UsageLimit>) -> UsageReport {
        UsageReport {
            provider: provider.into(),
            limits,
            ..UsageReport::default()
        }
    }

    fn snap(reports: Vec<UsageReport>) -> UsageSnapshot {
        UsageSnapshot {
            reports,
            ..UsageSnapshot::default()
        }
    }

    #[test]
    fn parses_omp_sample_and_builds_the_card() {
        let s = UsageSnapshot::parse(SAMPLE).unwrap();
        assert_eq!(s.generated_at_ms, NOW);
        assert_eq!(s.reports.len(), 1);
        let r = &s.reports[0];
        assert_eq!(r.limits.len(), 4);
        assert_eq!(r.metadata.email.as_deref(), Some("sandbox@example.test"));
        assert_eq!(r.limits[3].amount.unit, "usd");
        assert_eq!(
            r.limits[0].window.as_ref().unwrap().resets_at_ms,
            Some(1_792_655_545_000)
        );

        let cards = provider_cards(&s, NOW);
        assert_eq!(cards.len(), 1);
        let c = &cards[0];
        assert_eq!(
            (c.name.as_str(), c.accounts, c.idle, c.unlimited),
            ("Anthropic", 1, false, false)
        );
        let labels: Vec<&str> = c.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(
            labels,
            [
                "Claude 5 Hour",
                "Claude 7 Day",
                "Claude 7 Day (Sonnet)",
                "Claude Extra Usage"
            ]
        );
        // omp printed resetMs 8039515 / 277199515 for this sample.
        assert_eq!(c.windows[0].resets_in_ms, Some(8_039_679));
        assert_eq!(c.windows[1].resets_in_ms, Some(277_199_679));
        assert!(c.windows[3].resets_in_ms.is_none());
        assert_eq!(
            c.windows[1].remaining().map(|r| (r * 1000.0).round()),
            Some(185.0)
        );
        assert!(
            c.reset_credits.is_none(),
            "zero banked credits are not a summary"
        );
        assert!(c.windows.iter().all(|w| w.window_tag.is_none()));
        let t = tightest(&cards).unwrap();
        assert_eq!((t.0, t.1, t.2), ("Anthropic", "Claude 7 Day", 0.815));
    }

    #[test]
    fn tolerates_unknown_fields_nulls_bad_entries_and_no_reports() {
        let s = UsageSnapshot::parse(
            r#"{"generatedAt": 5, "future": {"a":1}, "reports": [
                 {"provider":"p","fetchedAt":"not-a-number","limits":[
                    {"id":"a","label":"A","scope":null,"amount":{"used":"x","unit":null},"status":"weird","newField":1},
                    "garbage", 7,
                    {"id":"b","label":"B","amount":{"usedFraction":0.5,"unit":"tokens"},"status":"warning"}],
                  "metadata": {"email": 5, "planType":"max","daybreak":"yes"}, "notes": null},
                 42
               ], "accountsWithoutUsage": [{"provider":"p","label":"x@y"}, null],
               "disabledCredentials": [{"provider":"q","cause":"expired"}]}"#,
        )
        .unwrap();
        assert_eq!(s.reports.len(), 1);
        let r = &s.reports[0];
        assert_eq!(r.fetched_at_ms, 0);
        assert_eq!(r.limits.len(), 2, "non-object entries are dropped");
        assert_eq!(r.limits[0].status, LimitStatus::Unknown);
        assert_eq!(r.limits[0].amount.unit, "unknown");
        assert_eq!(r.limits[1].status, LimitStatus::Warning);
        assert_eq!(r.metadata.email, None);
        assert_eq!(r.metadata.plan_type.as_deref(), Some("max"));
        assert!(!r.metadata.daybreak);
        assert_eq!(s.accounts_without_usage.len(), 1);
        assert_eq!(s.disabled_credentials[0].reason.as_deref(), Some("expired"));

        let empty = UsageSnapshot::parse(
            r#"{ "generatedAt": 1, "reports": [], "accountsWithoutUsage": [], "disabledCredentials": [], "capacity": {} }"#,
        )
        .unwrap();
        assert!(provider_cards(&empty, 0).is_empty());
        assert!(tightest(&[]).is_none());
        assert!(matches!(
            UsageSnapshot::parse("not json"),
            Err(UsageError::Parse(_))
        ));
    }

    #[test]
    fn multi_account_windows_group_and_average() {
        let a = report(
            "anthropic",
            vec![limit(
                "Claude 7 Day",
                "7d",
                0.8,
                LimitStatus::Ok,
                Some(NOW + 3_600_000),
            )],
        );
        let b = report(
            "anthropic",
            vec![limit(
                "Claude 7 Day",
                "7d",
                0.4,
                LimitStatus::Ok,
                Some(NOW + 7_200_000),
            )],
        );
        let c = &provider_cards(&snap(vec![a, b]), NOW)[0];
        assert_eq!(c.accounts, 2);
        assert_eq!(c.windows.len(), 1);
        assert!(
            (c.windows[0].fraction.unwrap() - 0.6).abs() < 1e-9,
            "mean across accounts"
        );
        assert_eq!(
            c.windows[0].resets_in_ms,
            Some(3_600_000),
            "reset of the fullest account"
        );
    }

    #[test]
    fn window_tag_only_when_labels_collide() {
        let r = report(
            "openai-codex",
            vec![
                limit("Codex", "5h", 0.2, LimitStatus::Ok, None),
                limit("Codex", "7d", 0.5, LimitStatus::Ok, None),
                limit("Spark", "mo", 0.1, LimitStatus::Ok, None),
            ],
        );
        let c = &provider_cards(&snap(vec![r]), NOW)[0];
        assert_eq!(c.name, "Codex");
        let tags: Vec<_> = c
            .windows
            .iter()
            .map(|w| (w.label.as_str(), w.window_tag.as_deref()))
            .collect();
        assert_eq!(
            tags,
            [
                ("Codex", Some("5h")),
                ("Codex", Some("7d")),
                ("Spark", None)
            ]
        );
    }

    #[test]
    fn status_thresholds_and_combination() {
        let mut derived = limit("L", "5h", 0.95, LimitStatus::Unknown, None);
        assert_eq!(effective_status(&derived), LimitStatus::Warning);
        derived.amount.used_fraction = Some(1.0);
        assert_eq!(effective_status(&derived), LimitStatus::Exhausted);
        derived.amount.used_fraction = Some(0.3);
        assert_eq!(effective_status(&derived), LimitStatus::Ok);
        derived.amount.used_fraction = None;
        assert_eq!(effective_status(&derived), LimitStatus::Unknown);
        // explicit wins over the fraction
        let explicit = limit("L", "5h", 0.95, LimitStatus::Ok, None);
        assert_eq!(effective_status(&explicit), LimitStatus::Ok);

        use LimitStatus::*;
        assert_eq!(combine_status(&[Ok, Exhausted]), Warning);
        assert_eq!(combine_status(&[Exhausted, Exhausted]), Exhausted);
        assert_eq!(combine_status(&[Unknown, Warning]), Warning);
        assert_eq!(combine_status(&[Unknown]), Unknown);

        // fractions from the other amount shapes (omp's Cr)
        let mut l = UsageLimit::default();
        l.amount.used = Some(25.0);
        l.amount.limit = Some(50.0);
        assert_eq!(limit_fraction(&l), Some(0.5));
        l.amount = UsageAmount {
            unit: "percent".into(),
            used: Some(30.0),
            ..UsageAmount::default()
        };
        assert_eq!(limit_fraction(&l), Some(0.3));
        l.amount = UsageAmount {
            remaining_fraction: Some(0.25),
            ..UsageAmount::default()
        };
        assert_eq!(limit_fraction(&l), Some(0.75));
    }

    #[test]
    fn idle_providers_sort_last_and_active_by_fraction_then_name() {
        let cards = provider_cards(
            &snap(vec![
                report("zai", vec![limit("Z", "5h", 0.001, LimitStatus::Ok, None)]),
                report("kimi", vec![limit("K", "5h", 0.4, LimitStatus::Ok, None)]),
                report(
                    "anthropic",
                    vec![limit("A", "5h", 0.4, LimitStatus::Ok, None)],
                ),
                report(
                    "gemini",
                    vec![limit("G", "5h", 0.9, LimitStatus::Warning, None)],
                ),
            ]),
            NOW,
        );
        let order: Vec<(&str, bool)> = cards
            .iter()
            .map(|c| (c.provider.as_str(), c.idle))
            .collect();
        assert_eq!(
            order,
            [
                ("gemini", false),
                ("anthropic", false),
                ("kimi", false),
                ("zai", true)
            ]
        );
        assert_eq!(cards[0].status(), LimitStatus::Warning);
        assert_eq!(cards[0].max_fraction(), Some(0.9));
    }

    #[test]
    fn unavailable_accounts_notes_and_unlimited() {
        let mut s = snap(vec![report(
            "anthropic",
            vec![limit("A", "5h", 0.0, LimitStatus::Ok, None)],
        )]);
        s.accounts_without_usage.push(UnavailableAccount {
            provider: "anthropic".into(),
            label: Some("old@x.test".into()),
            reason: None,
            ..Default::default()
        });
        s.disabled_credentials.push(UnavailableAccount {
            provider: "cursor".into(),
            label: None,
            reason: Some("expired".into()),
            ..Default::default()
        });
        let cards = provider_cards(&s, NOW);
        let anthropic = cards.iter().find(|c| c.provider == "anthropic").unwrap();
        assert_eq!((anthropic.accounts, anthropic.idle), (2, false));
        assert_eq!(anthropic.unavailable_accounts, ["old@x.test"]);
        let cursor = cards.iter().find(|c| c.provider == "cursor").unwrap();
        assert_eq!(cursor.unavailable_accounts, ["account"]);
        assert!(!cursor.unlimited && !cursor.idle);

        let mut noted = report("kimi", vec![limit("K", "5h", 0.0, LimitStatus::Ok, None)]);
        noted.notes = vec!["beta".into()];
        assert!(!provider_cards(&snap(vec![noted]), NOW)[0].idle);
        let unlimited = &provider_cards(&snap(vec![report("ollama", vec![])]), NOW)[0];
        assert!(
            unlimited.unlimited && !unlimited.idle,
            "a card without measurable windows stays visible"
        );
    }

    #[test]
    fn unavailable_identity_label_and_account_count() {
        let s = UsageSnapshot::parse(
            r#"{"generatedAt":1,"reports":[{"provider":"anthropic","fetchedAt":1,"limits":[]}],
               "accountsWithoutUsage":[{"provider":"anthropic","email":"d@example.test","accountId":"acc-d"},
                                       {"provider":"anthropic","accountId":"acc-e"},
                                       {"provider":"anthropic"}],
               "disabledCredentials":[{"provider":"anthropic","email":"c@example.test","cause":"expired"}]}"#,
        )
        .unwrap();
        let c = &provider_cards(&s, NOW)[0];
        assert_eq!(
            c.unavailable_accounts,
            ["d@example.test", "acc-e", "account", "c@example.test"]
        );
        assert_eq!(
            c.accounts, 4,
            "1 report + 3 without usage; disabled not counted"
        );
    }

    #[test]
    fn reset_credits_summary_and_text() {
        let mut r = report(
            "openai-codex",
            vec![limit("Codex", "5h", 0.6, LimitStatus::Ok, None)],
        );
        r.reset_credits = Some(ResetCredits {
            available_count: 3,
            redeemable_count: Some(2),
            credits: vec![
                ResetCredit {
                    title: Some("Reset".into()),
                    remaining_count: Some(1),
                    expires_at: Some("2026-10-22T10:00:00Z".into()),
                    ..ResetCredit::default()
                },
                ResetCredit {
                    remaining_count: Some(0),
                    expires_at: Some("2026-10-21T10:00:00Z".into()),
                    ..ResetCredit::default()
                },
            ],
            ..ResetCredits::default()
        });
        let now = parse_iso_ms("2026-10-20T10:00:00Z").unwrap();
        let c = &provider_cards(&snap(vec![r]), now)[0];
        let rc = c.reset_credits.as_ref().unwrap();
        assert_eq!((rc.banked, rc.redeemable), (3, 2));
        assert_eq!(
            rc.soonest_expiry_in_ms,
            Some(2 * MS_DAY),
            "spent credits are ignored"
        );
        assert_eq!(rc.text(), "3 resets banked · 2 usable · expires in 2d0h");
        assert!(!c.idle, "banked resets keep a provider active");

        let blocked = ResetSummary {
            banked: 1,
            redeemable: 0,
            soonest_expiry_in_ms: Some(-5),
            unavailable_reasons: vec!["not usable right now".into()],
        };
        assert_eq!(
            blocked.text(),
            "1 reset banked · 0 usable · expired · not usable right now"
        );
    }

    #[test]
    fn iso_and_duration_formats() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(parse_iso_ms("1970-01-01T01:00:00.250+01:00"), Some(250));
        assert_eq!(parse_iso_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("nope"), None);
        assert_eq!(format_remaining(30_000), "<1m");
        assert_eq!(format_remaining(45 * 60_000), "45m");
        assert_eq!(format_remaining(131 * 60_000), "2h11m");
        assert_eq!(format_remaining(76 * 3_600_000), "3d4h");
        assert_eq!(
            provider_display_name("some-new_provider"),
            "Some New Provider"
        );
    }

    #[cfg(unix)]
    /// Executable `dest` created by an external process so this process never holds a write fd to
    /// the executed inode (a sibling thread's fork would leak it and exec would fail with ETXTBSY).
    fn install_exec(dest: &std::path::Path, content: &str) {
        let mut stage = dest.as_os_str().to_owned();
        stage.push(".src");
        std::fs::write(&stage, content).unwrap();
        let status = std::process::Command::new("install")
            .args(["-m", "755"])
            .arg(&stage)
            .arg(dest)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    fn fake_omp(dir: &Path, script: &str) -> Omp {
        let p = dir.join("omp");
        install_exec(&p, &format!("#!/bin/sh\n{script}\n"));
        Omp::at(p)
    }

    #[cfg(unix)]
    #[test]
    fn fetch_usage_runs_the_cli_and_maps_failures() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("argv.log");
        let ok = fake_omp(
            dir.path(),
            &format!(
                "echo \"$@\" >> {}\ncase \"$1 $2\" in \"usage invalidate\") exit 0;; esac\ncat <<'EOF'\n{}\nEOF",
                log.display(),
                SAMPLE
            ),
        );
        let s = fetch_usage(&ok, true, Duration::from_secs(10)).unwrap();
        assert_eq!(s.reports.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "usage invalidate\nusage --json --no-extensions\n"
        );

        let bad = fake_omp(dir.path(), "echo 'boom: no login' >&2; exit 3");
        match fetch_usage(&bad, false, Duration::from_secs(10)) {
            Err(UsageError::Exit {
                code: Some(3),
                stderr_tail,
            }) => assert!(stderr_tail.contains("boom")),
            other => panic!("{other:?}"),
        }
        let junk = fake_omp(dir.path(), "echo 'hello'");
        assert!(matches!(
            fetch_usage(&junk, false, Duration::from_secs(10)),
            Err(UsageError::Parse(_))
        ));
        let slow = fake_omp(dir.path(), "sleep 30");
        let t0 = Instant::now();
        assert!(matches!(
            fetch_usage(&slow, false, Duration::from_millis(200)),
            Err(UsageError::Timeout(_))
        ));
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "child killed on timeout"
        );
        let gone = Omp::at(dir.path().join("nope"));
        assert!(matches!(
            fetch_usage(&gone, false, Duration::from_secs(1)),
            Err(UsageError::Spawn(_))
        ));
    }

    /// Number of live processes whose command line contains `marker`.
    #[cfg(unix)]
    fn count_procs(marker: &str) -> usize {
        std::fs::read_dir("/proc")
            .unwrap()
            .filter_map(|e| std::fs::read(e.ok()?.path().join("cmdline")).ok())
            .filter(|c| String::from_utf8_lossy(c).contains(marker))
            .count()
    }

    #[cfg(unix)]
    #[test]
    fn fetch_usage_never_outlives_its_deadline_or_leaks_descendants() {
        let dir = tempfile::tempdir().unwrap();
        // the child exits at once but a background descendant keeps the output pipes open
        let marker_a = format!("31{}", std::process::id());
        let bg = fake_omp(dir.path(), &format!("sleep {marker_a} &\nexit 0"));
        let t0 = Instant::now();
        let r = fetch_usage(&bg, false, Duration::from_secs(2));
        assert!(t0.elapsed() < Duration::from_secs(4), "{:?}", t0.elapsed());
        assert!(matches!(r, Err(UsageError::Parse(_))), "{r:?}");
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(count_procs(&format!("sleep\0{marker_a}")), 0);

        // a hang with a grandchild
        let marker_b = format!("32{}", std::process::id());
        let hang = fake_omp(dir.path(), &format!("sleep {marker_b} &\nsleep {marker_b}"));
        let t0 = Instant::now();
        let r = fetch_usage(&hang, false, Duration::from_millis(300));
        assert!(matches!(r, Err(UsageError::Timeout(_))), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(count_procs(&format!("sleep\0{marker_b}")), 0);
    }

    // ---- activity ----

    fn make_db(path: &Path, wal: bool) -> Connection {
        let c = Connection::open(path).unwrap();
        if wal {
            c.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))
                .unwrap();
        }
        c.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, session_file TEXT, entry_id TEXT, folder TEXT, \
             model TEXT, provider TEXT, timestamp INTEGER, input_tokens INTEGER, output_tokens INTEGER, \
             cache_read_tokens INTEGER, cache_write_tokens INTEGER, total_tokens INTEGER, cost_total REAL, \
             cost_unpriced INTEGER, agent_type TEXT, stop_reason TEXT);",
        )
        .unwrap();
        c
    }

    fn msg(c: &Connection, ts: i64, provider: &str, model: &str, tokens: i64, cost: Option<f64>) {
        c.execute(
            "INSERT INTO messages (session_file, entry_id, folder, model, provider, timestamp, total_tokens, cost_total) \
             VALUES ('s','e','/f',?1,?2,?3,?4,?5)",
            params![model, provider, ts, tokens, cost],
        )
        .unwrap();
    }

    /// 12:20 UTC: no time zone has its midnight within minutes of it.
    const DAY0: i64 = 1_790_000_000_000 - 1_790_000_000_000 % MS_DAY + 12 * 3_600_000 + 20 * 60_000;

    #[test]
    fn activity_groups_per_local_day_and_per_model() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let c = make_db(&db, false);
        let now = DAY0 + 10 * MS_DAY;
        msg(&c, DAY0, "anthropic", "sonnet", 1000, Some(0.5));
        msg(&c, DAY0 + 300_000, "anthropic", "sonnet", 500, Some(0.25));
        msg(&c, DAY0 + 3 * MS_DAY, "openai-codex", "gpt", 200, None);
        msg(&c, now - 2 * MS_DAY, "anthropic", "opus", 4000, Some(3.0));
        msg(&c, now - 400 * MS_DAY, "anthropic", "old", 9, Some(99.0));

        let a = activity_at(&db, 371, now).unwrap();
        assert_eq!(
            a.days.len(),
            3,
            "the 400-day-old message is outside the window"
        );
        assert_eq!(
            a.days[0].totals,
            DayTotals {
                cost: 0.75,
                requests: 2,
                tokens: 1500
            }
        );
        assert_eq!(a.days[1].day.days() - a.days[0].day.days(), 3);
        assert_eq!(a.days[1].totals.cost, 0.0, "NULL cost sums as zero");
        assert!(a.days.windows(2).all(|w| w[0].day < w[1].day));

        // last 30 days per model, most expensive first
        let models: Vec<(&str, f64, u64)> = a
            .models
            .iter()
            .map(|m| (m.model.as_str(), m.cost, m.requests))
            .collect();
        assert_eq!(
            models,
            [("opus", 3.0, 1), ("sonnet", 0.75, 2), ("gpt", 0.0, 1)]
        );
        assert_eq!(a.models[0].provider, "anthropic");
        assert_eq!(a.models[0].tokens, 4000);

        // a shorter window drops the early days
        assert_eq!(activity_at(&db, 5, now).unwrap().days.len(), 1);
    }

    #[test]
    fn activity_is_empty_without_db_or_table_and_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            activity_at(&dir.path().join("none.db"), 371, 0).unwrap(),
            Activity::default()
        );
        let empty = dir.path().join("empty.db");
        Connection::open(&empty)
            .unwrap()
            .execute_batch("CREATE TABLE other (x)")
            .unwrap();
        assert_eq!(activity_at(&empty, 371, 0).unwrap(), Activity::default());

        let db = dir.path().join("stats.db");
        let c = make_db(&db, false);
        msg(&c, DAY0, "p", "m", 1, Some(1.0));
        drop(c);
        let before = std::fs::read(&db).unwrap();
        activity_at(&db, 371, DAY0 + MS_DAY).unwrap();
        assert_eq!(
            std::fs::read(&db).unwrap(),
            before,
            "read-only open leaves the file untouched"
        );
    }

    /// Every entry of `dir` with its bytes and mtime.
    fn dir_state(dir: &Path) -> std::collections::BTreeMap<String, (Vec<u8>, SystemTime)> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let mtime = e.metadata().unwrap().modified().unwrap();
                (
                    e.file_name().to_string_lossy().into_owned(),
                    (std::fs::read(e.path()).unwrap(), mtime),
                )
            })
            .collect()
    }

    /// Runs only as the child of the live-writer test: a separate process holds a WAL writer open
    /// (SQLite shares the wal-index between connections of one process, which would hide the
    /// reader's behaviour against a real, foreign writer).
    #[test]
    fn wal_writer_process_helper() {
        let Some(db) = std::env::var_os("DECK_TEST_WAL_WRITER_DB") else {
            return;
        };
        let w = make_db(Path::new(&db), true);
        msg(&w, DAY0, "p", "m", 7, Some(2.0)); // stays in the -wal file, not checkpointed
        println!("writer-ready");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }

    #[test]
    fn activity_sees_wal_rows_and_touches_nothing_with_a_live_writer() {
        use std::io::{BufRead, BufReader, Write};
        use std::process::Stdio;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let mut writer = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "usage::tests::wal_writer_process_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("DECK_TEST_WAL_WRITER_DB", &db)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut lines = BufReader::new(writer.stdout.take().unwrap()).lines();
        while !lines.next().unwrap().unwrap().contains("writer-ready") {}
        assert!(sidecar(&db, "-wal").exists() && sidecar(&db, "-shm").exists());

        let before = dir_state(dir.path());
        let a = activity_at(&db, 371, DAY0 + MS_DAY).unwrap();
        let after = dir_state(dir.path());
        writer.stdin.take().unwrap().write_all(b"\n").unwrap();
        writer.wait().unwrap();

        assert_eq!(a.days.len(), 1);
        assert_eq!(a.days[0].totals.tokens, 7, "WAL-only rows are read");
        assert!(
            after == before,
            "db, -wal and -shm keep their bytes and mtimes; nothing is created"
        );
    }

    #[test]
    fn activity_reads_a_wal_snapshot_without_a_live_writer_and_touches_nothing() {
        let live = tempfile::tempdir().unwrap();
        let db = live.path().join("stats.db");
        let writer = make_db(&db, true);
        msg(&writer, DAY0, "p", "m", 9, Some(2.0));
        // a crashed writer: sidecars copied while nobody holds the -shm
        let dead = tempfile::tempdir().unwrap();
        for name in ["stats.db", "stats.db-wal", "stats.db-shm"] {
            std::fs::copy(live.path().join(name), dead.path().join(name)).unwrap();
        }
        drop(writer);
        let copy = dead.path().join("stats.db");
        let before = dir_state(dead.path());
        let a = activity_at(&copy, 371, DAY0 + MS_DAY).unwrap();
        assert_eq!(a.days[0].totals.tokens, 9, "rows only in the -wal are read");
        assert!(dir_state(dead.path()) == before);
    }

    #[test]
    fn activity_creates_no_files_for_a_wal_database_without_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let w = make_db(&db, true);
        msg(&w, DAY0, "p", "m", 4, Some(1.0));
        drop(w); // last connection checkpoints and removes the sidecars
        assert!(is_wal_mode(&db));
        assert!(!sidecar(&db, "-wal").exists() && !sidecar(&db, "-shm").exists());
        let before = dir_state(dir.path());
        let a = activity_at(&db, 371, DAY0 + MS_DAY).unwrap();
        assert_eq!(a.days[0].totals.tokens, 4);
        assert!(dir_state(dir.path()) == before, "no -wal/-shm created");
    }

    #[test]
    fn activity_creates_no_journal_for_a_rollback_journal_database() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("stats.db");
        let c = make_db(&db, false);
        msg(&c, DAY0, "p", "m", 5, Some(1.0));
        drop(c);
        assert!(!is_wal_mode(&db));
        let before = dir_state(dir.path());
        let a = activity_at(&db, 371, DAY0 + MS_DAY).unwrap();
        assert_eq!(a.days[0].totals.tokens, 5);
        assert!(dir_state(dir.path()) == before, "no -journal created");
    }

    #[test]
    fn activity_handles_paths_that_need_uri_escaping() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a b#c?d%e");
        std::fs::create_dir(&sub).unwrap();
        let db = sub.join("stats.db");
        let c = make_db(&db, false);
        msg(&c, DAY0, "p", "m", 3, Some(1.0));
        drop(c);
        assert_eq!(
            activity_at(&db, 371, DAY0 + MS_DAY).unwrap().days[0]
                .totals
                .tokens,
            3
        );
    }

    // ---- heatmap ----

    fn day(y: i32, m: u32, d: u32) -> Date {
        Date::new(y, m, d).unwrap()
    }

    fn act(days: &[(Date, f64, u64)]) -> Activity {
        Activity {
            days: days
                .iter()
                .map(|&(day, cost, requests)| DayActivity {
                    day,
                    totals: DayTotals {
                        cost,
                        requests,
                        tokens: requests * 10,
                    },
                })
                .collect(),
            models: Vec::new(),
        }
    }

    #[test]
    fn date_arithmetic() {
        assert_eq!(day(1970, 1, 1).days(), 0);
        assert_eq!(day(1970, 1, 1).weekday_mon0(), 3, "a Thursday");
        assert_eq!(day(2026, 10, 22).weekday_mon0(), 3, "a Thursday");
        assert_eq!(day(2024, 2, 28).add_days(2), day(2024, 3, 1));
        assert_eq!(Date::parse("2026-10-22"), Some(day(2026, 10, 22)));
        assert_eq!(Date::parse("2026-02-30"), None);
        assert_eq!(day(2026, 10, 22).to_string(), "2026-10-22");
    }

    #[test]
    fn heatmap_levels_grid_and_future_cells() {
        let today = day(2026, 10, 22); // Thursday
        let a = act(&[
            (day(2026, 10, 22), 4.0, 5),
            (day(2026, 10, 21), 1.0, 2),
            (day(2026, 10, 20), 0.01, 1),
            (day(2026, 10, 19), 0.0, 0),
        ]);
        let h = Heatmap::build(&a, today, 53);
        assert_eq!((h.cells.len(), h.cells[0].len()), (7, 53));
        assert!(h.by_cost);
        let last = h.weeks - 1;
        assert_eq!(h.cells[3][last], Some(4), "the busiest day is level 4");
        assert_eq!(h.cells[2][last], Some(2), "ceil(sqrt(1/4)·4) = 2");
        assert_eq!(h.cells[1][last], Some(1), "tiny but non-zero stays visible");
        assert_eq!(h.cells[0][last], Some(0));
        assert_eq!(h.cells[4][last], None, "Friday is after today");
        assert_eq!(h.cells[6][last], None);
        assert_eq!(h.date_at(3, last), today);
        assert_eq!(h.start.weekday_mon0(), 0);
        assert_eq!(h.start, day(2025, 10, 20));
        assert_eq!(h.totals[3][last].requests, 5);

        // with no cost anywhere the request count drives the levels
        let r = Heatmap::build(
            &act(&[(today, 0.0, 4), (day(2026, 10, 21), 0.0, 1)]),
            today,
            53,
        );
        assert!(!r.by_cost);
        assert_eq!((r.cells[3][last], r.cells[2][last]), (Some(4), Some(2)));
        // an empty history is all level 0
        let e = Heatmap::build(&Activity::default(), today, 53);
        assert_eq!(
            (e.cells[3][last], e.active_days, e.longest_streak, e.busiest),
            (Some(0), 0, 0, None)
        );
    }

    #[test]
    fn heatmap_month_labels_mark_month_changes() {
        let today = day(2026, 10, 22);
        let h = Heatmap::build(&Activity::default(), today, 53);
        assert_eq!(h.month_labels.len(), 53);
        assert_eq!(
            h.month_labels[0],
            Some("Oct"),
            "first column always labelled"
        );
        assert_eq!(h.month_labels[1], None);
        let labels: Vec<&str> = h.month_labels.iter().flatten().copied().collect();
        assert_eq!(
            labels,
            [
                "Oct", "Nov", "Dec", "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep",
                "Oct"
            ]
        );
    }

    #[test]
    fn heatmap_totals_streak_busiest_and_last30() {
        let today = day(2026, 10, 22);
        let a = act(&[
            (day(2025, 9, 1), 100.0, 100), // before the grid start (2025-10-20): ignored
            (day(2026, 1, 1), 2.0, 3),
            (day(2026, 1, 2), 9.0, 4), // busiest by cost
            (day(2026, 1, 3), 1.0, 1),
            (day(2026, 1, 5), 1.0, 1), // gap on the 4th
            (day(2026, 10, 21), 0.5, 2),
            (day(2026, 10, 22), 0.5, 1),
            (day(2026, 9, 23), 1.0, 1), // exactly 29 days back: inside last 30
            (day(2026, 9, 22), 7.0, 1), // 30 days back: outside
        ]);
        let h = Heatmap::build(&a, today, 53); // deliberately unsorted input
        assert_eq!(h.active_days, 8);
        assert_eq!(h.longest_streak, 3, "Jan 1–3");
        assert_eq!(h.busiest.unwrap().day, day(2026, 1, 2));
        assert_eq!(h.total_requests, 3 + 4 + 1 + 1 + 2 + 1 + 1 + 1);
        assert!((h.total_cost - (2.0 + 9.0 + 1.0 + 1.0 + 0.5 + 0.5 + 1.0 + 7.0)).abs() < 1e-9);
        assert_eq!(h.last30.requests, 2 + 1 + 1);
        assert!((h.last30.cost - 2.0).abs() < 1e-9);
        assert_eq!(h.last30.daily.len(), 30);
        assert_eq!(h.last30.daily[29], 0.5, "today is the last point");
        assert_eq!(h.last30.daily[0], 1.0, "29 days back is the first");

        // a streak that ends today still counts
        let run = act(&[
            (day(2026, 10, 20), 0.0, 1),
            (day(2026, 10, 21), 0.0, 1),
            (today, 0.0, 1),
        ]);
        assert_eq!(Heatmap::build(&run, today, 53).longest_streak, 3);
    }
}
