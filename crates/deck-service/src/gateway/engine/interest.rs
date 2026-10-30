//! Interest-driven state: the heavy watchers (inventory, git changes, session trees,
//! transcripts, the agents fleet and the agents hub) run only while some link subscribes.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deck_core::agents::{AgentsTarget, AgentsWatcher, TranscriptWatcher};
use deck_core::changes::Changes;
use deck_core::fleet::{FleetTarget, FleetWatcher};
use deck_core::inventory::InventoryEntry;
use deck_core::tree::TreeWatcher;

use super::{Engine, Fin, lock};
use crate::gateway::protocol::{GatewayError, Role, Topic, TopicValue};

/// A followed repository's changes are recomputed this often.
const CHANGES_EVERY: Duration = Duration::from_secs(3);

type ChangesResult = Result<Option<Changes>, String>;

struct ChangesWatch {
    value: Option<ChangesResult>,
    busy: bool,
    next: Option<Instant>,
    /// A mutation changed the repository: recompute as soon as the running pass ends.
    force: bool,
}

#[derive(Default)]
pub(super) struct Interest {
    inventory: Option<Arc<Vec<InventoryEntry>>>,
    /// The sessions generation the inventory was (or is being) scanned at.
    inventory_gen: Option<u64>,
    inventory_busy: bool,
    changes: HashMap<PathBuf, ChangesWatch>,
    trees: HashMap<PathBuf, (TreeWatcher, u64)>,
    transcripts: HashMap<PathBuf, (TranscriptWatcher, u64)>,
    fleet: Option<FleetFollow>,
    agents: Option<AgentsFollow>,
}

struct FleetFollow {
    watcher: FleetWatcher,
    targets: Vec<FleetTarget>,
    seen: Option<Arc<deck_core::fleet::FleetSnapshot>>,
}

struct AgentsFollow {
    watcher: AgentsWatcher,
    target: Option<AgentsTarget>,
    seen: Option<Arc<deck_core::agents::AgentsSnapshot>>,
}

fn same<T>(a: &Option<Arc<T>>, b: &Option<Arc<T>>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        (None, None) => true,
        _ => false,
    }
}

