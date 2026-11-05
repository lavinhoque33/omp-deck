//! Pure model of removing subagents from the agents dashboard: which agents a "Clean up" or a
//! selection deletes, the multi-select state, the confirmation wording and the summary of omp's
//! answer. No egui and no app state here.
//!
//! A subagent's files live in its parent's directory, so deleting an agent takes its whole
//! subtree with it. Requests therefore carry only the top-most targets, and "not running" always
//! means "no running agent anywhere in the subtree".

use std::collections::{HashMap, HashSet};

use deck_core::agents::{AgentStatus, AgentSummary};
use deck_core::ctl::{CtlReply, DeleteOutcome, DeleteTarget};

use crate::confirm::Confirm;
use crate::sessions_model::Check;

/// Why the controls of a card cannot be used.
pub const NO_LIVE_CONTROL: &str = "Live control is not connected for this session";
pub const OPEN_TO_MANAGE: &str = "Open this session to manage its agents";
/// Answer of an extension that predates agent deletion.
pub const STALE_EXTENSION: &str = "This session's live-control extension predates agent deletion: \
    restart the session with live control";

/// Whether a card's agents can be deleted right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Ready,
    /// A delete for this card is in flight.
    Busy,
    Blocked(&'static str),
}

impl Gate {
    pub fn ready(self) -> bool {
        self == Gate::Ready
    }

    /// Tooltip of a disabled control.
    pub fn why(self) -> &'static str {
        match self {
            Gate::Ready => "",
            Gate::Busy => "Deleting agents…",
            Gate::Blocked(reason) => reason,
        }
    }
}

/// What a delete request is about (the confirmation's wording).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Everything that is not running.
    CleanUp,
    /// The agents checked in select mode.
    Selected,
    /// One agent, from its row.
    One(String),
}

/// What one delete request removes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// The top-most agents to remove (their descendants go with them).
    pub targets: Vec<DeleteTarget>,
    /// Agents that disappear, nested ones included (advisors are never counted).
    pub total: usize,
    /// Of `total`, agents running now: they are aborted first.
    pub running: usize,
    /// Of `total`, agents that go only because an ancestor was targeted.
    pub nested: usize,
    /// Clean-up only: running agents that stay.
    pub kept_running: usize,
    /// Clean-up only: not-running agents that stay because a descendant runs.
    pub kept_parents: usize,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }
}

/// Parent links and children lists of a snapshot (by index).
struct Forest {
    parent: Vec<Option<usize>>,
    kids: Vec<Vec<usize>>,
}

impl Forest {
    fn of(agents: &[AgentSummary]) -> Forest {
        let by_id: HashMap<&str, usize> = agents
            .iter()
            .enumerate()
            .map(|(i, a)| (a.id.as_str(), i))
            .collect();
        let parent: Vec<Option<usize>> = agents
            .iter()
            .enumerate()
            .map(|(i, a)| {
                a.parent_id
                    .as_deref()
                    .and_then(|p| by_id.get(p).copied())
                    .filter(|&p| p != i)
            })
            .collect();
        let mut kids = vec![Vec::new(); agents.len()];
        for (i, p) in parent.iter().enumerate() {
            if let Some(p) = p {
                kids[*p].push(i);
            }
        }
        Forest { parent, kids }
    }

    /// Whether any proper ancestor of `i` satisfies `hit`.
    fn ancestor(&self, i: usize, hit: impl Fn(usize) -> bool) -> bool {
        let mut cur = self.parent[i];
        let mut guard = self.parent.len();
        while let Some(p) = cur {
            if hit(p) {
                return true;
            }
            guard -= 1;
            if guard == 0 {
                break;
            }
            cur = self.parent[p];
        }
        false
    }

    /// `i` and every descendant of it.
    fn subtree(&self, i: usize) -> Vec<usize> {
        let mut seen = HashSet::from([i]);
        let mut out = vec![i];
        let mut at = 0;
        while let Some(&cur) = out.get(at) {
            at += 1;
            for &k in &self.kids[cur] {
                if seen.insert(k) {
                    out.push(k);
                }
            }
        }
        out
    }
}

fn running(a: &AgentSummary) -> bool {
    a.status == AgentStatus::Running
}

