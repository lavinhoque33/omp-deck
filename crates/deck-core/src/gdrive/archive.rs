//! The backup archive: what is synced, the `.tar.gz` format, and the restore.
//!
//! Layout: `manifest.json` (see [`Manifest`]) followed by `agent/<path>` entries, paths relative
//! to the omp agent directory. `history.db` is a consistent SQLite snapshot, never the raw
//! db/-wal/-shm files.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, DirBuilder, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::backup::{Backup, StepResult};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Error, KEEP_SAFETY_COPIES, Result, compact_stamp, io_err, rfc3339};

/// Top-level files omp reads from its agent directory (user-authored config).
pub const SYNCED_FILES: &[&str] = &[
    "config.yml",
    "config.yaml",
    "models.yml",
    "models.yaml",
    "keybindings.yml",
    "keybindings.yaml",
    "keybindings.json",
    "mcp.json",
    ".mcp.json",
    "lsp.json",
    "AGENTS.md",
    "SYSTEM.md",
    "SYSTEM_TEMPLATE.md",
    "APPEND_SYSTEM.md",
    "TITLE_SYSTEM.md",
    "PERSONALITY.md",
    "RULES.md",
];

/// Directories omp discovers user config in; a restore makes each one match the backup.
pub const SYNCED_DIRS: &[&str] = &[
    "agents",
    "commands",
    "extensions",
    "hooks",
    "instructions",
    "managed-skills",
    "prompts",
    "rules",
    "skills",
    "themes",
    "tools",
];

/// omp's prompt history (SQLite, WAL mode, kept open by running omp processes).
pub const HISTORY_DB: &str = "history.db";

/// Never synced inside the synced directories: installed dependencies, VCS data, and omp's
/// own copies of config files it could not parse.
fn skipped(name: &str) -> bool {
    matches!(name, "node_modules" | ".git") || name.contains(".broken-")
}

const FORMAT: u32 = 1;
const MANIFEST: &str = "manifest.json";
const PREFIX: &str = "agent/";
/// Synced content above this is refused (it is held in memory and uploaded in one request).
const MAX_TOTAL: u64 = 256 << 20;
/// Unpacked-size cap when reading an archive (a guard against decompression bombs).
const MAX_UNPACKED: u64 = 1 << 30;
/// How long SQLite copies wait for other connections' locks.
const BUSY: Duration = Duration::from_secs(15);
const MAX_DEPTH: usize = 16;

/// `manifest.json`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: u32,
    /// RFC 3339 UTC.
    pub created: String,
    pub host: String,
    #[serde(default)]
    pub app_version: String,
    /// Content fingerprint of the synced set (what auto backup compares).
    pub fingerprint: String,
    /// Synced directories present at backup time (possibly empty).
    pub dirs: Vec<String>,
    pub files: Vec<ManifestFile>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestFile {
    /// Relative to the agent directory, `/`-separated.
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub mode: u32,
}

struct SnapFile {
    rel: String,
    data: Vec<u8>,
    mode: u32,
}

/// The synced set read from the agent directory, with its content fingerprint.
pub(super) struct Snapshot {
    files: Vec<SnapFile>,
    dirs: Vec<String>,
    fingerprint: String,
}

impl Snapshot {
    pub(super) fn take(agent_dir: &Path) -> Result<Snapshot> {
        let mut files = Vec::new();
        let mut content_hashes = Vec::new();
        let mut total = 0u64;
        let mut add = |rel: String, data: Vec<u8>, mode: u32, hash: String| -> Result<()> {
            total += data.len() as u64;
            if total > MAX_TOTAL {
                return Err(Error::Io(format!(
                    "the synced omp config is larger than {} MiB; not backing it up",
                    MAX_TOTAL >> 20
                )));
            }
            content_hashes.push((rel.clone(), mode, hash));
            files.push(SnapFile { rel, data, mode });
            Ok(())
        };
        let mut read_file = |rel: String| -> Result<()> {
            let path = agent_dir.join(&rel);
            let data = fs::read(&path).map_err(io_err("cannot read", &path))?;
            let mode = fs::metadata(&path).map_or(0o644, |m| m.permissions().mode() & 0o777);
            let hash = hex(&Sha256::digest(&data));
            add(rel, data, mode, hash)
        };
        for name in SYNCED_FILES {
            if fs::metadata(agent_dir.join(name)).is_ok_and(|m| m.is_file()) {
                read_file((*name).to_owned())?;
            }
        }
        let mut dirs = Vec::new();
        for dir in SYNCED_DIRS {
            if fs::metadata(agent_dir.join(dir)).is_ok_and(|m| m.is_dir()) {
                dirs.push((*dir).to_owned());
                for rel in walk(agent_dir, dir)? {
                    read_file(rel)?;
                }
            }
        }
        let history = agent_dir.join(HISTORY_DB);
        if fs::metadata(&history).is_ok_and(|m| m.is_file()) {
            let (data, logical) = snapshot_history(&history)?;
            add(HISTORY_DB.to_owned(), data, 0o644, logical)?;
        }
        // Content only: the exec bit counts, timestamps and other mode bits do not. The
        // history db counts by its rows (its page bytes vary between snapshots).
        content_hashes.sort();
        let mut h = Sha256::new();
        for d in &dirs {
            h.update(format!("dir\0{d}\0"));
        }
        for (rel, mode, hash) in &content_hashes {
            h.update(format!("file\0{rel}\0{}\0{hash}\0", mode & 0o111 != 0));
        }
        files.sort_by(|a, b| a.rel.cmp(&b.rel));
        Ok(Snapshot {
            files,
            dirs,
            fingerprint: hex(&h.finalize()),
        })
    }