impl Engine {
    pub(super) fn step_interest(&mut self, now: Instant) {
        let wanted: BTreeSet<Topic> = self
            .conns
            .values()
            .flat_map(|c| c.subs.iter().cloned())
            .collect();
        let mut changed: Vec<Topic> = Vec::new();
        self.step_inventory(wanted.contains(&Topic::Inventory));
        self.step_changes(&wanted, now);
        let wake = self.waker.callback();

        // Session trees.
        self.interest
            .trees
            .retain(|p, _| wanted.contains(&Topic::Tree(p.clone())));
        for t in &wanted {
            let Topic::Tree(path) = t else { continue };
            let (w, seen) = self.interest.trees.entry(path.clone()).or_insert_with(|| {
                let wake = wake.clone();
                (TreeWatcher::spawn(path.clone(), move || wake()), u64::MAX)
            });
            let rev = w.revision();
            if rev != *seen {
                *seen = rev;
                changed.push(t.clone());
            }
        }

        // Subagent transcripts.
        self.interest
            .transcripts
            .retain(|p, _| wanted.contains(&Topic::Transcript(p.clone())));
        for t in &wanted {
            let Topic::Transcript(path) = t else { continue };
            let (w, seen) = self
                .interest
                .transcripts
                .entry(path.clone())
                .or_insert_with(|| {
                    let wake = wake.clone();
                    (
                        TranscriptWatcher::spawn(path.clone(), move || wake()),
                        u64::MAX,
                    )
                });
            let rev = w.revision();
            if rev != *seen {
                *seen = rev;
                changed.push(t.clone());
            }
        }

        // The fleet: the union of the subscribers' sessions.
        if wanted.contains(&Topic::Fleet) {
            let mut targets: Vec<FleetTarget> = Vec::new();
            for c in self
                .conns
                .values()
                .filter(|c| c.subs.contains(&Topic::Fleet))
            {
                for t in &c.fleet {
                    if !targets.iter().any(|x| x.session == t.session) {
                        targets.push(t.clone());
                    }
                }
            }
            let follow = self.interest.fleet.get_or_insert_with(|| {
                let wake = wake.clone();
                FleetFollow {
                    watcher: FleetWatcher::spawn(move || wake()),
                    targets: Vec::new(),
                    seen: None,
                }
            });
            if follow.targets != targets {
                follow.watcher.set_targets(targets.clone());
                follow.targets = targets;
            }
            let latest = follow.watcher.latest();
            if !same(&follow.seen, &latest) {
                follow.seen = latest;
                changed.push(Topic::Fleet);
            }
        } else {
            self.interest.fleet = None;
        }

        // The agents hub: the focused GUI's session, else the most recently set one.
        if wanted.contains(&Topic::Agents) {
            let subscribed = || {
                self.conns
                    .values()
                    .filter(|c| c.subs.contains(&Topic::Agents) && c.agents.is_some())
            };
            let target = subscribed()
                .filter(|c| c.is(Role::Gui) && c.state.focused)
                .max_by_key(|c| c.agents_seq)
                .or_else(|| subscribed().max_by_key(|c| c.agents_seq))
                .and_then(|c| c.agents.clone());
            let follow = self.interest.agents.get_or_insert_with(|| {
                let wake = wake.clone();
                AgentsFollow {
                    watcher: AgentsWatcher::spawn(move || wake()),
                    target: None,
                    seen: None,
                }
            });
            if follow.target != target {
                follow.watcher.set_target(target.clone());
                follow.target = target;
            }
            let latest = follow.watcher.latest();
            if !same(&follow.seen, &latest) {
                follow.seen = latest;
                changed.push(Topic::Agents);
            }
        } else {
            self.interest.agents = None;
        }

        for t in changed {
            self.changed(t);
        }
    }

    fn step_inventory(&mut self, wanted: bool) {
        if !wanted {
            self.interest.inventory = None;
            self.interest.inventory_gen = None;
            return;
        }
        let generation = self.sessions_watch.generation();
        if self.interest.inventory_busy || self.interest.inventory_gen == Some(generation) {
            return;
        }
        self.interest.inventory_busy = true;
        self.interest.inventory_gen = Some(generation);
        let root = self.root.clone();
        self.spawn_worker("inventory", move || {
            Fin::Inventory(deck_core::inventory::scan(&root))
        });
    }

    pub(super) fn on_inventory(&mut self, entries: Vec<InventoryEntry>) {
        self.interest.inventory_busy = false;
        let entries = Arc::new(entries);
        // Stats queries compute for the latest scan, followed or not.
        *lock(&self.inventory_shared) = entries.clone();
        if self.interest.inventory_gen.is_none() {
            // Nobody follows it any more.
            return;
        }
        if self.interest.inventory.as_deref() != Some(&*entries) {
            self.interest.inventory = Some(entries);
            self.changed(Topic::Inventory);
        }
    }

    fn step_changes(&mut self, wanted: &BTreeSet<Topic>, now: Instant) {
        self.interest
            .changes
            .retain(|p, _| wanted.contains(&Topic::Changes(p.clone())));
        let mut due: Vec<PathBuf> = Vec::new();
        let mut next: Option<Instant> = None;
        for t in wanted {
            let Topic::Changes(path) = t else { continue };
            let w = self
                .interest
                .changes
                .entry(path.clone())
                .or_insert_with(|| ChangesWatch {
                    value: None,
                    busy: false,
                    next: None,
                    force: false,
                });
            if w.busy {
                continue;
            }
            if w.force || w.next.is_none_or(|n| n <= now) {
                w.busy = true;
                w.force = false;
                due.push(path.clone());
            } else if let Some(n) = w.next {
                next = Some(next.map_or(n, |x| x.min(n)));
            }
        }
        if let Some(n) = next {
            self.wake_within(n.saturating_duration_since(now));
        }
        for root in due {
            self.spawn_worker("changes", move || Fin::Changes {
                result: deck_core::changes::list(&root).map_err(|e| e.to_string()),
                root,
            });
        }
    }

