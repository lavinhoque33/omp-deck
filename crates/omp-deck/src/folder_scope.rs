//! Per-folder mode helpers: turning typed/picked/CLI paths into a scope and testing sessions
//! against it. Pure except [`resolve`], which touches the filesystem (call it off the UI thread).

use std::path::{Component, Path, PathBuf};

/// A session belongs to the scope when its cwd is the folder or lies inside it (whole path
/// components, so `/a/bc` is not inside `/a/b`).
pub fn contains(scope: &Path, cwd: &Path) -> bool {
    cwd.starts_with(scope)
}

/// `~` / `~/x` expanded, relative paths joined to `base`, `.`/`..`/trailing slashes removed.
pub fn normalize(text: &str, home: Option<&Path>, base: &Path) -> PathBuf {
    let text = text.trim();
    let expanded = match (text, home) {
        ("~", Some(h)) => h.to_path_buf(),
        (t, Some(h)) if t.starts_with("~/") || (cfg!(windows) && t.starts_with("~\\")) => {
            h.join(&t[2..])
        }
        (t, _) => PathBuf::from(t),
    };
    let absolute = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    let mut out = PathBuf::new();
    for c in absolute.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Normalizes `text` and requires an existing directory; returns its canonical path (symlinks
/// resolved, as omp records session cwds; on Windows without the `\\?\` verbatim prefix, which
/// omp's cwds never carry). Blocking.
pub fn resolve(text: &str, home: Option<&Path>, base: &Path) -> Result<PathBuf, String> {
    if text.trim().is_empty() {
        return Err("Enter a folder path".to_owned());
    }
    let path = normalize(text, home, base);
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => Ok(deck_core::platform::strip_verbatim(
            std::fs::canonicalize(&path).unwrap_or(path),
        )),
        Ok(_) => Err(format!("Not a folder: {}", path.display())),
        Err(_) => Err(format!("Folder not found: {}", path.display())),
    }
}

/// Last path component (`/` for the root).
pub fn folder_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// `~/dev` for `/home/u/dev/proj`: the abbreviated parent, empty when there is none.
pub fn parent_display(path: &Path) -> String {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(deck_core::sessions::abbreviate_home)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_matches_the_folder_and_descendants_by_whole_components() {
        let scope = Path::new("/w/proj");
        assert!(contains(scope, Path::new("/w/proj")));
        assert!(contains(scope, Path::new("/w/proj/sub/deep")));
        assert!(!contains(scope, Path::new("/w/project")));
        assert!(!contains(scope, Path::new("/w")));
        assert!(!contains(scope, Path::new("/x/w/proj")));
    }

    #[test]
    fn normalize_expands_tilde_and_cleans() {
        let home = Path::new("/home/u");
        let base = Path::new("/cwd");
        assert_eq!(normalize("~", Some(home), base), Path::new("/home/u"));
        assert_eq!(
            normalize("  ~/dev/./x/../proj/ ", Some(home), base),
            Path::new("/home/u/dev/proj")
        );
        assert_eq!(
            normalize("rel/p", Some(home), base),
            Path::new("/cwd/rel/p")
        );
        assert_eq!(normalize("/a//b/", None, base), Path::new("/a/b"));
        assert_eq!(normalize("~/x", None, base), Path::new("/cwd/~/x"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_requires_an_existing_directory_and_follows_symlinks() {
        let dir = tempfile::tempdir().expect("tempdir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "x").expect("write");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let canon = std::fs::canonicalize(&real).expect("canon");

        let base = dir.path();
        assert_eq!(resolve("link/", None, base), Ok(canon.clone()));
        assert_eq!(resolve("real/../real", None, base), Ok(canon));
        assert!(
            resolve(file.to_str().expect("utf8"), None, base)
                .expect_err("file")
                .starts_with("Not a folder")
        );
        assert!(
            resolve("nope", None, base)
                .expect_err("missing")
                .starts_with("Folder not found")
        );
        assert!(resolve("   ", None, base).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn resolve_returns_the_drive_form_omp_records_not_the_verbatim_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let scope = resolve(dir.path().to_str().expect("utf8"), None, dir.path()).expect("resolve");
        let text = scope.to_str().expect("utf8");
        assert!(!text.starts_with(r"\\?\"), "{text}");
        assert!(matches!(
            scope.components().next(),
            Some(Component::Prefix(p)) if matches!(p.kind(), std::path::Prefix::Disk(_))
        ));
        // A session cwd under it, as omp records it (no verbatim prefix).
        assert!(contains(&scope, &scope.join("sub")));
    }

    #[test]
    fn names() {
        assert_eq!(folder_name(Path::new("/w/proj")), "proj");
        assert_eq!(folder_name(Path::new("/")), "/");
        assert_eq!(parent_display(Path::new("/")), "");
    }
}