/// Fills the counters of a plan whose `targets` are the indices `tops`.
fn finish(agents: &[AgentSummary], forest: &Forest, tops: &[usize], selected: usize) -> Plan {
    let mut plan = Plan::default();
    for &t in tops {
        plan.targets.push(DeleteTarget {
            id: agents[t].id.clone(),
            transcript: agents[t].path.clone(),
        });
        for i in forest.subtree(t) {
            if agents[i].advisor {
                continue;
            }
            plan.total += 1;
            plan.running += usize::from(running(&agents[i]));
        }
    }
    plan.nested = plan.total.saturating_sub(selected);
    plan
}

/// "Clean up": every non-advisor agent that is not running and has no running descendant.
pub fn cleanup(agents: &[AgentSummary]) -> Plan {
    let forest = Forest::of(agents);
    // Subtrees holding a running agent: the agent and all its ancestors.
    let mut holds = vec![false; agents.len()];
    for (i, a) in agents.iter().enumerate() {
        if !running(a) {
            continue;
        }
        holds[i] = true;
        let mut cur = forest.parent[i];
        let mut guard = agents.len();
        while let Some(p) = cur {
            if holds[p] || guard == 0 {
                break;
            }
            holds[p] = true;
            guard -= 1;
            cur = forest.parent[p];
        }
    }
    let eligible = |i: usize| !agents[i].advisor && !holds[i];
    let tops: Vec<usize> = (0..agents.len())
        .filter(|&i| eligible(i) && !forest.ancestor(i, eligible))
        .collect();
    let selected = (0..agents.len()).filter(|&i| eligible(i)).count();
    let mut plan = finish(agents, &forest, &tops, selected);
    plan.kept_running = agents.iter().filter(|a| running(a)).count();
    plan.kept_parents = (0..agents.len())
        .filter(|&i| holds[i] && !running(&agents[i]) && !agents[i].advisor)
        .count();
    plan
}

/// The agents in `ids` (advisors and unknown ids ignored), reduced to their top-most members.
pub fn selected(agents: &[AgentSummary], ids: &HashSet<String>) -> Plan {
    let forest = Forest::of(agents);
    let picked = |i: usize| !agents[i].advisor && ids.contains(&agents[i].id);
    let tops: Vec<usize> = (0..agents.len())
        .filter(|&i| picked(i) && !forest.ancestor(i, picked))
        .collect();
    let count = (0..agents.len()).filter(|&i| picked(i)).count();
    finish(agents, &forest, &tops, count)
}

// ------------------------------------------------------------------ multi-select

/// Checked agents of one card and the anchor of Shift-click ranges.
#[derive(Clone, Debug, Default)]
pub struct Select {
    set: HashSet<String>,
    anchor: Option<String>,
}

impl Select {
    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.set.contains(id)
    }

    pub fn ids(&self) -> &HashSet<String> {
        &self.set
    }

    pub fn clear(&mut self) {
        self.set.clear();
        self.anchor = None;
    }

    pub fn remove(&mut self, ids: &[String]) {
        for id in ids {
            self.set.remove(id);
        }
        if self.anchor.as_ref().is_some_and(|a| ids.contains(a)) {
            self.anchor = None;
        }
    }

    /// Click on the row `id` of the selectable rows `order` (display order). With Shift and an
    /// anchor still in the list, every row between them is added; otherwise the row toggles.
    pub fn click(&mut self, order: &[&str], id: &str, shift: bool) {
        let at = order.iter().position(|&o| o == id);
        let anchor = self
            .anchor
            .as_deref()
            .and_then(|a| order.iter().position(|&o| o == a));
        match (shift, anchor, at) {
            (true, Some(a), Some(b)) => {
                for &o in &order[a.min(b)..=a.max(b)] {
                    self.set.insert(o.to_owned());
                }
            }
            _ => {
                if !self.set.remove(id) {
                    self.set.insert(id.to_owned());
                }
            }
        }
        self.anchor = Some(id.to_owned());
    }

    /// State of the select-all checkbox over `order`.
    pub fn check(&self, order: &[&str]) -> Check {
        let n = order.iter().filter(|&&o| self.set.contains(o)).count();
        match n {
            0 => Check::None,
            n if n == order.len() => Check::All,
            _ => Check::Some,
        }
    }

    /// Select-all: every row of `order` on, unless all already are (then they go off).
    pub fn toggle_all(&mut self, order: &[&str]) {
        if self.check(order) == Check::All {
            for o in order {
                self.set.remove(*o);
            }
        } else {
            self.set.extend(order.iter().map(|o| (*o).to_owned()));
        }
        self.anchor = None;
    }

    /// Drops what is no longer a selectable agent of `agents`.
    pub fn prune(&mut self, agents: &[AgentSummary]) {
        if self.set.is_empty() {
            return;
        }
        let live: HashSet<&str> = agents
            .iter()
            .filter(|a| !a.advisor)
            .map(|a| a.id.as_str())
            .collect();
        self.set.retain(|id| live.contains(id.as_str()));
        if self.anchor.as_deref().is_some_and(|a| !live.contains(a)) {
            self.anchor = None;
        }
    }
}

