//! Restoring a project archive: unpack + validate into the staging directory, plan against the
//! target folder and the local sessions store, apply.
//!
//! Nothing here trusts the archive: member paths are checked against a fixed shape, every member
//! must be a regular file listed in the manifest with matching size and sha256, nothing may be
//! missing or extra. `.omp` files are only ever added or overwritten (after a safety copy);
//! sessions are written by `omp session unpack`, never by us, apart from the restored file's
//! mtime (set to its backed-up time). A replaced local session is saved by `omp session pack`
//! and removed by omp's delete extension, never by us.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use super::snapshot::valid_id;
use super::{
    ConflictChoice, FORMAT, ItemManifest, KIND, LocalCopy, MAX_MEMBER, Manifest, Meter,
    PreparedProjectRestore, Progress, ProjectRestorePlan, ProjectRestoreReport, Pump, SessionPlan,
    Stage, invalid, job_io,
};
use crate::gdrive::archive::{ManifestFile, PROJECT_SAFETY_PREFIX, hex, prune_safety};
use crate::gdrive::{Error, KEEP_SAFETY_COPIES, Result, compact_stamp, io_err, unix_now};
use crate::omp::{DeleteError, DeleteTarget, MoveKind, MoveOutcome, Omp};
use crate::platform;
use crate::sharpshooter::{self, MEMORY_FILES};

/// Unpacked-size cap while reading an archive (a guard against decompression bombs).
const MAX_UNPACKED: u64 = 64 << 30;
const MAX_MANIFEST: u64 = 64 << 20;
/// How far into a session file its header is looked for.
const HEADER_SEARCH: usize = 1 << 20;

/// Where an archive member belongs.
#[derive(Debug, PartialEq, Eq)]
enum Member {
    Manifest,
    Omp,
    Memory,
    Jsonl,
    Artifact,
    Blob,
}

/// Checks the shape of an archive member path (`/`-separated, already free of `.`/`..`).
fn parse_member(path: &str) -> Result<Member> {
    let parts: Vec<&str> = path.split('/').collect();
    let member = match parts.as_slice() {
        ["manifest.json"] => Member::Manifest,
        ["omp", _, ..] => Member::Omp,
        ["memory", name] if MEMORY_FILES.contains(name) => Member::Memory,
        ["sessions", id, "session.jsonl"] if valid_id(id) => Member::Jsonl,
        ["sessions", id, "artifacts", _, ..] if valid_id(id) => Member::Artifact,
        ["blobs", name] if is_sha256(name) => Member::Blob,
        _ => return Err(invalid(format!("unexpected entry {path}"))),
    };
    Ok(member)
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A relative, `..`-free, UTF-8 path as a `/`-joined string; no backslashes, no NUL.
fn safe_path(path: &Path) -> Result<String> {
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(s) => {
                let s = s
                    .to_str()
                    .ok_or_else(|| invalid(format!("non-UTF-8 path {}", path.display())))?;
                if s.contains(['\\', '\0']) {
                    return Err(invalid(format!("unsafe path {}", path.display())));
                }
                parts.push(s);
            }
            _ => return Err(invalid(format!("unsafe path {}", path.display()))),
        }
    }
    if parts.is_empty() {
        return Err(invalid("empty path"));
    }
    Ok(parts.join("/"))
}

