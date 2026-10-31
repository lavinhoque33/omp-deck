//! What a project consists of, found with metadata only: the `.omp` files, the omp memory bank
//! files, the sessions of the folder with their artifacts, and the fingerprint over their
//! (path, length, mtime).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use super::{MemoryBank, OmpDir, Parts, ProjectSizes, SessionSize, same_path};
use crate::backup::archive::hex;
use crate::platform;
use crate::sessions::{self, SessionsRoot};
use crate::sharpshooter::{self, MEMORY_FILES};

/// Deepest nesting (path components below `.omp` / the artifacts directory) that is followed.
const MAX_DEPTH: usize = 16;
/// Titles kept in the manifest.
const TITLE_CHARS: usize = 200;

/// One regular file as it was when the snapshot was taken.
#[derive(Clone, Debug)]
pub(super) struct FileRef {
    /// `/`-separated, relative to its part's root.
    pub rel: String,
    pub path: PathBuf,
    pub len: u64,
    pub mtime_ns: i128,
    pub mode: u32,
}

#[derive(Clone, Debug)]
pub(super) struct SessionSnap {
    pub id: String,
    pub file_name: String,
    pub title: String,
    pub modified_unix: i64,
    pub modified: SystemTime,
    pub jsonl: FileRef,
    pub artifacts: Vec<FileRef>,
}

pub(super) struct Snapshot {
    parts: Parts,
    pub omp: OmpDir,
    pub omp_files: Vec<FileRef>,
    pub memory: MemoryBank,
    /// The bank folder of the project (it may not exist).
    pub memory_dir: PathBuf,
    /// Non-empty regular memory files (`rel` = the file name).
    pub memory_files: Vec<FileRef>,
    /// Memory files left out (`memory/<name>: <reason>`).
    pub memory_skipped: Vec<String>,
    pub sessions: Vec<SessionSnap>,
    /// `<agent>/blobs`.
    pub blob_dir: PathBuf,
    /// `.omp` entries left out (`<path>: <reason>`).
    pub skipped: Vec<String>,
    /// Session artifacts left out (`session “<title>”: <reason>`).
    pub session_notes: Vec<String>,
    /// Excluded ids that are sessions of this project (stale ids do not count as a change).
    excluded: BTreeSet<String>,
    fingerprint: String,
}

impl Snapshot {
    /// Looks at the project without reading any file content. `home` is `$HOME`: a project
    /// that is the home folder has no project `.omp` (it is omp's global directory).
    /// Sessions whose id is in `excluded` are left out silently.
    pub(super) fn take(
        root: &SessionsRoot,
        project: &Path,
        parts: Parts,
        excluded: &BTreeSet<String>,
        home: Option<&Path>,
    ) -> io::Result<Snapshot> {
        let mut omp_files = Vec::new();
        let mut skipped = Vec::new();
        let omp = if home.is_some_and(|h| same_path(h, project)) {
            OmpDir::Global
        } else {
            let dir = project.join(".omp");
            // `symlink_metadata`: a symlinked `.omp` is never followed (backup nor restore).
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.file_type().is_symlink() => {
                    skipped.push(".omp: symbolic link, not backed up".to_owned());
                    OmpDir::Symlink
                }
                Ok(m) if m.is_dir() => {
                    walk(&dir, "", 0, true, true, &mut omp_files, &mut skipped)?;
                    OmpDir::Present {
                        files: omp_files.len() as u64,
                        bytes: omp_files.iter().map(|f| f.len).sum(),
                    }
                }
                Ok(_) => OmpDir::Missing,
                Err(e) if matches!(e.kind(), io::ErrorKind::NotFound) => OmpDir::Missing,
                Err(e) if e.kind() == io::ErrorKind::NotADirectory => OmpDir::Missing,
                Err(e) => return Err(e),
            }
        };
        let memory_dir = sharpshooter::bank_dir(root, project);
        let (memory, memory_files, memory_skipped) = take_memory(&memory_dir);
        let mut session_notes = Vec::new();
        let mut matched = BTreeSet::new();
        let sessions = if parts.sessions {
            project_sessions(root, project, excluded, &mut matched, &mut session_notes)
        } else {
            Vec::new()
        };
        let mut snap = Snapshot {
            parts,
            omp,
            omp_files,
            memory,
            memory_dir,
            memory_files,
            memory_skipped,
            sessions,
            blob_dir: root.agent_dir().join("blobs"),
            skipped,
            session_notes,
            excluded: matched,
            fingerprint: String::new(),
        };
        snap.fingerprint = snap.compute_fingerprint();
        Ok(snap)
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(super) fn omp_included(&self) -> bool {
        self.parts.omp && !self.omp_files.is_empty()
    }

