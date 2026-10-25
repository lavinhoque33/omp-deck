//! Usage data owned by the app: what `omp usage --json` and omp's `stats.db` last said, when to
//! ask again, and the pre-formatted view model the chip, the hover cards and the Usage view paint.
//!
//! Fetching itself runs on worker threads (see `app/usage.rs`); this module is pure state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_core::usage::{
    Activity, Heatmap, LimitStatus, ProviderCard, UnavailableAccount, UsageLimit, UsageReport,
    UsageSnapshot, format_remaining, limit_fraction, provider_cards, provider_display_name,
    tightest,
};

use crate::usage_fmt::{left_pct, left_text, money, requests, requests_short, short_date};
use deck_service::statusfmt::fmt_tokens;

/// First fetch after startup.
pub const FIRST_FETCH_DELAY: Duration = Duration::from_secs(3);
/// Background refresh period (omp caches reports for about as long).
pub const REFRESH_EVERY: Duration = Duration::from_secs(300);
/// Opening the view refreshes data older than this.
pub const OPEN_STALE: Duration = Duration::from_secs(60);
/// Opening the view reads `stats.db` again when the last read is at least this old.
pub const ACTIVITY_EVERY: Duration = Duration::from_secs(600);
/// "resets in" labels are recomputed this often.
pub const RECOMPUTE_EVERY: Duration = Duration::from_secs(30);
/// An open view never retries a failed fetch faster than this on its own.
const RETRY_GAP: Duration = Duration::from_secs(10);
/// Rows of one provider card before "N more in Details".
pub const CARD_ROWS: usize = 4;

/// Results of the workers.
pub enum UsageEvent {
    Fetched(Result<UsageSnapshot, String>),
    Activity(Result<Activity, String>),
}

/// What the status-bar chip shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    /// `63% left`, or `Usage` when no window has a measurable fraction.
    pub text: String,
    pub status: LimitStatus,
    /// `Anthropic · Claude 7 Day` for the hover header; empty without a tightest window.
    pub tight: String,
}

/// One row of a Details table.
#[derive(Clone, Debug, PartialEq)]
pub struct DetailRow {
    pub limit: String,
    pub account: String,
    pub left: String,
    /// Remaining share for the mini bar.
    pub left_frac: Option<f32>,
    pub status: LimitStatus,
    pub resets: String,
}

/// One provider's Details table.
#[derive(Clone, Debug, PartialEq)]
pub struct DetailBlock {
    pub name: String,
    /// `Pro · 2 accounts` style summary under the name.
    pub meta: String,
    pub rows: Vec<DetailRow>,
}

/// One quota row of a provider card, every string formatted.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotaRow {
    pub label: String,
    pub tag: Option<String>,
    /// `63% left` (known fraction) or the amount fallback (`12.3 usd used`).
    pub value: Option<String>,
    /// Whether `value` is a measured percentage (painted strong, in the status colour).
    pub measured: bool,
    pub status: LimitStatus,
    /// Remaining share for the bar; `Some` only with a known fraction.
    pub remaining: Option<f32>,
    /// `resets in 2h14m`, only for rows with a bar.
    pub reset: Option<String>,
}

/// One provider card as painted: the quota rows plus the extras, all pre-formatted.
#[derive(Clone, Debug, PartialEq)]
pub struct CardView {
    pub name: String,
    pub status: LimitStatus,
    /// `2 accounts`.
    pub accounts: Option<String>,
    pub unlimited: bool,
    pub rows: Vec<QuotaRow>,
    /// `3 more in Details` / `+3 more`, when rows exceed [`CARD_ROWS`].
    pub more_full: Option<String>,
    pub more_compact: Option<String>,
    pub reset_credits: Option<String>,
    pub daybreak: Vec<String>,
    /// `work@example.test — no usage endpoint`: the one place an unavailable account shows.
    pub unavailable: Vec<String>,
    pub notes: Vec<String>,
    pub idle: bool,
    /// Rough height, to balance the two columns.
    pub weight: f32,
}