    /// A mutation touched `root`: recompute its changes now (when followed).
    pub(super) fn refresh_changes(&mut self, root: &Path) {
        if let Some(w) = self.interest.changes.get_mut(root) {
            w.force = true;
            self.waker.now();
        }
    }

    pub(super) fn on_changes(&mut self, root: PathBuf, result: ChangesResult) {
        let Some(w) = self.interest.changes.get_mut(&root) else {
            return;
        };
        w.busy = false;
        w.next = Some(Instant::now() + CHANGES_EVERY);
        if w.value.as_ref() != Some(&result) {
            w.value = Some(result);
            self.changed(Topic::Changes(root));
        }
    }

    /// The followed changes listing of `root` (`None`: not computed yet or not followed).
    pub(super) fn changes_of(&self, root: &Path) -> Option<&ChangesResult> {
        self.interest.changes.get(root)?.value.as_ref()
    }

    /// The items of a followed transcript (`None`: not followed yet).
    pub(super) fn transcript_of(
        &self,
        path: &Path,
    ) -> Option<Arc<Vec<deck_core::agents::TranscriptItem>>> {
        self.interest.transcripts.get(path).map(|(w, _)| w.latest())
    }

    /// The fleet's latest snapshot, while followed.
    pub(super) fn fleet_latest(&self) -> Option<Arc<deck_core::fleet::FleetSnapshot>> {
        self.interest.fleet.as_ref()?.watcher.latest()
    }

    /// The value of an interest-driven topic (the caller checked the subscription).
    pub(super) fn interest_value(&self, topic: &Topic) -> Result<TopicValue, GatewayError> {
        let i = &self.interest;
        Ok(match topic {
            Topic::Inventory => {
                TopicValue::Inventory(i.inventory.as_deref().cloned().unwrap_or_default())
            }
            Topic::Changes(path) => TopicValue::Changes(
                i.changes
                    .get(path)
                    .and_then(|w| w.value.clone())
                    .unwrap_or(Ok(None)),
            ),
            Topic::Tree(path) => match i.trees.get(path) {
                Some((w, _)) => TopicValue::Tree {
                    revision: w.revision(),
                    tree: w.latest().map(|t| (*t).clone()),
                    error: w.error(),
                },
                None => TopicValue::Tree {
                    revision: 0,
                    tree: None,
                    error: None,
                },
            },
            Topic::Transcript(path) => match i.transcripts.get(path) {
                Some((w, _)) => TopicValue::Transcript {
                    revision: w.revision(),
                    items: (*w.latest()).clone(),
                },
                None => TopicValue::Transcript {
                    revision: 0,
                    items: Vec::new(),
                },
            },
            Topic::Fleet => TopicValue::Fleet(
                i.fleet
                    .as_ref()
                    .and_then(|f| f.watcher.latest())
                    .map(|s| (*s).clone()),
            ),
            Topic::Agents => TopicValue::Agents(
                i.agents
                    .as_ref()
                    .and_then(|a| a.watcher.latest())
                    .map(|s| (*s).clone()),
            ),
            Topic::Gateway
            | Topic::Settings
            | Topic::Sessions
            | Topic::Running
            | Topic::Controllers
            | Topic::Notifications
            | Topic::Status
            | Topic::Usage
            | Topic::AppUpdate
            | Topic::Drive
            | Topic::Live(_)
            | Topic::OmpUpdate
            | Topic::Mobile => {
                return Err(GatewayError::Refused(format!(
                    "{topic:?} is not an interest-driven topic"
                )));
            }
        })
    }
}
