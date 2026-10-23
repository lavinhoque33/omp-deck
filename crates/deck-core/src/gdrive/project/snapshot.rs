//! What a project consists of, found with metadata only: the `.omp` files, the sessions of the
//! folder with their artifacts, and the fingerprint over their (path, length, mtime).

use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use super::{OmpDir, Parts, ProjectSizes, same_path};
use crate::gdrive::archive::hex;
use crate::sessions::{self, SessionsRoot};

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
    pub jsonl: FileRef,
    pub artifacts: Vec<FileRef>,
}

pub(super) struct Snapshot {
    parts: Parts,
    pub omp: OmpDir,
    pub omp_files: Vec<FileRef>,
    pub sessions: Vec<SessionSnap>,
    /// `<agent>/blobs`.
    pub blob_dir: PathBuf,
    /// `.omp` entries left out (`<path>: <reason>`).
    pub skipped: Vec<String>,
    /// Session artifacts left out (`session “<title>”: <reason>`).
    pub session_notes: Vec<String>,
    fingerprint: String,
}

impl Snapshot {
    /// Looks at the project without reading any file content. `home` is `$HOME`: a project
    /// that is the home folder has no project `.omp` (it is omp's global directory).
    pub(super) fn take(
        root: &SessionsRoot,
        project: &Path,
        parts: Parts,
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
                Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => OmpDir::Missing,
                Err(e) => return Err(e),
            }
        };
        let mut session_notes = Vec::new();
        let sessions = if parts.sessions {
            project_sessions(root, project, &mut session_notes)
        } else {
            Vec::new()
        };
        let mut snap = Snapshot {
            parts,
            omp,
            omp_files,
            sessions,
            blob_dir: root.agent_dir().join("blobs"),
            skipped,
            session_notes,
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

    /// Nothing of the requested parts exists.
    pub(super) fn is_empty(&self) -> bool {
        !self.omp_included() && !self.sessions_included()
    }

    pub(super) fn sizes(&self) -> ProjectSizes {
        ProjectSizes {
            omp: self.omp,
            sessions: self.sessions.len() as u64,
            session_bytes: self
                .sessions
                .iter()
                .map(|s| s.jsonl.len + s.artifacts.iter().map(|a| a.len).sum::<u64>())
                .sum(),
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
    notes: &mut Vec<String>,
) -> Vec<SessionSnap> {
    let mut out = Vec::new();
    let mut ids = HashSet::new();
    for group in sessions::scan(root) {
        if group.cwd != project {
            continue;
        }
        for e in group.sessions {
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
        mode: meta.permissions().mode() & 0o777,
    })
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
    use std::os::unix::fs::symlink;

    const BOTH: Parts = Parts {
        omp: true,
        sessions: true,
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
                "{{\"type\":\"session\",\"id\":\"{id}\",\"cwd\":\"{}\"}}\n\
                 {{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"hello\"}}}}\n",
                cwd.display()
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
        symlink(proj.join(".omp/AGENTS.md"), proj.join(".omp/link.md")).unwrap();
        symlink(&proj, proj.join(".omp/dirlink")).unwrap();
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

        let snap = Snapshot::take(&root, &proj, BOTH, None).unwrap();
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

        let snap = Snapshot::take(&root, &proj, BOTH, None).unwrap();
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
            Snapshot::take(&root, &proj, BOTH, None).unwrap().omp,
            OmpDir::Missing
        );
        fs::remove_file(proj.join(".omp")).unwrap();

        // The home folder's `.omp` is omp's global directory: never offered.
        write(&proj.join(".omp/AGENTS.md"), "x");
        let global = Snapshot::take(&root, &proj, BOTH, Some(&proj)).unwrap();
        assert_eq!(global.omp, OmpDir::Global);
        assert!(global.omp_files.is_empty() && !global.omp_included());
        let elsewhere = Snapshot::take(&root, &proj, BOTH, Some(&other)).unwrap();
        assert!(matches!(elsewhere.omp, OmpDir::Present { files: 1, .. }));
    }

    #[test]
    fn fingerprint_follows_parts_and_metadata_only() {
        let tmp = tempfile::tempdir().unwrap();
        let proj = tmp.path().join("proj");
        let root = SessionsRoot::at(tmp.path().join("agent/sessions"));
        write(&proj.join(".omp/AGENTS.md"), "one");
        let fp = |parts| {
            Snapshot::take(&root, &proj, parts, None)
                .unwrap()
                .fingerprint()
                .to_owned()
        };
        let omp_only = Parts {
            omp: true,
            sessions: false,
        };
        let first = fp(omp_only);
        assert_eq!(fp(omp_only), first);
        assert_ne!(fp(BOTH), first, "the part flags are part of the identity");
        write(&proj.join(".omp/AGENTS.md"), "longer");
        assert_ne!(fp(omp_only), first);
    }
}
