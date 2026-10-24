//! Pure model of the agents dashboard: what a session card shows, which cards match the search and
//! status filter, their order, and the responsive grid geometry. No egui here: the view
//! (`dashboard.rs`) paints what this module computed once per change, never per frame.

use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use deck_core::fleet::FleetSession;
use deck_core::sessions::abbreviate_home;

use crate::agent_model::short_model;
use crate::sidebar::format_age;
use crate::statusfmt::{
    Activity, ContextLevel, context_level, context_percent, fmt_cost, fmt_tokens,
};
use crate::tabs::TabStatus;

// ------------------------------------------------------------------ status

/// What a card says its session is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    /// omp waits for the user (`π !`, or a bell on a background tab).
    Attention,
    Busy,
    Idle,
    /// Alive in a backend but its terminal title says nothing about omp's state (yet).
    Running,
    /// The tab's process ended.
    Exited {
        clean: bool,
    },
}

impl Status {
    /// A tab: the tab strip's own indicator decides.
    pub fn of_tab(s: TabStatus) -> Status {
        match s {
            TabStatus::Exited { clean } => Status::Exited { clean },
            TabStatus::Attention | TabStatus::Bell => Status::Attention,
            TabStatus::Busy => Status::Busy,
            TabStatus::Idle => Status::Idle,
        }
    }

    /// A live backend session without a tab: only its terminal title tells.
    pub fn of_backend(a: Activity) -> Status {
        match a {
            Activity::Unknown => Status::Running,
            Activity::Idle => Status::Idle,
            Activity::Busy => Status::Busy,
            Activity::Attention => Status::Attention,
        }
    }

    /// The tab strip's indicator for this status (`Running` has none of its own: idle look).
    pub fn as_tab(self) -> TabStatus {
        match self {
            Status::Attention => TabStatus::Attention,
            Status::Busy => TabStatus::Busy,
            Status::Idle | Status::Running => TabStatus::Idle,
            Status::Exited { clean } => TabStatus::Exited { clean },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Status::Attention => "Needs input",
            Status::Busy => "Working",
            Status::Idle => "Idle",
            Status::Running => "Running",
            Status::Exited { clean: true } => "Exited",
            Status::Exited { clean: false } => "Exited with error",
        }
    }

    /// Wants the user: waiting for input, or crashed.
    pub fn needs_attention(self) -> bool {
        matches!(self, Status::Attention | Status::Exited { clean: false })
    }

    /// Sort rank: attention first, then busy, then the rest.
    fn rank(self) -> u8 {
        if self.needs_attention() {
            0
        } else if self == Status::Busy {
            1
        } else {
            2
        }
    }
}

/// The status chips of the header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum StatusFilter {
    #[default]
    All,
    Attention,
    Busy,
    Idle,
}

pub const FILTERS: [StatusFilter; 4] = [
    StatusFilter::All,
    StatusFilter::Attention,
    StatusFilter::Busy,
    StatusFilter::Idle,
];

impl StatusFilter {
    pub fn label(self) -> &'static str {
        match self {
            StatusFilter::All => "All",
            StatusFilter::Attention => "Needs attention",
            StatusFilter::Busy => "Busy",
            StatusFilter::Idle => "Idle",
        }
    }

    pub fn admits(self, s: Status) -> bool {
        match self {
            StatusFilter::All => true,
            StatusFilter::Attention => s.needs_attention(),
            StatusFilter::Busy => s == Status::Busy,
            StatusFilter::Idle => matches!(
                s,
                Status::Idle | Status::Running | Status::Exited { clean: true }
            ),
        }
    }
}

// ------------------------------------------------------------------ cards

/// Identity of a card: an open tab, or a live backend session nobody has a tab for.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CardKey {
    Tab(u64),
    Backend(String),
}

/// What the app knows about one session before the fleet watcher reports.
pub struct CardSrc<'a> {
    pub key: CardKey,
    pub title: &'a str,
    pub cwd: &'a Path,
    pub session: Option<&'a Path>,
    pub id: &'a str,
    pub status: Status,
    pub activity_at: Option<SystemTime>,
    /// This card's tab is the active one.
    pub active: bool,
}

