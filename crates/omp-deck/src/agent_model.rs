//! Pure model of the Agent Hub panel: status counts, filtering / ordering of the roster, and the
//! strings every card shows. Formatting happens once per snapshot (or age tick), never per frame.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use deck_core::agents::{AgentStatus, AgentSummary, AgentsSnapshot};
use deck_core::ctl::{LiveAgent, LiveSnapshot, LiveStatus};

use crate::sidebar::format_age;
use deck_service::statusfmt::fmt_tokens;

/// Age strings only change by minutes; they are refreshed at this granularity.
const AGE_BUCKET_SECS: u64 = 15;
const GIST_CHARS: usize = 260;

/// What a chip counts and the status filter selects: the file-inferred status, except that agents
/// omp reports as parked (live link only) form their own bucket instead of counting as done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Running,
    Done,
    Failed,
    Aborted,
    Interrupted,
    Parked,
}

impl Bucket {
    pub fn of(status: AgentStatus, parked: bool) -> Bucket {
        if parked {
            return Bucket::Parked;
        }
        match status {
            AgentStatus::Running => Bucket::Running,
            AgentStatus::Done => Bucket::Done,
            AgentStatus::Failed => Bucket::Failed,
            AgentStatus::Aborted => Bucket::Aborted,
            AgentStatus::Interrupted => Bucket::Interrupted,
        }
    }
}

pub fn status_label(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Running => "running",
        AgentStatus::Done => "done",
        AgentStatus::Failed => "failed",
        AgentStatus::Aborted => "aborted",
        AgentStatus::Interrupted => "interrupted",
    }
}

/// Agents per bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    pub aborted: usize,
    pub interrupted: usize,
    pub parked: usize,
}

impl Counts {
    /// `parked(i)`: omp reports agent `i` as parked.
    pub fn of(agents: &[AgentSummary], parked: impl Fn(usize) -> bool) -> Counts {
        let mut c = Counts::default();
        for (i, a) in agents.iter().enumerate() {
            match Bucket::of(a.status, parked(i)) {
                Bucket::Running => c.running += 1,
                Bucket::Done => c.done += 1,
                Bucket::Failed => c.failed += 1,
                Bucket::Aborted => c.aborted += 1,
                Bucket::Interrupted => c.interrupted += 1,
                Bucket::Parked => c.parked += 1,
            }
        }
        c
    }

    pub fn total(&self) -> usize {
        self.running + self.done + self.failed + self.aborted + self.interrupted + self.parked
    }
}

/// Indices into `agents` in display order. Tree mode keeps snapshot order (parent before child)
/// and hides collapsed subtrees. Flat mode lists everything: running first, then most recent
/// activity.
pub fn visible(agents: &[AgentSummary], tree: bool, collapsed: &HashSet<String>) -> Vec<usize> {
    if !tree {
        let mut out: Vec<usize> = (0..agents.len()).collect();
        out.sort_by_key(|&i| {
            let a = &agents[i];
            (
                a.status != AgentStatus::Running,
                std::cmp::Reverse(a.last_activity.or(a.created)),
            )
        });
        return out;
    }
    let by_id: HashMap<&str, usize> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| (a.id.as_str(), i))
        .collect();
    let parent = |i: usize| {
        agents[i]
            .parent_id
            .as_deref()
            .and_then(|p| by_id.get(p).copied())
    };
    (0..agents.len())
        .filter(|&i| {
            if collapsed.is_empty() {
                return true;
            }
            let mut cur = parent(i);
            let mut guard = agents.len();
            while let Some(p) = cur {
                if collapsed.contains(&agents[p].id) {
                    return false;
                }
                guard -= 1;
                if guard == 0 {
                    break;
                }
                cur = parent(p);
            }
            true
        })
        .collect()
}

// ------------------------------------------------------------------ formatting