    pub(super) fn sessions_included(&self) -> bool {
        self.parts.sessions && !self.sessions.is_empty()
    }

    pub(super) fn memory_included(&self) -> bool {
        self.parts.memory && !self.memory_files.is_empty()
    }

    /// Nothing of the requested parts exists.
    pub(super) fn is_empty(&self) -> bool {
        !self.omp_included() && !self.memory_included() && !self.sessions_included()
    }

    /// Totals over the sessions in the snapshot (take it with no exclusions to measure a
    /// project) and the per-session list, newest first.
    pub(super) fn sizes(&self) -> ProjectSizes {
        let mut session_list: Vec<SessionSize> = self
            .sessions
            .iter()
            .map(|s| SessionSize {
                id: s.id.clone(),
                title: s.title.clone(),
                modified: s.modified,
                bytes: s.jsonl.len + s.artifacts.iter().map(|a| a.len).sum::<u64>(),
            })
            .collect();
        session_list.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.id.cmp(&b.id)));
        ProjectSizes {
            omp: self.omp,
            memory: self.memory,
            memory_dir: self.memory_dir.clone(),
            sessions: session_list.len() as u64,
            session_bytes: session_list.iter().map(|s| s.bytes).sum(),
            session_list,
        }
    }

    /// sha256 over the part flags and each included file's (part, path, len, mtime ns).
    fn compute_fingerprint(&self) -> String {
        let mut h = Sha256::new();
        let mut line = |fields: &[&str]| {
            for f in fields {
                h.update(f.as_bytes());
                h.update([0]);
            }
            h.update(b"\n");
        };
        line(&[
            "omp-deck project sync v1",
            &u8::from(self.parts.omp).to_string(),
            &u8::from(self.parts.sessions).to_string(),
        ]);
        // Only when selected: fingerprints of rows without Memory stay what they always were.
        if self.parts.memory {
            line(&["memory"]);
            for f in &self.memory_files {
                line(&[
                    "memory",
                    &f.rel,
                    &f.len.to_string(),
                    &f.mtime_ns.to_string(),
                ]);
            }
        }
        if self.parts.sessions {
            for id in &self.excluded {
                line(&["excluded", id]);
            }
        }
        if self.parts.omp {
            for f in &self.omp_files {
                line(&["omp", &f.rel, &f.len.to_string(), &f.mtime_ns.to_string()]);
            }
        }
        for s in &self.sessions {
            line(&[
                "session",
                &s.id,
                &s.file_name,
                &s.jsonl.len.to_string(),
                &s.jsonl.mtime_ns.to_string(),
            ]);
            for a in &s.artifacts {
                line(&[
                    "artifact",
                    &s.id,
                    &a.rel,
                    &a.len.to_string(),
                    &a.mtime_ns.to_string(),
                ]);
            }
        }
        hex(&h.finalize())
    }
}