/// Unpacks `archive` under `into`, validating every member, and returns the manifest.
pub(super) fn extract(
    archive: &Path,
    into: &Path,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<Manifest> {
    let file = File::open(archive).map_err(io_err("cannot open", archive))?;
    let total = file
        .metadata()
        .map_err(io_err("cannot read", archive))?
        .len();
    let mut pump = Pump::new(cancel, progress, Stage::Verifying, total);
    platform::create_private_dir_all(into).map_err(io_err("cannot create", into))?;
    let bad = |e: io::Error| match job_io(cancel, "cannot unpack")(e) {
        Error::Io(m) => invalid(m),
        other => other,
    };
    let counted = Meter::new(file, &mut pump, false);
    let gz = GzDecoder::new(BufReader::with_capacity(64 * 1024, counted)).take(MAX_UNPACKED);
    let mut tar = tar::Archive::new(gz);
    // archive path → (size, sha256)
    let mut seen: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut manifest: Option<Manifest> = None;
    let mut buf = vec![0u8; 64 * 1024];
    for entry in tar.entries().map_err(bad)? {
        let mut entry = entry.map_err(bad)?;
        let kind = entry.header().entry_type();
        if kind.is_pax_global_extensions() {
            continue;
        }
        let rel = safe_path(&entry.path().map_err(bad)?)?;
        if kind != tar::EntryType::Regular {
            return Err(invalid(format!(
                "{rel}: only regular files are allowed (found {kind:?})"
            )));
        }
        let member = parse_member(&rel)?;
        if member == Member::Manifest {
            if manifest.is_some() {
                return Err(invalid("two manifests"));
            }
            let mut data = Vec::new();
            (&mut entry)
                .take(MAX_MANIFEST + 1)
                .read_to_end(&mut data)
                .map_err(bad)?;
            if data.len() as u64 > MAX_MANIFEST {
                return Err(invalid("the manifest is too large"));
            }
            manifest = Some(
                serde_json::from_slice(&data)
                    .map_err(|e| invalid(format!("unreadable manifest: {e}")))?,
            );
            continue;
        }
        if seen.contains_key(&rel) {
            return Err(invalid(format!("{rel} appears twice")));
        }
        let dest = into.join(&rel);
        // A `.omp` file whose name Windows cannot store is verified but not staged: the restore
        // skips it (see `check_dest`) and never reads the staged copy.
        let staged = !(cfg!(windows) && member == Member::Omp && unstorable_on_windows(&rel));
        let mut out = if staged {
            if let Some(dir) = dest.parent() {
                platform::create_private_dir_all(dir).map_err(io_err("cannot create", dir))?;
            }
            Some(platform::open_private_file(&dest).map_err(io_err("cannot create", &dest))?)
        } else {
            None
        };
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        loop {
            let n = entry.read(&mut buf).map_err(bad)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as u64;
            if let Some(out) = &mut out {
                out.write_all(&buf[..n])
                    .map_err(io_err("cannot write", &dest))?;
            }
        }
        seen.insert(rel, (size, hex(&hasher.finalize())));
    }
    drop(tar);
    pump.finish();
    let manifest = manifest.ok_or_else(|| invalid("no manifest"))?;
    check_manifest(&manifest, &seen)?;
    check_headers(&manifest, into)?;
    Ok(manifest)
}

/// The manifest must describe exactly the unpacked members.
fn check_manifest(m: &Manifest, seen: &BTreeMap<String, (u64, String)>) -> Result<()> {
    if m.format != FORMAT {
        return Err(invalid(format!(
            "format {} is not supported (made by a newer OmpDeck?)",
            m.format
        )));
    }
    if m.kind != KIND {
        return Err(invalid(format!(
            "this is a {:?} backup, not a project backup",
            m.kind
        )));
    }
    let mut expected: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut list = |key: String, size: u64, sha: &str| -> Result<()> {
        parse_member(&key)?;
        if expected
            .insert(key.clone(), (size, sha.to_owned()))
            .is_some()
        {
            return Err(invalid(format!("{key} is listed twice")));
        }
        Ok(())
    };
    if let Some(omp) = &m.omp {
        for f in &omp.files {
            list(format!("omp/{}", f.path), f.size, &f.sha256)?;
        }
    }
    if let Some(memory) = &m.memory {
        for f in &memory.files {
            list(format!("memory/{}", f.path), f.size, &f.sha256)?;
        }
    }
    if let Some(sessions) = &m.sessions {
        let blob_names: BTreeSet<&str> = sessions.blobs.iter().map(|b| b.sha256.as_str()).collect();
        for b in &sessions.blobs {
            // A blob's name is its content hash.
            list(format!("blobs/{}", b.sha256), b.size, &b.sha256)?;
        }
        for item in &sessions.items {
            list(
                format!("sessions/{}/session.jsonl", item.id),
                item.size,
                &item.sha256,
            )?;
            for a in &item.artifacts {
                list(
                    format!("sessions/{}/artifacts/{}", item.id, a.path),
                    a.size,
                    &a.sha256,
                )?;
            }
            if let Some(b) = item.blobs.iter().find(|b| !blob_names.contains(b.as_str())) {
                return Err(invalid(format!(
                    "session {} lists unknown blob {b}",
                    item.id
                )));
            }
        }
    }
    for (key, (size, sha)) in &expected {
        match seen.get(key) {
            None => return Err(invalid(format!("{key} is listed but missing"))),
            Some((s, h)) if s != size || h != sha => {
                return Err(invalid(format!("{key} is corrupt (checksum mismatch)")));
            }
            Some(_) => {}
        }
    }
    if let Some(extra) = seen.keys().find(|k| !expected.contains_key(*k)) {
        return Err(invalid(format!("{extra} is not listed in the manifest")));
    }
    Ok(())
}

/// The first `session` header of `path`, looked for in its first [`HEADER_SEARCH`] bytes.
fn header_id(path: &Path) -> io::Result<Option<String>> {
    use io::BufRead;
    let mut input = BufReader::new(File::open(path)?.take(HEADER_SEARCH as u64));
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line)
            && v["type"] == "session"
        {
            return Ok(v["id"].as_str().map(str::to_owned));
        }
    }
}

/// Every archived session's header must carry the id the manifest lists it under: that id
/// names its directory in the archive, decides the conflict check and is what omp unpacks.
fn check_headers(m: &Manifest, root: &Path) -> Result<()> {
    let Some(sessions) = &m.sessions else {
        return Ok(());
    };
    for item in &sessions.items {
        let path = root.join("sessions").join(&item.id).join("session.jsonl");
        match header_id(&path) {
            Ok(Some(id)) if id == item.id => {}
            Ok(Some(id)) => {
                return Err(invalid(format!("session {} says it is {id}", item.id)));
            }
            Ok(None) => return Err(invalid(format!("session {} has no header", item.id))),
            Err(e) => return Err(invalid(format!("cannot read session {}: {e}", item.id))),
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- plan

/// Every `*.jsonl` file of the sessions store's project directories, found with one directory
/// walk and indexed by the session id its name ends with (`<timestamp>_<id>.jsonl`). A file
/// only counts as a copy of an id once its header says so ([`LocalFiles::copies`]).
struct LocalFiles {
    by_id: BTreeMap<String, Vec<PathBuf>>,
}

impl LocalFiles {
    fn scan(root: &crate::sessions::SessionsRoot) -> LocalFiles {
        let mut by_id: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
        let Ok(projects) = fs::read_dir(&root.path) else {
            return LocalFiles { by_id };
        };
        for project in projects.flatten() {
            if !project.path().is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(project.path()) else {
                continue;
            };
            for file in files.flatten() {
                if !file.file_type().is_ok_and(|t| t.is_file()) {
                    continue;
                }
                let name = file.file_name();
                let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".jsonl")) else {
                    continue;
                };
                // The id is what follows the timestamp's `_`; try every `_` rather than assume.
                for (at, _) in stem.match_indices('_') {
                    by_id
                        .entry(stem[at + 1..].to_owned())
                        .or_default()
                        .push(file.path());
                }
            }
        }
        LocalFiles { by_id }
    }

    /// The local copies of session `id`: named `*_<id>.jsonl`, header id `id` (a bounded
    /// read), with their mtime, size and header cwd. Files that vanished or cannot be read
    /// are not copies.
    fn copies(&self, id: &str) -> Vec<LocalCopy> {
        let mut copies: Vec<LocalCopy> = self
            .by_id
            .get(id)
            .into_iter()
            .flatten()
            .filter(|path| matches!(header_id(path), Ok(Some(h)) if h == id))
            .filter_map(|path| {
                let md = fs::metadata(path).ok()?;
                let modified_unix = md
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                    .and_then(|d| i64::try_from(d.as_secs()).ok())
                    .unwrap_or(0);
                let artifacts = path.with_extension("");
                Some(LocalCopy {
                    path: path.clone(),
                    cwd: crate::sessions::header_cwd(path),
                    modified_unix,
                    bytes: md.len() + dir_bytes(&artifacts),
                })
            })
            .collect();
        copies.sort_by(|a, b| a.path.cmp(&b.path));
        copies
    }
}

/// Total size of the regular files below `dir`, links not followed; 0 when it is not a folder.
fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(e.path()),
                Ok(t) if t.is_file() => total += e.metadata().map_or(0, |m| m.len()),
                _ => {}
            }
        }
    }
    total
}