/// Facts the card needs from outside the fleet snapshot.
#[derive(Default)]
pub struct Extra {
    /// Model display name from the catalog; the id part of the selector without it.
    pub model_name: Option<String>,
    /// Context window of the model from the catalog.
    pub window: Option<u64>,
    /// Live `(used tokens, window)` from the control link (active tab only).
    pub live_ctx: Option<(u64, u64)>,
}

/// The context meter of a card.
#[derive(Clone, Debug, PartialEq)]
pub struct Ctx {
    /// Fill 0..=1; `None` when the window is unknown.
    pub frac: Option<f32>,
    pub label: String,
    pub level: Option<ContextLevel>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Files {
    pub staged: u32,
    pub unstaged: u32,
    pub untracked: u32,
}

impl Files {
    pub fn is_clean(&self) -> bool {
        self.staged + self.unstaged + self.untracked == 0
    }
}

/// Numbers read from the session file (absent until the fleet watcher read it).
#[derive(Clone, Debug, PartialEq)]
pub struct Metrics {
    pub model: String,
    pub thinking: Option<String>,
    pub ctx: Option<Ctx>,
    pub cost: String,
    pub input: String,
    pub output: String,
    pub last_reply: Option<String>,
    pub failed: bool,
}

/// One session card, formatted.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub key: CardKey,
    pub session: Option<PathBuf>,
    pub title: String,
    pub cwd: PathBuf,
    /// `~`-abbreviated working directory.
    pub project: String,
    pub id: String,
    pub status: Status,
    pub activity_at: Option<SystemTime>,
    /// `3m`, `2h`… of the last activity; empty when unknown.
    pub age: String,
    pub active: bool,
    pub metrics: Option<Metrics>,
    pub branch: Option<String>,
    pub worktree: bool,
    pub files: Option<Files>,
    /// Lowercase `title project branch` for the search box.
    hay: String,
    id_lc: String,
}

impl Card {
    pub fn build(
        src: &CardSrc<'_>,
        fleet: Option<&FleetSession>,
        extra: &Extra,
        now: SystemTime,
    ) -> Card {
        let project = abbreviate_home(src.cwd);
        let (branch, worktree, files) = match fleet.and_then(|f| f.git.as_ref()) {
            Some(g) => (
                Some(g.branch.clone()),
                g.worktree_of.is_some(),
                Some(Files {
                    staged: g.staged,
                    unstaged: g.unstaged,
                    untracked: g.untracked,
                }),
            ),
            None => (None, false, None),
        };
        let metrics = fleet.map(|f| Metrics {
            model: f
                .model
                .as_deref()
                .map(|m| extra.model_name.clone().unwrap_or_else(|| short_model(m)))
                .unwrap_or_default(),
            thinking: f.thinking.clone().filter(|t| t != "off"),
            ctx: context(f.context_tokens, extra),
            cost: fmt_cost(f.cost),
            input: fmt_tokens(f.input_tokens),
            output: fmt_tokens(f.output_tokens),
            last_reply: f.last_reply.clone().filter(|r| !r.is_empty()),
            failed: f.last_turn_failed,
        });
        let hay = format!(
            "{}\n{}\n{}",
            src.title,
            project,
            branch.as_deref().unwrap_or("")
        )
        .to_lowercase();
        Card {
            key: src.key.clone(),
            session: src.session.map(Path::to_path_buf),
            title: src.title.to_owned(),
            cwd: src.cwd.to_path_buf(),
            project,
            id: src.id.to_owned(),
            status: src.status,
            activity_at: src.activity_at,
            age: src
                .activity_at
                .map_or_else(String::new, |t| format_age(now, t)),
            active: src.active,
            metrics,
            branch,
            worktree,
            files,
            hay,
            id_lc: src.id.to_lowercase(),
        }
    }

    pub fn shape(&self, has_agents: bool) -> Shape {
        Shape {
            metrics: self.metrics.is_some(),
            git: self.files.is_some(),
            reply: self
                .metrics
                .as_ref()
                .is_some_and(|m| m.last_reply.is_some()),
            agents: has_agents,
        }
    }
}