impl CardView {
    fn build(c: &ProviderCard, snap: &UsageSnapshot) -> CardView {
        let rows: Vec<QuotaRow> = c
            .windows
            .iter()
            .map(|w| QuotaRow {
                label: w.label.clone(),
                tag: w.window_tag.clone(),
                value: match (w.fraction, &w.used_text) {
                    (Some(f), _) => Some(left_text(f)),
                    (None, t) => t.clone(),
                },
                measured: w.fraction.is_some(),
                status: w.status,
                remaining: w.fraction.map(|f| (1.0 - f).clamp(0.0, 1.0) as f32),
                reset: w
                    .fraction
                    .and(w.resets_in_ms)
                    .map(crate::usage_fmt::resets_in_text),
            })
            .collect();
        let more = rows.len().saturating_sub(CARD_ROWS);
        let unavailable: Vec<String> = unavailable_accounts(snap)
            .filter(|a| a.provider == c.provider)
            .map(|a| {
                let who = who_of(a);
                format!("{who} — {}", reason_of(a))
            })
            .collect();
        let notes: Vec<String> = c.notes.iter().take(2).cloned().collect();
        let extras = usize::from(c.reset_credits.is_some())
            + unavailable.len()
            + c.daybreak_accounts.len()
            + notes.len();
        CardView {
            name: c.name.clone(),
            status: c.status(),
            accounts: (c.accounts > 1).then(|| format!("{} accounts", c.accounts)),
            unlimited: c.unlimited && c.windows.is_empty(),
            more_full: (more > 0).then(|| format!("{more} more in Details")),
            more_compact: (more > 0).then(|| format!("+{more} more")),
            rows,
            reset_credits: c.reset_credits.as_ref().map(|r| r.text()),
            daybreak: c
                .daybreak_accounts
                .iter()
                .map(|a| format!("daybreak · {a}"))
                .collect(),
            unavailable,
            notes,
            idle: c.idle,
            weight: c.windows.len().min(CARD_ROWS) as f32 * 2.0 + extras as f32 + 2.0,
        }
    }
}

fn unavailable_accounts(snap: &UsageSnapshot) -> impl Iterator<Item = &UnavailableAccount> {
    snap.accounts_without_usage
        .iter()
        .chain(&snap.disabled_credentials)
}

fn reason_of(a: &UnavailableAccount) -> &str {
    a.reason
        .as_deref()
        .filter(|r| !r.is_empty())
        .unwrap_or("usage unavailable")
}

/// The chip shows only when some report carries limits; accounts without usage never make one.
pub fn chip_visible(snap: &UsageSnapshot) -> bool {
    snap.reports.iter().any(|r| !r.limits.is_empty())
}

/// Everything derived from one snapshot at one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct Summary {
    pub cards: Vec<CardView>,
    pub chip: Option<Chip>,
    pub details: Vec<DetailBlock>,
    /// `Untouched: A · B` footnote for providers with nothing used; empty when none.
    pub untouched: String,
    /// Unavailable accounts that name no provider, so no card can hold them.
    pub orphans: Vec<String>,
    /// Set when no report has limits yet accounts exist that produced nothing (all fetches
    /// failed, backoff, disabled): Overview and Details both show it instead of their content.
    pub warning: Option<NoDataWarning>,
}

/// "No provider returned usage" with the per-account reasons.
#[derive(Clone, Debug, PartialEq)]
pub struct NoDataWarning {
    pub title: String,
    /// `Provider · who — reason`, one per unavailable account.
    pub accounts: Vec<String>,
}

/// Warning state for a snapshot: no report with limits, at least one unavailable account.
pub fn no_data_warning(snap: &UsageSnapshot) -> Option<NoDataWarning> {
    if chip_visible(snap) {
        return None;
    }
    let accounts: Vec<String> = unavailable_accounts(snap)
        .map(|a| {
            format!(
                "{} · {} — {}",
                provider_display_name(&a.provider),
                who_of(a),
                reason_of(a)
            )
        })
        .collect();
    if accounts.is_empty() {
        return None;
    }
    let n = accounts.len();
    Some(NoDataWarning {
        title: format!(
            "No provider returned usage — {n} account{} unavailable",
            if n == 1 { "" } else { "s" }
        ),
        accounts,
    })
}