/// `850ms`, `12.3s`, `4m 05s`, `1h 02m`.
pub fn fmt_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let secs = d.as_secs();
    match secs {
        0..=59 => format!("{:.1}s", d.as_secs_f64()),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Sub-cent costs keep four decimals (`$0.0091`), as omp's roster does.
pub fn fmt_agent_cost(cost: f64) -> String {
    let c = if cost.is_finite() { cost.max(0.0) } else { 0.0 };
    if c > 0.0 && c < 0.01 {
        format!("${c:.4}")
    } else {
        format!("${c:.2}")
    }
}

/// `provider/id:level` → `id` (the thinking level is shown separately).
pub fn short_model(model: &str) -> String {
    let id = model.rsplit('/').next().unwrap_or(model);
    id.split(':').next().unwrap_or(id).to_owned()
}

/// Whitespace collapsed to single spaces, cut to `max` chars with an ellipsis.
pub fn one_line(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max + 4));
    let mut chars = 0;
    for word in text.split_whitespace() {
        let need = usize::from(!out.is_empty()) + word.chars().count();
        if chars + need > max {
            let room = max.saturating_sub(chars + usize::from(!out.is_empty()));
            if !out.is_empty() {
                out.push(' ');
            }
            out.extend(word.chars().take(room));
            out.push('…');
            return out;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
        chars += need;
    }
    out
}

fn tokens_text(n: u64) -> String {
    format!("{} tok", fmt_tokens(n))
}

/// Elapsed time to show: a running agent has no final duration yet, so it is `now - created`.
pub fn live_duration(a: &AgentSummary, now: SystemTime) -> Option<Duration> {
    if a.status == AgentStatus::Running
        && let Some(created) = a.created
    {
        return now.duration_since(created).ok();
    }
    a.duration
}

/// `$0.0091 · 12.3s · 4 req · 9 tools · 15K tok`.
pub fn metrics_line(a: &AgentSummary, now: SystemTime) -> String {
    let mut parts = vec![fmt_agent_cost(a.cost)];
    if let Some(d) = live_duration(a, now) {
        parts.push(fmt_duration(d));
    }
    parts.push(format!("{} req", a.requests));
    parts.push(format!("{} tools", a.tool_calls));
    parts.push(tokens_text(a.tokens));
    parts.join(" · ")
}

/// Age of the last activity (falling back to creation); `"now"` when unknown.
pub fn agent_age(a: &AgentSummary, now: SystemTime) -> String {
    a.last_activity
        .or(a.created)
        .map_or_else(|| "now".to_owned(), |t| format_age(now, t))
}

// ------------------------------------------------------------------ cached view

/// Everything one roster card shows, formatted.
#[derive(Clone, Debug, Default)]
pub struct CardView {
    pub short_model: String,
    pub metrics: String,
    pub age: String,
    pub gist: String,
    /// Direct children in the snapshot.
    pub children: usize,
    /// omp's own state for this agent while the control link is up (`None`: file inference only).
    pub live: Option<LiveStatus>,
}

/// The file-inferred snapshot with the live registry laid over it: omp knows what is running,
/// idle between turns, parked or killed, and what tool a running agent is in. Unchanged → the
/// same `Arc` (no copy).
pub fn overlay(base: &Arc<AgentsSnapshot>, live: &LiveSnapshot) -> Arc<AgentsSnapshot> {
    let by_id: HashMap<&str, &LiveAgent> = live.agents.iter().map(|l| (l.id.as_str(), l)).collect();
    let mut edits: Vec<(usize, AgentStatus, Option<String>)> = Vec::new();
    for (i, a) in base.agents.iter().enumerate() {
        let Some(l) = by_id.get(a.id.as_str()) else {
            continue;
        };
        let (status, current) = match l.status {
            LiveStatus::Running => (
                AgentStatus::Running,
                l.activity.clone().or_else(|| a.current.clone()),
            ),
            LiveStatus::Aborted => (AgentStatus::Aborted, a.current.clone()),
            // Between turns: the file may still look "running" (or "aborted", after a revive)
            // until its last entry settles; the agent is waiting for input, not working.
            LiveStatus::Idle => (
                if matches!(a.status, AgentStatus::Running | AgentStatus::Aborted) {
                    AgentStatus::Done
                } else {
                    a.status
                },
                a.current.clone(),
            ),
            // Parked agents keep whatever the files say.
            LiveStatus::Parked => (
                if a.status == AgentStatus::Running {
                    AgentStatus::Done
                } else {
                    a.status
                },
                a.current.clone(),
            ),
        };
        if status != a.status || current != a.current {
            edits.push((i, status, current));
        }
    }
    if edits.is_empty() {
        return Arc::clone(base);
    }
    let mut snap = (**base).clone();
    for (i, status, current) in edits {
        snap.agents[i].status = status;
        snap.agents[i].current = current;
    }
    Arc::new(snap)
}

