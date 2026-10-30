//! The private, durable record of accepted mutations (`<state dir>/journal`, 0600).
//!
//! A mutation is journaled — and the file synced — before it executes, and its final outcome is
//! appended when it ends. Only identity, kind, target, a hash of the operation and the outcome
//! class are written: never prompt text, typed input, setting values or tokens.
//!
//! - A repeated key with the same operation hash recovers the earlier record (no new work).
//! - A repeated key with a different hash is refused ([`Admission::KeyReused`]).
//! - Keys older than a client's retained window stay [`Admission::Expired`] (watermarks persist).
//! - After a crash, records accepted without an outcome become [`Outcome::Uncertain`]. Nothing
//!   is ever replayed.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::protocol::{Mutation, Outcome};
use crate::controller::{RequestKey, ServiceEpoch};

/// Records kept per client before the oldest are evicted (and their keys expire).
const PER_CLIENT: usize = 256;
/// Clients whose watermarks are kept.
const MAX_CLIENTS: usize = 1024;
/// Appended lines before the file is rewritten compactly.
const COMPACT_AFTER: usize = 8192;

/// The persisted outcome class of a record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Class {
    Done,
    Failed,
    Rejected,
    Cancelled,
    Uncertain,
}

impl Class {
    fn of(o: &Outcome) -> Option<Class> {
        Some(match o {
            Outcome::Running => return None,
            Outcome::Done(_) => Class::Done,
            Outcome::Failed(_) => Class::Failed,
            Outcome::Rejected(_) => Class::Rejected,
            Outcome::Cancelled => Class::Cancelled,
            Outcome::Uncertain => Class::Uncertain,
            Outcome::Recorded { .. } => return None,
        })
    }

