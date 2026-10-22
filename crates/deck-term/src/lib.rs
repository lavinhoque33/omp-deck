//! deck-term: PTY + VT emulation (alacritty_terminal) + egui terminal widget.
//!
//! Layering: `core` (PTY, Term, listener) and `input` (pure encoders) have no egui widget
//! types; `render` and `widget` adapt them to egui.

mod clipboard;
mod core;
mod fonts;
pub mod input;
mod pty;
mod render;
mod theme;
mod widget;

pub use crate::clipboard::Clipboard;
pub use crate::core::{
    ScreenText, SpawnSpec, TermOptions, Terminal, TerminalEvents, wait_detached,
};
pub use crate::fonts::{FontReport, install_fonts};
pub use crate::theme::TermTheme;
pub use crate::widget::{TerminalResponse, TerminalView};