/// The scan group of `project`, with artifacts. Entries that cannot be archived are skipped
/// (logged).
fn project_sessions(
    root: &SessionsRoot,
    project: &Path,
    excluded: &BTreeSet<String>,
    matched: &mut BTreeSet<String>,
    notes: &mut Vec<String>,
) -> Vec<SessionSnap> {
    let mut out = Vec::new();
    let mut ids = HashSet::new();
    for group in sessions::scan(root) {
        if !same_path(&group.cwd, project) {
            continue;
        }
        for e in group.sessions {
            if excluded.contains(&e.id) {
                matched.insert(e.id);
                continue;
            }
            if !valid_id(&e.id) {
                log::warn!("skipping session {}: unusable id", e.path.display());
                continue;
            }
            if !ids.insert(e.id.clone()) {
                log::warn!("skipping {}: id {} appears twice", e.path.display(), e.id);
                continue;
            }
            let Some(file_name) = e.path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(stem) = file_name.strip_suffix(".jsonl") else {
                continue;
            };
            let Some(jsonl) = file_ref(&e.path, file_name.to_owned()) else {
                continue;
            };
            let mut artifacts = Vec::new();
            let dir = e.path.with_file_name(stem);
            let label: String = if e.label().is_empty() {
                &e.id
            } else {
                e.label()
            }
            .chars()
            .take(60)
            .collect();
            // A symlinked artifact directory is not followed.
            match fs::symlink_metadata(&dir) {
                Ok(m) if m.file_type().is_symlink() => notes.push(format!(
                    "session “{label}”: artifacts folder is a symbolic link, not backed up"
                )),
                Ok(m) if m.is_dir() => {
                    let mut left_out = Vec::new();
                    if let Err(err) = walk(&dir, "", 0, false, false, &mut artifacts, &mut left_out)
                    {
                        log::warn!("cannot read artifacts {}: {err}", dir.display());
                    }
                    notes.extend(
                        left_out
                            .into_iter()
                            .map(|what| format!("session “{label}”: artifact {what}")),
                    );
                }
                _ => {}
            }
            let title: String = e.label().chars().take(TITLE_CHARS).collect();
            out.push(SessionSnap {
                id: e.id,
                file_name: file_name.to_owned(),
                title,
                modified: e.modified,
                modified_unix: e
                    .modified
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0)),
                jsonl,
                artifacts,
            });
        }
    }
    out
}

/// omp's session id rule: `^[A-Za-z0-9_-]{1,128}$`.
pub(super) fn valid_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

/// `path` as a [`FileRef`] when it is a regular file (symlinks and the like are not).
fn file_ref(path: &Path, rel: String) -> Option<FileRef> {
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            log::debug!("skip {}: {e}", path.display());
            return None;
        }
    };
    if !meta.is_file() {
        return None;
    }
    Some(FileRef {
        rel,
        path: path.to_owned(),
        len: meta.len(),
        mtime_ns: meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .and_then(|d| i128::try_from(d.as_nanos()).ok())
            .unwrap_or(0),
        mode: platform::file_mode(path, &meta),
    })
}

/// The memory bank folder at `bank`, looked at without following links: its non-empty regular
/// memory files (the only ones omp reads), and the entries that are left out with a reason
/// (`memory/<name>: <reason>`). A symlinked bank folder is not followed: all three names are
/// reported.
fn take_memory(bank: &Path) -> (MemoryBank, Vec<FileRef>, Vec<String>) {
    match fs::symlink_metadata(bank) {
        Ok(m) if m.file_type().is_symlink() => {
            let skipped = MEMORY_FILES
                .iter()
                .map(|n| format!("memory/{n}: the memory folder is a symbolic link, not backed up"))
                .collect();
            return (MemoryBank::Missing, Vec::new(), skipped);
        }
        Ok(m) if m.is_dir() => {}
        Ok(_) => return (MemoryBank::Missing, Vec::new(), Vec::new()),
        Err(e) => {
            if e.kind() != io::ErrorKind::NotFound {
                log::warn!("cannot read {}: {e}", bank.display());
            }
            return (MemoryBank::Missing, Vec::new(), Vec::new());
        }
    }
    let (mut files, mut skipped) = (Vec::new(), Vec::new());
    for name in MEMORY_FILES {
        let path = bank.join(name);
        match fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                skipped.push(format!("memory/{name}: symbolic link, not backed up"));
            }
            Ok(m) if m.is_file() => {
                // An empty file is "no memory for this category", nothing to keep.
                match file_ref(&path, name.to_owned()) {
                    Some(f) if f.len > 0 => files.push(f),
                    Some(_) => {}
                    None => skipped.push(format!("memory/{name}: cannot be read")),
                }
            }
            Ok(_) => skipped.push(format!("memory/{name}: not a regular file")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => skipped.push(format!("memory/{name}: cannot be read ({e})")),
        }
    }
    let memory = if files.is_empty() {
        MemoryBank::Missing
    } else {
        MemoryBank::Present {
            files: files.len() as u64,
            bytes: files.iter().map(|f| f.len).sum(),
        }
    };
    (memory, files, skipped)
}

