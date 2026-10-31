//! `<state-dir>/rooms.json`: the enrolled rooms (room id, `sha256(secret)`, label, creation time).
//!
//! Written atomically (temp file + rename) and owner-only; the server only reads it. `enroll` and
//! `remove` hold an exclusive lock on `<state-dir>/rooms.lock` across their read-modify-write, so
//! concurrent commands (other processes included) never lose each other's changes.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use deck_remote::relay::{self, HASH_LEN, Hash, ROOM_LEN, Room, RoomKey, SECRET_LEN};
use serde::{Deserialize, Serialize};

use crate::RelayError;

pub const ROOMS_FILE: &str = "rooms.json";
/// Never removed or replaced: every writer must lock the same file.
const LOCK_FILE: &str = "rooms.lock";

/// One entry of `rooms.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomRecord {
    /// [`relay::room_id`] (base64url, no padding).
    pub room: String,
    /// Lowercase hex SHA-256 of the room secret.
    pub secret_sha256: String,
    #[serde(default)]
    pub label: String,
    /// Unix seconds.
    pub created: u64,
}

pub fn rooms_path(state_dir: &Path) -> PathBuf {
    state_dir.join(ROOMS_FILE)
}

/// Parses a printed room id.
pub fn parse_room_id(id: &str) -> Option<Room> {
    URL_SAFE_NO_PAD.decode(id.trim()).ok()?.try_into().ok()
}

fn parse_hex_hash(text: &str) -> Option<Hash> {
    if text.len() != HASH_LEN * 2 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; HASH_LEN];
    for (byte, pair) in out.iter_mut().zip(text.as_bytes().as_chunks::<2>().0) {
        let pair = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Reads `rooms.json`; a missing file is an empty list.
pub fn read_records(state_dir: &Path) -> Result<Vec<RoomRecord>, RelayError> {
    let path = rooms_path(state_dir);
    let text = match std::fs::read(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(RelayError::Io(path, e)),
    };
    serde_json::from_slice(&text).map_err(|e| RelayError::Rooms(path, e.to_string()))
}

/// The server's view: room → stored secret hash. Any malformed entry rejects the whole file, so a
/// half-edited file never silently drops rooms.
pub fn read_table(state_dir: &Path) -> Result<HashMap<Room, Hash>, RelayError> {
    let records = read_records(state_dir)?;
    let mut table = HashMap::with_capacity(records.len());
    for r in &records {
        let bad = |what: &str| {
            RelayError::Rooms(
                rooms_path(state_dir),
                format!("entry {:?}: invalid {what}", r.room),
            )
        };
        let room = parse_room_id(&r.room).ok_or_else(|| bad("room"))?;
        let hash = parse_hex_hash(&r.secret_sha256).ok_or_else(|| bad("secret_sha256"))?;
        table.insert(room, hash);
    }
    Ok(table)
}

/// Replaces `rooms.json` atomically with an owner-only file.
pub fn write_records(state_dir: &Path, records: &[RoomRecord]) -> Result<(), RelayError> {
    create_state_dir(state_dir)?;
    let path = rooms_path(state_dir);
    let io = |e| RelayError::Io(path.clone(), e);
    let mut json = serde_json::to_vec_pretty(records).expect("room records serialize");
    json.push(b'\n');
    // `tempfile` creates the file 0600 on Unix; on Windows the state dir's ACL (the user
    // profile) applies.
    let mut tmp = tempfile::Builder::new()
        .prefix(".rooms-")
        .suffix(".tmp")
        .tempfile_in(state_dir)
        .map_err(io)?;
    tmp.write_all(&json).map_err(io)?;
    tmp.as_file().sync_all().map_err(io)?;
    tmp.persist(&path).map_err(|e| io(e.error))?;
    Ok(())
}

fn create_state_dir(dir: &Path) -> Result<(), RelayError> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(dir)
        .map_err(|e| RelayError::Io(dir.to_path_buf(), e))
}

