//! The client's latest-state cache: one [`Slot`] per always-on topic (and the dashboard topics),
//! plus a map for the other interest-driven topics. Values are replaced whole, never patched, so a
//! reader always sees one consistent gateway state per topic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use deck_core::agents::AgentsSnapshot;
use deck_core::fleet::FleetSnapshot;
use deck_core::status::SessionStatus;

use crate::controller::SessionTarget;
use crate::gateway::protocol::{
    AppUpdateView, BackupView, ControllerInfo, GatewayStatus, InboxView, OmpUpdateView,
    SessionsView, Topic, TopicValue, UsageView,
};
use crate::gateway::running::RunningSnapshot;
use crate::settings::Snapshot;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Topics the gateway keeps current for every GUI client without a subscription.
pub const ALWAYS_ON: &[Topic] = &[
    Topic::Gateway,
    Topic::Settings,
    Topic::Sessions,
    Topic::Running,
    Topic::Controllers,
    Topic::Notifications,
    Topic::Status,
    Topic::Usage,
    Topic::AppUpdate,
    Topic::Backup,
    Topic::OmpUpdate,
];

/// The latest value of one topic and its client-side revision.
pub struct Slot<T> {
    inner: Mutex<(u64, Option<Arc<T>>)>,
}

impl<T> Default for Slot<T> {
    fn default() -> Slot<T> {
        Slot {
            inner: Mutex::new((0, None)),
        }
    }
}

impl<T> Slot<T> {
    /// The latest value; `None` before the first fetch. Kept (stale) while disconnected.
    pub fn get(&self) -> Option<Arc<T>> {
        lock(&self.inner).1.clone()
    }

    /// Grows with every stored value (0 = never stored); unique across the cache, so it changes
    /// on every update even across gateway restarts.
    pub fn revision(&self) -> u64 {
        lock(&self.inner).0
    }

    fn set(&self, revision: u64, value: T) {
        *lock(&self.inner) = (revision, Some(Arc::new(value)));
    }
}

/// Latest-state cache of a [`super::GatewayClient`]. Cheap to read from the UI thread.
#[derive(Default)]
pub struct StateCache {
    pub gateway: Slot<GatewayStatus>,
    pub settings: Slot<Snapshot>,
    pub sessions: Slot<SessionsView>,
    pub running: Slot<RunningSnapshot>,
    pub controllers: Slot<Vec<ControllerInfo>>,
    pub notifications: Slot<InboxView>,
    /// The status of the focused client's active session.
    pub status: Slot<Option<(SessionTarget, SessionStatus)>>,
    pub usage: Slot<UsageView>,
    pub app_update: Slot<AppUpdateView>,
    pub backup: Slot<BackupView>,
    pub omp_update: Slot<OmpUpdateView>,
    /// [`Topic::Fleet`] (interest-driven).
    pub fleet: Slot<Option<FleetSnapshot>>,
    /// [`Topic::Agents`] (interest-driven).
    pub agents: Slot<Option<AgentsSnapshot>>,
    /// Every other interest-driven topic (`Inventory`, `Changes`, `Tree`, `Transcript`, `Live`).
    other: Mutex<HashMap<Topic, (u64, TopicValue)>>,
    revisions: AtomicU64,
}

impl StateCache {
    /// The cached value of any topic as the gateway sent it.
    pub fn topic(&self, topic: &Topic) -> Option<TopicValue> {
        fn arc<T: Clone>(s: &Slot<T>) -> Option<T> {
            s.get().map(|v| (*v).clone())
        }
        match topic {
            Topic::Gateway => arc(&self.gateway).map(TopicValue::Gateway),
            Topic::Settings => arc(&self.settings).map(TopicValue::Settings),
            Topic::Sessions => arc(&self.sessions).map(TopicValue::Sessions),
            Topic::Running => arc(&self.running).map(TopicValue::Running),
            Topic::Controllers => arc(&self.controllers).map(TopicValue::Controllers),
            Topic::Notifications => arc(&self.notifications).map(TopicValue::Notifications),
            Topic::Status => arc(&self.status).map(|v| TopicValue::Status(Box::new(v))),
            Topic::Usage => arc(&self.usage).map(TopicValue::Usage),
            Topic::AppUpdate => arc(&self.app_update).map(TopicValue::AppUpdate),
            Topic::Backup => arc(&self.backup).map(TopicValue::Backup),
            Topic::OmpUpdate => arc(&self.omp_update).map(TopicValue::OmpUpdate),
            Topic::Fleet => arc(&self.fleet).map(TopicValue::Fleet),
            Topic::Agents => arc(&self.agents).map(TopicValue::Agents),
            other => lock(&self.other).get(other).map(|(_, v)| v.clone()),
        }
    }