pub(super) fn plan(
    m: &Manifest,
    target: &Path,
    parts: super::Parts,
    root: &crate::sessions::SessionsRoot,
) -> Result<ProjectRestorePlan> {
    let create_target = match fs::metadata(target) {
        Ok(md) if md.is_dir() => false,
        Ok(_) => {
            return Err(Error::Io(format!(
                "{} is a file, not a folder; pick a folder",
                target.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if fs::symlink_metadata(target).is_ok() {
                return Err(Error::Io(format!(
                    "{} is a broken symbolic link; pick another folder",
                    target.display()
                )));
            }
            true
        }
        Err(e) => return Err(io_err("cannot read", target)(e)),
    };
    let mut plan = ProjectRestorePlan {
        target: target.to_owned(),
        create_target,
        ..ProjectRestorePlan::default()
    };
    if parts.omp
        && let Some(omp) = &m.omp
    {
        let dir = target.join(".omp");
        plan.omp_exists = omp_root_exists(target)?;
        for f in &omp.files {
            match check_dest(&dir, &f.path) {
                Dest::Free => plan.omp_added.push(f.path.clone()),
                Dest::File => plan.omp_overwritten.push(f.path.clone()),
                Dest::Blocked(why) => plan.omp_skipped.push(format!("{}: {why}", f.path)),
            }
        }
        plan.omp_overwritten.sort();
        plan.omp_added.sort();
        plan.omp_skipped.sort();
    }
    if parts.memory
        && let Some(memory) = &m.memory
    {
        let bank = sharpshooter::bank_dir(root, target);
        plan.memory_exists = bank_has_memory(&bank);
        for f in &memory.files {
            match memory_dest(&bank, &f.path) {
                MemDest::Add => plan.memory_added.push(f.path.clone()),
                MemDest::Replace => plan.memory_overwritten.push(f.path.clone()),
                MemDest::Blocked(why) => plan.memory_skipped.push(format!("{}: {why}", f.path)),
            }
        }
        plan.memory_added.sort();
        plan.memory_overwritten.sort();
        plan.memory_skipped.sort();
        plan.memory_dir = bank;
    }
    if parts.sessions
        && let Some(sessions) = &m.sessions
    {
        let local = LocalFiles::scan(root);
        let blob_size: BTreeMap<&str, u64> = sessions
            .blobs
            .iter()
            .map(|b| (b.sha256.as_str(), b.size))
            .collect();
        plan.sessions = sessions
            .items
            .iter()
            .map(|i| {
                let big_blob = i
                    .blobs
                    .iter()
                    .find(|h| blob_size.get(h.as_str()).is_some_and(|s| *s > MAX_MEMBER));
                let problem = if i.size > MAX_MEMBER {
                    Some("the session file is larger than 2 GiB (omp cannot restore it)".to_owned())
                } else if let Some(a) = i.artifacts.iter().find(|a| a.size > MAX_MEMBER) {
                    Some(format!(
                        "artifact {} is larger than 2 GiB (omp cannot restore it)",
                        a.path
                    ))
                } else {
                    big_blob
                        .map(|h| format!("blob {h} is larger than 2 GiB (omp cannot restore it)"))
                };
                SessionPlan {
                    id: i.id.clone(),
                    title: i.title.clone(),
                    modified_unix: i.modified_unix,
                    bytes: i.size + i.artifacts.iter().map(|a| a.size).sum::<u64>(),
                    local: local.copies(&i.id),
                    problem,
                }
            })
            .collect();
    }
    Ok(plan)
}

// ---------------------------------------------------------------- apply

pub(super) fn apply(
    p: &PreparedProjectRestore,
    conflicts: &BTreeMap<String, ConflictChoice>,
    safety_root: &Path,
    omp: Option<&Omp>,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
) -> Result<ProjectRestoreReport> {
    let unpacked = p.stage.dir.join("x");
    let target = &p.plan.target;
    let sessions = p.manifest.sessions.as_ref().filter(|_| p.parts.sessions);
    let omp_files = p
        .manifest
        .omp
        .as_ref()
        .filter(|_| p.parts.omp)
        .map(|o| &o.files);
    if sessions.is_some_and(|s| !s.items.is_empty()) && omp.is_none() {
        return Err(Error::Io("omp is required to restore sessions".to_owned()));
    }
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    let memory_files = p
        .manifest
        .memory
        .as_ref()
        .filter(|_| p.parts.memory)
        .map(|m| &m.files);
    if omp_files.is_some() {
        // Things may have changed since the plan was made.
        omp_root_exists(target)?;
    }
    fs::create_dir_all(target).map_err(io_err("cannot create", target))?;
    let mut report = ProjectRestoreReport::default();
    if let Some(files) = omp_files {
        match restore_omp(
            files,
            &unpacked,
            target,
            safety_root,
            cancel,
            progress,
            &mut report,
        ) {
            Ok(()) => {}
            Err(Error::Cancelled) => {
                // What was written stays written; say so instead of throwing it away.
                report.cancelled = true;
                return Ok(report);
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(files) = memory_files {
        match restore_memory(
            files,
            &unpacked,
            &p.plan.memory_dir,
            safety_root,
            cancel,
            progress,
            &mut report,
        ) {
            Ok(()) => {}
            Err(Error::Cancelled) => {
                report.cancelled = true;
                return Ok(report);
            }
            // The memory is an extra: a failure here is reported, `.omp` and sessions go on.
            Err(e) => report.memory_error = Some(e.to_string()),
        }
    }
    if let (Some(_), Some(omp)) = (sessions, omp) {
        restore_sessions(
            p,
            conflicts,
            safety_root,
            omp,
            cancel,
            progress,
            &mut report,
        )?;
    }
    Ok(report)
}

fn restore_omp(
    files: &[ManifestFile],
    unpacked: &Path,
    target: &Path,
    safety_root: &Path,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
    report: &mut ProjectRestoreReport,
) -> Result<()> {
    let omp_dir = target.join(".omp");
    let existing: Vec<&ManifestFile> = files
        .iter()
        .filter(|f| matches!(check_dest(&omp_dir, &f.path), Dest::File))
        .collect();
    if !existing.is_empty() {
        let safety = safety_dir(safety_root, unix_now());
        platform::create_private_dir_all(&safety).map_err(io_err("cannot create", &safety))?;
        for f in &existing {
            // Regular files only (checked above), so `copy` cannot follow a link.
            let (from, to) = (omp_dir.join(&f.path), safety.join(&f.path));
            if let Some(dir) = to.parent() {
                fs::create_dir_all(dir).map_err(io_err("cannot create", dir))?;
            }
            fs::copy(&from, &to).map_err(io_err("cannot copy", &from))?;
        }
        prune_safety(safety_root, KEEP_SAFETY_COPIES, true);
        report.safety_dir = Some(safety);
    }
    let keep_note = |e: Error| match (&report.safety_dir, e) {
        (Some(dir), Error::Io(m)) => Error::Io(format!(
            "{m} (restore incomplete; the previous files are in {})",
            dir.display()
        )),
        (_, e) => e,
    };
    let mut pump = Pump::new(cancel, progress, Stage::RestoringOmp, files.len() as u64);
    let mut written = 0;
    let mut overwritten = 0;
    for f in files {
        if cancel.load(Ordering::Relaxed) {
            report.omp_written = written;
            report.omp_overwritten = overwritten;
            return Err(Error::Cancelled);
        }
        let dest = omp_dir.join(&f.path);
        let step = match check_dest(&omp_dir, &f.path) {
            Dest::Blocked(why) => {
                report.omp_skipped.push(format!("{}: {why}", f.path));
                Ok(())
            }
            state => {
                let done = install(&unpacked.join("omp").join(&f.path), &dest, f.mode);
                if done.is_ok() {
                    written += 1;
                    overwritten += usize::from(matches!(state, Dest::File));
                }
                done
            }
        };
        if let Err(e) = step.and_then(|()| pump.item()) {
            report.omp_written = written;
            report.omp_overwritten = overwritten;
            return Err(keep_note(e));
        }
    }
    report.omp_written = written;
    report.omp_overwritten = overwritten;
    Ok(())
}

/// How long a restore waits for omp's consolidation lock on the bank (shortened in this crate's
/// tests, which hold the lock on purpose).
const MEMORY_LOCK_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(1500)
} else {
    Duration::from_secs(10)
};
/// [`ProjectRestoreReport::memory_error`] while omp holds the bank's lock.
const MEMORY_LOCKED: &str = "omp is updating this folder's memory right now; try again in a moment";

/// What restoring one memory file into the bank would do.
#[derive(Debug, PartialEq, Eq)]
enum MemDest {
    /// Nothing non-empty there (a new file, or an empty one omp left behind).
    Add,
    /// A non-empty regular file: replaced, after a safety copy.
    Replace,
    /// Not written; why (a symlink in the way, a folder where the file goes, ...).
    Blocked(String),
}

/// Looks at `<bank>/<name>` without following links.
fn memory_dest(bank: &Path, name: &str) -> MemDest {
    match fs::symlink_metadata(bank) {
        Ok(m) if m.file_type().is_symlink() => {
            return MemDest::Blocked("the memory folder is a symbolic link".to_owned());
        }
        Ok(m) if !m.is_dir() => {
            return MemDest::Blocked("a file is where the memory folder goes".to_owned());
        }
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return MemDest::Add,
        Err(e) => return MemDest::Blocked(format!("cannot inspect the memory folder ({e})")),
    }
    match check_dest(bank, name) {
        Dest::Free => MemDest::Add,
        // Unreadable metadata → assume content: the safety copy is cheap.
        Dest::File if fs::metadata(bank.join(name)).is_ok_and(|m| m.len() == 0) => MemDest::Add,
        Dest::File => MemDest::Replace,
        Dest::Blocked(why) => MemDest::Blocked(why),
    }
}

/// The bank folder holds a non-empty memory file (what omp would lose to a restore).
fn bank_has_memory(bank: &Path) -> bool {
    fs::symlink_metadata(bank).is_ok_and(|m| m.is_dir())
        && MEMORY_FILES.iter().any(|name| {
            fs::symlink_metadata(bank.join(name)).is_ok_and(|m| m.is_file() && m.len() > 0)
        })
}

/// Writes the memory files into `bank` while holding omp's consolidation lock (omp renames its
/// own output over these files, computed from what it read earlier, so unlocked writes could be
/// silently undone). Overwritten files are copied to `<safety>/memory/<name>` first; nothing is
/// deleted; `state.json` and `queue/` are never touched. A held lock is
/// [`ProjectRestoreReport::memory_error`], not an `Err`.
fn restore_memory(
    files: &[ManifestFile],
    unpacked: &Path,
    bank: &Path,
    safety_root: &Path,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
    report: &mut ProjectRestoreReport,
) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Error::Cancelled);
    }
    // Polled, not one long wait, so a cancel is honored while omp holds the lock.
    let deadline = Instant::now() + MEMORY_LOCK_WAIT;
    let held = loop {
        if let Some(held) = sharpshooter::lock_bank(bank, Duration::ZERO)
            .map_err(io_err("cannot lock the memory folder", bank))?
        {
            break Some(held);
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
    };
    let Some(_held) = held else {
        report.memory_error = Some(MEMORY_LOCKED.to_owned());
        return Ok(());
    };
    // Looked at again under the lock: omp may have written since the plan was made.
    let dests: Vec<MemDest> = files.iter().map(|f| memory_dest(bank, &f.path)).collect();
    if dests.iter().any(|d| !matches!(d, MemDest::Blocked(_))) {
        fs::create_dir_all(bank).map_err(io_err("cannot create", bank))?;
    }
    let replaced: Vec<&ManifestFile> = files
        .iter()
        .zip(&dests)
        .filter(|(_, d)| **d == MemDest::Replace)
        .map(|(f, _)| f)
        .collect();
    if !replaced.is_empty() {
        let safety = ensure_safety(safety_root, report)?;
        let copies = safety.join("memory");
        fs::create_dir_all(&copies).map_err(io_err("cannot create", &copies))?;
        for f in replaced {
            // A regular file (checked above), so `copy` cannot follow a link.
            let from = bank.join(&f.path);
            fs::copy(&from, copies.join(&f.path)).map_err(io_err("cannot copy", &from))?;
        }
    }
    let mut pump = Pump::new(cancel, progress, Stage::RestoringMemory, files.len() as u64);
    for (f, dest) in files.iter().zip(&dests) {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        match dest {
            MemDest::Blocked(why) => report.memory_skipped.push(format!("{}: {why}", f.path)),
            MemDest::Add | MemDest::Replace => {
                install(
                    &unpacked.join("memory").join(&f.path),
                    &bank.join(&f.path),
                    f.mode,
                )?;
                report.memory_written += 1;
                report.memory_overwritten += usize::from(*dest == MemDest::Replace);
            }
        }
        pump.item()?;
    }
    Ok(())
}

/// What is at a backup file's destination under `.omp`.
enum Dest {
    /// Nothing there (it is created, with its folders).
    Free,
    /// A regular file: overwritten.
    File,
    /// Not written; why.
    Blocked(String),
}

/// Why Windows cannot store a file or folder called `name`: a character from `<>:"|?*` or a
/// control character, a trailing dot or space, or a reserved device name (`CON`, `PRN`, `AUX`,
/// `NUL`, `COM1`-`COM9`, `LPT1`-`LPT9`, with or without an extension).
fn windows_name_problem(name: &str) -> Option<&'static str> {
    if name
        .chars()
        .any(|c| c.is_control() || "<>:\"|?*".contains(c))
    {
        return Some("the name has a character Windows does not allow");
    }
    if name.ends_with(['.', ' ']) {
        return Some("the name ends with a dot or a space, which Windows does not allow");
    }
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ["COM", "LPT"].iter().any(|p| {
            stem.strip_prefix(p)
                .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
        });
    reserved.then_some("the name is reserved by Windows")
}

/// True when some component of the `/`-separated `rel` is a [`windows_name_problem`].
fn unstorable_on_windows(rel: &str) -> bool {
    rel.split('/').any(|p| windows_name_problem(p).is_some())
}

/// Walks `rel` below `omp_dir` without following links: a symlink or junction anywhere on the
/// way (or as the file), a file where a folder is needed, a folder where the file goes, or (on
/// Windows) a name Windows cannot store blocks it.
fn check_dest(omp_dir: &Path, rel: &str) -> Dest {
    let parts: Vec<&str> = rel.split('/').collect();
    if cfg!(windows)
        && let Some(why) = parts.iter().find_map(|p| windows_name_problem(p))
    {
        return Dest::Blocked(why.to_owned());
    }
    let mut cur = omp_dir.to_owned();
    for (i, part) in parts.iter().enumerate() {
        cur.push(part);
        let last = i + 1 == parts.len();
        let here = || parts[..=i].join("/");
        match fs::symlink_metadata(&cur) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Dest::Free,
            Err(e) => return Dest::Blocked(format!("cannot inspect {} ({e})", here())),
            Ok(m) if m.file_type().is_symlink() => {
                return Dest::Blocked(if last {
                    "a symbolic link is in the way".to_owned()
                } else {
                    format!("{} is a symbolic link", here())
                });
            }
            Ok(m) if last => {
                return if m.is_file() {
                    Dest::File
                } else if m.is_dir() {
                    Dest::Blocked("a folder is in the way".to_owned())
                } else {
                    Dest::Blocked("something that is not a regular file is in the way".to_owned())
                };
            }
            Ok(m) if !m.is_dir() => {
                return Dest::Blocked(format!(
                    "{} is a file, the backup has a folder there",
                    here()
                ));
            }
            Ok(_) => {}
        }
    }
    Dest::Free
}

/// Whether `<target>/.omp` exists. A symlink or a file there is refused: nothing is restored
/// through a link, and a file cannot hold the folder.
fn omp_root_exists(target: &Path) -> Result<bool> {
    let dir = target.join(".omp");
    match fs::symlink_metadata(&dir) {
        Ok(m) if m.file_type().is_symlink() => Err(Error::Io(format!(
            "{} is a symbolic link; the backup is never restored through a link. Remove the link or pick another folder",
            dir.display()
        ))),
        Ok(m) if m.is_dir() => Ok(true),
        Ok(_) => Err(Error::Io(format!(
            "{} is a file, not a folder; move it away or pick another folder",
            dir.display()
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err("cannot read", &dir)(e)),
    }
}

/// `<root>/project-<stamp>` that does not exist yet.
fn safety_dir(root: &Path, now: i64) -> PathBuf {
    let stamp = format!("{PROJECT_SAFETY_PREFIX}{}", compact_stamp(now));
    let mut dir = root.join(&stamp);
    let mut n = 2;
    while dir.exists() {
        dir = root.join(format!("{stamp}-{n}"));
        n += 1;
    }
    dir
}

/// Copies `src` over `dest` atomically (temp file in the destination directory + rename),
/// keeping the executable bits of `mode`. The rename replaces a symlink at `dest` itself; it
/// never writes through one (callers skip symlinked destinations before getting here).
fn install(src: &Path, dest: &Path, mode: u32) -> Result<()> {
    let dir = dest.parent().unwrap_or(Path::new("."));
    let write = || -> io::Result<()> {
        fs::create_dir_all(dir)?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".omp-deck-restore")
            .tempfile_in(dir)?;
        io::copy(&mut File::open(src)?, tmp.as_file_mut())?;
        platform::set_file_mode(tmp.path(), (mode & 0o777) | 0o600)?;
        tmp.as_file().sync_all()?;
        tmp.persist(dest).map(drop).map_err(|e| e.error)
    };
    write().map_err(io_err("cannot write", dest))
}

/// How often a restore retries a pack/delete that failed only because a live omp writes the
/// session, and how long it waits in between (~3 s in all).
const LIVE_RETRIES: u32 = 10;
const LIVE_WAIT: Duration = Duration::from_millis(300);

/// What happened to one backed-up session.
enum Step {
    Added(PathBuf),
    Replaced(PathBuf),
    Kept,
    Failed(String),
}

/// What restoring sessions needs besides the report: the env-configured omp and the staging
/// directories.
struct SessionJob<'a> {
    p: &'a PreparedProjectRestore,
    omp: Omp,
    unpacked: &'a Path,
    work: PathBuf,
    omp_version: &'a str,
    safety_root: &'a Path,
}

/// Sets `path`'s mtime to `unix` seconds (metadata only). `unix <= 0` leaves it; a failure is
/// logged, never a session failure.
fn set_mtime(path: &Path, unix: i64) {
    let Ok(secs) = u64::try_from(unix) else {
        return;
    };
    if secs == 0 {
        return;
    }
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    if let Err(e) = File::options()
        .write(true)
        .open(path)
        .and_then(|f| f.set_modified(when))
    {
        log::warn!("cannot set the time of {}: {e}", path.display());
    }
}

/// The restore's safety directory, created on first use (shared with the `.omp` and memory
/// parts).
fn ensure_safety(safety_root: &Path, report: &mut ProjectRestoreReport) -> Result<PathBuf> {
    if let Some(dir) = &report.safety_dir {
        return Ok(dir.clone());
    }
    let dir = safety_dir(safety_root, unix_now());
    platform::create_private_dir_all(&dir).map_err(io_err("cannot create", &dir))?;
    prune_safety(safety_root, KEEP_SAFETY_COPIES, true);
    report.safety_dir = Some(dir.clone());
    Ok(dir)
}

impl SessionJob<'_> {
    /// Builds the pack for item `i` (under a fresh id when `fresh`), unpacks it into the
    /// restore target and removes the tar.
    fn unpack(&self, item: &ItemManifest, i: usize, fresh: bool) -> Result<MoveOutcome, String> {
        let tar = self.work.join(format!("{i}.tar"));
        let outcome = build_pack(
            item,
            fresh,
            &self.p.manifest.project,
            self.omp_version,
            self.unpacked,
            &self.work,
            &tar,
        )
        .map_err(|e| format!("cannot prepare the session: {e}"))
        .and_then(|()| {
            self.omp
                .unpack_session(&tar, &self.p.plan.target)
                .map_err(|e| tidy_failure(&e.to_string()))
        });
        if let Err(e) = fs::remove_file(&tar)
            && e.kind() != io::ErrorKind::NotFound
        {
            log::warn!("cannot remove {}: {e}", tar.display());
        }
        outcome
    }

    /// The session file omp created, which must be a new session; its mtime becomes the
    /// backed-up one.
    fn created(item: &ItemManifest, done: MoveOutcome) -> Result<PathBuf, String> {
        if done.kind == MoveKind::Created {
            set_mtime(&done.new_path, item.modified_unix);
            Ok(done.new_path)
        } else {
            Err(format!(
                "omp did not add the session as new ({:?}); it wrote {}",
                done.kind,
                done.new_path.display()
            ))
        }
    }

    /// `omp session pack`, retried while another omp process is writing the session.
    fn pack(&self, src: &Path, tar: &Path) -> Result<(), String> {
        let mut attempt = 1;
        loop {
            match self.omp.pack_session(src, tar) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let text = e.to_string();
                    if attempt < LIVE_RETRIES && text.contains("live omp process") {
                        attempt += 1;
                        std::thread::sleep(LIVE_WAIT);
                    } else {
                        return Err(tidy_failure(&text));
                    }
                }
            }
        }
    }

    /// Replaces every local copy of the session by the backup (original id). Copies are saved
    /// first; a failure after one was deleted puts the deleted ones back.
    fn overwrite(
        &self,
        item: &ItemManifest,
        i: usize,
        copies: &[LocalCopy],
        report: &mut ProjectRestoreReport,
    ) -> Result<PathBuf, String> {
        let safety = ensure_safety(self.safety_root, report)
            .map_err(|e| format!("cannot save the local copy first: {e}"))?;
        let saved_in = safety.join("sessions");
        fs::create_dir_all(&saved_in)
            .map_err(|e| format!("cannot save the local copy first: {e}"))?;
        let mut tars = Vec::with_capacity(copies.len());
        for (k, copy) in copies.iter().enumerate() {
            let name = if k == 0 {
                format!("{}.tar", item.id)
            } else {
                format!("{}-{k}.tar", item.id)
            };
            let tar = saved_in.join(name);
            self.pack(&copy.path, &tar)
                .map_err(|e| format!("cannot save the local copy first: {e}"))?;
            tars.push(tar);
        }
        let targets: Vec<DeleteTarget> = copies
            .iter()
            .map(|c| DeleteTarget {
                path: c.path.clone(),
                id: item.id.clone(),
            })
            .collect();
        let mut deleted = vec![false; copies.len()];
        let mut failure: Option<String> = None;
        self.omp.delete_sessions(&targets, |path, result| {
            match result {
                Ok(()) => {
                    if let Some(k) = copies.iter().position(|c| c.path == path) {
                        deleted[k] = true;
                    }
                }
                Err(DeleteError::Live) => {
                    failure.get_or_insert_with(|| "it is open in another omp process".to_owned());
                }
                Err(e) => {
                    failure.get_or_insert_with(|| format!("cannot remove the local copy: {e}"));
                }
            };
        });
        // A target that never reported (it should not happen) is a failure, not a success.
        let reason = match failure {
            Some(why) => Some(why),
            None if deleted.iter().all(|d| *d) => None,
            None => Some("cannot remove the local copy".to_owned()),
        };
        let restored = match reason {
            Some(why) => Err(why),
            None => self
                .unpack(item, i, false)
                .and_then(|done| Self::created(item, done)),
        };
        match restored {
            Ok(path) => Ok(path),
            Err(why) if deleted.iter().any(|d| *d) => {
                Err(self.put_back(why, copies, &tars, &deleted))
            }
            Err(why) => Err(why),
        }
    }

    /// Unpacks every deleted local copy again from its safety tar (into its own folder when
    /// that exists, else the restore target) with its previous mtime; returns the failure text,
    /// naming the safety tar of every copy that could not be put back.
    fn put_back(
        &self,
        why: String,
        copies: &[LocalCopy],
        tars: &[PathBuf],
        deleted: &[bool],
    ) -> String {
        let mut stranded = Vec::new();
        for (k, copy) in copies.iter().enumerate().filter(|(k, _)| deleted[*k]) {
            let cwd = copy
                .cwd
                .as_deref()
                .filter(|c| c.is_dir())
                .unwrap_or(&self.p.plan.target);
            match self.omp.unpack_session(&tars[k], cwd) {
                Ok(done) => set_mtime(&done.new_path, copy.modified_unix),
                Err(e) => {
                    log::warn!("cannot put {} back: {e}", copy.path.display());
                    stranded.push(tars[k].display().to_string());
                }
            }
        }
        match stranded.as_slice() {
            [] => format!("{why}; the local copy was put back"),
            [one] => format!("{why}; the local copy is saved at {one}"),
            many => format!("{why}; the local copies are saved at {}", many.join(", ")),
        }
    }

    fn restore_one(
        &self,
        item: &ItemManifest,
        i: usize,
        local: &LocalFiles,
        conflicts: &BTreeMap<String, ConflictChoice>,
        report: &mut ProjectRestoreReport,
    ) -> Step {
        let copies = local.copies(&item.id);
        let choice = if copies.is_empty() {
            None
        } else {
            Some(conflicts.get(&item.id).copied().unwrap_or_default())
        };
        if choice == Some(ConflictChoice::KeepLocal) {
            return Step::Kept;
        }
        if let Some(why) = self.p.plan.sessions.get(i).and_then(|s| s.problem.clone()) {
            return Step::Failed(why);
        }
        if choice == Some(ConflictChoice::Overwrite) {
            return match self.overwrite(item, i, &copies, report) {
                Ok(path) => Step::Replaced(path),
                Err(why) => Step::Failed(why),
            };
        }
        let fresh = choice == Some(ConflictChoice::KeepBoth);
        match self
            .unpack(item, i, fresh)
            .and_then(|done| Self::created(item, done))
        {
            Ok(path) => Step::Added(path),
            Err(why) => Step::Failed(why),
        }
    }
}