/// The context meter: live control's numbers win over the file's.
fn context(file_tokens: Option<u64>, extra: &Extra) -> Option<Ctx> {
    let (tokens, window) = match extra.live_ctx {
        Some((used, window)) if window > 0 => (Some(used), Some(window)),
        _ => (file_tokens, extra.window),
    };
    let tokens = tokens?;
    match context_percent(Some(tokens), window) {
        Some(pct) => Some(Ctx {
            frac: Some((pct / 100.0).clamp(0.0, 1.0) as f32),
            label: format!("{pct:.1}%"),
            level: window.map(|w| context_level(pct, w)),
        }),
        None => Some(Ctx {
            frac: None,
            label: format!("{} tok", fmt_tokens(tokens)),
            level: None,
        }),
    }
}

// ------------------------------------------------------------------ search, filter, order

/// The search box: whitespace-separated tokens, every one must hit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Query(Vec<String>);

impl Query {
    pub fn new(text: &str) -> Query {
        Query(text.split_whitespace().map(str::to_lowercase).collect())
    }
}

/// A token hits the title, the project (path or branch), the session id as a prefix, or any
/// subagent (`agent_hay`: lowercase `id agent model gist task` per subagent).
pub fn matches(card: &Card, agent_hay: &[String], q: &Query, filter: StatusFilter) -> bool {
    filter.admits(card.status)
        && q.0.iter().all(|t| {
            card.hay.contains(t.as_str())
                || card.id_lc.starts_with(t.as_str())
                || agent_hay.iter().any(|h| h.contains(t.as_str()))
        })
}

/// Indices of the cards that match, in display order: attention, busy, then the most recent
/// activity; equal → title.
pub fn visible<'a>(
    cards: &[Card],
    q: &Query,
    filter: StatusFilter,
    agent_hay: impl Fn(&Card) -> &'a [String],
) -> Vec<usize> {
    let mut out: Vec<usize> = (0..cards.len())
        .filter(|&i| matches(&cards[i], agent_hay(&cards[i]), q, filter))
        .collect();
    out.sort_by(|&a, &b| {
        let (a, b) = (&cards[a], &cards[b]);
        (a.status.rank(), Reverse(a.activity_at))
            .cmp(&(b.status.rank(), Reverse(b.activity_at)))
            .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
    });
    out
}

// ------------------------------------------------------------------ geometry

pub const GAP: f32 = 12.0;
pub const MIN_CARD_W: f32 = 340.0;
pub const MAX_COLS: usize = 4;
pub const PAD: f32 = 14.0;
pub const ROW_HEAD: f32 = 24.0;
pub const ROW_PROJECT: f32 = 18.0;
pub const ROW_MODEL: f32 = 24.0;
pub const ROW_CTX: f32 = 20.0;
pub const ROW_FILES: f32 = 20.0;
pub const REPLY_LINES: usize = 3;
pub const REPLY_LINE_H: f32 = 16.0;
pub const ROW_AGENTS: f32 = 28.0;
pub const SECTION_GAP: f32 = 6.0;

/// Which optional rows a card has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    pub metrics: bool,
    pub git: bool,
    pub reply: bool,
    pub agents: bool,
}

/// Height of a card without its expanded subagent rows: a pure function of what it shows.
pub fn card_height(s: Shape) -> f32 {
    let mut h = PAD * 2.0 + ROW_HEAD + ROW_PROJECT;
    if s.metrics {
        h += SECTION_GAP + ROW_MODEL + ROW_CTX;
    }
    if s.git {
        h += ROW_FILES;
    }
    if s.reply {
        h += SECTION_GAP + REPLY_LINES as f32 * REPLY_LINE_H;
    }
    if s.agents {
        h += SECTION_GAP + ROW_AGENTS;
    }
    h
}

/// 1–4 columns: as many as fit [`MIN_CARD_W`] with [`GAP`] between.
pub fn columns(width: f32) -> usize {
    if !width.is_finite() || width <= 0.0 {
        return 1;
    }
    (((width + GAP) / (MIN_CARD_W + GAP)).floor() as usize).clamp(1, MAX_COLS)
}

/// One card to place.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Item {
    pub height: f32,
    /// Takes a whole row (an expanded card).
    pub wide: bool,
}

/// Where an item goes: the grid row's top `y`, its column and span, the row's height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub col: usize,
    pub span: usize,
    pub y: f32,
    pub h: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Grid {
    pub slots: Vec<Slot>,
    pub total: f32,
}