    pub(super) fn is_empty(&self) -> bool {
        self.files.is_empty() && self.dirs.is_empty()
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The `.tar.gz` bytes.
    pub(super) fn archive(&self, created_unix: i64, host: &str) -> Result<Vec<u8>> {
        let manifest = Manifest {
            format: FORMAT,
            created: rfc3339(created_unix),
            host: host.to_owned(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            fingerprint: self.fingerprint.clone(),
            dirs: self.dirs.clone(),
            files: self
                .files
                .iter()
                .map(|f| ManifestFile {
                    path: f.rel.clone(),
                    size: f.data.len() as u64,
                    sha256: hex(&Sha256::digest(&f.data)),
                    mode: f.mode,
                })
                .collect(),
        };
        let json = serde_json::to_vec_pretty(&manifest).map_err(|e| Error::Io(e.to_string()))?;
        let mtime = u64::try_from(created_unix).unwrap_or(0);
        let build = || -> io::Result<Vec<u8>> {
            let mut tar = tar::Builder::new(GzEncoder::new(Vec::new(), Compression::default()));
            append(&mut tar, MANIFEST, &json, 0o644, mtime)?;
            for f in &self.files {
                append(
                    &mut tar,
                    &format!("{PREFIX}{}", f.rel),
                    &f.data,
                    f.mode,
                    mtime,
                )?;
            }
            tar.into_inner()?.finish()
        };
        build().map_err(|e| Error::Io(format!("cannot build the backup archive: {e}")))
    }
}

fn append(
    tar: &mut tar::Builder<GzEncoder<Vec<u8>>>,
    path: &str,
    data: &[u8],
    mode: u32,
    mtime: u64,
) -> io::Result<()> {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_size(data.len() as u64);
    header.set_mode(mode);
    header.set_mtime(mtime);
    tar.append_data(&mut header, path, data)
}

/// Regular files under `agent_dir/dir` as agent-relative `/` paths, sorted. Symlinks are
/// followed (dotfile setups link config in); [`skipped`] names and non-UTF-8 names are left out.
fn walk(agent_dir: &Path, dir: &str) -> Result<Vec<String>> {
    fn visit(agent_dir: &Path, rel: &str, depth: usize, out: &mut Vec<String>) -> Result<()> {
        let path = agent_dir.join(rel);
        if depth > MAX_DEPTH {
            log::warn!("not descending into {}: nested too deep", path.display());
            return Ok(());
        }
        for entry in fs::read_dir(&path).map_err(io_err("cannot list", &path))? {
            let entry = entry.map_err(io_err("cannot list", &path))?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                log::warn!("skipping non-UTF-8 name in {}", path.display());
                continue;
            };
            if skipped(&name) {
                continue;
            }
            let child = format!("{rel}/{name}");
            // Follows symlinks; a dangling link is skipped.
            match fs::metadata(entry.path()) {
                Ok(m) if m.is_dir() => visit(agent_dir, &child, depth + 1, out)?,
                Ok(m) if m.is_file() => out.push(child),
                _ => {}
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(agent_dir, dir, 0, &mut out)?;
    out.sort();
    Ok(out)
}

fn history_err(e: rusqlite::Error) -> Error {
    Error::History(e.to_string())
}

/// Copies `src` into `dst` with SQLite's online backup API in one step, retrying while other
/// connections hold locks. Consistent even while omp writes (WAL or not).
fn copy_db(src: &Connection, dst: &mut Connection) -> Result<()> {
    let deadline = Instant::now() + BUSY;
    let backup = Backup::new(src, dst).map_err(history_err)?;
    loop {
        match backup.step(-1).map_err(history_err)? {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            StepResult::Busy | StepResult::Locked if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => return Err(Error::History("the database stayed locked".to_owned())),
        }
    }
}

/// A consistent copy of the live history db (as one self-contained file) and a hash of its
/// rows.
fn snapshot_history(live: &Path) -> Result<(Vec<u8>, String)> {
    let tmp = tempfile::tempdir().map_err(|e| Error::Io(e.to_string()))?;
    let copy = tmp.path().join(HISTORY_DB);
    {
        let src = Connection::open_with_flags(live, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(history_err)?;
        src.busy_timeout(BUSY).map_err(history_err)?;
        let mut dst = Connection::open(&copy).map_err(history_err)?;
        copy_db(&src, &mut dst)?;
        dst.query_row("PRAGMA journal_mode=DELETE", [], |_| Ok(()))
            .map_err(history_err)?;
    }
    let conn = Connection::open_with_flags(&copy, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(history_err)?;
    let logical = logical_hash(&conn).map_err(history_err)?;
    drop(conn);
    let data = fs::read(&copy).map_err(io_err("cannot read", &copy))?;
    Ok((data, logical))
}

/// Hash of every ordinary table's schema and rows (virtual tables are covered by their
/// shadow tables).
fn logical_hash(conn: &Connection) -> rusqlite::Result<String> {
    let mut h = Sha256::new();
    let tables: Vec<(String, String)> = conn
        .prepare(
            "SELECT name, sql FROM sqlite_master WHERE type = 'table' \
             AND sql NOT LIKE 'CREATE VIRTUAL TABLE%' ORDER BY name",
        )?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (name, sql) in tables {
        h.update(format!("{name}\0{sql}\0"));
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{}\"", name.replace('"', "\"\"")))?;
        let n = stmt.column_count();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            for i in 0..n {
                match row.get_ref(i)? {
                    ValueRef::Null => h.update([0]),
                    ValueRef::Integer(v) => {
                        h.update([1]);
                        h.update(v.to_le_bytes());
                    }
                    ValueRef::Real(v) => {
                        h.update([2]);
                        h.update(v.to_bits().to_le_bytes());
                    }
                    ValueRef::Text(t) | ValueRef::Blob(t) => {
                        h.update([3]);
                        h.update((t.len() as u64).to_le_bytes());
                        h.update(t);
                    }
                }
            }
        }
    }
    Ok(hex(&h.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Restored {
    data: Vec<u8>,
    mode: u32,
}

/// What a restore would change, relative to the agent directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RestorePlan {
    /// Top-level files that exist locally and are overwritten (may include `history.db`).
    pub replaced: Vec<String>,
    /// Top-level files the backup adds.
    pub added: Vec<String>,
    /// Synced directories made to match the backup exactly.
    pub dirs: Vec<String>,
    /// Local files inside `dirs` the backup lacks: they are removed.
    pub removed: Vec<String>,
}

/// A downloaded, validated backup and what restoring it would change.
pub struct PreparedRestore {
    /// Drive file name.
    pub name: String,
    pub manifest: Manifest,
    pub plan: RestorePlan,
    /// Where the current versions are copied before anything is replaced.
    pub safety_dir: PathBuf,
    safety_root: PathBuf,
    files: BTreeMap<String, Restored>,
}

#[derive(Clone, Debug)]
pub struct RestoreReport {
    /// Where the previous versions were copied.
    pub safety_dir: PathBuf,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidBackup(msg.into())
}

impl PreparedRestore {
    pub(super) fn new(
        bytes: &[u8],
        agent_dir: &Path,
        safety_root: &Path,
        name: String,
        now: i64,
    ) -> Result<PreparedRestore> {
        let (manifest, files) = read_archive(bytes)?;
        let mut plan = RestorePlan {
            dirs: manifest.dirs.clone(),
            ..RestorePlan::default()
        };
        for rel in files.keys().filter(|r| !r.contains('/')) {
            if fs::symlink_metadata(agent_dir.join(rel)).is_ok() {
                plan.replaced.push(rel.clone());
            } else {
                plan.added.push(rel.clone());
            }
        }
        for dir in &plan.dirs {
            if fs::metadata(agent_dir.join(dir)).is_ok_and(|m| m.is_dir()) {
                plan.removed.extend(
                    walk(agent_dir, dir)?
                        .into_iter()
                        .filter(|rel| !files.contains_key(rel)),
                );
            }
        }
        let stamp = compact_stamp(now);
        let mut safety_dir = safety_root.join(&stamp);
        let mut n = 2;
        while safety_dir.exists() {
            safety_dir = safety_root.join(format!("{stamp}-{n}"));
            n += 1;
        }
        Ok(PreparedRestore {
            name,
            manifest,
            plan,
            safety_dir,
            safety_root: safety_root.to_owned(),
            files,
        })
    }

    /// Copies everything that will be replaced or removed into the safety directory, then
    /// writes the backup's files (atomically), removes the files the synced directories should
    /// no longer have, and restores `history.db` into the live database.
    pub(super) fn apply(&self, agent_dir: &Path) -> Result<RestoreReport> {
        self.save_safety_copy(agent_dir)?;
        prune_safety(&self.safety_root, KEEP_SAFETY_COPIES);
        let keep_note = |e: Error| {
            Error::Io(format!(
                "{e} (restore incomplete; the previous files are in {})",
                self.safety_dir.display()
            ))
        };
        for (rel, f) in self.files.iter().filter(|(rel, _)| *rel != HISTORY_DB) {
            write_atomic(&agent_dir.join(rel), &f.data, f.mode).map_err(keep_note)?;
        }
        for rel in &self.plan.removed {
            let path = agent_dir.join(rel);
            match fs::remove_file(&path) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    return Err(keep_note(io_err("cannot remove", &path)(e)));
                }
                _ => remove_empty_parents(agent_dir, rel),
            }
        }
        if let Some(db) = self.files.get(HISTORY_DB) {
            restore_history(&agent_dir.join(HISTORY_DB), &db.data).map_err(keep_note)?;
        }
        Ok(RestoreReport {
            safety_dir: self.safety_dir.clone(),
        })
    }

    fn save_safety_copy(&self, agent_dir: &Path) -> Result<()> {
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.safety_dir)
            .map_err(io_err("cannot create", &self.safety_dir))?;
        let copy = |rel: &str| -> Result<()> {
            let (from, to) = (agent_dir.join(rel), self.safety_dir.join(rel));
            if let Some(dir) = to.parent() {
                fs::create_dir_all(dir).map_err(io_err("cannot create", dir))?;
            }
            if rel == HISTORY_DB {
                let (data, _) = snapshot_history(&from)?;
                fs::write(&to, data).map_err(io_err("cannot write", &to))
            } else {
                fs::copy(&from, &to)
                    .map(drop)
                    .map_err(io_err("cannot copy", &from))
            }
        };
        for rel in &self.plan.replaced {
            copy(rel)?;
        }
        for dir in &self.plan.dirs {
            if fs::metadata(agent_dir.join(dir)).is_ok_and(|m| m.is_dir()) {
                for rel in walk(agent_dir, dir)? {
                    copy(&rel)?;
                }
            }
        }
        Ok(())
    }
}

/// Parses and validates an archive: only regular files and directories at safe relative paths
/// inside the synced set, matching a supported manifest.
fn read_archive(bytes: &[u8]) -> Result<(Manifest, BTreeMap<String, Restored>)> {
    let bad = |e: io::Error| invalid(format!("cannot unpack: {e}"));
    let mut archive = tar::Archive::new(GzDecoder::new(bytes).take(MAX_UNPACKED));
    let mut manifest: Option<Manifest> = None;
    let mut files = BTreeMap::new();
    for entry in archive.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let rel = safe_path(&entry.path().map_err(bad)?)?;
        let kind = entry.header().entry_type();
        if kind.is_dir() || kind.is_pax_global_extensions() || kind.is_pax_local_extensions() {
            continue;
        }
        if !kind.is_file() {
            return Err(invalid(format!(
                "{rel}: only regular files are allowed (found {kind:?})"
            )));
        }
        let mode = entry.header().mode().unwrap_or(0o644) & 0o777;
        let mut data = Vec::new();
        entry.read_to_end(&mut data).map_err(bad)?;
        if rel == MANIFEST {
            if manifest.is_some() {
                return Err(invalid("two manifests"));
            }
            manifest = Some(
                serde_json::from_slice(&data)
                    .map_err(|e| invalid(format!("unreadable manifest: {e}")))?,
            );
            continue;
        }
        let Some(inner) = rel.strip_prefix(PREFIX) else {
            return Err(invalid(format!("unexpected entry {rel}")));
        };
        check_synced(inner)?;
        if files
            .insert(inner.to_owned(), Restored { data, mode })
            .is_some()
        {
            return Err(invalid(format!("{inner} appears twice")));
        }
    }
    let manifest = manifest.ok_or_else(|| invalid("no manifest"))?;
    if manifest.format != FORMAT {
        return Err(invalid(format!(
            "format {} is not supported (made by a newer OmpDeck?)",
            manifest.format
        )));
    }
    if let Some(d) = manifest
        .dirs
        .iter()
        .find(|d| !SYNCED_DIRS.contains(&d.as_str()))
    {
        return Err(invalid(format!("unexpected directory {d}")));
    }
    let listed: BTreeSet<&str> = manifest.files.iter().map(|f| f.path.as_str()).collect();
    if listed.len() != manifest.files.len()
        || !listed.iter().copied().eq(files.keys().map(String::as_str))
    {
        return Err(invalid("the files do not match the manifest"));
    }
    for f in &manifest.files {
        let data = &files[&f.path].data;
        if data.len() as u64 != f.size || hex(&Sha256::digest(data)) != f.sha256 {
            return Err(invalid(format!(
                "{} is corrupt (checksum mismatch)",
                f.path
            )));
        }
        if let Some((dir, _)) = f.path.split_once('/')
            && !manifest.dirs.iter().any(|d| d == dir)
        {
            return Err(invalid(format!(
                "{} lies outside the listed directories",
                f.path
            )));
        }
    }
    Ok((manifest, files))
}

/// A relative, `..`-free, UTF-8 path as a `/`-joined string.
fn safe_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => parts.push(
                s.to_str()
                    .ok_or_else(|| invalid(format!("non-UTF-8 path {}", path.display())))?,
            ),
            _ => return Err(invalid(format!("unsafe path {}", path.display()))),
        }
    }
    if parts.is_empty() {
        return Err(invalid("empty path"));
    }
    Ok(parts.join("/"))
}