/// Takes the exclusive `rooms.lock` lock (waiting for other writers); released on drop.
fn lock_rooms(state_dir: &Path) -> Result<std::fs::File, RelayError> {
    create_state_dir(state_dir)?;
    let path = state_dir.join(LOCK_FILE);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options
        .open(&path)
        .map_err(|e| RelayError::Io(path.clone(), e))?;
    file.lock().map_err(|e| RelayError::Io(path, e))?;
    Ok(file)
}

fn random<const N: usize>() -> Result<[u8; N], RelayError> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(|e| RelayError::Random(e.to_string()))?;
    Ok(out)
}

/// Creates a room: stores `sha256(secret)` and returns the key to hand to the gateway (shown once,
/// never stored).
pub fn enroll(state_dir: &Path, label: &str) -> Result<(RoomKey, RoomRecord), RelayError> {
    let _lock = lock_rooms(state_dir)?;
    let mut records = read_records(state_dir)?;
    let room: [u8; ROOM_LEN] = random()?;
    let secret: [u8; SECRET_LEN] = random()?;
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let record = RoomRecord {
        room: relay::room_id(&room),
        secret_sha256: hex(&relay::hash(&secret)),
        label: label.to_owned(),
        created,
    };
    records.push(record.clone());
    write_records(state_dir, &records)?;
    Ok((RoomKey { room, secret }, record))
}

/// Removes a room by its printed id; `false` if it was not enrolled.
pub fn remove(state_dir: &Path, room_id: &str) -> Result<bool, RelayError> {
    if !state_dir.exists() {
        // Nothing enrolled; do not create the directory for a no-op.
        return Ok(false);
    }
    let _lock = lock_rooms(state_dir)?;
    let mut records = read_records(state_dir)?;
    let before = records.len();
    records.retain(|r| r.room != room_id.trim());
    if records.len() == before {
        return Ok(false);
    }
    write_records(state_dir, &records)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enroll_list_remove_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let (key, record) = enroll(&state, "home").unwrap();
        let table = read_table(&state).unwrap();
        assert_eq!(table.get(&key.room), Some(&relay::hash(&key.secret)));
        assert_eq!(read_records(&state).unwrap(), vec![record.clone()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(rooms_path(&state))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(!remove(&state, "nope").unwrap());
        assert!(remove(&state, &record.room).unwrap());
        assert!(read_table(&state).unwrap().is_empty());
    }

    #[test]
    fn malformed_entries_reject_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            rooms_path(dir.path()),
            r#"[{"room":"AAAA","secret_sha256":"00","label":"","created":0}]"#,
        )
        .unwrap();
        assert!(read_table(dir.path()).is_err());
        std::fs::write(rooms_path(dir.path()), "{").unwrap();
        assert!(read_table(dir.path()).is_err());
        assert_eq!(parse_hex_hash(&hex(&[0xab; 32])), Some([0xab; 32]));
    }

    #[test]
    fn concurrent_enrolls_and_removes_never_lose_an_update() {
        use std::collections::HashSet;
        use std::sync::{Arc, Barrier};

        let dir = tempfile::tempdir().unwrap();
        let state = Arc::new(dir.path().join("state"));
        let doomed: Vec<String> = (0..8)
            .map(|_| enroll(&state, "old").unwrap().1.room)
            .collect();
        let barrier = Arc::new(Barrier::new(16));
        let enrollers: Vec<_> = (0..8)
            .map(|_| {
                let (state, barrier) = (state.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    (0..4)
                        .map(|_| enroll(&state, "new").unwrap().1.room)
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let removers: Vec<_> = doomed
            .into_iter()
            .map(|room| {
                let (state, barrier) = (state.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    assert!(remove(&state, &room).unwrap());
                })
            })
            .collect();
        let mut expected = HashSet::new();
        for t in enrollers {
            expected.extend(t.join().unwrap());
        }
        for t in removers {
            t.join().unwrap();
        }
        let stored: HashSet<String> = read_records(&state)
            .unwrap()
            .into_iter()
            .map(|r| r.room)
            .collect();
        assert_eq!(stored, expected);
    }
}