fn who_of(a: &UnavailableAccount) -> &str {
    a.display_label()
}

impl Summary {
    pub fn build(snap: &UsageSnapshot, now_ms: i64) -> Summary {
        let provider_cards = provider_cards(snap, now_ms);
        let chip = if chip_visible(snap) {
            Some(match tightest(&provider_cards) {
                Some((provider, label, used, status)) => Chip {
                    text: left_text(used),
                    status,
                    tight: format!("{provider} · {label}"),
                },
                None => Chip {
                    text: "Usage".to_owned(),
                    status: LimitStatus::Unknown,
                    tight: String::new(),
                },
            })
        } else {
            None
        };
        let names = provider_cards
            .iter()
            .filter(|c| c.idle)
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(" · ");
        let untouched = if names.is_empty() {
            names
        } else {
            format!("Untouched: {names}")
        };
        let orphans = unavailable_accounts(snap)
            .filter(|a| a.provider.is_empty())
            .map(|a| {
                let who = who_of(a);
                format!(
                    "{} · {who} — {}",
                    provider_display_name(&a.provider),
                    reason_of(a)
                )
            })
            .collect();
        Summary {
            details: detail_blocks(&snap.reports, now_ms),
            cards: provider_cards
                .iter()
                .map(|c| CardView::build(c, snap))
                .collect(),
            chip,
            untouched,
            orphans,
            warning: no_data_warning(snap),
        }
    }
}

fn status_of(l: &UsageLimit, fraction: Option<f64>) -> LimitStatus {
    if l.status != LimitStatus::Unknown {
        return l.status;
    }
    match fraction {
        Some(f) if f >= 1.0 => LimitStatus::Exhausted,
        Some(f) if f >= 0.9 => LimitStatus::Warning,
        Some(_) => LimitStatus::Ok,
        None => LimitStatus::Unknown,
    }
}

/// `37.7 usd`, `1,200 requests` for a limit with no fraction.
fn amount_text(l: &UsageLimit) -> String {
    let a = &l.amount;
    let num = |v: f64| {
        if v.fract().abs() < 0.005 {
            crate::statusbar::group_digits(v.max(0.0) as u64)
        } else {
            format!("{v:.2}")
        }
    };
    let unit = if a.unit.is_empty() || a.unit == "unknown" {
        ""
    } else {
        a.unit.as_str()
    };
    let text = match (a.remaining, a.used, a.limit) {
        (Some(r), _, Some(l)) => format!("{} of {}", num(r), num(l)),
        (Some(r), _, None) => format!("{} left", num(r)),
        (None, Some(u), Some(l)) => format!("{} of {} used", num(u), num(l)),
        (None, Some(u), None) => format!("{} used", num(u)),
        _ => return "—".to_owned(),
    };
    if unit.is_empty() {
        text
    } else {
        format!("{text} {unit}")
    }
}

fn account_of(r: &UsageReport) -> String {
    let m = &r.metadata;
    [&m.email, &m.org_name, &m.account_id]
        .into_iter()
        .flatten()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| "—".to_owned())
}