/// Roster + header totals for the current snapshot.
#[derive(Default)]
pub struct AgentsView {
    /// File-inferred snapshot (identity decides rebuilds).
    base: Option<Arc<AgentsSnapshot>>,
    live: Option<Arc<LiveSnapshot>>,
    /// `base` with the live overlay: what the UI shows.
    snap: Option<Arc<AgentsSnapshot>>,
    /// Refresh granule last applied: seconds while an agent runs (live durations), else
    /// [`AGE_BUCKET_SECS`] buckets.
    bucket: Option<u64>,
    age_bucket: Option<u64>,
    /// Bumps whenever the shown snapshot (not just ages) changed.
    pub version: u64,
    /// Bumps only when the file-inferred snapshot changed (text layouts depend on it, not on the
    /// live overlay).
    pub base_version: u64,
    /// Bumps on every refresh (snapshot, age or running duration).
    pub tick: u64,
    pub cards: Vec<CardView>,
    /// Lowercase `id agent model gist task` haystack per agent, for the search box.
    pub search: Vec<String>,
    pub counts: Counts,
    /// `$1.23 · 4.2M tok · 120 req · 340 tools`.
    pub totals: String,
}

impl AgentsView {
    pub fn agents(&self) -> &[AgentSummary] {
        self.snap.as_ref().map_or(&[], |s| s.agents.as_slice())
    }

