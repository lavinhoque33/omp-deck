//! Directory listing for the folder picker of remote clients.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// One subdirectory of a listed folder.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirInfo {
    pub name: String,
    /// `dir` joined with `name`; a symlink keeps its own path (never canonicalized, so no
    /// Windows `\\?\` prefix can appear).
    pub path: PathBuf,
    /// Contains a `.git` entry (directory, or the file of a linked worktree).
    pub repo: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DirListing {
    pub entries: Vec<DirInfo>,
    /// More directories exist than `limit`; `entries` holds the first `limit` in sort order.
    pub truncated: bool,
}

/// The subdirectories of `dir`: symlinks to directories included (broken links and links to
/// files are skipped), dot-entries and files skipped, sorted case-insensitively. An entry that
/// cannot be read is skipped; only `dir` itself failing is an error.
pub fn list_dirs(dir: &Path, limit: usize) -> io::Result<DirListing> {
    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let is_dir = if kind.is_symlink() {
            fs::metadata(entry.path()).is_ok_and(|m| m.is_dir())
        } else {
            kind.is_dir()
        };
        if is_dir {
            names.push(name);
        }
    }
    names.sort_by(|a, b| {
        a.to_lowercase()
            .cmp(&b.to_lowercase())
            .then_with(|| a.cmp(b))
    });
    let truncated = names.len() > limit;
    names.truncate(limit);
    let entries = names
        .into_iter()
        .map(|name| {
            let path = dir.join(&name);
            let repo = fs::symlink_metadata(path.join(".git")).is_ok();
            DirInfo { name, path, repo }
        })
        .collect();
    Ok(DirListing { entries, truncated })
}