fn restore_sessions(
    p: &PreparedProjectRestore,
    conflicts: &BTreeMap<String, ConflictChoice>,
    safety_root: &Path,
    omp: &Omp,
    cancel: &AtomicBool,
    progress: &(dyn Fn(Progress) + Sync),
    report: &mut ProjectRestoreReport,
) -> Result<()> {
    let items: &[ItemManifest] = p.manifest.sessions.as_ref().map_or(&[], |s| &s.items);
    let unpacked = p.stage.dir.join("x");
    let omp_version = if p.manifest.omp_version.is_empty() {
        "unknown"
    } else {
        &p.manifest.omp_version
    };
    let work = p.stage.dir.join("work");
    // omp stages every unpack in the temp directory; /tmp can be a small tmpfs, the cache
    // directory the staging lives in is where the archive already fits.
    let scratch = p.stage.dir.join("tmp");
    for dir in [&work, &scratch] {
        fs::create_dir_all(dir).map_err(io_err("cannot create", dir))?;
    }
    // The same sessions store the plan checked for conflicts is the one omp writes to.
    let omp = omp
        .clone()
        .with_env("PI_CODING_AGENT_DIR", p.root.agent_dir())
        .with_env("TMPDIR", &scratch)
        .with_env("TMP", &scratch)
        .with_env("TEMP", &scratch);
    let job = SessionJob {
        p,
        omp,
        unpacked: &unpacked,
        work,
        omp_version,
        safety_root,
    };
    let local = LocalFiles::scan(&p.root);
    let mut pump = Pump::new(
        cancel,
        progress,
        Stage::RestoringSessions,
        items.len() as u64,
    );
    for (i, item) in items.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            report.cancelled = true;
            return Ok(());
        }
        let label = if item.title.is_empty() {
            item.id.clone()
        } else {
            item.title.clone()
        };
        match job.restore_one(item, i, &local, conflicts, report) {
            Step::Added(path) => report.sessions_added.push(path),
            Step::Replaced(path) => {
                report.sessions_added.push(path);
                report.sessions_replaced += 1;
            }
            Step::Kept => report.sessions_kept.push(label),
            Step::Failed(why) => report.sessions_failed.push((label, why)),
        }
        if pump.item().is_err() {
            report.cancelled = true;
            return Ok(());
        }
    }
    Ok(())
}