    /// The revision of a topic's cached value (0 = none yet): a cheap change test.
    pub fn revision(&self, topic: &Topic) -> u64 {
        match topic {
            Topic::Gateway => self.gateway.revision(),
            Topic::Settings => self.settings.revision(),
            Topic::Sessions => self.sessions.revision(),
            Topic::Running => self.running.revision(),
            Topic::Controllers => self.controllers.revision(),
            Topic::Notifications => self.notifications.revision(),
            Topic::Status => self.status.revision(),
            Topic::Usage => self.usage.revision(),
            Topic::AppUpdate => self.app_update.revision(),
            Topic::Backup => self.backup.revision(),
            Topic::OmpUpdate => self.omp_update.revision(),
            Topic::Fleet => self.fleet.revision(),
            Topic::Agents => self.agents.revision(),
            other => lock(&self.other).get(other).map_or(0, |(r, _)| *r),
        }
    }

    fn next_revision(&self) -> u64 {
        self.revisions.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Stores a fetched value under `topic`; returns its new revision.
    pub(crate) fn store(&self, topic: &Topic, value: TopicValue) -> u64 {
        let rev = self.next_revision();
        match value {
            TopicValue::Gateway(v) => self.gateway.set(rev, v),
            TopicValue::Settings(v) => self.settings.set(rev, v),
            TopicValue::Sessions(v) => self.sessions.set(rev, v),
            TopicValue::Running(v) => self.running.set(rev, v),
            TopicValue::Controllers(v) => self.controllers.set(rev, v),
            TopicValue::Notifications(v) => self.notifications.set(rev, v),
            TopicValue::Status(v) => self.status.set(rev, *v),
            TopicValue::Usage(v) => self.usage.set(rev, v),
            TopicValue::AppUpdate(v) => self.app_update.set(rev, v),
            TopicValue::Backup(v) => self.backup.set(rev, v),
            TopicValue::OmpUpdate(v) => self.omp_update.set(rev, v),
            TopicValue::Fleet(v) => self.fleet.set(rev, v),
            TopicValue::Agents(v) => self.agents.set(rev, v),
            other => {
                lock(&self.other).insert(topic.clone(), (rev, other));
            }
        }
        rev
    }

    /// A settings result newer than the cached one (an edit's own commit, before its `Changed`).
    pub(crate) fn offer_settings(&self, snap: &Snapshot) -> Option<u64> {
        let newer = self
            .settings
            .get()
            .is_none_or(|cur| snap.revision > cur.revision);
        newer.then(|| self.store(&Topic::Settings, TopicValue::Settings(snap.clone())))
    }

    /// Forgets an interest-driven topic no longer followed.
    pub(crate) fn forget(&self, topic: &Topic) {
        lock(&self.other).remove(topic);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::protocol::InboxView;

    #[test]
    fn values_land_in_their_slot_and_revisions_always_grow() {
        let c = StateCache::default();
        assert_eq!(c.revision(&Topic::Notifications), 0);
        assert!(c.notifications.get().is_none());
        let inbox = InboxView {
            entries: Vec::new(),
            warns: true,
            badge: Some("1".into()),
        };
        let r1 = c.store(
            &Topic::Notifications,
            TopicValue::Notifications(inbox.clone()),
        );
        assert_eq!(
            c.notifications.get().expect("stored").badge.as_deref(),
            Some("1")
        );
        let r2 = c.store(&Topic::Notifications, TopicValue::Notifications(inbox));
        assert!(r2 > r1);
        assert_eq!(c.revision(&Topic::Notifications), r2);

        let t = Topic::Inventory;
        let r3 = c.store(&t, TopicValue::Inventory(Vec::new()));
        assert!(r3 > r2);
        assert!(matches!(c.topic(&t), Some(TopicValue::Inventory(v)) if v.is_empty()));
        c.forget(&t);
        assert!(c.topic(&t).is_none());
        assert_eq!(c.revision(&t), 0);
    }
}
