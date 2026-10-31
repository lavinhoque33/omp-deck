//! Usage data the window shows: what the gateway's `omp usage --json` fetches and omp's
//! `stats.db` last said, and the pre-formatted view model the chip, the hover cards and the Usage
//! view paint.
//!
//! The gateway owns the fetch schedule (see `app/usage.rs` for the window's side); this module is
//! pure state.

use std::cmp::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_core::ctl::LiveAccount;
use deck_core::usage::{
    AccountPolicy, Activity, CardWindow, Heatmap, LimitStatus, OAuthIdentity, PolicyState,
    ProviderCard, ReportIdentity, UnavailableAccount, UsageLimit, UsageReport, UsageSnapshot,
    format_remaining, limit_fraction, policy_providers, provider_cards, provider_display_name,
    tightest,
};
use deck_service::gateway::protocol::UsageView;

use crate::usage_fmt::{left_pct, left_text, money, requests, requests_short, short_date};
use deck_service::statusfmt::fmt_tokens;

/// Opening the view reads `stats.db` again when the last read is at least this old.
pub const ACTIVITY_EVERY: Duration = Duration::from_secs(600);
/// "resets in" labels are recomputed this often.
pub const RECOMPUTE_EVERY: Duration = Duration::from_secs(30);
/// Rows of one provider card before "N more in Details".
pub const CARD_ROWS: usize = 4;

/// Results of the window's own reads.
pub enum UsageEvent {
    Activity(Result<Activity, String>),
}

/// What the status-bar chip shows.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    /// `63% left`, or `Usage` when no window has a measurable fraction.
    pub text: String,
    pub status: LimitStatus,
    /// `Anthropic · Claude 7 Day` (`Anthropic · me@example.test · Claude 7 Day` when the provider
    /// has several accounts) for the hover header; empty without a tightest window.
    pub tight: String,
}

/// Which window a chip pill stands for; the chip gives each its own colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillKind {
    FiveHour,
    Weekly,
    /// A provider without a 5 h / weekly window: its shortest measured windows instead.
    Other,
}

/// One quota window as the chip paints it.
#[derive(Clone, Debug, PartialEq)]
pub struct QuotaPill {
    pub kind: PillKind,
    /// `5h`, `wk`, or the window's own length (`1d`, `mo`) for [`PillKind::Other`].
    pub tag: String,
    /// `63% left`.
    pub text: String,
    pub status: LimitStatus,
}

const HOUR_MS: i64 = 3_600_000;
/// Window lengths counted as "5 hours" and "a week" (omp reports `durationMs` or ids like `5h`).
const FIVE_HOUR_MS: std::ops::RangeInclusive<i64> = 9 * HOUR_MS / 2..=11 * HOUR_MS / 2;
const WEEK_MS: std::ops::RangeInclusive<i64> = 6 * 24 * HOUR_MS..=8 * 24 * HOUR_MS;

/// The windows of one card the chip shows: its 5-hour and its weekly quota (the fullest of each
/// when there are several, such as per-model weekly limits), else its two shortest measured ones.
fn chip_pills(c: &ProviderCard) -> Vec<QuotaPill> {
    let measured: Vec<&CardWindow> = c.windows.iter().filter(|w| w.fraction.is_some()).collect();
    let fullest = |span: std::ops::RangeInclusive<i64>| {
        measured
            .iter()
            .copied()
            .filter(|w| w.window_ms.is_some_and(|m| span.contains(&m)))
            .max_by(|a, b| {
                a.fraction
                    .partial_cmp(&b.fraction)
                    .unwrap_or(Ordering::Equal)
            })
    };
    let pills: Vec<QuotaPill> = [
        (fullest(FIVE_HOUR_MS), PillKind::FiveHour, "5h"),
        (fullest(WEEK_MS), PillKind::Weekly, "wk"),
    ]
    .into_iter()
    .filter_map(|(w, kind, tag)| w.map(|w| QuotaPill::of(w, kind, tag.to_owned())))
    .collect();
    if !pills.is_empty() {
        return pills;
    }
    let mut rest = measured;
    rest.sort_by_key(|w| w.window_ms.unwrap_or(i64::MAX));
    rest.into_iter()
        .take(2)
        .map(|w| QuotaPill::of(w, PillKind::Other, other_tag(w)))
        .collect()
}