/// The reason out of omp's failure output: the `error:` line when there is one (Bun prints the
/// failing source lines and a stack around it), else the first line that is not source or stack.
/// Capped, never a raw dump.
fn tidy_failure(raw: &str) -> String {
    const MAX_CHARS: usize = 240;
    let lines: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let is_noise = |l: &str| {
        l.starts_with("at ")
            || l.chars().all(|c| c == '^' || c == '~')
            || (l.starts_with(|c: char| c.is_ascii_digit()) && l.contains(" | "))
    };
    let reason = lines
        .iter()
        .rev()
        .find_map(|l| {
            let rest = l
                .strip_prefix("error:")
                .or_else(|| l.strip_prefix("Error:"))?;
            Some(rest.trim()).filter(|r| !r.is_empty())
        })
        .or_else(|| lines.iter().find(|l| !is_noise(l)).copied())
        .unwrap_or("omp failed without a message");
    let mut out: String = reason.chars().take(MAX_CHARS).collect();
    if reason.chars().count() > MAX_CHARS {
        out.push('…');
    }
    out
}

/// A pack-format tar for one session (what `omp session unpack` reads).
fn build_pack(
    item: &ItemManifest,
    new_id: bool,
    project: &str,
    omp_version: &str,
    unpacked: &Path,
    work: &Path,
    tar_path: &Path,
) -> io::Result<()> {
    let id = if new_id { fresh_id()? } else { item.id.clone() };
    let base = unpacked.join("sessions").join(&item.id);
    let jsonl = base.join("session.jsonl");
    let rewritten;
    let jsonl_src = if new_id {
        rewritten = work.join("rewritten.jsonl");
        rewrite_header_id(&jsonl, &rewritten, &id)?;
        &rewritten
    } else {
        &jsonl
    };
    let mut tar = tar::Builder::new(BufWriter::new(File::create(tar_path)?));
    let manifest = serde_json::json!({
        "version": 1,
        "id": id,
        "cwd": project,
        "omp": omp_version,
    })
    .to_string();
    let now = u64::try_from(unix_now()).unwrap_or(0);
    let header = |size: u64| {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(size);
        h.set_mode(0o644);
        h.set_mtime(now);
        h
    };
    tar.append_data(
        &mut header(manifest.len() as u64),
        "manifest.json",
        manifest.as_bytes(),
    )?;
    let append = |tar: &mut tar::Builder<_>, name: &str, src: &Path| -> io::Result<()> {
        let file = File::open(src)?;
        tar.append_data(&mut header(file.metadata()?.len()), name, file)
    };
    append(&mut tar, "session.jsonl", jsonl_src)?;
    for a in &item.artifacts {
        append(
            &mut tar,
            &format!("artifacts/{}", a.path),
            &base.join("artifacts").join(&a.path),
        )?;
    }
    for blob in &item.blobs {
        append(
            &mut tar,
            &format!("blobs/{blob}"),
            &unpacked.join("blobs").join(blob),
        )?;
    }
    tar.into_inner()?.flush()
}