fn detail_blocks(reports: &[UsageReport], now_ms: i64) -> Vec<DetailBlock> {
    let mut blocks: Vec<(String, DetailBlock)> = Vec::new();
    for r in reports {
        let idx = match blocks.iter().position(|(p, _)| *p == r.provider) {
            Some(i) => i,
            None => {
                blocks.push((
                    r.provider.clone(),
                    DetailBlock {
                        name: provider_display_name(&r.provider),
                        meta: String::new(),
                        rows: Vec::new(),
                    },
                ));
                blocks.len() - 1
            }
        };
        let account = account_of(r);
        let block = &mut blocks[idx].1;
        for l in &r.limits {
            let fraction = limit_fraction(l);
            let (left, left_frac) = match fraction {
                Some(f) => (
                    format!("{}%", left_pct(f)),
                    Some((1.0 - f).clamp(0.0, 1.0) as f32),
                ),
                None => (amount_text(l), None),
            };
            let resets = match l.window.as_ref() {
                Some(w) => match (w.resets_at_ms, w.reset_label.as_deref()) {
                    (Some(at), _) => format!("in {}", format_remaining(at - now_ms)),
                    (None, Some(label)) if !label.is_empty() => label.to_owned(),
                    _ => "—".to_owned(),
                },
                None => "—".to_owned(),
            };
            block.rows.push(DetailRow {
                limit: l.label.clone(),
                account: account.clone(),
                left,
                left_frac,
                status: status_of(l, fraction),
                resets,
            });
        }
        // `Pro · 2 accounts`: plan of the first account, then the account count.
        let plan = r
            .metadata
            .plan_type
            .as_deref()
            .or(r.metadata.tier.as_deref())
            .filter(|p| !p.is_empty());
        if block.meta.is_empty()
            && let Some(p) = plan
        {
            block.meta = p.to_owned();
        }
    }
    let mut out: Vec<DetailBlock> = blocks.into_iter().map(|(_, b)| b).collect();
    for b in &mut out {
        // Grouped by limit, then account (stable: omp's own order within a group).
        b.rows.sort_by(|a, c| {
            a.limit
                .cmp(&c.limit)
                .then_with(|| a.account.cmp(&c.account))
        });
    }
    for b in &mut out {
        let accounts = reports
            .iter()
            .filter(|r| provider_display_name(&r.provider) == b.name)
            .count();
        if accounts > 1 {
            let tail = format!("{accounts} accounts");
            b.meta = if b.meta.is_empty() {
                tail
            } else {
                format!("{} · {tail}", b.meta)
            };
        }
    }
    out
}

// ------------------------------------------------------------------ activity view model

/// One row of "Top models, last 30 days".
#[derive(Clone, Debug, PartialEq)]
pub struct ModelRow {
    pub model: String,
    pub provider: String,
    pub cost: String,
    pub requests: String,
    pub tokens: String,
    /// Share of the busiest model, for the inline bar.
    pub share: f32,
}

/// One stat tile of the strip.
#[derive(Clone, Debug, PartialEq)]
pub struct Tile {
    pub value: String,
    pub sub: String,
}

/// `stats.db` as the view paints it.
#[derive(Clone, Debug, PartialEq)]
pub struct ActivityView {
    pub heatmap: Heatmap,
    pub past_year: Tile,
    pub last30: Tile,
    pub busiest: Option<Tile>,
    pub models: Vec<ModelRow>,
    pub empty: bool,
}

impl ActivityView {
    pub fn build(a: &Activity, heatmap: Heatmap) -> ActivityView {
        let by_cost = heatmap.by_cost;
        let value = |cost: f64, req: u64| if by_cost { money(cost) } else { requests(req) };
        let sub = |req: u64, tokens: u64| {
            if by_cost {
                format!("{} · {} tok", requests_short(req), fmt_tokens(tokens))
            } else {
                format!("{} tokens", fmt_tokens(tokens))
            }
        };
        let past_year = Tile {
            value: value(heatmap.total_cost, heatmap.total_requests),
            sub: sub(heatmap.total_requests, heatmap.total_tokens),
        };
        let l = &heatmap.last30;
        let last30 = Tile {
            value: value(l.cost, l.requests),
            sub: sub(l.requests, l.tokens),
        };
        let busiest = heatmap.busiest.map(|d| Tile {
            value: value(d.totals.cost, d.totals.requests),
            sub: if by_cost {
                format!(
                    "{} · {}",
                    short_date(d.day),
                    requests_short(d.totals.requests)
                )
            } else {
                short_date(d.day)
            },
        });
        let top = a.models.iter().map(|m| m.cost).fold(0.0_f64, f64::max);
        let top_req = a.models.iter().map(|m| m.requests).max().unwrap_or(0);
        let models = a
            .models
            .iter()
            .map(|m| ModelRow {
                model: m.model.clone(),
                provider: provider_display_name(&m.provider),
                cost: money(m.cost),
                requests: crate::statusbar::group_digits(m.requests),
                tokens: fmt_tokens(m.tokens),
                share: if top > 0.0 {
                    (m.cost / top) as f32
                } else if top_req > 0 {
                    m.requests as f32 / top_req as f32
                } else {
                    0.0
                },
            })
            .collect();
        ActivityView {
            empty: a.days.is_empty(),
            heatmap,
            past_year,
            last30,
            busiest,
            models,
        }
    }
}