/// `rel` (agent-relative) must be a synced top-level file or lie inside a synced directory.
fn check_synced(rel: &str) -> Result<()> {
    let parts: Vec<&str> = rel.split('/').collect();
    let ok = match parts.as_slice() {
        [name] => SYNCED_FILES.contains(name) || *name == HISTORY_DB,
        [dir, rest @ ..] => SYNCED_DIRS.contains(dir) && !rest.iter().any(|p| skipped(p)),
        [] => false,
    };
    if ok {
        Ok(())
    } else {
        Err(invalid(format!(
            "{rel} is not part of the synced omp config"
        )))
    }
}

/// Temp file in the target's directory + rename. A symlinked target is written through (the
/// link stays).
fn write_atomic(dest: &Path, data: &[u8], mode: u32) -> Result<()> {
    let dest = match fs::symlink_metadata(dest) {
        Ok(m) if m.file_type().is_symlink() => {
            fs::canonicalize(dest).map_err(io_err("cannot resolve", dest))?
        }
        _ => dest.to_owned(),
    };
    let dir = dest.parent().unwrap_or(Path::new("."));
    let write = || -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".omp-deck-restore")
            .tempfile_in(dir)?;
        tmp.write_all(data)?;
        tmp.as_file()
            .set_permissions(Permissions::from_mode(mode | 0o600))?;
        tmp.as_file().sync_all()?;
        tmp.persist(&dest).map(drop).map_err(|e| e.error)
    };
    write().map_err(io_err("cannot write", &dest))
}

