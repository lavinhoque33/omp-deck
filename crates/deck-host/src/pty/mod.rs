//! The session child in a pseudo terminal: Unix `openpty`-style master/slave with the child as
//! session leader, Windows ConPTY with the child in a kill-on-close job object.

#[cfg(unix)]
mod unix;
#[cfg(any(windows, test))]
mod winargs;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::{PtyChild, PtyReader, PtyWriter};
#[cfg(windows)]
pub use windows::{PtyChild, PtyReader, PtyWriter};

use crate::error::Result;
use crate::protocol::SessionSpec;

/// Terminal size in cells (and cell pixels, for `CSI 14 t` / `TIOCGWINSZ` pixel fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

/// A started child: output reader, input writer and control handle.
///
/// - `reader` returns `Ok(0)` once the PTY output ended (Unix: all slave holders exited;
///   Windows: the pseudoconsole was closed).
/// - `child` is shared between the waiter, the resize path and kill.
pub struct Spawned {
    pub reader: PtyReader,
    pub writer: PtyWriter,
    pub child: PtyChild,
}

/// Starts `spec` (program, args, cwd, env changes) in a new pseudo terminal of `size`.
pub fn spawn(spec: &SessionSpec, size: PtySize) -> Result<Spawned> {
    #[cfg(unix)]
    return unix::spawn(spec, size);
    #[cfg(windows)]
    return windows::spawn(spec, size);
}
