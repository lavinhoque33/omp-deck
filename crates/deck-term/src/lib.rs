//! deck-term: VT emulation (alacritty_terminal) + egui terminal widget, fed by a local PTY
//! (Unix) or a remote session host.
//!
//! Layering: `core` (Term, listener), `out` (byte path), `pty`/`remote` (the two child links),
//! `scan` and `input` (pure scanners/encoders) have no egui widget types; `render` and `widget`
//! adapt them to egui.

mod clipboard;
mod core;
mod fonts;
mod graphics;
pub mod input;
mod links;
mod out;
#[cfg(unix)]
mod pty;
mod remote;
mod render;
mod scan;
mod theme;
mod widget;

pub use crate::clipboard::Clipboard;
#[cfg(unix)]
pub use crate::core::SpawnSpec;
pub use crate::core::{ScreenText, TermOptions, Terminal, TerminalEvents};
pub use crate::fonts::{FontReport, install_fonts};
pub use crate::out::wait_detached;
pub use crate::remote::{Remote, RemoteEvent, RemoteRx, RemoteTx};
pub use crate::theme::TermTheme;
pub use crate::widget::{TerminalResponse, TerminalView};
