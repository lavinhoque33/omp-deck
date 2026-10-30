//! One folder, several spellings: a symlinked path and its target; on Windows also 8.3 short
//! names (`C:\Users\RUNNER~1`), letter case, either separator and the `\\?\` prefix. What a
//! device sees compares paths obtained independently (the inventory's session headers, omp's
//! own reported folder and file) through [`Canon`]: the same spelling matches at once; other
//! spellings match by their canonical forms, which a worker resolves (file system I/O never
//! runs on the engine thread) and the engine caches.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use deck_core::platform::strip_verbatim;
use deck_core::sessions::abbreviate_home;

/// Canonical forms kept at most (the cache starts over beyond it).
const CAPACITY: usize = 4096;

/// Canonical forms of the paths the device projections compared.
#[derive(Default)]
pub(super) struct Canon {
    /// Path → its canonical form (`None`: it could not be resolved, e.g. missing).
    known: HashMap<PathBuf, Option<PathBuf>>,
    /// Paths asked for and not resolved yet (projections run on `&self`).
    wanted: Mutex<BTreeSet<PathBuf>>,
    /// A worker resolves the last batch.
    busy: bool,
}

impl Canon {
    /// `p`'s canonical form once it was resolved; a miss is noted for the next batch.
    pub(super) fn get(&self, p: &Path) -> Option<&Path> {
        match self.known.get(p) {
            Some(c) => c.as_deref(),
            None => {
                self.wanted
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(p.to_path_buf());
                None
            }
        }
    }

    /// The paths to resolve next, unless a batch is still being resolved.
    pub(super) fn take_wanted(&mut self) -> Option<Vec<PathBuf>> {
        if self.busy {
            return None;
        }
        let wanted = std::mem::take(
            self.wanted
                .get_mut()
                .unwrap_or_else(PoisonError::into_inner),
        );
        if wanted.is_empty() {
            return None;
        }
        self.busy = true;
        Some(wanted.into_iter().collect())
    }

    /// A worker resolved a batch ([`resolve`]).
    pub(super) fn resolved(&mut self, found: Vec<(PathBuf, Option<PathBuf>)>) {
        self.busy = false;
        if self.known.len() + found.len() > CAPACITY {
            self.known.clear();
        }
        self.known.extend(found);
    }

    /// `a` and `b` are spelled alike, or resolve to the same folder or file.
    pub(super) fn same(&self, a: &Path, b: &Path) -> bool {
        same_spelling(a, b)
            || matches!((self.get(a), self.get(b)), (Some(x), Some(y)) if same_spelling(x, y))
    }

    /// `p` for a device: below the home folder as `~…` whichever way either is spelled.
    pub(super) fn display(&self, p: &Path) -> String {
        let Some(home) = dirs::home_dir().filter(|h| !h.as_os_str().is_empty()) else {
            return abbreviate_home(p);
        };
        let rest = rest_below(p, &home).or_else(|| rest_below(self.get(p)?, self.get(&home)?));
        match rest {
            Some(rest) => abbreviate_home(&home.join(rest)),
            None => abbreviate_home(p),
        }
    }
}

/// `p`'s canonical form (worker side: file system I/O).
pub(super) fn resolve(p: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(p).ok().map(strip_verbatim)
}

/// One component as compared: case-folded on Windows, whose file systems are case-insensitive
/// by default.
fn fold(c: Component<'_>) -> String {
    let s = c.as_os_str().to_string_lossy();
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.into_owned()
    }
}

/// The same spelling (component-wise; on Windows also case, separators, `\\?\` prefix).
pub(super) fn same_spelling(a: &Path, b: &Path) -> bool {
    if !cfg!(windows) {
        return a == b;
    }
    let (a, b) = (
        strip_verbatim(a.to_path_buf()),
        strip_verbatim(b.to_path_buf()),
    );
    a.components().map(fold).eq(b.components().map(fold))
}

/// `p` relative to `base` when `p` lies below it in the same spelling.
fn rest_below(p: &Path, base: &Path) -> Option<PathBuf> {
    if !cfg!(windows) {
        return p.strip_prefix(base).ok().map(Path::to_path_buf);
    }
    let (p, base) = (
        strip_verbatim(p.to_path_buf()),
        strip_verbatim(base.to_path_buf()),
    );
    let mut rest = p.components();
    for b in base.components() {
        if fold(rest.next()?) != fold(b) {
            return None;
        }
    }
    Some(rest.as_path().to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved(canon: &mut Canon) {
        let wanted = canon.take_wanted().unwrap_or_default();
        canon.resolved(
            wanted
                .into_iter()
                .map(|p| {
                    let c = resolve(&p);
                    (p, c)
                })
                .collect(),
        );
    }

    #[test]
    fn a_symlink_and_its_target_are_one_folder_once_resolved() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("dir");
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        #[cfg(windows)]
        if std::os::windows::fs::symlink_dir(&real, &link).is_err() {
            // Symlinks need a privilege on Windows.
            return;
        }
        let mut canon = Canon::default();
        assert!(
            !canon.same(&link, &real),
            "unresolved: only the spelling counts"
        );
        resolved(&mut canon);
        assert!(canon.same(&link, &real));
        assert!(canon.same(&real, &link));
        assert_eq!(
            canon.take_wanted(),
            None,
            "resolved paths are not asked again"
        );
        assert!(!canon.same(&link, dir.path()));
    }

    #[test]
    fn a_missing_path_is_resolved_once_and_matches_by_spelling_only() {
        let mut canon = Canon::default();
        let gone = Path::new("/no/such/omp-deck-folder");
        assert!(canon.same(gone, gone));
        assert!(!canon.same(gone, Path::new("/no/such/other")));
        resolved(&mut canon);
        assert!(!canon.same(gone, Path::new("/no/such/other")));
        assert!(canon.take_wanted().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_spellings_of_one_folder_match() {
        assert!(same_spelling(
            Path::new(r"C:\Users\Me\Proj"),
            Path::new(r"c:/users/me/proj/")
        ));
        assert!(same_spelling(
            Path::new(r"\\?\C:\Users\Me\Proj"),
            Path::new(r"C:\Users\Me\Proj")
        ));
        assert_eq!(
            rest_below(Path::new(r"c:\users\ME\proj\x"), Path::new(r"C:\Users\me")),
            Some(PathBuf::from(r"proj\x"))
        );
        // An 8.3 short name and the long one resolve alike.
        let dir = tempfile::tempdir().expect("tempdir");
        let long = dir.path().join("a rather long folder name");
        std::fs::create_dir(&long).expect("dir");
        let Some(short) = short_name(&long) else {
            // 8.3 names are off on this volume.
            return;
        };
        if short == long {
            return;
        }
        let mut canon = Canon::default();
        assert!(!canon.same(&short, &long));
        resolved(&mut canon);
        assert!(canon.same(&short, &long), "{short:?} vs {long:?}");
    }

    #[cfg(windows)]
    fn short_name(p: &Path) -> Option<PathBuf> {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetShortPathNameW(long: *const u16, short: *mut u16, len: u32) -> u32;
        }
        let wide: Vec<u16> = p.as_os_str().encode_wide().chain([0]).collect();
        let mut buf = vec![0u16; 1024];
        // SAFETY: `wide` is NUL-terminated; `buf` holds `buf.len()` elements.
        let n = unsafe { GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
        if n == 0 || n as usize >= buf.len() {
            return None;
        }
        buf.truncate(n as usize);
        Some(PathBuf::from(std::ffi::OsString::from_wide(&buf)))
    }
}