impl Grid {
    /// Index range of the slots whose rows intersect `[top, bottom]` (slots are in y order).
    pub fn visible(&self, top: f32, bottom: f32) -> std::ops::Range<usize> {
        let first = self.slots.partition_point(|s| s.y + s.h < top);
        let end = self.slots.partition_point(|s| s.y <= bottom);
        first..end.max(first)
    }
}

/// Row-major placement in `cols` columns; a wide item gets a row of its own. Every card of a grid
/// row is as tall as the tallest of that row.
pub fn layout(items: &[Item], cols: usize) -> Grid {
    let cols = cols.max(1);
    let mut slots = Vec::with_capacity(items.len());
    let mut y = 0.0_f32;
    let mut row_start = 0;
    let flush = |slots: &mut Vec<Slot>, y: &mut f32, from: usize, to: usize| {
        if from == to {
            return;
        }
        let h = items[from..to]
            .iter()
            .map(|i| i.height)
            .fold(0.0_f32, f32::max);
        for (k, _) in items[from..to].iter().enumerate() {
            slots.push(Slot {
                col: k,
                span: 1,
                y: *y,
                h,
            });
        }
        *y += h + GAP;
    };
    for (i, item) in items.iter().enumerate() {
        if item.wide || cols == 1 {
            flush(&mut slots, &mut y, row_start, i);
            slots.push(Slot {
                col: 0,
                span: cols,
                y,
                h: item.height,
            });
            y += item.height + GAP;
            row_start = i + 1;
        } else if i + 1 - row_start == cols {
            flush(&mut slots, &mut y, row_start, i + 1);
            row_start = i + 1;
        }
    }
    flush(&mut slots, &mut y, row_start, items.len());
    Grid {
        slots,
        total: (y - GAP).max(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::status::GitInfo;
    use std::time::Duration;

    fn at(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    fn card(key: u64, title: &str, status: Status, secs: Option<u64>) -> Card {
        let cwd = PathBuf::from("/work/proj");
        let session = PathBuf::from(format!("/s/{key}.jsonl"));
        Card::build(
            &CardSrc {
                key: CardKey::Tab(key),
                title,
                cwd: &cwd,
                session: Some(&session),
                id: &format!("abcd{key}"),
                status,
                activity_at: secs.and_then(at),
                active: false,
            },
            None,
            &Extra::default(),
            SystemTime::UNIX_EPOCH,
        )
    }

    #[test]
    fn tab_and_backend_statuses_map_from_their_own_sources() {
        assert_eq!(
            Status::of_tab(TabStatus::Exited { clean: false }),
            Status::Exited { clean: false }
        );
        assert_eq!(Status::of_tab(TabStatus::Attention), Status::Attention);
        assert_eq!(
            Status::of_tab(TabStatus::Bell),
            Status::Attention,
            "a bell on a background tab wants the user"
        );
        assert_eq!(Status::of_tab(TabStatus::Busy), Status::Busy);
        assert_eq!(Status::of_tab(TabStatus::Idle), Status::Idle);
        // No tab: the pane title decides; no omp title = just "Running".
        assert_eq!(
            Status::of_backend(Activity::from_title(Some("π > p"))),
            Status::Idle
        );
        assert_eq!(
            Status::of_backend(Activity::from_title(Some("π ⠋ p"))),
            Status::Busy
        );
        assert_eq!(
            Status::of_backend(Activity::from_title(Some("π ! p"))),
            Status::Attention
        );
        assert_eq!(Status::of_backend(Activity::Unknown), Status::Running);
        assert_eq!(Status::Running.label(), "Running");
    }

    #[test]
    fn status_filters_admit_by_group() {
        use Status::*;
        let all = [
            Attention,
            Busy,
            Idle,
            Running,
            Exited { clean: true },
            Exited { clean: false },
        ];
        let pick = |f: StatusFilter| -> Vec<Status> {
            all.iter().copied().filter(|s| f.admits(*s)).collect()
        };
        assert_eq!(pick(StatusFilter::All).len(), 6);
        assert_eq!(
            pick(StatusFilter::Attention),
            [Attention, Exited { clean: false }]
        );
        assert_eq!(pick(StatusFilter::Busy), [Busy]);
        assert_eq!(
            pick(StatusFilter::Idle),
            [Idle, Running, Exited { clean: true }]
        );
    }

    #[test]
    fn order_is_attention_busy_then_most_recent() {
        let cards = vec![
            card(1, "idle old", Status::Idle, Some(10)),
            card(2, "busy", Status::Busy, Some(5)),
            card(3, "idle new", Status::Idle, Some(99)),
            card(4, "attention", Status::Attention, Some(1)),
            card(5, "crashed", Status::Exited { clean: false }, Some(2)),
            card(6, "no activity", Status::Running, None),
        ];
        let order = visible(&cards, &Query::default(), StatusFilter::All, |_| &[]);
        let titles: Vec<&str> = order.iter().map(|&i| cards[i].title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "crashed",
                "attention",
                "busy",
                "idle new",
                "idle old",
                "no activity"
            ],
            "attention (newest first), busy, then recency, unknown last"
        );
    }

    #[test]
    fn search_hits_title_project_id_prefix_and_subagents() {
        let c = card(7, "Fix Login Bug", Status::Idle, Some(1));
        let none: &[String] = &[];
        let hit = |q: &str, hay: &[String]| matches(&c, hay, &Query::new(q), StatusFilter::All);
        assert!(hit("", none));
        assert!(hit("login", none), "title, case-insensitive");
        assert!(hit("work/proj", none), "project");
        assert!(hit("ABCD7", none), "id prefix");
        assert!(!hit("bcd7", none), "id matches as a prefix only");
        assert!(!hit("refactor", none));
        let hay = vec!["scout-3 explore the parser\ndetails".to_owned()];
        assert!(hit("parser", &hay), "subagent task");
        assert!(hit("scout-3", &hay), "subagent id");
        assert!(
            hit("login parser", &hay),
            "every token needs a hit, anywhere"
        );
        assert!(!hit("login nothing", &hay));
    }

    #[test]
    fn filter_and_search_combine() {
        let cards = vec![
            card(1, "alpha", Status::Busy, Some(1)),
            card(2, "alpha two", Status::Idle, Some(2)),
        ];
        let v = |q: &str, f| visible(&cards, &Query::new(q), f, |_| &[]);
        assert_eq!(v("alpha", StatusFilter::All), [0, 1]);
        assert_eq!(v("alpha", StatusFilter::Idle), [1]);
        assert_eq!(v("alpha", StatusFilter::Attention), Vec::<usize>::new());
    }

    #[test]
    fn columns_follow_width() {
        assert_eq!(columns(0.0), 1);
        assert_eq!(columns(f32::NAN), 1);
        assert_eq!(columns(339.0), 1);
        assert_eq!(columns(691.0), 1);
        assert_eq!(columns(692.0), 2, "two cards plus the gap");
        assert_eq!(columns(1043.0), 2);
        assert_eq!(columns(1044.0), 3);
        assert_eq!(columns(1396.0), 4);
        assert_eq!(columns(5000.0), 4, "capped");
    }

    #[test]
    fn grid_rows_share_height_and_wide_items_span() {
        let it = |h: f32, wide| Item { height: h, wide };
        let g = layout(
            &[
                it(100.0, false),
                it(140.0, false),
                it(90.0, false),
                it(300.0, true),
                it(50.0, false),
            ],
            2,
        );
        let s = &g.slots;
        assert_eq!((s[0].col, s[0].span, s[0].y, s[0].h), (0, 1, 0.0, 140.0));
        assert_eq!((s[1].col, s[1].y, s[1].h), (1, 0.0, 140.0));
        // Third card is alone in its row when the next one is wide.
        assert_eq!((s[2].col, s[2].y, s[2].h), (0, 140.0 + GAP, 90.0));
        let wide_y = 140.0 + GAP + 90.0 + GAP;
        assert_eq!((s[3].col, s[3].span, s[3].y, s[3].h), (0, 2, wide_y, 300.0));
        assert_eq!(s[4].y, wide_y + 300.0 + GAP);
        assert_eq!(g.total, wide_y + 300.0 + GAP + 50.0);
        // One column: everything stacks.
        let g = layout(&[it(10.0, false), it(20.0, false)], 1);
        assert_eq!(g.slots[1].y, 10.0 + GAP);
        assert_eq!(g.total, 10.0 + GAP + 20.0);
        assert_eq!(layout(&[], 3), Grid::default());
    }

    #[test]
    fn grid_visible_range_covers_the_viewport() {
        let it = Item {
            height: 100.0,
            wide: false,
        };
        let g = layout(&[it; 8], 1);
        let r = g.visible(0.0, 50.0);
        assert_eq!(r, 0..1);
        let r = g.visible(500.0, 700.0);
        assert!(r.start >= 3 && r.end <= 8 && r.contains(&5), "{r:?}");
        assert_eq!(g.visible(10_000.0, 11_000.0).len(), 0);
    }

    #[test]
    fn card_height_grows_with_each_optional_row() {
        let base = card_height(Shape::default());
        let mut prev = base;
        for s in [
            Shape {
                metrics: true,
                ..Shape::default()
            },
            Shape {
                metrics: true,
                git: true,
                ..Shape::default()
            },
            Shape {
                metrics: true,
                git: true,
                reply: true,
                ..Shape::default()
            },
            Shape {
                metrics: true,
                git: true,
                reply: true,
                agents: true,
            },
        ] {
            let h = card_height(s);
            assert!(h > prev, "{s:?}");
            prev = h;
        }
    }

    #[test]
    fn metrics_come_from_the_fleet_and_live_context_wins() {
        let fleet = FleetSession {
            session: PathBuf::from("/s/1.jsonl"),
            model: Some("anthropic/claude-opus-5".into()),
            thinking: Some("high".into()),
            context_tokens: Some(100_000),
            input_tokens: 1_500,
            output_tokens: 2_000_000,
            cost: 1.234,
            last_reply: Some("All done.".into()),
            last_turn_failed: true,
            agents: Default::default(),
            git: Some(GitInfo {
                branch: "feat/x".into(),
                staged: 1,
                unstaged: 2,
                untracked: 3,
                worktree_of: Some(PathBuf::from("/primary")),
                ..GitInfo::default()
            }),
        };
        let cwd = PathBuf::from("/work/proj");
        let src = CardSrc {
            key: CardKey::Backend("omp-x".into()),
            title: "t",
            cwd: &cwd,
            session: None,
            id: "i",
            status: Status::Running,
            activity_at: None,
            active: false,
        };
        let c = Card::build(
            &src,
            Some(&fleet),
            &Extra {
                model_name: Some("Opus 5".into()),
                window: Some(1_000_000),
                live_ctx: None,
            },
            SystemTime::UNIX_EPOCH,
        );
        let m = c.metrics.as_ref().expect("metrics");
        assert_eq!(m.model, "Opus 5");
        assert_eq!(m.thinking.as_deref(), Some("high"));
        assert_eq!(m.cost, "$1.23");
        assert_eq!((m.input.as_str(), m.output.as_str()), ("1.5K", "2M"));
        assert!(m.failed);
        let ctx = m.ctx.as_ref().expect("ctx");
        assert_eq!(ctx.label, "10.0%");
        assert_eq!(ctx.frac, Some(0.1));
        assert_eq!(c.branch.as_deref(), Some("feat/x"));
        assert!(c.worktree);
        assert_eq!(
            c.files,
            Some(Files {
                staged: 1,
                unstaged: 2,
                untracked: 3
            })
        );
        assert!(c.hay.contains("feat/x"), "branch is searchable");

        // Live control reports its own numbers (and window) for the active tab.
        let live = Card::build(
            &src,
            Some(&fleet),
            &Extra {
                model_name: None,
                window: Some(1_000_000),
                live_ctx: Some((50_000, 200_000)),
            },
            SystemTime::UNIX_EPOCH,
        );
        let m = live.metrics.as_ref().expect("metrics");
        assert_eq!(
            m.model, "claude-opus-5",
            "falls back to the id of the selector"
        );
        assert_eq!(m.ctx.as_ref().map(|c| c.label.as_str()), Some("25.0%"));

        // No window known: show the token count, no meter.
        let nowin = Card::build(
            &src,
            Some(&fleet),
            &Extra::default(),
            SystemTime::UNIX_EPOCH,
        );
        let ctx = nowin.metrics.and_then(|m| m.ctx).expect("ctx");
        assert_eq!((ctx.frac, ctx.label.as_str()), (None, "100K tok"));
    }

    #[test]
    fn a_card_without_fleet_data_has_no_metrics() {
        let c = card(1, "new", Status::Idle, None);
        assert!(c.metrics.is_none() && c.files.is_none());
        assert_eq!(c.shape(false), Shape::default());
    }
}
