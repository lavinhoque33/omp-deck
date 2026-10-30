//! The paired devices (`devices.json`, owner-only). Every change is written before it is applied
//! in memory, so the registry the engine decides with never runs ahead of the file: a device's
//! `last_ts` in particular is durable before its reconnect is accepted (replay protection survives
//! a restart). Only the SHA-256 of each device's relay token is stored; the token itself left the
//! gateway once, inside the encrypted pairing reply.

use std::io;
use std::path::{Path, PathBuf};

use deck_remote::noise::{PublicKey, fingerprint};
use deck_remote::relay::Hash;
use serde::{Deserialize, Serialize};

use super::RemoteError;
use crate::gateway::protocol::{DeviceView, Permissions};

pub const FILE: &str = "devices.json";
const VERSION: u32 = 1;

/// One paired device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub platform: String,
    #[serde(with = "b64")]
    pub public: PublicKey,
    /// SHA-256 of its relay admission token.
    #[serde(with = "b64")]
    pub token_hash: Hash,
    pub permissions: Permissions,
    /// The newest accepted handshake timestamp (a later one must be greater).
    pub last_ts: u64,
    pub created_unix_ms: u64,
    pub last_seen_unix_ms: u64,
}

impl Device {
    pub fn view(&self, online: bool) -> DeviceView {
        DeviceView {
            id: self.id.clone(),
            name: self.name.clone(),
            platform: self.platform.clone(),
            fingerprint: fingerprint(&self.public),
            permissions: self.permissions,
            created_unix_ms: self.created_unix_ms,
            last_seen_unix_ms: self.last_seen_unix_ms,
            online,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    devices: Vec<Device>,
}

/// The registry and its file.
#[derive(Debug)]
pub struct Registry {
    path: PathBuf,
    devices: Vec<Device>,
}

impl Registry {
    /// Reads `path` (no file = no devices).
    pub fn load(path: &Path) -> Result<Registry, RemoteError> {
        let devices = match std::fs::read(path) {
            Ok(bytes) => {
                let stored: Stored = serde_json::from_slice(&bytes)
                    .map_err(|e| RemoteError::corrupt(path, format!("not a device list: {e}")))?;
                if stored.version != VERSION {
                    return Err(RemoteError::corrupt(
                        path,
                        format!("unsupported version {}", stored.version),
                    ));
                }
                stored.devices
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(RemoteError::io(path, e)),
        };
        Ok(Registry {
            path: path.to_path_buf(),
            devices,
        })
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn get(&self, id: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.id == id)
    }

    pub fn by_public(&self, public: &PublicKey) -> Option<&Device> {
        self.devices.iter().find(|d| d.public == *public)
    }

    /// The admission token hashes of every device.
    pub fn token_hashes(&self) -> impl Iterator<Item = Hash> + '_ {
        self.devices.iter().map(|d| d.token_hash)
    }

    /// Writes `devices`, then makes them current.
    fn commit(&mut self, devices: Vec<Device>) -> Result<(), RemoteError> {
        let stored = Stored {
            version: VERSION,
            devices,
        };
        let bytes = serde_json::to_vec_pretty(&stored)
            .map_err(|e| RemoteError::Invalid(format!("device list: {e}")))?;
        crate::gateway::write_private(&self.path, &bytes)
            .map_err(|e| RemoteError::io(&self.path, e))?;
        self.devices = stored.devices;
        Ok(())
    }

    /// Adds a device; an earlier record of the same key is replaced (re-pairing a phone).
    /// Returns the ids of the records it replaced (other than `device.id` itself).
    pub fn add(&mut self, device: Device) -> Result<Vec<String>, RemoteError> {
        let (replaced, mut next): (Vec<Device>, Vec<Device>) = self
            .devices
            .iter()
            .cloned()
            .partition(|d| d.public == device.public || d.id == device.id);
        let replaced = replaced
            .into_iter()
            .map(|d| d.id)
            .filter(|id| *id != device.id)
            .collect();
        next.push(device);
        self.commit(next)?;
        Ok(replaced)
    }

    /// Deletes a device; `Ok(None)` when there is none with `id`.
    pub fn remove(&mut self, id: &str) -> Result<Option<Device>, RemoteError> {
        let Some(gone) = self.get(id).cloned() else {
            return Ok(None);
        };
        let next = self
            .devices
            .iter()
            .filter(|d| d.id != id)
            .cloned()
            .collect();
        self.commit(next)?;
        Ok(Some(gone))
    }

    /// Applies `f` to the device `id`; `Ok(false)` when there is none.
    pub fn update(&mut self, id: &str, f: impl FnOnce(&mut Device)) -> Result<bool, RemoteError> {
        let mut next = self.devices.clone();
        let Some(d) = next.iter_mut().find(|d| d.id == id) else {
            return Ok(false);
        };
        f(d);
        self.commit(next)?;
        Ok(true)
    }
}

/// Fixed-size byte arrays as base64url (no padding).
mod b64 {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&URL_SAFE_NO_PAD.encode(bytes))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        let raw = URL_SAFE_NO_PAD
            .decode(text.as_bytes())
            .map_err(serde::de::Error::custom)?;
        raw.try_into()
            .map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, key: u8) -> Device {
        Device {
            id: id.to_owned(),
            name: format!("phone {id}"),
            platform: "test".to_owned(),
            public: [key; 32],
            token_hash: [key ^ 0xff; 32],
            permissions: Permissions::default(),
            last_ts: 10,
            created_unix_ms: 1,
            last_seen_unix_ms: 1,
        }
    }

