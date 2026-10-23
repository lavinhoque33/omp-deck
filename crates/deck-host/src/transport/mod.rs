//! Per-OS session endpoints: Unix domain sockets or Windows named pipes. Both expose the same
//! `Endpoint` / `Listener` / `Stream` / `connect` / `list_names` surface.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::{Endpoint, Listener, Stream, connect, list_names, prepare};
#[cfg(windows)]
pub(crate) use windows::{Endpoint, Listener, Stream, connect, list_names, user_key};