/// Copies `src` to `dst` with the `id` of the session header (the first line of type `session`)
/// replaced; every other byte is unchanged.
fn rewrite_header_id(src: &Path, dst: &Path, id: &str) -> io::Result<()> {
    use io::BufRead;
    let mut input = BufReader::new(File::open(src)?);
    let mut out = BufWriter::new(File::create(dst)?);
    let mut line = Vec::new();
    let mut scanned = 0;
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 || scanned > HEADER_SEARCH {
            return Err(io::Error::other("the session file has no session header"));
        }
        scanned += line.len();
        let header = serde_json::from_slice::<serde_json::Value>(&line)
            .ok()
            .filter(|v| v["type"] == "session");
        match header {
            Some(mut v) => {
                v["id"] = serde_json::Value::String(id.to_owned());
                serde_json::to_writer(&mut out, &v)?;
                out.write_all(b"\n")?;
                break;
            }
            None => out.write_all(&line)?,
        }
    }
    io::copy(&mut input, &mut out)?;
    out.flush()
}

/// A random id shaped like a UUIDv7 (time-ordered, like omp's own).
fn fresh_id() -> io::Result<String> {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).map_err(io::Error::other)?;
    let ms = u64::try_from(unix_now()).unwrap_or(0) * 1000;
    b[..6].copy_from_slice(&ms.to_be_bytes()[2..]);
    b[6] = b[6] & 0x0f | 0x70;
    b[8] = b[8] & 0x3f | 0x80;
    let h = hex(&b);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}