/// Regular files below `dir` in name order. `skip_vcs`: leave out `node_modules` and `.git`.
/// A failing top-level directory is an error when `top`; deeper failures are skipped. Everything
/// that is left out for a reason the user may care about (not `node_modules`/`.git`) is
/// described in `skipped` as `<path>: <reason>`.
fn walk(
    dir: &Path,
    rel: &str,
    depth: usize,
    skip_vcs: bool,
    top: bool,
    out: &mut Vec<FileRef>,
    skipped: &mut Vec<String>,
) -> io::Result<()> {
    let shown = |name: &str| {
        if rel.is_empty() {
            name.to_owned()
        } else {
            format!("{rel}/{name}")
        }
    };
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if top => return Err(e),
        Err(e) => {
            skipped.push(format!("{rel}: cannot read folder ({e})"));
            return Ok(());
        }
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            skipped.push(format!(
                "{}: file name is not valid UTF-8",
                shown(&entry.file_name().to_string_lossy())
            ));
            continue;
        };
        if name.contains('\\') {
            skipped.push(format!("{}: backslash in the file name", shown(&name)));
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            skipped.push(format!("{}: cannot be inspected", shown(&name)));
            continue;
        };
        let child = shown(&name);
        if kind.is_dir() {
            if skip_vcs && matches!(name.as_str(), "node_modules" | ".git") {
                continue;
            }
            if depth + 1 < MAX_DEPTH {
                walk(
                    &entry.path(),
                    &child,
                    depth + 1,
                    skip_vcs,
                    false,
                    out,
                    skipped,
                )?;
            } else {
                skipped.push(format!("{child}: nested too deeply"));
            }
        } else if kind.is_file() {
            match file_ref(&entry.path(), child.clone()) {
                Some(f) => out.push(f),
                None => skipped.push(format!("{child}: cannot be read")),
            }
        } else if kind.is_symlink() {
            skipped.push(format!("{child}: symbolic link"));
        } else {
            skipped.push(format!("{child}: not a regular file"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    const BOTH: Parts = Parts {
        omp: true,
        sessions: true,
        memory: false,
    };

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn session(agent: &Path, id: &str, cwd: &Path) {
        let path = agent
            .join("sessions/enc")
            .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
        write(
            &path,
            &format!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":{}}}\n\
                 {{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n",
                serde_json::to_string(&cwd.display().to_string()).unwrap()
            ),
        );
        write(&path.with_extension("").join("sub.jsonl"), "{}\n");
    }

    #[test]
    fn omp_files_skip_vcs_symlinks_and_too_deep_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let root = SessionsRoot::at(tmp.path().join("agent/sessions"));
        write(&proj.join(".omp/AGENTS.md"), "abcd");
        write(&proj.join(".omp/rules/r.md"), "ab");
        write(&proj.join(".omp/node_modules/x/y.js"), "dep");
        write(&proj.join(".omp/skills/.git/HEAD"), "ref");
        #[cfg(unix)]
        {
            symlink(proj.join(".omp/AGENTS.md"), proj.join(".omp/link.md")).unwrap();
            symlink(&proj, proj.join(".omp/dirlink")).unwrap();
        }
        // 15 directories + the file = 16 components (kept); one more is too deep.
        let nested = |n: usize| {
            let mut p = proj.join(".omp");
            for i in 0..n {
                p.push(format!("d{i}"));
            }
            p.join("f.txt")
        };
        write(&nested(15), "x");
        write(&nested(16).with_file_name("deep.txt"), "x");

        let snap = Snapshot::take(&root, &proj, BOTH, &BTreeSet::new(), None).unwrap();
        let rels: Vec<&str> = snap.omp_files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels.len(), 3, "{rels:?}");
        assert_eq!(
            &rels[..2],
            [
                "AGENTS.md",
                "d0/d1/d2/d3/d4/d5/d6/d7/d8/d9/d10/d11/d12/d13/d14/f.txt"
            ]
        );
        assert_eq!(rels[2], "rules/r.md");
        assert_eq!(
            snap.omp,
            OmpDir::Present {
                files: 3,
                bytes: 4 + 1 + 2
            }
        );
        assert!(snap.omp_included() && !snap.sessions_included());
    }

    #[test]
    fn measure_counts_this_projects_sessions_and_knows_the_home_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let (proj, other) = (tmp.path().join("proj"), tmp.path().join("other"));
        let agent = tmp.path().join("agent");
        let root = SessionsRoot::at(agent.join("sessions"));
        fs::create_dir_all(&proj).unwrap();
        session(&agent, "s1", &proj);
        session(&agent, "s2", &proj);
        session(&agent, "s3", &other);

        let snap = Snapshot::take(&root, &proj, BOTH, &BTreeSet::new(), None).unwrap();
        assert_eq!(snap.omp, OmpDir::Missing);
        let sizes = snap.sizes();
        assert_eq!(sizes.sessions, 2);
        let on_disk: u64 = ["s1", "s2"]
            .iter()
            .map(|id| {
                let f = agent
                    .join("sessions/enc")
                    .join(format!("2026-01-01T00-00-00-000Z_{id}.jsonl"));
                fs::metadata(&f).unwrap().len()
                    + fs::metadata(f.with_extension("").join("sub.jsonl"))
                        .unwrap()
                        .len()
            })
            .sum();
        assert_eq!(sizes.session_bytes, on_disk);
        assert!(snap.sessions_included() && !snap.omp_included());

        // A `.omp` that is a file is not a project directory.
        write(&proj.join(".omp"), "not a dir");
        assert_eq!(
            Snapshot::take(&root, &proj, BOTH, &BTreeSet::new(), None)
                .unwrap()
                .omp,
            OmpDir::Missing
        );
        fs::remove_file(proj.join(".omp")).unwrap();

        // The home folder's `.omp` is omp's global directory: never offered.
        write(&proj.join(".omp/AGENTS.md"), "x");
        let global = Snapshot::take(&root, &proj, BOTH, &BTreeSet::new(), Some(&proj)).unwrap();
        assert_eq!(global.omp, OmpDir::Global);
        assert!(global.omp_files.is_empty() && !global.omp_included());
        let elsewhere = Snapshot::take(&root, &proj, BOTH, &BTreeSet::new(), Some(&other)).unwrap();
        assert!(matches!(elsewhere.omp, OmpDir::Present { files: 1, .. }));
    }

    #[test]
    fn fingerprint_follows_parts_and_metadata_only() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let root = SessionsRoot::at(tmp.path().join("agent/sessions"));
        write(&proj.join(".omp/AGENTS.md"), "one");
        let fp = |parts| {
            Snapshot::take(&root, &proj, parts, &BTreeSet::new(), None)
                .unwrap()
                .fingerprint()
                .to_owned()
        };
        let omp_only = Parts {
            omp: true,
            sessions: false,
            memory: false,
        };
        let first = fp(omp_only);
        assert_eq!(fp(omp_only), first);
        assert_ne!(fp(BOTH), first, "the part flags are part of the identity");
        write(&proj.join(".omp/AGENTS.md"), "longer");
        assert_ne!(fp(omp_only), first);
    }

    fn stamp(path: &Path) {
        // A multiple of 100 ns: every platform's file times can hold it exactly.
        let t = UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123_456_700);
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(t)
            .unwrap();
    }

    /// Every install stores the fingerprint of its last backup to decide "unchanged". A row
    /// without Memory must hash exactly what it hashed before the Memory part existed, or all
    /// of them would re-upload once. The expected values are sha256 over the pre-Memory format
    /// (header line, then `omp` / `session` / `artifact` lines with name, length, mtime ns),
    /// computed independently of this code and checked against the old code's output.
    #[test]
    fn fingerprints_of_rows_without_memory_are_byte_identical_to_before() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let agent = tmp.path().join("agent");
        let root = SessionsRoot::at(agent.join("sessions"));
        for (rel, text) in [(".omp/AGENTS.md", "rules"), (".omp/rules/r.md", "r")] {
            write(&proj.join(rel), text);
            stamp(&proj.join(rel));
        }
        // The jsonl is padded to 600 bytes so its length does not depend on the temp path.
        let jsonl = agent
            .join("sessions/enc")
            .join("2026-01-01T00-00-00-000Z_s1.jsonl");
        let mut text = format!(
            "{{\"type\":\"session\",\"id\":\"s1\",\"cwd\":{}}}\n\
             {{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n{{}}",
            serde_json::to_string(&proj.display().to_string()).unwrap()
        );
        assert!(text.len() < 599, "temp path too long for the fixture");
        text.push_str(&" ".repeat(599 - text.len()));
        text.push('\n');
        write(&jsonl, &text);
        stamp(&jsonl);
        let art = jsonl.with_extension("").join("a.txt");
        write(&art, "art");
        stamp(&art);

        let fp = |omp, sessions| {
            let parts = Parts {
                omp,
                sessions,
                memory: false,
            };
            Snapshot::take(&root, &proj, parts, &BTreeSet::new(), None)
                .unwrap()
                .fingerprint()
                .to_owned()
        };
        assert_eq!(
            fp(true, false),
            "f2a6f85b635c6b9bbfea9b96687070d1379654c1a1d09bb247d2279753490129"
        );
        assert_eq!(
            fp(false, true),
            "5d4f1eca073b6d263d2b2f731f0b4d8a0410209c2ca1a6601037e2f95222e4fd"
        );
        assert_eq!(
            fp(true, true),
            "580dc83fbabe17ec4046b27f298ad63eaf5a18c224abffd52e9e9cd7544cb327"
        );

        // With Memory selected the fingerprint is a different one, and it follows the memory
        // files (metadata only) but not memory files that are not selected.
        let bank = sharpshooter::bank_dir(&root, &proj);
        let with = |memory| {
            let parts = Parts {
                omp: true,
                sessions: true,
                memory,
            };
            Snapshot::take(&root, &proj, parts, &BTreeSet::new(), None)
                .unwrap()
                .fingerprint()
                .to_owned()
        };
        let off = with(false);
        assert_eq!(off, fp(true, true));
        write(&bank.join("style.md"), "one");
        assert_eq!(with(false), off, "memory files do not matter while off");
        let on = with(true);
        assert_ne!(on, off);
        write(&bank.join("style.md"), "longer");
        assert_ne!(with(true), on);
    }

    #[test]
    fn memory_files_are_the_non_empty_regular_ones_and_links_are_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        fs::create_dir_all(&proj).unwrap();
        let root = SessionsRoot::at(tmp.path().join("agent/sessions"));
        let bank = sharpshooter::bank_dir(&root, &proj);
        let memory =
            |parts: Parts| Snapshot::take(&root, &proj, parts, &BTreeSet::new(), None).unwrap();
        let only = Parts {
            omp: false,
            sessions: false,
            memory: true,
        };

        // No bank at all.
        let snap = memory(only);
        assert_eq!(snap.memory, MemoryBank::Missing);
        assert_eq!(snap.memory_dir, bank);
        assert!(snap.is_empty() && snap.memory_skipped.is_empty());

        // Empty files are "no memory for that category"; queue/state files are not memory.
        write(&bank.join("architecture.md"), "");
        write(&bank.join("product.md"), "12345");
        write(&bank.join("state.json"), "{}");
        write(&bank.join("queue/s/1.json"), "{}");
        let snap = memory(only);
        assert_eq!(snap.memory, MemoryBank::Present { files: 1, bytes: 5 });
        let rels: Vec<&str> = snap.memory_files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["product.md"]);
        assert!(snap.memory_included() && !snap.is_empty());
        // Not selected → sized (for the Sync page) but not part of the backup.
        let unselected = memory(Parts::default());
        assert_eq!(unselected.memory, snap.memory);
        assert!(!unselected.memory_included() && unselected.is_empty());
        assert_eq!(unselected.sizes().memory_dir, bank);

        #[cfg(unix)]
        {
            // A symlinked memory file is listed, not followed.
            let outside = tmp.path().join("outside.md");
            fs::write(&outside, "secret").unwrap();
            symlink(&outside, bank.join("style.md")).unwrap();
            let snap = memory(only);
            assert_eq!(snap.memory_files.len(), 1);
            assert_eq!(
                snap.memory_skipped,
                ["memory/style.md: symbolic link, not backed up"]
            );

            // A symlinked bank folder is not followed at all.
            let real = tmp.path().join("real-bank");
            write(&real.join("product.md"), "12345");
            fs::remove_dir_all(&bank).unwrap();
            symlink(&real, &bank).unwrap();
            let snap = memory(only);
            assert!(snap.memory_files.is_empty() && snap.is_empty());
            assert_eq!(snap.memory, MemoryBank::Missing);
            assert_eq!(snap.memory_skipped.len(), 3);
            assert!(snap.memory_skipped[0].starts_with("memory/architecture.md: "));
        }
    }
}