// ------------------------------------------------------------------ state

/// The app's usage state.
pub struct Usage {
    omp_found: bool,
    started: Instant,
    snapshot: Option<UsageSnapshot>,
    summary: Option<Arc<Summary>>,
    summary_at: Option<Instant>,
    fetched: Option<Instant>,
    last_attempt: Option<Instant>,
    fetching: bool,
    error: Option<String>,
    activity: Option<Arc<ActivityView>>,
    activity_at: Option<Instant>,
    activity_busy: bool,
    activity_error: Option<String>,
}

/// Loading / error / data, for the view's empty states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Load {
    OmpMissing,
    Loading,
    Failed,
    Ready,
}

impl Usage {
    pub fn new(omp_found: bool, now: Instant) -> Usage {
        Usage {
            omp_found,
            started: now,
            snapshot: None,
            summary: None,
            summary_at: None,
            fetched: None,
            last_attempt: None,
            fetching: false,
            error: None,
            activity: None,
            activity_at: None,
            activity_busy: false,
            activity_error: None,
        }
    }

    /// omp was installed while OmpDeck runs: quotas can be fetched from now on.
    pub fn omp_found(&mut self) {
        self.omp_found = true;
    }

    pub fn summary(&self) -> Option<&Arc<Summary>> {
        self.summary.as_ref()
    }