/// After removing `rel`, removes the directories it leaves empty (below its synced directory).
fn remove_empty_parents(agent_dir: &Path, rel: &str) {
    let mut cur = Path::new(rel).parent();
    while let Some(p) = cur.filter(|p| p.components().count() > 1) {
        if fs::remove_dir(agent_dir.join(p)).is_err() {
            break;
        }
        cur = p.parent();
    }
}

/// Writes `data` into the live history db through the backup API: other connections (omp in
/// WAL mode) keep working and see the restored rows; the file itself is never replaced.
fn restore_history(live: &Path, data: &[u8]) -> Result<()> {
    let tmp = tempfile::tempdir().map_err(|e| Error::Io(e.to_string()))?;
    let src_path = tmp.path().join(HISTORY_DB);
    fs::write(&src_path, data).map_err(io_err("cannot write", &src_path))?;
    let src = Connection::open_with_flags(&src_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(history_err)?;
    let mut dst = Connection::open(live).map_err(history_err)?;
    dst.busy_timeout(BUSY).map_err(history_err)?;
    copy_db(&src, &mut dst)
}

/// Keeps the newest `keep` safety copies (directory names are UTC timestamps).
fn prune_safety(root: &Path, keep: usize) {
    let Ok(rd) = fs::read_dir(root) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    let excess = dirs.len().saturating_sub(keep);
    for d in &dirs[..excess] {
        if let Err(e) = fs::remove_dir_all(d) {
            log::warn!("cannot remove old safety copy {}: {e}", d.display());
        }
    }
}
