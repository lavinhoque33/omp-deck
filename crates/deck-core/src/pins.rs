//! omp's pinned sessions: `<agent dir>/session-pins.json`, a JSON array of session ids. Pins are
//! keyed by session id (they survive `/move`). OmpDeck only **reads** the file; every write goes
//! through omp ([`crate::omp::Omp::set_pinned`]), which locks and replaces it atomically.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

use crate::sessions::Lineage;

const PINS_FILE: &str = "session-pins.json";
/// A pins file is a few ids; anything larger is not one.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The pinned session ids of `agent_dir`. A missing file, a file that is not a JSON array, an
/// unreadable one and non-string elements all degrade to what omp itself would read (fewer or no
/// pins) with a debug log, never an error: omp does the same for its resume picker.
pub fn read(agent_dir: &Path) -> HashSet<String> {
    let path = agent_dir.join(PINS_FILE);
    let text = match std::fs::metadata(&path) {
        Ok(m) if m.len() > MAX_BYTES => {
            log::debug!("pins: {} is too large, ignored", path.display());
            return HashSet::new();
        }
        Ok(_) => match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                log::debug!("pins: cannot read {}: {e}", path.display());
                return HashSet::new();
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => return HashSet::new(),
        Err(e) => {
            log::debug!("pins: cannot stat {}: {e}", path.display());
            return HashSet::new();
        }
    };
    match serde_json::from_str::<Vec<serde_json::Value>>(&text) {
        Ok(items) => items
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        Err(e) => {
            log::debug!("pins: {} is not a JSON array: {e}", path.display());
            HashSet::new()
        }
    }
}

/// The bookmark move `(from id, to id)` when a tab follows its omp from the session file `from`
/// to `to`. omp moves a conversation onto a new file naming the old one as its `parentSession`
/// (a rewind, a turn's Edit or Retry, an entry fork, a `/btw` branch, `/fork`, a continuation),
/// and a rewind's Undo resumes the old file again; so the pin moves when one file names the other
/// as its parent, `from` is pinned and `to` is not. Unrelated files, the same session (a `/move`)
/// and both or neither pinned move nothing.
pub fn followed_bookmark(
    from: (&Path, &Lineage),
    to: (&Path, &Lineage),
    pinned: &HashSet<String>,
) -> Option<(String, String)> {
    let ((from_path, from), (to_path, to)) = (from, to);
    let related =
        names_as_parent(to, from_path, &from.id) || names_as_parent(from, to_path, &to.id);
    (from.id != to.id && related && pinned.contains(&from.id) && !pinned.contains(&to.id))
        .then(|| (from.id.clone(), to.id.clone()))
}

/// `child`'s `parentSession` is the session `id` stored at `path`. omp writes the parent's id or
/// its path; a path is compared by file name, which carries the id, so another spelling of the
/// directory or a parent `/move`d since still matches.
fn names_as_parent(child: &Lineage, path: &Path, id: &str) -> bool {
    child.parent.as_deref().is_some_and(|parent| {
        parent == id
            || Path::new(parent)
                .file_name()
                .is_some_and(|name| path.file_name() == Some(name))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(content: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(c) = content {
            std::fs::write(dir.path().join(PINS_FILE), c).unwrap();
        }
        dir
    }

    #[test]
    fn missing_file_is_empty() {
        assert!(read(dir_with(None).path()).is_empty());
    }

    #[test]
    fn reads_string_ids_and_skips_other_elements() {
        // omp writes tab-indented JSON; non-string elements are dropped like omp's filter.
        let dir = dir_with(Some("[\n\t\"a1\",\n\t7,\n\tnull,\n\t\"b2\"\n]"));
        let pins = read(dir.path());
        assert_eq!(pins, HashSet::from(["a1".to_owned(), "b2".to_owned()]));
    }

    #[test]
    fn corrupt_or_non_array_file_is_empty() {
        assert!(read(dir_with(Some("{\"a\":1}")).path()).is_empty());
        assert!(read(dir_with(Some("[\"a\"")).path()).is_empty());
        assert!(read(dir_with(Some("")).path()).is_empty());
    }

    fn lineage(id: &str, parent: Option<&str>) -> Lineage {
        Lineage {
            id: id.to_owned(),
            parent: parent.map(str::to_owned),
        }
    }

    fn pinned(id: &str) -> HashSet<String> {
        HashSet::from([id.to_owned()])
    }

    const OLD: &str = "/s/-p/2026-11-27T02-42-03-180Z_a1.jsonl";
    const NEW: &str = "/s/-p/2026-11-27T02-42-03-924Z_b2.jsonl";

    #[test]
    fn a_bookmark_follows_a_rewind_and_its_undo() {
        let (old_path, new_path) = (Path::new(OLD), Path::new(NEW));
        let old = lineage("a1", None);
        // A rewind names the old file by path (here spelled through another directory), `/fork`
        // and a continuation by id.
        let rewound = lineage(
            "b2",
            Some("/home/u/.omp/agent/sessions/-p/2026-11-27T02-42-03-180Z_a1.jsonl"),
        );
        for new in [&rewound, &lineage("b2", Some("a1"))] {
            assert_eq!(
                followed_bookmark((old_path, &old), (new_path, new), &pinned("a1")),
                Some(("a1".to_owned(), "b2".to_owned()))
            );
        }
        // Undo resumes the file the rewind left: the bookmark comes back.
        assert_eq!(
            followed_bookmark((new_path, &rewound), (old_path, &old), &pinned("b2")),
            Some(("b2".to_owned(), "a1".to_owned()))
        );
    }

    #[test]
    fn only_a_lone_bookmark_moves_between_parent_and_child() {
        let (old_path, new_path) = (Path::new(OLD), Path::new(NEW));
        let old = lineage("a1", None);
        let child = lineage("b2", Some(OLD));
        // `/new` or `/resume` of another session: no lineage.
        let unrelated = lineage("b2", Some("/s/-p/2026-11-01T00-00-00-000Z_c3.jsonl"));
        assert_eq!(
            followed_bookmark((old_path, &old), (new_path, &unrelated), &pinned("a1")),
            None
        );
        assert_eq!(
            followed_bookmark(
                (old_path, &old),
                (new_path, &lineage("b2", None)),
                &pinned("a1")
            ),
            None
        );
        // `/move`: the same session in another place.
        assert_eq!(
            followed_bookmark(
                (old_path, &old),
                (new_path, &lineage("a1", Some(OLD))),
                &pinned("a1")
            ),
            None
        );
        // Both bookmarked, or the file left is not.
        let both = HashSet::from(["a1".to_owned(), "b2".to_owned()]);
        assert_eq!(
            followed_bookmark((old_path, &old), (new_path, &child), &both),
            None
        );
        assert_eq!(
            followed_bookmark((old_path, &old), (new_path, &child), &pinned("b2")),
            None
        );
        assert_eq!(
            followed_bookmark((old_path, &old), (new_path, &child), &HashSet::new()),
            None
        );
    }
}