    pub fn activity(&self) -> Option<&Arc<ActivityView>> {
        self.activity.as_ref()
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn activity_error(&self) -> Option<&str> {
        self.activity_error.as_deref()
    }

    pub fn fetching(&self) -> bool {
        self.fetching
    }

    pub fn activity_loading(&self) -> bool {
        self.activity_busy
    }

    pub fn load(&self) -> Load {
        if !self.omp_found {
            Load::OmpMissing
        } else if self.summary.is_some() {
            Load::Ready
        } else if self.error.is_some() && !self.fetching {
            Load::Failed
        } else {
            Load::Loading
        }
    }

    /// Seconds since the last successful fetch.
    pub fn age_secs(&self, now: Instant) -> Option<u64> {
        self.fetched
            .map(|t| now.saturating_duration_since(t).as_secs())
    }

    /// Whether a background refresh is due now (startup delay, then every five minutes).
    pub fn auto_due(&self, now: Instant) -> bool {
        if self.fetching || !self.omp_found {
            return false;
        }
        match self.last_attempt {
            None => now.saturating_duration_since(self.started) >= FIRST_FETCH_DELAY,
            Some(t) => now.saturating_duration_since(t) >= REFRESH_EVERY,
        }
    }

    /// When the next background refresh falls due (for repaint scheduling).
    pub fn next_due(&self, now: Instant) -> Option<Duration> {
        if self.fetching || !self.omp_found {
            return None;
        }
        let at = match self.last_attempt {
            None => self.started + FIRST_FETCH_DELAY,
            Some(t) => t + REFRESH_EVERY,
        };
        Some(at.saturating_duration_since(now))
    }

    /// Whether opening the view should fetch: data older than a minute (or none), not retried
    /// within seconds of the last attempt.
    pub fn open_due(&self, now: Instant) -> bool {
        if self.fetching || !self.omp_found {
            return false;
        }
        let stale = self
            .fetched
            .is_none_or(|t| now.saturating_duration_since(t) > OPEN_STALE);
        let calm = self
            .last_attempt
            .is_none_or(|t| now.saturating_duration_since(t) >= RETRY_GAP);
        stale && calm
    }

    /// Whether opening the view should read `stats.db` again.
    pub fn activity_due(&self, now: Instant) -> bool {
        !self.activity_busy
            && self
                .activity_at
                .is_none_or(|t| now.saturating_duration_since(t) >= ACTIVITY_EVERY)
    }

    /// A manual refresh: refused only while a fetch is already running.
    pub fn can_refresh(&self) -> bool {
        self.omp_found && !self.fetching
    }

    pub fn begin_fetch(&mut self, now: Instant) {
        self.fetching = true;
        self.last_attempt = Some(now);
    }

    pub fn begin_activity(&mut self) {
        self.activity_busy = true;
    }

    /// Whether the "resets in" labels should be recomputed.
    pub fn recompute_due(&self, now: Instant) -> bool {
        self.snapshot.is_some()
            && self
                .summary_at
                .is_none_or(|t| now.saturating_duration_since(t) >= RECOMPUTE_EVERY)
    }

    pub fn recompute(&mut self, now: Instant, now_ms: i64) {
        if let Some(s) = &self.snapshot {
            self.summary = Some(Arc::new(Summary::build(s, now_ms)));
            self.summary_at = Some(now);
        }
    }

    pub fn apply(&mut self, ev: UsageEvent, now: Instant, now_ms: i64) {
        match ev {
            UsageEvent::Fetched(Ok(snap)) => {
                self.fetching = false;
                self.error = None;
                self.fetched = Some(now);
                self.snapshot = Some(snap);
                self.recompute(now, now_ms);
            }
            UsageEvent::Fetched(Err(e)) => {
                self.fetching = false;
                self.error = Some(e);
            }
            UsageEvent::Activity(r) => {
                self.activity_busy = false;
                self.activity_at = Some(now);
                match r {
                    Ok(a) => {
                        self.activity_error = None;
                        let today = deck_core::usage::today_local();
                        let heatmap = Heatmap::build(&a, today, 53);
                        self.activity = Some(Arc::new(ActivityView::build(&a, heatmap)));
                    }
                    Err(e) => self.activity_error = Some(e),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(json: &str) -> UsageSnapshot {
        UsageSnapshot::parse(json).expect("fixture parses")
    }

    const ONE: &str = r#"{"generatedAt":1000,"reports":[{"provider":"anthropic","fetchedAt":1000,
      "limits":[
       {"id":"a:5h","label":"Claude 5 Hour","scope":{"windowId":"5h"},"window":{"id":"5h","label":"5 Hour","resetsAt":8040000},
        "amount":{"usedFraction":0.37,"unit":"percent"},"status":"ok"},
       {"id":"a:7d","label":"Claude 7 Day","scope":{"windowId":"7d"},"window":{"id":"7d","label":"7 Day","resetsAt":277200000},
        "amount":{"usedFraction":0.815,"unit":"percent"},"status":"ok"},
       {"id":"a:extra","label":"Claude Extra Usage","scope":{"windowId":"extra"},
        "amount":{"used":12.34,"limit":50,"remaining":37.66,"unit":"usd"},"status":"ok"}],
      "metadata":{"email":"me@example.test","planType":"pro"}}],
      "accountsWithoutUsage":[{"provider":"openai-codex","label":"work@example.test","reason":"no usage endpoint"}],
      "disabledCredentials":[]}"#;

    #[test]
    fn chip_reads_the_tightest_window() {
        let s = Summary::build(&snap(ONE), 0);
        let chip = s.chip.expect("chip");
        assert_eq!(chip.text, "19% left");
        assert_eq!(chip.tight, "Anthropic · Claude 7 Day");
        assert_eq!(chip.status, LimitStatus::Ok);
    }

    #[test]
    fn no_reports_means_no_chip() {
        let s = Summary::build(&snap(r#"{"generatedAt":1,"reports":[]}"#), 0);
        assert!(s.chip.is_none());
        assert!(s.cards.is_empty());
    }

    #[test]
    fn details_rows_show_left_and_resets() {
        let s = Summary::build(&snap(ONE), 0);
        let block = &s.details[0];
        assert_eq!(block.name, "Anthropic");
        assert_eq!(block.meta, "pro");
        let five = block
            .rows
            .iter()
            .find(|r| r.limit == "Claude 5 Hour")
            .expect("row");
        assert_eq!(five.left, "63%");
        assert_eq!(five.account, "me@example.test");
        assert_eq!(five.resets, "in 2h14m");
        let extra = block
            .rows
            .iter()
            .find(|r| r.limit == "Claude Extra Usage")
            .expect("row");
        assert_eq!(extra.resets, "—");
    }

    #[test]
    fn unavailable_account_shows_once_in_its_provider_card_with_reason() {
        let s = Summary::build(&snap(ONE), 0);
        assert!(s.orphans.is_empty());
        let rows: Vec<&String> = s.cards.iter().flat_map(|c| &c.unavailable).collect();
        assert_eq!(rows, ["work@example.test — no usage endpoint"]);
        let codex = s.cards.iter().find(|c| c.name == "Codex").expect("card");
        assert_eq!(codex.unavailable.len(), 1);
    }

    #[test]
    fn account_without_provider_is_listed_once_as_an_orphan() {
        let s = Summary::build(
            &snap(
                r#"{"generatedAt":1,"reports":[],
                "disabledCredentials":[{"provider":"","label":"x@y.test","reason":"expired"}]}"#,
            ),
            0,
        );
        assert_eq!(s.orphans.len(), 1);
        assert!(s.orphans[0].ends_with("x@y.test — expired"));
        assert!(s.cards.iter().all(|c| c.unavailable.is_empty()));
    }

    #[test]
    fn chip_needs_a_report_with_limits() {
        let only_unavailable = Summary::build(
            &snap(
                r#"{"generatedAt":1,"reports":[],
                "accountsWithoutUsage":[{"provider":"cursor","label":"a","reason":"nope"}]}"#,
            ),
            0,
        );
        assert!(only_unavailable.chip.is_none());
        assert_eq!(only_unavailable.cards.len(), 1, "the view still lists it");
        assert_eq!(only_unavailable.cards[0].unavailable, ["a — nope"]);
        let empty_report = Summary::build(
            &snap(r#"{"generatedAt":1,"reports":[{"provider":"ollama","limits":[]}]}"#),
            0,
        );
        assert!(empty_report.chip.is_none());
        assert!(Summary::build(&snap(ONE), 0).chip.is_some());
    }

    #[test]
    fn all_accounts_unavailable_is_one_warning_state() {
        let s = Summary::build(
            &snap(
                r#"{"generatedAt":1,"reports":[],
                "accountsWithoutUsage":[{"provider":"anthropic","email":"a@x.test","reason":"HTTP 500"}],
                "disabledCredentials":[{"provider":"openai-codex","accountId":"acc-1","reason":"expired"}]}"#,
            ),
            0,
        );
        let w = s.warning.expect("warning");
        assert_eq!(
            w.title,
            "No provider returned usage — 2 accounts unavailable"
        );
        assert_eq!(
            w.accounts,
            ["Anthropic · a@x.test — HTTP 500", "Codex · acc-1 — expired"]
        );
        let one = no_data_warning(&snap(
            r#"{"generatedAt":1,"reports":[],
            "accountsWithoutUsage":[{"provider":"cursor","label":"a","reason":"nope"}]}"#,
        ))
        .expect("warning");
        assert_eq!(
            one.title,
            "No provider returned usage — 1 account unavailable"
        );
    }

    #[test]
    fn no_warning_with_limits_or_without_accounts() {
        assert!(
            Summary::build(&snap(ONE), 0).warning.is_none(),
            "has limits"
        );
        let none = snap(r#"{"generatedAt":1,"reports":[]}"#);
        assert!(no_data_warning(&none).is_none(), "nothing to warn about");
    }

    #[test]
    fn card_rows_are_preformatted() {
        let s = Summary::build(&snap(ONE), 0);
        let a = s
            .cards
            .iter()
            .find(|c| c.name == "Anthropic")
            .expect("card");
        let five = a
            .rows
            .iter()
            .find(|r| r.label == "Claude 5 Hour")
            .expect("row");
        assert_eq!(five.value.as_deref(), Some("63% left"));
        assert_eq!(five.reset.as_deref(), Some("resets in 2h14m"));
    }

    #[test]
    fn schedule_waits_three_seconds_then_five_minutes() {
        let t0 = Instant::now();
        let mut u = Usage::new(true, t0);
        assert!(!u.auto_due(t0 + Duration::from_secs(2)));
        assert!(u.auto_due(t0 + Duration::from_secs(3)));
        u.begin_fetch(t0 + Duration::from_secs(3));
        assert!(!u.auto_due(t0 + Duration::from_secs(400)), "one at a time");
        u.apply(
            UsageEvent::Fetched(Err("boom".to_owned())),
            t0 + Duration::from_secs(4),
            0,
        );
        assert_eq!(u.load(), Load::Failed);
        assert!(!u.auto_due(t0 + Duration::from_secs(200)));
        assert!(u.auto_due(t0 + Duration::from_secs(303)));
    }

    #[test]
    fn opening_refreshes_only_stale_data() {
        let t0 = Instant::now();
        let mut u = Usage::new(true, t0);
        assert!(u.open_due(t0 + Duration::from_secs(1)));
        u.begin_fetch(t0);
        u.apply(
            UsageEvent::Fetched(Ok(snap(ONE))),
            t0 + Duration::from_secs(1),
            0,
        );
        assert_eq!(u.load(), Load::Ready);
        assert!(!u.open_due(t0 + Duration::from_secs(50)));
        assert!(u.open_due(t0 + Duration::from_secs(70)));
        assert!(u.activity_due(t0));
    }

    #[test]
    fn without_omp_nothing_is_fetched() {
        let t0 = Instant::now();
        let u = Usage::new(false, t0);
        assert_eq!(u.load(), Load::OmpMissing);
        assert!(!u.auto_due(t0 + Duration::from_secs(999)));
        assert!(!u.open_due(t0 + Duration::from_secs(999)));
        assert!(!u.can_refresh());
    }

    #[test]
    fn a_failed_refresh_keeps_the_old_data() {
        let t0 = Instant::now();
        let mut u = Usage::new(true, t0);
        u.apply(UsageEvent::Fetched(Ok(snap(ONE))), t0, 0);
        u.apply(UsageEvent::Fetched(Err("offline".to_owned())), t0, 0);
        assert_eq!(u.load(), Load::Ready);
        assert_eq!(u.error(), Some("offline"));
        assert!(u.summary().is_some());
    }

    #[test]
    fn model_rows_scale_against_the_top_model() {
        use deck_core::usage::ModelActivity;
        let a = Activity {
            days: Vec::new(),
            models: vec![
                ModelActivity {
                    provider: "anthropic".to_owned(),
                    model: "claude-a".to_owned(),
                    cost: 10.0,
                    requests: 4,
                    tokens: 1_500_000,
                },
                ModelActivity {
                    provider: "openai-codex".to_owned(),
                    model: "gpt-x".to_owned(),
                    cost: 2.5,
                    requests: 1,
                    tokens: 900,
                },
            ],
        };
        let today = deck_core::usage::Date::new(2026, 3, 14).expect("date");
        let v = ActivityView::build(&a, Heatmap::build(&a, today, 53));
        assert!(v.empty);
        assert_eq!(v.models[0].cost, "$10.00");
        assert_eq!(v.models[0].tokens, "1.5M");
        assert_eq!(v.models[1].provider, "Codex");
        assert!((v.models[1].share - 0.25).abs() < 1e-6);
    }
}