// ------------------------------------------------------------------ wording

fn agents_of(n: usize) -> String {
    if n == 1 {
        "1 agent".to_owned()
    } else {
        format!("{n} agents")
    }
}

const IRREVERSIBLE: &str =
    "The files are removed from disk and omp stops tracking them. This cannot be undone.";

/// The confirmation of a request (the plan must not be empty).
pub fn confirm(scope: &Scope, plan: &Plan) -> Confirm {
    let (title, confirm_label, mut body) = match scope {
        Scope::CleanUp => (
            format!("Clean up {}?", agents_of(plan.total)),
            "Clean up",
            format!(
                "Permanently deletes {} that {} not running.",
                agents_of(plan.total),
                if plan.total == 1 { "is" } else { "are" }
            ),
        ),
        Scope::Selected => (
            format!("Delete {}?", agents_of(plan.total)),
            "Delete",
            format!("Permanently deletes the {} you selected.", {
                let picked = plan.total - plan.nested;
                if picked == 1 {
                    "agent".to_owned()
                } else {
                    format!("{picked} agents")
                }
            }),
        ),
        Scope::One(id) => (
            format!("Delete agent “{id}”?"),
            "Delete",
            format!("Permanently deletes “{id}”."),
        ),
    };
    if plan.nested > 0 {
        let them = if plan.targets.len() == 1 {
            "it"
        } else {
            "them"
        };
        body.push_str(&format!(
            " {} nested below {them} {} deleted with {them}.",
            agents_of(plan.nested),
            if plan.nested == 1 { "is" } else { "are" },
        ));
    }
    if plan.running > 0 {
        body.push_str(&format!(
            " {} {} running and will be aborted first.",
            agents_of(plan.running),
            if plan.running == 1 { "is" } else { "are" }
        ));
    }
    if *scope == Scope::CleanUp && plan.kept_running > 0 {
        body.push_str(&format!(
            " {} running {} kept",
            agents_of(plan.kept_running),
            if plan.kept_running == 1 { "is" } else { "are" }
        ));
        if plan.kept_parents > 0 {
            body.push_str(&format!(
                ", together with {} of running agents",
                if plan.kept_parents == 1 {
                    "1 parent".to_owned()
                } else {
                    format!("{} parents", plan.kept_parents)
                },
            ));
        }
        body.push('.');
    }
    body.push(' ');
    body.push_str(IRREVERSIBLE);
    Confirm {
        title,
        body,
        confirm_label: confirm_label.to_owned(),
        danger: true,
    }
}

/// What the user is told about omp's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// Something was deleted (a success toast); otherwise an error toast.
    pub ok: bool,
    pub text: String,
    /// The ids that are gone.
    pub deleted: Vec<String>,
}

/// `Deleted 3 agents · 2 skipped: 1 running, 1 not found`, or why nothing happened.
pub fn verdict(reply: Result<CtlReply, String>) -> Verdict {
    let fail = |text: String| Verdict {
        ok: false,
        text,
        deleted: Vec::new(),
    };
    let reply = match reply {
        Ok(r) => r,
        Err(e) => return fail(format!("Could not delete agents: {e}")),
    };
    if !reply.ok {
        return fail(match reply.error.as_deref() {
            Some("unknown cmd") => STALE_EXTENSION.to_owned(),
            Some("session changed") => {
                "Nothing was deleted: the session changed since you asked".to_owned()
            }
            Some(other) => format!("Could not delete agents: {other}"),
            None => "Could not delete agents: omp refused".to_owned(),
        });
    }
    let Some(outcome) = DeleteOutcome::of(&reply) else {
        return fail("Could not delete agents: omp sent no result".to_owned());
    };
    summarize(&outcome)
}