    /// A record of an earlier instance: only the class is known.
    fn recalled(&self) -> Outcome {
        match self {
            Class::Uncertain => Outcome::Uncertain,
            Class::Cancelled => Outcome::Cancelled,
            c => Outcome::Recorded {
                succeeded: *c == Class::Done,
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
enum Line {
    Accept {
        client: String,
        seq: u64,
        kind: String,
        target: Option<String>,
        hash: String,
        epoch: ServiceEpoch,
    },
    End {
        client: String,
        seq: u64,
        class: Class,
    },
    Watermark {
        client: String,
        seq: u64,
    },
}

struct Record {
    seq: u64,
    kind: String,
    target: Option<String>,
    hash: String,
    epoch: ServiceEpoch,
    /// Full outcome of this instance, or the recalled class of an earlier one.
    outcome: Outcome,
}

#[derive(Default)]
struct Client {
    /// Oldest first.
    records: VecDeque<Record>,
    /// Every seq at or below is expired.
    watermark: u64,
    touched: u64,
}

/// What [`Journal::admit`] decided.
#[derive(Debug)]
pub enum Admission {
    /// New: journaled and synced; execute it.
    Accepted,
    /// Accepted before: here is its record. Do not execute again.
    Duplicate(Outcome),
    KeyReused,
    Expired,
}

/// The journal of one gateway identity.
pub struct Journal {
    path: PathBuf,
    file: File,
    clients: HashMap<String, Client>,
    lines: usize,
    clock: u64,
}

/// Hash of an operation: what decides "same work" for a repeated key.
pub fn hash(op: &Mutation) -> String {
    hash_of(op)
}

/// Hash of any serializable description of the work (a device's request names its work in
/// device terms: the mutation it becomes may depend on state that changes between retries).
pub fn hash_of(work: &impl serde::Serialize) -> String {
    let bytes = serde_json::to_vec(work).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

impl Journal {
    /// Opens (creating owner-only) and recovers the journal: accepted records without an
    /// outcome are marked uncertain, durably.
    pub fn open(path: &Path) -> io::Result<Journal> {
        if let Some(parent) = path.parent() {
            super::private_dir(parent)?;
        }
        let mut clients: HashMap<String, Client> = HashMap::new();
        let mut lines = 0;
        match File::open(path) {
            Ok(f) => {
                for line in BufReader::new(f).lines() {
                    let Ok(line) = line else { break };
                    lines += 1;
                    // A torn last line (crash mid-append) is skipped; it was never synced as
                    // accepted, so its request was not executed.
                    let Ok(parsed) = serde_json::from_str::<Line>(&line) else {
                        continue;
                    };
                    apply(&mut clients, parsed);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let mut uncertain = Vec::new();
        for (client, c) in &mut clients {
            for r in &mut c.records {
                if matches!(r.outcome, Outcome::Running) {
                    r.outcome = Outcome::Uncertain;
                    uncertain.push((client.clone(), r.seq));
                }
            }
        }
        let file = open_append(path)?;
        let mut j = Journal {
            path: path.to_path_buf(),
            file,
            clients,
            lines,
            clock: 0,
        };
        // Rewrite compactly at startup: it also persists the uncertain verdicts in one go.
        if !uncertain.is_empty() || j.lines > COMPACT_AFTER {
            j.compact()?;
        }
        Ok(j)
    }

    /// What a repeated `key` with work hash `work` is answered (`None`: the key is new).
    pub fn recall(&self, key: &RequestKey, work: &str) -> Option<Admission> {
        let c = self.clients.get(&key.client)?;
        if let Some(r) = c.records.iter().find(|r| r.seq == key.seq) {
            return Some(if r.hash == work {
                Admission::Duplicate(r.outcome.clone())
            } else {
                Admission::KeyReused
            });
        }
        (key.seq <= c.watermark).then_some(Admission::Expired)
    }

    /// Decides about `key`; a new key is journaled (synced) before this returns. `work` is the
    /// hash deciding "same work" ([`hash`] of `op` when `None`).
    pub fn admit(
        &mut self,
        key: &RequestKey,
        op: &Mutation,
        work: Option<String>,
        epoch: ServiceEpoch,
    ) -> io::Result<Admission> {
        let h = work.unwrap_or_else(|| hash(op));
        if let Some(known) = self.recall(key, &h) {
            return Ok(known);
        }
        let target = op.target().map(|t| format!("{}#{}", t.key, t.generation));
        self.append(&Line::Accept {
            client: key.client.clone(),
            seq: key.seq,
            kind: op.label().to_owned(),
            target: target.clone(),
            hash: h.clone(),
            epoch,
        })?;
        self.clock += 1;
        let clock = self.clock;
        let c = self.clients.entry(key.client.clone()).or_default();
        c.touched = clock;
        c.records.push_back(Record {
            seq: key.seq,
            kind: op.label().to_owned(),
            target,
            hash: h,
            epoch,
            outcome: Outcome::Running,
        });
        let mut evicted = None;
        while c.records.len() > PER_CLIENT {
            if let Some(old) = c.records.pop_front() {
                c.watermark = c.watermark.max(old.seq);
                evicted = Some(c.watermark);
            }
        }
        if let Some(seq) = evicted {
            self.append(&Line::Watermark {
                client: key.client.clone(),
                seq,
            })?;
        }
        self.evict_clients();
        Ok(Admission::Accepted)
    }

    /// Records the final outcome (best effort durable: a lost outcome line only makes a
    /// finished request look uncertain after a crash).
    pub fn finish(&mut self, key: &RequestKey, outcome: Outcome) {
        let Some(class) = Class::of(&outcome) else {
            return;
        };
        let Some(r) = self
            .clients
            .get_mut(&key.client)
            .and_then(|c| c.records.iter_mut().find(|r| r.seq == key.seq))
        else {
            return;
        };
        r.outcome = outcome;
        if let Err(e) = self.append(&Line::End {
            client: key.client.clone(),
            seq: key.seq,
            class,
        }) {
            log::warn!("journal: recording an outcome failed: {e}");
        }
        if self.lines > COMPACT_AFTER
            && let Err(e) = self.compact()
        {
            log::warn!("journal: compaction failed: {e}");
        }
    }

    /// The record of `key`; never executes anything. `Err(Expired)` for evicted keys; `Ok(None)`
    /// for keys never seen.
    pub fn lookup(&self, key: &RequestKey) -> Result<Option<Outcome>, Admission> {
        let Some(c) = self.clients.get(&key.client) else {
            return Ok(None);
        };
        if let Some(r) = c.records.iter().find(|r| r.seq == key.seq) {
            return Ok(Some(r.outcome.clone()));
        }
        if key.seq <= c.watermark {
            return Err(Admission::Expired);
        }
        Ok(None)
    }

    /// One above the highest seq remembered for `client` (record or watermark): new keys of
    /// that client must not go below it.
    pub fn next_seq(&self, client: &str) -> u64 {
        self.clients.get(client).map_or(1, |c| {
            let top = c.records.iter().map(|r| r.seq).max().unwrap_or(0);
            top.max(c.watermark).saturating_add(1)
        })
    }

    /// Accepted records of this instance that are still running.
    pub fn running(&self) -> usize {
        self.clients
            .values()
            .flat_map(|c| &c.records)
            .filter(|r| matches!(r.outcome, Outcome::Running))
            .count()
    }

    fn append(&mut self, line: &Line) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(line).map_err(io::Error::other)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.file.sync_data()?;
        self.lines += 1;
        Ok(())
    }

    fn evict_clients(&mut self) {
        while self.clients.len() > MAX_CLIENTS {
            let Some(oldest) = self
                .clients
                .iter()
                .min_by_key(|(_, c)| c.touched)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            self.clients.remove(&oldest);
        }
    }

    /// Rewrites the file with the retained records and watermarks only.
    fn compact(&mut self) -> io::Result<()> {
        let mut out = Vec::new();
        let sorted: BTreeMap<&String, &Client> = self.clients.iter().collect();
        for (client, c) in sorted {
            let mut push = |line: Line| -> io::Result<()> {
                out.extend(serde_json::to_vec(&line).map_err(io::Error::other)?);
                out.push(b'\n');
                Ok(())
            };
            if c.watermark > 0 {
                push(Line::Watermark {
                    client: client.clone(),
                    seq: c.watermark,
                })?;
            }
            for r in &c.records {
                push(Line::Accept {
                    client: client.clone(),
                    seq: r.seq,
                    kind: r.kind.clone(),
                    target: r.target.clone(),
                    hash: r.hash.clone(),
                    epoch: r.epoch,
                })?;
                if let Some(class) = Class::of(&r.outcome).or(match r.outcome {
                    Outcome::Recorded { succeeded } => Some(if succeeded {
                        Class::Done
                    } else {
                        Class::Failed
                    }),
                    _ => None,
                }) {
                    push(Line::End {
                        client: client.clone(),
                        seq: r.seq,
                        class,
                    })?;
                }
            }
        }
        super::write_private(&self.path, &out)?;
        self.file = open_append(&self.path)?;
        self.lines = out.iter().filter(|&&b| b == b'\n').count();
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    let mut o = OpenOptions::new();
    o.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(path)
}

fn apply(clients: &mut HashMap<String, Client>, line: Line) {
    match line {
        Line::Accept {
            client,
            seq,
            kind,
            target,
            hash,
            epoch,
        } => {
            let c = clients.entry(client).or_default();
            if seq <= c.watermark || c.records.iter().any(|r| r.seq == seq) {
                return;
            }
            c.records.push_back(Record {
                seq,
                kind,
                target,
                hash,
                epoch,
                outcome: Outcome::Running,
            });
            while c.records.len() > PER_CLIENT {
                if let Some(old) = c.records.pop_front() {
                    c.watermark = c.watermark.max(old.seq);
                }
            }
        }
        Line::End { client, seq, class } => {
            if let Some(r) = clients
                .get_mut(&client)
                .and_then(|c| c.records.iter_mut().find(|r| r.seq == seq))
            {
                r.outcome = class.recalled();
            }
        }
        Line::Watermark { client, seq } => {
            let c = clients.entry(client).or_default();
            c.watermark = c.watermark.max(seq);
            c.records.retain(|r| r.seq > seq);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::protocol::Done;

    fn key(client: &str, seq: u64) -> RequestKey {
        RequestKey {
            client: client.to_owned(),
            seq,
        }
    }

    fn op(n: &str) -> Mutation {
        Mutation::OmpReset { key: n.to_owned() }
    }

    #[test]
    fn duplicates_recover_reused_keys_refuse_and_nothing_is_replayed_after_a_crash() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("journal");
        let epoch = ServiceEpoch([1; 16]);
        {
            let mut j = Journal::open(&path).expect("open");
            let admit = |j: &mut Journal, seq, name| {
                j.admit(&key("a", seq), &op(name), None, epoch)
                    .expect("admit")
            };
            assert!(matches!(admit(&mut j, 1, "x"), Admission::Accepted));
            assert!(matches!(
                admit(&mut j, 1, "x"),
                Admission::Duplicate(Outcome::Running)
            ));
            assert!(matches!(admit(&mut j, 1, "y"), Admission::KeyReused));
            assert!(matches!(admit(&mut j, 2, "y"), Admission::Accepted));
            j.finish(&key("a", 2), Outcome::Done(Box::new(Done::Unit)));
            assert!(matches!(
                j.lookup(&key("a", 2)),
                Ok(Some(Outcome::Done(d))) if matches!(*d, Done::Unit)
            ));
            assert_eq!(j.running(), 1);
            // Dropped without finishing "a"/1: the owner crashed mid-way.
        }
        let mut j = Journal::open(&path).expect("reopen");
        assert!(matches!(
            j.lookup(&key("a", 1)),
            Ok(Some(Outcome::Uncertain))
        ));
        assert!(matches!(
            j.lookup(&key("a", 2)),
            Ok(Some(Outcome::Recorded { succeeded: true }))
        ));
        assert_eq!(j.running(), 0, "nothing is resumed");
        assert!(
            matches!(
                j.admit(&key("a", 1), &op("x"), None, epoch).expect("admit"),
                Admission::Duplicate(Outcome::Uncertain)
            ),
            "a retry after the crash recovers the uncertain record instead of running again"
        );
        assert!(matches!(j.lookup(&key("b", 1)), Ok(None)));
        // The uncertain verdict itself is durable.
        drop(j);
        let j = Journal::open(&path).expect("reopen");
        assert!(matches!(
            j.lookup(&key("a", 1)),
            Ok(Some(Outcome::Uncertain))
        ));
    }

    #[test]
    fn evicted_keys_stay_expired_across_restarts_and_no_payload_is_stored() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("journal");
        let epoch = ServiceEpoch([2; 16]);
        {
            let mut j = Journal::open(&path).expect("open");
            for seq in 1..=(PER_CLIENT as u64 + 3) {
                assert!(matches!(
                    j.admit(&key("c", seq), &op("secret-setting-name"), None, epoch)
                        .expect("admit"),
                    Admission::Accepted
                ));
                j.finish(&key("c", seq), Outcome::Failed("boom".into()));
            }
            assert!(matches!(j.lookup(&key("c", 1)), Err(Admission::Expired)));
            assert!(matches!(
                j.admit(&key("c", 2), &op("secret-setting-name"), None, epoch)
                    .expect("admit"),
                Admission::Expired
            ));
        }
        let j = Journal::open(&path).expect("reopen");
        assert!(matches!(j.lookup(&key("c", 3)), Err(Admission::Expired)));
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("secret-setting-name"));
        assert!(!text.contains("boom"));
    }
}
