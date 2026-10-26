//! Per-OS session endpoints: Unix domain sockets or Windows named pipes. Both expose the same
//! `Endpoint` / `Listener` / `Stream` / `connect` / `list_names` surface.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::list_names;
#[cfg(unix)]
pub use unix::{Endpoint, Listener, Stream, connect, prepare};
#[cfg(windows)]
pub(crate) use windows::list_names;
#[cfg(windows)]
pub use windows::{Endpoint, Listener, Stream, connect, user_key, user_sid};