    /// Rebuild when the snapshot or the live overlay (by identity or content) changed, ages when
    /// their bucket did, and the metrics of running agents once per second. True when anything did.
    pub fn update(
        &mut self,
        base: Option<&Arc<AgentsSnapshot>>,
        live: Option<&Arc<LiveSnapshot>>,
        now: SystemTime,
    ) -> bool {
        let secs = now
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let age_bucket = secs / AGE_BUCKET_SECS;
        let bucket = if self.counts.running > 0 {
            secs
        } else {
            age_bucket
        };
        let base_same = match (&self.base, base) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        let live_same = match (&self.live, live) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || **a == **b,
            (None, None) => true,
            _ => false,
        };
        let same = base_same && live_same;
        if same && self.bucket == Some(bucket) {
            return false;
        }
        let rebuilt = !same;
        if rebuilt {
            if !base_same {
                self.base = base.cloned();
                self.base_version += 1;
            }
            if !live_same {
                self.live = live.cloned();
            }
            self.snap = match (&self.base, &self.live) {
                (Some(b), Some(l)) => Some(overlay(b, l)),
                (b, _) => b.clone(),
            };
            self.version += 1;
            self.rebuild_all(now, base_same);
        }
        if let Some(s) = &self.snap {
            let ages = rebuilt || self.age_bucket != Some(age_bucket);
            for (card, a) in self.cards.iter_mut().zip(&s.agents) {
                if ages {
                    card.age = agent_age(a, now);
                }
                if !rebuilt && a.status == AgentStatus::Running {
                    card.metrics = metrics_line(a, now);
                }
            }
        }
        self.age_bucket = Some(age_bucket);
        self.bucket = Some(bucket);
        self.tick += 1;
        true
    }

    /// `keep_search`: the file-inferred snapshot is unchanged, so the search haystacks still hold.
    fn rebuild_all(&mut self, now: SystemTime, keep_search: bool) {
        let Some(snap) = self.snap.clone() else {
            self.cards.clear();
            self.search.clear();
            self.counts = Counts::default();
            self.totals.clear();
            return;
        };
        let agents = snap.agents.as_slice();
        let mut children: HashMap<&str, usize> = HashMap::new();
        for a in agents {
            if let Some(p) = a.parent_id.as_deref() {
                *children.entry(p).or_default() += 1;
            }
        }
        let live: HashMap<&str, LiveStatus> = self
            .live
            .as_ref()
            .map(|l| l.agents.iter().map(|a| (a.id.as_str(), a.status)).collect())
            .unwrap_or_default();
        self.cards = agents
            .iter()
            .map(|a| CardView {
                short_model: a.model.as_deref().map(short_model).unwrap_or_default(),
                metrics: metrics_line(a, now),
                age: String::new(),
                gist: one_line(&a.task_gist, GIST_CHARS),
                children: children.get(a.id.as_str()).copied().unwrap_or(0),
                live: live.get(a.id.as_str()).copied(),
            })
            .collect();
        if !(keep_search && self.search.len() == agents.len()) {
            self.search = agents
                .iter()
                .map(|a| {
                    format!(
                        "{} {} {} {}\n{}",
                        a.id,
                        a.agent.as_deref().unwrap_or(""),
                        a.model.as_deref().unwrap_or(""),
                        a.task_gist,
                        a.task
                    )
                    .to_lowercase()
                })
                .collect();
        }
        self.counts = Counts::of(agents, |i| {
            self.cards
                .get(i)
                .is_some_and(|c| c.live == Some(LiveStatus::Parked))
        });
        let (mut cost, mut tokens, mut req, mut tools) = (0.0, 0u64, 0u64, 0u64);
        for a in agents {
            cost += a.cost;
            tokens += a.tokens;
            req += u64::from(a.requests);
            tools += u64::from(a.tool_calls);
        }
        self.totals = format!(
            "{} · {} · {req} req · {tools} tools",
            fmt_agent_cost(cost),
            tokens_text(tokens)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn agent(id: &str, parent: Option<&str>, status: AgentStatus, secs: u64) -> AgentSummary {
        AgentSummary {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            depth: 0,
            path: PathBuf::from(format!("/x/{id}.jsonl")),
            advisor: false,
            agent: Some("task".to_owned()),
            model: Some("anthropic/claude-sonnet-5-5:medium".to_owned()),
            thinking: None,
            task_gist: format!("do {id}"),
            task: String::new(),
            status,
            error: None,
            current: None,
            created: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
            last_activity: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs)),
            duration: None,
            requests: 1,
            tool_calls: 2,
            tokens: 1500,
            cost: 0.0091,
            context_tokens: None,
            output_path: None,
        }
    }

    fn fixture() -> (Vec<AgentSummary>, Vec<String>) {
        // Snapshot order: parent before child, newest first among siblings.
        let agents = vec![
            agent("Beta", None, AgentStatus::Running, 30),
            agent("Beta.Leaf", Some("Beta"), AgentStatus::Failed, 31),
            agent("Alpha", None, AgentStatus::Done, 20),
            agent("Alpha.Kid", Some("Alpha"), AgentStatus::Done, 21),
            agent(
                "Alpha.Kid.Deep",
                Some("Alpha.Kid"),
                AgentStatus::Aborted,
                22,
            ),
        ];
        let search = agents
            .iter()
            .map(|a| format!("{} task {}", a.id, a.task_gist).to_lowercase())
            .collect();
        (agents, search)
    }

    fn ids(agents: &[AgentSummary], idx: &[usize]) -> Vec<String> {
        idx.iter().map(|&i| agents[i].id.clone()).collect()
    }

    #[test]
    fn tree_keeps_snapshot_order_and_honours_collapse() {
        let (a, _) = fixture();
        let all = visible(&a, true, &HashSet::new());
        assert_eq!(
            ids(&a, &all),
            ["Beta", "Beta.Leaf", "Alpha", "Alpha.Kid", "Alpha.Kid.Deep"]
        );
        let collapsed: HashSet<String> = ["Alpha".to_owned()].into();
        let v = visible(&a, true, &collapsed);
        assert_eq!(ids(&a, &v), ["Beta", "Beta.Leaf", "Alpha"]);
    }

    #[test]
    fn tree_hides_collapsed_subtrees_only() {
        let (a, _) = fixture();
        let collapsed: HashSet<String> = ["Alpha.Kid".to_owned()].into();
        let v = visible(&a, true, &collapsed);
        assert_eq!(ids(&a, &v), ["Beta", "Beta.Leaf", "Alpha", "Alpha.Kid"]);
    }

    #[test]
    fn flat_lists_running_first_then_recent() {
        let (a, _) = fixture();
        let v = visible(&a, false, &HashSet::new());
        assert_eq!(
            ids(&a, &v),
            ["Beta", "Beta.Leaf", "Alpha.Kid.Deep", "Alpha.Kid", "Alpha"]
        );
        // Collapsing does not apply to the flat list.
        let collapsed: HashSet<String> = ["Alpha".to_owned()].into();
        assert_eq!(visible(&a, false, &collapsed).len(), 5);
    }

    #[test]
    fn parked_agents_form_their_own_bucket() {
        let (a, _) = fixture();
        // The two done agents: only the first is parked by omp.
        let done: Vec<usize> = (0..a.len())
            .filter(|&i| a[i].status == AgentStatus::Done)
            .collect();
        let parked = |i: usize| i == done[0];
        let c = Counts::of(&a, parked);
        assert_eq!((c.done, c.parked), (1, 1));
        assert_eq!(c.total(), a.len());
    }

    #[test]
    fn counts_and_totals() {
        let (a, _) = fixture();
        let c = Counts::of(&a, |_| false);
        assert_eq!((c.running, c.done, c.failed, c.aborted), (1, 2, 1, 1));
        assert_eq!(c.total(), 5);
        let snap = Arc::new(AgentsSnapshot {
            session: PathBuf::from("/x.jsonl"),
            agents: a,
        });
        let mut v = AgentsView::default();
        assert!(v.update(Some(&snap), None, SystemTime::UNIX_EPOCH));
        assert_eq!(v.totals, "$0.05 · 7.5K tok · 5 req · 10 tools");
        assert_eq!(v.cards[2].children, 1);
        assert!(!v.update(Some(&snap), None, SystemTime::UNIX_EPOCH));
        assert_eq!(v.version, 1);
    }

    #[test]
    fn duration_cost_and_text_formatting() {
        assert_eq!(fmt_duration(Duration::from_millis(850)), "850ms");
        assert_eq!(fmt_duration(Duration::from_millis(12_340)), "12.3s");
        assert_eq!(fmt_duration(Duration::from_secs(245)), "4m 05s");
        assert_eq!(fmt_duration(Duration::from_secs(3720)), "1h 02m");
        assert_eq!(fmt_agent_cost(0.0091), "$0.0091");
        assert_eq!(fmt_agent_cost(0.0), "$0.00");
        assert_eq!(fmt_agent_cost(1.234), "$1.23");
        assert_eq!(
            short_model("anthropic/claude-sonnet-5-5:medium"),
            "claude-sonnet-5-5"
        );
        assert_eq!(one_line("a\n\n  b   c", 20), "a b c");
        assert_eq!(one_line("alpha beta gamma", 8), "alpha be…");
        let a = agent("X", None, AgentStatus::Done, 1);
        assert_eq!(
            metrics_line(&a, SystemTime::UNIX_EPOCH),
            "$0.0091 · 1 req · 2 tools · 1.5K tok"
        );
    }

    #[test]
    fn running_agents_show_elapsed_time_others_their_recorded_duration() {
        let mut a = agent("X", None, AgentStatus::Running, 100);
        a.duration = Some(Duration::from_secs(1));
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(165);
        assert_eq!(live_duration(&a, now), Some(Duration::from_secs(65)));
        assert!(metrics_line(&a, now).contains("1m 05s"));
        a.status = AgentStatus::Done;
        assert_eq!(live_duration(&a, now), Some(Duration::from_secs(1)));
    }

    #[test]
    fn view_refreshes_running_metrics_every_second_and_idle_views_do_not() {
        let mk = |status| {
            Arc::new(AgentsSnapshot {
                agents: vec![agent("X", None, status, 100)],
                ..AgentsSnapshot::default()
            })
        };
        let t = |s| SystemTime::UNIX_EPOCH + Duration::from_secs(s);
        let running = mk(AgentStatus::Running);
        let mut v = AgentsView::default();
        assert!(v.update(Some(&running), None, t(160)));
        assert!(
            v.update(Some(&running), None, t(161)),
            "running view ticks per second"
        );
        assert!(v.cards[0].metrics.contains("1m 01s"));
        let done = mk(AgentStatus::Done);
        let mut idle = AgentsView::default();
        assert!(idle.update(Some(&done), None, t(160)));
        assert!(
            !idle.update(Some(&done), None, t(161)),
            "idle view only per age bucket"
        );
    }

    #[test]
    fn search_text_has_model_and_full_task_text() {
        let mut a = agent("X", None, AgentStatus::Done, 1);
        a.task = "Refactor the Frobnicator module".to_owned();
        let snap = Arc::new(AgentsSnapshot {
            agents: vec![a],
            ..AgentsSnapshot::default()
        });
        let mut v = AgentsView::default();
        v.update(Some(&snap), None, SystemTime::UNIX_EPOCH);
        assert_eq!(v.search.len(), 1);
        assert!(v.search[0].contains("frobnicator"));
        assert!(v.search[0].contains("sonnet"));
        assert!(v.search[0].starts_with("x "), "id first");
    }

    fn live_agent(id: &str, status: LiveStatus, activity: Option<&str>) -> LiveAgent {
        LiveAgent {
            id: id.to_owned(),
            name: "task".to_owned(),
            parent: None,
            status,
            activity: activity.map(str::to_owned),
            live: status != LiveStatus::Parked,
            streaming: None,
            session_file: None,
            model: None,
        }
    }

    fn live_of(agents: Vec<LiveAgent>) -> Arc<LiveSnapshot> {
        Arc::new(LiveSnapshot {
            agents,
            ..LiveSnapshot::default()
        })
    }

    #[test]
    fn live_state_overrides_file_inference_for_matching_ids_only() {
        let (agents, _) = fixture();
        let base = Arc::new(AgentsSnapshot {
            agents,
            ..AgentsSnapshot::default()
        });
        let live = live_of(vec![
            // Beta ran per the file and still runs; the tool name comes from omp.
            live_agent("Beta", LiveStatus::Running, Some("running bash")),
            // Done per the file, killed since.
            live_agent("Alpha", LiveStatus::Aborted, None),
            live_agent("Alpha.Kid", LiveStatus::Parked, None),
            live_agent("Unknown.Id", LiveStatus::Running, None),
        ]);
        let shown = overlay(&base, &live);
        let by = |id: &str| shown.agents.iter().find(|a| a.id == id).cloned();
        let beta = by("Beta").expect("Beta");
        assert_eq!(beta.status, AgentStatus::Running);
        assert_eq!(beta.current.as_deref(), Some("running bash"));
        assert_eq!(by("Alpha").expect("Alpha").status, AgentStatus::Aborted);
        // Parked keeps what the file says (it finished); the view marks it parked.
        assert_eq!(by("Alpha.Kid").expect("Kid").status, AgentStatus::Done);
        // Not in the live list: untouched.
        assert_eq!(by("Beta.Leaf").expect("Leaf").status, AgentStatus::Failed);
        assert_eq!(shown.agents.len(), base.agents.len());
    }

    #[test]
    fn file_running_agent_that_omp_reports_idle_is_not_running() {
        let base = Arc::new(AgentsSnapshot {
            agents: vec![agent("A", None, AgentStatus::Running, 1)],
            ..AgentsSnapshot::default()
        });
        let shown = overlay(
            &base,
            &live_of(vec![live_agent("A", LiveStatus::Idle, None)]),
        );
        assert_eq!(shown.agents[0].status, AgentStatus::Done);
    }

    #[test]
    fn revived_agent_that_is_idle_is_no_longer_shown_aborted() {
        let base = Arc::new(AgentsSnapshot {
            agents: vec![agent("A", None, AgentStatus::Aborted, 1)],
            ..AgentsSnapshot::default()
        });
        let idle = overlay(
            &base,
            &live_of(vec![live_agent("A", LiveStatus::Idle, None)]),
        );
        assert_eq!(idle.agents[0].status, AgentStatus::Done);
        let parked = overlay(
            &base,
            &live_of(vec![live_agent("A", LiveStatus::Parked, None)]),
        );
        assert_eq!(parked.agents[0].status, AgentStatus::Aborted);
    }

    #[test]
    fn overlay_without_changes_shares_the_snapshot() {
        let base = Arc::new(AgentsSnapshot {
            agents: vec![agent("A", None, AgentStatus::Done, 1)],
            ..AgentsSnapshot::default()
        });
        let live = live_of(vec![live_agent("A", LiveStatus::Idle, None)]);
        assert!(Arc::ptr_eq(&overlay(&base, &live), &base));
    }

    #[test]
    fn view_marks_live_agents_and_counts_the_overlaid_status() {
        let base = Arc::new(AgentsSnapshot {
            agents: vec![
                agent("A", None, AgentStatus::Done, 1),
                agent("B", None, AgentStatus::Done, 2),
            ],
            ..AgentsSnapshot::default()
        });
        let mut v = AgentsView::default();
        let t = SystemTime::UNIX_EPOCH;
        v.update(Some(&base), None, t);
        assert!(v.cards.iter().all(|c| c.live.is_none()));
        assert_eq!(v.counts.running, 0);
        let live = live_of(vec![
            live_agent("A", LiveStatus::Running, None),
            live_agent("B", LiveStatus::Parked, None),
        ]);
        assert!(v.update(Some(&base), Some(&live), t));
        assert_eq!(v.cards[0].live, Some(LiveStatus::Running));
        assert_eq!(v.cards[1].live, Some(LiveStatus::Parked));
        assert_eq!(v.counts.running, 1);
        // The live link only changed what is shown: layouts keyed on the file snapshot hold.
        assert_eq!(v.base_version, 1);
        // Disconnect: back to file inference.
        assert!(v.update(Some(&base), None, t));
        assert!(v.cards.iter().all(|c| c.live.is_none()));
        assert_eq!(v.counts.running, 0);
    }

    #[test]
    fn identical_live_content_does_not_rebuild() {
        let base = Arc::new(AgentsSnapshot {
            agents: vec![agent("A", None, AgentStatus::Done, 1)],
            ..AgentsSnapshot::default()
        });
        let mut v = AgentsView::default();
        let t = SystemTime::UNIX_EPOCH;
        v.update(
            Some(&base),
            Some(&live_of(vec![live_agent("A", LiveStatus::Idle, None)])),
            t,
        );
        let version = v.version;
        // The extension pushes a fresh (equal) snapshot every second.
        let again = live_of(vec![live_agent("A", LiveStatus::Idle, None)]);
        assert!(!v.update(Some(&base), Some(&again), t));
        assert_eq!(v.version, version);
    }
}