impl QuotaPill {
    fn of(w: &CardWindow, kind: PillKind, tag: String) -> QuotaPill {
        QuotaPill {
            kind,
            tag,
            text: w.fraction.map(left_text).unwrap_or_default(),
            status: w.status,
        }
    }
}

/// `6h`, `1d`, `mo` from the window length, else omp's tag, else the limit's label.
fn other_tag(w: &CardWindow) -> String {
    match w.window_ms {
        Some(ms) if ms < 24 * HOUR_MS => format!("{}h", (ms / HOUR_MS).max(1)),
        Some(ms) if ms < 28 * 24 * HOUR_MS => format!("{}d", ms / (24 * HOUR_MS)),
        Some(_) => "mo".to_owned(),
        None => w.window_tag.clone().unwrap_or_else(|| w.label.clone()),
    }
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

/// What a provider card shows for one account (or for the provider when it has one report).
#[derive(Clone, Debug, PartialEq)]
pub struct CardPage {
    /// The account (`me@example.test`); empty on a single-page card.
    pub account: String,
    pub status: LimitStatus,
    pub unlimited: bool,
    pub rows: Vec<QuotaRow>,
    /// `3 more in Details` / `+3 more`, when rows exceed [`CARD_ROWS`].
    pub more_full: Option<String>,
    pub more_compact: Option<String>,
    pub reset_credits: Option<String>,
    pub daybreak: Vec<String>,
    pub notes: Vec<String>,
    /// Whose report the page shows; `None` on a single-page card.
    pub identity: Option<ReportIdentity>,
    /// The status-bar chip's pills for this account (empty: no measurable window).
    pub pills: Vec<QuotaPill>,
}

impl CardPage {
    fn build(c: &ProviderCard, account: String, identity: Option<ReportIdentity>) -> CardPage {
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
        CardPage {
            account,
            status: c.status(),
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
            notes: c.notes.iter().take(2).cloned().collect(),
            identity,
            pills: chip_pills(c),
        }
    }

    /// Rough height in rows.
    fn weight(&self) -> f32 {
        let extras =
            usize::from(self.reset_credits.is_some()) + self.daybreak.len() + self.notes.len();
        self.rows.len().min(CARD_ROWS) as f32 * 2.0 + extras as f32
    }
}

/// One provider card as painted: a page per account plus the provider-wide extras, all
/// pre-formatted.
#[derive(Clone, Debug, PartialEq)]
pub struct CardView {
    /// omp's provider id: what the shown page is remembered under.
    pub provider: String,
    pub name: String,
    /// `2 accounts` on a single-page card whose provider also has accounts without usage.
    pub accounts: Option<String>,
    /// One page per account that reported usage, each with that account's own numbers; a
    /// single page (unnamed) when the provider has at most one report. Never empty.
    pub pages: Vec<CardPage>,
    /// `work@example.test — no usage endpoint`: the one place an unavailable account shows.
    pub unavailable: Vec<String>,
    pub idle: bool,
    /// Rough height, to balance the two columns.
    pub weight: f32,
}

impl CardView {
    fn build(c: &ProviderCard, snap: &UsageSnapshot) -> CardView {
        let pages: Vec<CardPage> = if c.per_account.is_empty() {
            vec![CardPage::build(c, String::new(), None)]
        } else {
            c.per_account
                .iter()
                .map(|a| CardPage::build(&a.card, a.label.clone(), Some(a.identity.clone())))
                .collect()
        };
        let unavailable: Vec<String> = unavailable_accounts(snap)
            .filter(|a| a.provider == c.provider)
            .map(|a| {
                let who = who_of(a);
                format!("{who} — {}", reason_of(a))
            })
            .collect();
        let pager = if pages.len() > 1 { 1.0 } else { 0.0 };
        let tallest = pages.iter().map(CardPage::weight).fold(0.0, f32::max);
        CardView {
            provider: c.provider.clone(),
            name: c.name.clone(),
            accounts: (pages.len() == 1 && c.accounts > 1)
                .then(|| format!("{} accounts", c.accounts)),
            weight: tallest + pager + unavailable.len() as f32 + 2.0,
            pages,
            unavailable,
            idle: c.idle,
        }
    }

    /// Index of the page named `account`, else the first.
    pub fn page_index(&self, account: Option<&str>) -> usize {
        account
            .and_then(|a| self.pages.iter().position(|p| p.account == a))
            .unwrap_or(0)
    }

    /// The page of the account an omp session uses, when `account` is this provider's and one
    /// page's report matches it (omp's `limitMatchesActiveAccount`); single-page cards: `None`.
    pub fn in_use(&self, account: Option<&LiveAccount>) -> Option<usize> {
        let account = account.filter(|a| a.provider == self.provider)?;
        self.pages.iter().position(|p| {
            p.identity
                .as_ref()
                .is_some_and(|id| id.matches(&account.identity))
        })
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
    /// The Accounts page: providers with several OAuth accounts, or with routing rules.
    pub accounts: Vec<AccountGroup>,
}

/// One provider's OAuth accounts in the order omp prefers them (`auth.accountPolicies`).
#[derive(Clone, Debug, PartialEq)]
pub struct AccountGroup {
    pub provider: String,
    pub name: String,
    /// omp measures this provider's quotas: reserve and reclaim apply.
    pub usage_capable: bool,
    /// omp has routing rules for this provider (else it uses its automatic order).
    pub has_policies: bool,
    /// Highest priority first.
    pub rows: Vec<AccountRow>,
}

/// One account of an [`AccountGroup`].
#[derive(Clone, Debug, PartialEq)]
pub struct AccountRow {
    pub label: String,
    pub identity: OAuthIdentity,
    /// What its rule names it by; `None`: no rule can single it out (omp keeps it automatic).
    pub key: Option<OAuthIdentity>,
    pub pills: Vec<QuotaPill>,
    /// Why it has no usage report.
    pub note: Option<String>,
    /// omp's verdict under the current rules (`None` without rules).
    pub state: Option<PolicyState>,
    /// Its own `reservePct` (`None`: `retry.usageReservePct`).
    pub reserve: Option<u32>,
    pub reclaim: Option<u32>,
}

/// Rounded whole percent, as omp's rules take it.
fn whole_pct(v: f64) -> u32 {
    v.round().clamp(0.0, 100.0) as u32
}

impl AccountGroup {
    fn build(snap: &UsageSnapshot, now_ms: i64) -> Vec<AccountGroup> {
        policy_providers(snap, now_ms)
            .into_iter()
            .filter(|p| p.accounts.len() > 1 || p.has_policies)
            .map(|p| AccountGroup {
                rows: p
                    .accounts
                    .into_iter()
                    .map(|a| AccountRow {
                        pills: a.card.as_ref().map(chip_pills).unwrap_or_default(),
                        note: a.reason,
                        state: a.policy.as_ref().map(|p| p.state),
                        reserve: a
                            .policy
                            .as_ref()
                            .and_then(|p| p.own_reserve())
                            .map(whole_pct),
                        reclaim: a
                            .policy
                            .as_ref()
                            .and_then(|p| p.reclaim_above_pct)
                            .map(whole_pct),
                        label: a.label,
                        identity: a.identity,
                        key: a.key,
                    })
                    .collect(),
                provider: p.provider,
                name: p.name,
                usage_capable: p.usage_capable,
                has_policies: p.has_policies,
            })
            .collect()
    }
}

/// One account as the Accounts page edits it.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftRow {
    pub identity: OAuthIdentity,
    pub reserve: Option<u32>,
    pub reclaim: Option<u32>,
}

impl DraftRow {
    pub fn of(r: &AccountRow) -> DraftRow {
        DraftRow {
            identity: r.identity.clone(),
            reserve: r.reserve,
            reclaim: r.reclaim,
        }
    }
}

/// The rows the Accounts page shows for `group`: the draft's order and values for the accounts
/// still there, then the accounts it does not know; without a draft, the group as omp has it.
/// Accounts with the same identity are told apart by count only.
pub fn merged_draft(group: &AccountGroup, draft: Option<&[DraftRow]>) -> Vec<DraftRow> {
    let in_group = |id: &OAuthIdentity| group.rows.iter().filter(|r| r.identity == *id).count();
    let mut rows: Vec<DraftRow> = Vec::with_capacity(group.rows.len());
    for d in draft.into_iter().flatten() {
        if rows.iter().filter(|r| r.identity == d.identity).count() < in_group(&d.identity) {
            rows.push(d.clone());
        }
    }
    for (i, r) in group.rows.iter().enumerate() {
        let upto = group.rows[..=i]
            .iter()
            .filter(|o| o.identity == r.identity)
            .count();
        if rows.iter().filter(|d| d.identity == r.identity).count() < upto {
            rows.push(DraftRow::of(r));
        }
    }
    rows
}

/// The rules that put `rows` (top first) in that order: falling priorities, each account named by
/// its [`AccountRow::key`]; accounts without one are left out (omp ranks them below every rule,
/// priority 0). Reserve and reclaim only where omp measures quotas; a reclaim at or under the
/// account's reserve (`global_reserve` when it has none) is dropped, as omp refuses it.
pub fn account_policies(
    group: &AccountGroup,
    rows: &[DraftRow],
    global_reserve: u32,
) -> Vec<AccountPolicy> {
    let keyed: Vec<(&DraftRow, OAuthIdentity)> = rows
        .iter()
        .filter_map(|d| {
            let row = group.rows.iter().find(|r| r.identity == d.identity)?;
            Some((d, row.key.clone()?))
        })
        .collect();
    let n = keyed.len() as i64;
    keyed
        .into_iter()
        .enumerate()
        .map(|(i, (d, key))| {
            let (reserve, reclaim) = if group.usage_capable {
                let floor = d.reserve.unwrap_or(global_reserve);
                (d.reserve, d.reclaim.filter(|r| *r > floor && *r <= 100))
            } else {
                (None, None)
            };
            AccountPolicy {
                provider: group.provider.clone(),
                account: key,
                priority: Some(n - i as i64),
                reserve_pct: reserve,
                reclaim_above_pct: reclaim,
            }
        })
        .collect()
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
                Some(t) => Chip {
                    text: left_text(t.used),
                    status: t.status,
                    tight: match t.account {
                        Some(a) => format!("{} · {a} · {}", t.provider, t.window),
                        None => format!("{} · {}", t.provider, t.window),
                    },
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
            accounts: AccountGroup::build(snap, now_ms),
        }
    }

    /// The card of `provider` (omp's provider id).
    pub fn card_of(&self, provider: &str) -> Option<&CardView> {
        self.cards.iter().find(|c| c.provider == provider)
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
    r.metadata.identity().unwrap_or("—").to_owned()
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
    snapshot: Option<UsageSnapshot>,
    summary: Option<Arc<Summary>>,
    summary_at: Option<Instant>,
    /// When the gateway last fetched successfully (unix ms).
    fetched_ms: Option<i64>,
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
    pub fn new(omp_found: bool) -> Usage {
        Usage {
            omp_found,
            snapshot: None,
            summary: None,
            summary_at: None,
            fetched_ms: None,
            fetching: false,
            error: None,
            activity: None,
            activity_at: None,
            activity_busy: false,
            activity_error: None,
        }
    }

    /// omp was installed while OmpDeck runs: quotas can be fetched from now on.
    pub fn omp_found(&mut self, found: bool) {
        self.omp_found = found;
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

    /// Seconds since the gateway's last successful fetch.
    pub fn age_secs(&self, now_ms: i64) -> Option<u64> {
        self.fetched_ms
            .map(|t| u64::try_from(now_ms.saturating_sub(t) / 1000).unwrap_or(0))
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

    /// Takes the gateway's usage view (it owns the schedule and the fetches).
    pub fn adopt(&mut self, view: &UsageView, now_ms: i64) {
        self.fetching = view.fetching;
        self.error.clone_from(&view.error);
        self.fetched_ms = view.fetched_unix_ms.and_then(|ms| i64::try_from(ms).ok());
        if let Some(snap) = &view.snapshot
            && self.snapshot.as_ref() != Some(snap)
        {
            self.snapshot = Some(snap.clone());
            self.recompute(Instant::now(), now_ms);
        }
    }

    pub fn begin_activity(&mut self) {
        self.activity_busy = true;
    }

    pub fn apply(&mut self, ev: UsageEvent, now: Instant) {
        let UsageEvent::Activity(r) = ev;
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

    /// The chip pills of `provider`'s first page; empty without a card or a measured window.
    fn pills<'a>(s: &'a Summary, provider: &str) -> Vec<(PillKind, &'a str, &'a str)> {
        s.card_of(provider)
            .into_iter()
            .flat_map(|c| &c.pages[0].pills)
            .map(|p| (p.kind, p.tag.as_str(), p.text.as_str()))
            .collect()
    }

    #[test]
    fn provider_quota_is_its_five_hour_then_weekly_window() {
        let s = Summary::build(&snap(ONE), 0);
        assert_eq!(
            pills(&s, "anthropic"),
            [
                (PillKind::FiveHour, "5h", "63% left"),
                (PillKind::Weekly, "wk", "19% left"),
            ],
            "the usd-only extra window has no fraction and is not a pill"
        );
        assert!(
            pills(&s, "openai-codex").is_empty(),
            "an account without usage has no windows"
        );
        assert!(pills(&s, "nobody").is_empty());
    }

    #[test]
    fn several_weekly_limits_show_the_fullest_and_other_providers_their_shortest_windows() {
        let s = Summary::build(
            &snap(
                r#"{"generatedAt":1,"reports":[
                {"provider":"anthropic","limits":[
                  {"id":"a","label":"Claude 7 Day","window":{"id":"7d","durationMs":604800000},"amount":{"usedFraction":0.2,"unit":"percent"}},
                  {"id":"b","label":"Claude 7 Day (Opus)","window":{"id":"7d","durationMs":604800000},"amount":{"usedFraction":0.7,"unit":"percent"}}]},
                {"provider":"google-gemini-cli","limits":[
                  {"id":"c","label":"Monthly","window":{"id":"m","durationMs":2592000000},"amount":{"usedFraction":0.1,"unit":"percent"}},
                  {"id":"d","label":"Daily","window":{"id":"d","durationMs":86400000},"amount":{"usedFraction":0.5,"unit":"percent"}},
                  {"id":"e","label":"Unmeasured","window":{"id":"x"},"amount":{"used":3,"unit":"requests"}}]}]}"#,
            ),
            0,
        );
        assert_eq!(
            pills(&s, "anthropic"),
            [(PillKind::Weekly, "wk", "30% left")],
            "no 5 h window: only the weekly pill, from the fullest weekly limit"
        );
        assert_eq!(
            pills(&s, "google-gemini-cli"),
            [
                (PillKind::Other, "1d", "50% left"),
                (PillKind::Other, "mo", "90% left"),
            ]
        );
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

    const ACCOUNTS: &str = r#"{"generatedAt":1,"reports":[
        {"provider":"anthropic","metadata":{"email":"a@x.test"},"limits":[{"id":"anthropic:5h","label":"5h",
         "scope":{"provider":"anthropic","windowId":"5h"},"window":{"id":"5h","durationMs":18000000},
         "amount":{"usedFraction":0.25}}],
         "policy":{"priority":1,"reservePct":20,"reserveSource":"override","reclaimAbovePct":60,"state":"eligible"}},
        {"provider":"anthropic","metadata":{"email":"b@x.test"},"limits":[],
         "policy":{"priority":2,"reservePct":10,"reserveSource":"global","state":"inside-reserve"}}],
        "accountsWithoutUsage":[
         {"provider":"anthropic","type":"oauth","email":"twin@x.test","reason":"backoff"},
         {"provider":"anthropic","type":"oauth","email":"twin@x.test","reason":"backoff"},
         {"provider":"cursor","type":"oauth","email":"solo@x.test"}]}"#;

    #[test]
    fn account_rules_follow_the_shown_order_and_omps_limits() {
        let s = Summary::build(&snap(ACCOUNTS), 0);
        assert_eq!(
            s.accounts.len(),
            1,
            "a provider with one account has nothing to order"
        );
        let g = &s.accounts[0];
        let labels: Vec<&str> = g.rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            ["b@x.test", "a@x.test", "twin@x.test", "twin@x.test"]
        );
        let a = &g.rows[1];
        assert_eq!((a.reserve, a.reclaim), (Some(20), Some(60)));
        assert_eq!(a.pills.len(), 1);
        assert_eq!(g.rows[0].state, Some(PolicyState::InsideReserve));

        // a moves to the top with its reserve cleared; its reclaim (60) stays above the global 10,
        // b gets a reclaim at the global reserve, which omp would refuse.
        let mut rows = merged_draft(g, None);
        rows.swap(0, 1);
        rows[0].reserve = None;
        rows[1].reclaim = Some(10);
        let rules = account_policies(g, &rows, 10);
        assert_eq!(
            serde_json::to_value(&rules).unwrap(),
            serde_json::json!([
                {"provider": "anthropic", "account": {"email": "a@x.test"}, "priority": 2, "reclaimAbovePct": 60},
                {"provider": "anthropic", "account": {"email": "b@x.test"}, "priority": 1},
            ]),
            "the twins cannot be singled out and get no rule"
        );
    }

    #[test]
    fn a_draft_keeps_its_order_across_refreshes() {
        let s = Summary::build(&snap(ACCOUNTS), 0);
        let g = &s.accounts[0];
        let mut draft = merged_draft(g, None);
        draft.reverse();
        // An account that left (logged out) drops out; the rest keep the drafted order.
        draft.insert(
            0,
            DraftRow {
                identity: OAuthIdentity::new(Some("gone@x.test"), None, None, None).unwrap(),
                reserve: None,
                reclaim: None,
            },
        );
        let rows = merged_draft(g, Some(&draft));
        let emails: Vec<&str> = rows
            .iter()
            .map(|r| r.identity.email.as_deref().unwrap())
            .collect();
        assert_eq!(
            emails,
            ["twin@x.test", "twin@x.test", "a@x.test", "b@x.test"]
        );
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
        let five = a.pages[0]
            .rows
            .iter()
            .find(|r| r.label == "Claude 5 Hour")
            .expect("row");
        assert_eq!(five.value.as_deref(), Some("63% left"));
        assert_eq!(five.reset.as_deref(), Some("resets in 2h14m"));
        assert_eq!(
            (a.pages.len(), a.pages[0].account.as_str()),
            (1, ""),
            "one report, one unnamed page"
        );
    }

    #[test]
    fn multi_account_card_has_one_page_per_account_with_its_own_numbers() {
        let s = Summary::build(
            &snap(
                r#"{"generatedAt":1,"reports":[
                {"provider":"anthropic","limits":[{"label":"Claude 7 Day","window":{"id":"7d"},
                  "amount":{"usedFraction":0.9,"unit":"percent"},"status":"warning"}],
                 "metadata":{"email":"a@x.test"}},
                {"provider":"anthropic","limits":[{"label":"Claude 7 Day","window":{"id":"7d"},
                  "amount":{"usedFraction":0.1,"unit":"percent"},"status":"ok"}],
                 "metadata":{"accountId":"acc-b"}}],
                "accountsWithoutUsage":[{"provider":"anthropic","email":"c@x.test","reason":"HTTP 500"}]}"#,
            ),
            0,
        );
        let c = &s.cards[0];
        let pages: Vec<(&str, &str, LimitStatus)> = c
            .pages
            .iter()
            .map(|p| {
                (
                    p.account.as_str(),
                    p.rows[0].value.as_deref().unwrap_or(""),
                    p.status,
                )
            })
            .collect();
        assert_eq!(
            pages,
            [
                ("a@x.test", "10% left", LimitStatus::Warning),
                ("acc-b", "90% left", LimitStatus::Ok),
            ]
        );
        assert_eq!(c.accounts, None, "the pager names the accounts");
        assert_eq!(c.unavailable, ["c@x.test — HTTP 500"]);
        assert_eq!(c.page_index(Some("acc-b")), 1);
        assert_eq!(
            c.page_index(Some("gone@x.test")),
            0,
            "a vanished account falls back"
        );
        let chip = s.chip.as_ref().expect("chip");
        assert_eq!(
            (chip.text.as_str(), chip.tight.as_str()),
            ("10% left", "Anthropic · a@x.test · Claude 7 Day"),
            "the fullest account, not the mean"
        );

        let session = |provider: &str, email: Option<&str>, account_id: Option<&str>| LiveAccount {
            provider: provider.into(),
            identity: deck_core::usage::OAuthIdentity {
                email: email.map(Into::into),
                account_id: account_id.map(Into::into),
                ..Default::default()
            },
            pinned: false,
            credentials: Vec::new(),
        };
        assert_eq!(
            c.in_use(Some(&session("anthropic", None, Some("ACC-B")))),
            Some(1)
        );
        assert_eq!(
            c.in_use(Some(&session("anthropic", Some("a@x.test"), None))),
            Some(0)
        );
        assert_eq!(
            c.in_use(Some(&session("openai-codex", Some("a@x.test"), None))),
            None,
            "another provider's account"
        );
        assert_eq!(
            c.in_use(Some(&session("anthropic", Some("c@x.test"), None))),
            None
        );
        assert_eq!(c.in_use(None), None);
        let wk = |i: usize| {
            c.pages[i]
                .pills
                .iter()
                .map(|p| p.text.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            (wk(0), wk(1)),
            (vec!["10% left"], vec!["90% left"]),
            "chip pills per account"
        );
    }

    #[test]
    fn an_adopted_view_replaces_the_data_and_keeps_the_old_one_on_error() {
        let mut u = Usage::new(true);
        let view = |snapshot, error: Option<&str>| UsageView {
            snapshot,
            fetched_unix_ms: Some(1_000),
            error: error.map(str::to_owned),
            fetching: false,
        };
        u.adopt(&view(Some(snap(ONE)), None), 0);
        assert_eq!(u.load(), Load::Ready);
        u.adopt(&view(Some(snap(ONE)), Some("offline")), 0);
        assert_eq!(u.load(), Load::Ready);
        assert_eq!(u.error(), Some("offline"));
        assert_eq!(u.age_secs(4_000), Some(3));
    }

    #[test]
    fn without_omp_nothing_can_be_refreshed() {
        let u = Usage::new(false);
        assert_eq!(u.load(), Load::OmpMissing);
        assert!(!u.can_refresh());
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