    #[test]
    fn changes_persist_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut reg = Registry::load(&path).unwrap();
        assert!(reg.devices().is_empty());
        reg.add(device("a", 1)).unwrap();
        reg.add(device("b", 2)).unwrap();
        assert!(
            reg.update("a", |d| {
                d.last_ts = 99;
                d.name = "renamed".into();
            })
            .unwrap()
        );
        assert!(!reg.update("zz", |_| {}).unwrap());
        let again = Registry::load(&path).unwrap();
        assert_eq!(again.devices(), reg.devices());
        assert_eq!(again.get("a").unwrap().last_ts, 99);
        assert_eq!(again.by_public(&[2; 32]).unwrap().id, "b");
        assert_eq!(
            again.token_hashes().collect::<Vec<_>>(),
            vec![[0xfe; 32], [0xfd; 32]]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn re_pairing_a_key_replaces_its_record_and_remove_reports_absence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        let mut reg = Registry::load(&path).unwrap();
        assert!(reg.add(device("a", 1)).unwrap().is_empty());
        assert!(reg.add(device("b", 2)).unwrap().is_empty());
        // Re-paired: the old record's id is reported so its work can be cancelled.
        assert_eq!(reg.add(device("c", 1)).unwrap(), ["a"]);
        assert!(reg.remove("b").unwrap().is_some());
        assert_eq!(reg.devices().len(), 1);
        assert_eq!(reg.devices()[0].id, "c");
        assert_eq!(reg.remove("a").unwrap(), None);
        assert_eq!(reg.remove("c").unwrap().map(|d| d.id), Some("c".into()));
        assert!(Registry::load(&path).unwrap().devices().is_empty());
    }

    #[test]
    fn a_failed_write_leaves_memory_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        // The registry path is a directory: every write fails.
        let path = dir.path().join("blocked");
        std::fs::create_dir(&path).unwrap();
        let mut reg = Registry {
            path,
            devices: vec![device("a", 1)],
        };
        assert!(reg.update("a", |d| d.last_ts = 50).is_err());
        assert_eq!(reg.get("a").unwrap().last_ts, 10);
        assert!(reg.remove("a").is_err());
        assert!(reg.get("a").is_some());
    }

    #[test]
    fn corrupt_files_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE);
        std::fs::write(&path, b"[]").unwrap();
        assert!(matches!(
            Registry::load(&path),
            Err(RemoteError::Corrupt { .. })
        ));
        std::fs::write(&path, br#"{"version":2,"devices":[]}"#).unwrap();
        assert!(
            Registry::load(&path)
                .unwrap_err()
                .to_string()
                .contains("version 2")
        );
    }
}