fn summarize(outcome: &DeleteOutcome) -> Verdict {
    let skipped = skipped_text(outcome);
    let n = outcome.deleted.len();
    if n == 0 {
        return Verdict {
            ok: false,
            text: if skipped.is_empty() {
                "Nothing was deleted".to_owned()
            } else {
                format!("Nothing was deleted: {skipped}")
            },
            deleted: Vec::new(),
        };
    }
    let mut text = format!("Deleted {}", agents_of(n));
    if !skipped.is_empty() {
        text.push_str(" · ");
        text.push_str(&skipped);
    }
    Verdict {
        ok: true,
        text,
        deleted: outcome.deleted.clone(),
    }
}

/// `2 skipped: 1 running, 1 not found`; empty when nothing was skipped.
fn skipped_text(outcome: &DeleteOutcome) -> String {
    let mut reasons: Vec<(&str, usize)> = Vec::new();
    for s in &outcome.skipped {
        match reasons.iter_mut().find(|(r, _)| *r == s.reason) {
            Some((_, n)) => *n += 1,
            None => reasons.push((s.reason.as_str(), 1)),
        }
    }
    if reasons.is_empty() {
        return String::new();
    }
    let list: Vec<String> = reasons.iter().map(|(r, n)| format!("{n} {r}")).collect();
    format!("{} skipped: {}", outcome.skipped.len(), list.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use deck_core::ctl::SkippedAgent;
    use std::path::PathBuf;

    fn agent(id: &str, parent: Option<&str>, status: AgentStatus) -> AgentSummary {
        AgentSummary {
            id: id.to_owned(),
            parent_id: parent.map(str::to_owned),
            depth: 0,
            path: PathBuf::from(format!("/s/{id}.jsonl")),
            advisor: false,
            agent: None,
            model: None,
            thinking: None,
            task_gist: String::new(),
            task: String::new(),
            status,
            error: None,
            current: None,
            created: None,
            last_activity: None,
            duration: None,
            requests: 0,
            tool_calls: 0,
            tokens: 0,
            cost: 0.0,
            context_tokens: None,
            output_path: None,
        }
    }

    fn advisor(id: &str, parent: Option<&str>) -> AgentSummary {
        AgentSummary {
            advisor: true,
            ..agent(id, parent, AgentStatus::Done)
        }
    }

    fn ids(plan: &Plan) -> Vec<&str> {
        plan.targets.iter().map(|t| t.id.as_str()).collect()
    }

    use AgentStatus::{Aborted, Done, Failed, Interrupted, Running};

    #[test]
    fn cleanup_takes_every_stopped_agent_and_keeps_running_ones() {
        let a = vec![
            agent("A", None, Done),
            agent("B", None, Running),
            agent("C", None, Failed),
            agent("D", None, Aborted),
            agent("E", None, Interrupted),
        ];
        let p = cleanup(&a);
        assert_eq!(ids(&p), ["A", "C", "D", "E"]);
        assert_eq!((p.total, p.running, p.nested), (4, 0, 0));
        assert_eq!((p.kept_running, p.kept_parents), (1, 0));
        // The request carries the scanned transcript of each.
        assert_eq!(p.targets[1].transcript, PathBuf::from("/s/C.jsonl"));
    }

    #[test]
    fn cleanup_sends_only_the_top_most_targets() {
        let a = vec![
            agent("P", None, Done),
            agent("P.k", Some("P"), Failed),
            agent("P.k.d", Some("P.k"), Done),
            agent("Q", None, Done),
        ];
        let p = cleanup(&a);
        assert_eq!(ids(&p), ["P", "Q"]);
        assert_eq!((p.total, p.nested), (4, 0));
    }

    #[test]
    fn a_running_descendant_keeps_every_ancestor() {
        let a = vec![
            agent("P", None, Done),
            agent("P.k", Some("P"), Done),
            agent("P.k.run", Some("P.k"), Running),
            agent("P.s", Some("P"), Failed),
            agent("Q", None, Done),
        ];
        let p = cleanup(&a);
        // P and P.k hold a running agent: only the quiet sibling subtree and Q go.
        assert_eq!(ids(&p), ["P.s", "Q"]);
        assert_eq!((p.total, p.nested), (2, 0));
        assert_eq!((p.kept_running, p.kept_parents), (1, 2));
    }

    #[test]
    fn advisors_are_never_targets_or_counted() {
        let a = vec![
            advisor("__advisor1", None),
            agent("P", None, Done),
            advisor("__advisor2", Some("P")),
            agent("P.k", Some("P"), Done),
        ];
        let p = cleanup(&a);
        assert_eq!(ids(&p), ["P"]);
        assert_eq!((p.total, p.nested), (2, 0));
        let s = selected(
            &a,
            &["__advisor1".to_owned(), "__advisor2".to_owned()].into(),
        );
        assert!(s.is_empty());
        // A quiet child of an advisor is still a target of its own.
        let b = vec![advisor("adv", None), agent("kid", Some("adv"), Done)];
        assert_eq!(ids(&cleanup(&b)), ["kid"]);
    }

    #[test]
    fn nothing_to_clean_up_gives_an_empty_plan() {
        let a = vec![agent("R", None, Running), agent("R.k", Some("R"), Done)];
        let p = cleanup(&a);
        // A finished child of a running agent is not running: it goes.
        assert_eq!(ids(&p), ["R.k"]);
        let busy = vec![agent("R", None, Running), agent("S", None, Running)];
        let p = cleanup(&busy);
        assert!(p.is_empty());
        assert_eq!((p.kept_running, p.kept_parents, p.total), (2, 0, 0));
        assert!(cleanup(&[]).is_empty());
    }

    #[test]
    fn a_selection_reduces_to_top_most_and_counts_what_goes_with_it() {
        let a = vec![
            agent("P", None, Done),
            agent("P.k", Some("P"), Running),
            agent("P.k.d", Some("P.k"), Done),
            agent("Q", None, Running),
            agent("R", None, Done),
        ];
        let pick: HashSet<String> = ["P", "P.k.d", "Q"].map(str::to_owned).into();
        let p = selected(&a, &pick);
        assert_eq!(ids(&p), ["P", "Q"]);
        // P, P.k, P.k.d and Q go; P.k runs without having been picked, Q runs and was.
        assert_eq!((p.total, p.running, p.nested), (4, 2, 1));
        let one = selected(&a, &["R".to_owned()].into());
        assert_eq!(
            (ids(&one), one.total, one.running, one.nested),
            (vec!["R"], 1, 0, 0)
        );
        assert!(selected(&a, &HashSet::new()).is_empty());
        assert!(selected(&a, &["ghost".to_owned()].into()).is_empty());
    }

    #[test]
    fn shift_click_selects_the_range_between_anchor_and_click() {
        let order = ["a", "b", "c", "d", "e"];
        let mut s = Select::default();
        s.click(&order, "b", false);
        s.click(&order, "d", true);
        assert_eq!(s.len(), 3);
        assert!(["b", "c", "d"].iter().all(|i| s.contains(i)));
        // Without an anchor in view Shift just toggles.
        let mut t = Select::default();
        t.click(&order, "c", true);
        assert!(t.contains("c") && t.len() == 1);
        t.click(&order, "c", false);
        assert!(t.is_empty());
        // Backwards ranges work too.
        let mut u = Select::default();
        u.click(&order, "e", false);
        u.click(&order, "c", true);
        assert_eq!(u.len(), 3);
    }

    #[test]
    fn select_all_toggles_over_the_visible_rows() {
        let order = ["a", "b", "c"];
        let mut s = Select::default();
        assert_eq!(s.check(&order), Check::None);
        s.click(&order, "a", false);
        assert_eq!(s.check(&order), Check::Some);
        s.toggle_all(&order);
        assert_eq!(s.check(&order), Check::All);
        s.toggle_all(&order);
        assert_eq!(s.check(&order), Check::None);
        assert_eq!(Select::default().check(&[]), Check::None);
    }

    #[test]
    fn pruning_drops_vanished_and_advisor_ids() {
        let a = vec![agent("A", None, Done), advisor("adv", None)];
        let mut s = Select::default();
        s.click(&["A", "ghost", "adv"], "A", false);
        s.click(&["A", "ghost", "adv"], "ghost", true);
        s.click(&["A", "ghost", "adv"], "adv", false);
        assert_eq!(s.len(), 3);
        s.prune(&a);
        assert_eq!(s.ids().iter().collect::<Vec<_>>(), [&"A".to_owned()]);
        s.remove(&["A".to_owned()]);
        assert!(s.is_empty());
    }

    #[test]
    fn the_cleanup_confirmation_states_counts_and_irreversibility() {
        let a = vec![
            agent("P", None, Done),
            agent("P.k", Some("P"), Done),
            agent("Hold", None, Done),
            agent("Hold.run", Some("Hold"), Running),
            agent("Run", None, Running),
        ];
        let c = confirm(&Scope::CleanUp, &cleanup(&a));
        assert!(c.danger);
        assert_eq!(c.title, "Clean up 2 agents?");
        assert!(
            c.body
                .contains("Permanently deletes 2 agents that are not running"),
            "{}",
            c.body
        );
        assert!(!c.body.contains("nested"), "{}", c.body);
        assert!(
            c.body
                .contains("2 agents running are kept, together with 1 parent of running agents."),
            "{}",
            c.body
        );
        assert!(c.body.contains("cannot be undone"));
    }

    #[test]
    fn the_delete_confirmation_names_running_and_nested_agents() {
        let a = vec![
            agent("P", None, Done),
            agent("P.k", Some("P"), Running),
            agent("P.j", Some("P"), Running),
            agent("Q", None, Done),
        ];
        let plan = selected(&a, &["P".to_owned(), "Q".to_owned()].into());
        let c = confirm(&Scope::Selected, &plan);
        assert_eq!(c.title, "Delete 4 agents?");
        assert!(
            c.body
                .contains("2 agents nested below them are deleted with them"),
            "{}",
            c.body
        );
        assert!(
            c.body.contains("2 agents are running and will be aborted"),
            "{}",
            c.body
        );
        assert!(c.body.contains("cannot be undone"));
        let one = confirm(
            &Scope::One("Q".into()),
            &selected(&a, &["Q".to_owned()].into()),
        );
        assert_eq!(one.title, "Delete agent “Q”?");
        assert!(!one.body.contains("nested") && !one.body.contains("running"));
    }

    fn reply(ok: bool, value: Option<serde_json::Value>, error: Option<&str>) -> CtlReply {
        CtlReply {
            ok,
            value,
            error: error.map(str::to_owned),
            ..CtlReply::default()
        }
    }

    #[test]
    fn the_verdict_summarizes_deleted_and_skipped_agents() {
        let outcome = DeleteOutcome {
            deleted: vec!["a".into(), "b".into(), "c".into()],
            skipped: vec![
                SkippedAgent {
                    id: "x".into(),
                    reason: "running".into(),
                },
                SkippedAgent {
                    id: "y".into(),
                    reason: "running".into(),
                },
                SkippedAgent {
                    id: "z".into(),
                    reason: "not found".into(),
                },
            ],
        };
        let v = verdict(Ok(reply(true, serde_json::to_value(&outcome).ok(), None)));
        assert!(v.ok);
        assert_eq!(
            v.text,
            "Deleted 3 agents · 3 skipped: 2 running, 1 not found"
        );
        assert_eq!(v.deleted, ["a", "b", "c"]);
        let clean = DeleteOutcome {
            deleted: vec!["a".into()],
            skipped: vec![],
        };
        let v = verdict(Ok(reply(true, serde_json::to_value(&clean).ok(), None)));
        assert_eq!(v.text, "Deleted 1 agent");
        let none = DeleteOutcome {
            deleted: vec![],
            skipped: outcome.skipped[..1].to_vec(),
        };
        let v = verdict(Ok(reply(true, serde_json::to_value(&none).ok(), None)));
        assert!(!v.ok && v.deleted.is_empty());
        assert_eq!(v.text, "Nothing was deleted: 1 skipped: 1 running");
    }

    #[test]
    fn refusals_become_specific_errors() {
        let stale = verdict(Ok(reply(false, None, Some("unknown cmd"))));
        assert!(!stale.ok);
        assert_eq!(stale.text, STALE_EXTENSION);
        let changed = verdict(Ok(reply(false, None, Some("session changed"))));
        assert!(changed.text.contains("session changed"));
        assert!(
            verdict(Err("gateway down".into()))
                .text
                .contains("gateway down")
        );
        assert!(
            verdict(Ok(reply(true, None, None)))
                .text
                .contains("no result")
        );
    }
}
