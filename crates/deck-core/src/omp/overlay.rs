//! The settings overlay of the omp sessions OmpDeck launches: one YAML file, named last in each
//! session's `PI_CONFIG_FILES`. omp reads those files as settings layers after the user's own, so
//! a key set here wins for that process, and polls them (`fs.watchFile`, 500 ms, omp 19.2), so a
//! running session follows a rewrite.
//!
//! It carries what OmpDeck's settings decide for omp: while "Let sessions message each other" is
//! off, `task.crossInstance` (omp ≥ 19's same-folder agent messaging, on by default since 19.2)
//! is off too; while it is on, the file sets nothing and the user's own omp setting applies.
//! omp starts that messaging in interactive sessions only, so headless `--mode rpc` runs need no
//! overlay.

use std::ffi::OsString;
use std::io;
use std::path::Path;

/// The overlay's file name in the gateway's state directory.
pub const FILE_NAME: &str = "omp-overlay.yml";

/// The environment variable omp reads extra settings files from (path-list separated).
pub const CONFIG_FILES_ENV: &str = "PI_CONFIG_FILES";

const MESSAGES_OFF: &str = concat!(
    "# Written by OmpDeck. \"Let sessions message each other\" is off: the omp sessions it\n",
    "# launches stay out of omp's same-folder agent messaging.\n",
    "task:\n",
    "  crossInstance: false\n",
);

const MESSAGES_ON: &str = concat!(
    "# Written by OmpDeck. \"Let sessions message each other\" is on: omp's own\n",
    "# task.crossInstance setting applies.\n",
    "{}\n",
);

/// Writes the overlay for "Let sessions message each other" (`allow_messages`): atomically, and
/// only when the file says something else (an unchanged file never wakes omp's watcher).
pub fn write(path: &Path, allow_messages: bool) -> io::Result<()> {
    let text = if allow_messages {
        MESSAGES_ON
    } else {
        MESSAGES_OFF
    };
    if std::fs::read_to_string(path).is_ok_and(|t| t == text) {
        return Ok(());
    }
    crate::platform::write_private(path, text.as_bytes())
}

/// [`CONFIG_FILES_ENV`] for a session: the files this process inherited, then `overlay` last
/// (the last layer wins). `None` when the paths cannot be joined (a separator inside one).
pub fn config_files(overlay: &Path) -> Option<OsString> {
    let inherited = std::env::var_os(CONFIG_FILES_ENV).unwrap_or_default();
    let files = std::env::split_paths(&inherited)
        .filter(|p| !p.as_os_str().is_empty() && p != overlay)
        .chain(std::iter::once(overlay.to_path_buf()));
    std::env::join_paths(files).ok()
}