#[cfg(test)]
mod tests {
    use super::{unstorable_on_windows, windows_name_problem};

    #[test]
    fn windows_name_problems() {
        for bad in [
            "CON",
            "con.txt",
            "Nul",
            "AUX.tar.gz",
            "PRN",
            "COM1",
            "com9.log",
            "LPT1",
            "LPT9",
            "a:b",
            "a?",
            "a*b",
            "a<b",
            "a>b",
            "a|b",
            "a\"b",
            "a.",
            "a ",
            "a\u{1}b",
            "tab\there",
        ] {
            assert!(
                windows_name_problem(bad).is_some(),
                "{bad:?} should be refused"
            );
        }
        for ok in [
            "COM0",
            "COM10",
            "console",
            "CONX",
            "ok.txt",
            ".gitignore",
            "a b",
            "ünï.md",
            "LPT",
        ] {
            assert_eq!(windows_name_problem(ok), None, "{ok:?} should pass");
        }
    }

    #[test]
    fn a_path_is_unstorable_when_any_component_is() {
        assert!(unstorable_on_windows("rules/con/x.md"));
        assert!(unstorable_on_windows("a/b/c?.md"));
        assert!(!unstorable_on_windows("rules/ok/x.md"));
    }

    use super::tidy_failure;

    #[test]
    fn failure_text_is_the_reason_never_a_dump() {
        // Bun: source excerpt, caret, the error line, then details.
        let dump = "642 |   throw new Error(`x`)\n              ^\nerror: Failed to load the addon.\n\nTried:\n- EDQUOT";
        assert_eq!(tidy_failure(dump), "Failed to load the addon.");
        // No `error:` line: the first line that is neither source nor stack.
        assert_eq!(
            tidy_failure("12 | code\n   at foo (bar.js:1)\nBad session archive\nmore"),
            "Bad session archive"
        );
        assert_eq!(tidy_failure("  \n"), "omp failed without a message");
        let long = tidy_failure(&"x".repeat(1000));
        assert_eq!(long.chars().count(), 241);
        assert!(long.ends_with('…'));
    }
}
