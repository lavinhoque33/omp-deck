//! OmpDeck's application service: everything that runs sessions, independent of any UI.
//!
//! - [`controller`]: the one owner of a live session's terminal and its command injection
//!   (targeted, deduplicated submissions with delivery/runtime outcomes; read-only views).
//! - [`inject`], [`composer`], [`session_actions`], [`model_switch`]: the injection protocol and
//!   the commands it carries.
//! - [`lifecycle`], [`backend`]: operations outside omp's composer (stop/kill, move, delete,
//!   export, …) and the session backends.
//! - [`host_remote`]: host sessions as terminal remotes, and read-only host observers.
//! - [`notifications`]: the canonical notification pipeline.
//! - [`settings`]: the single, cross-process-safe writer of the settings file.
//! - [`gateway`]: the per-user headless owner of all of the above, its private IPC and client.
//! - [`status`], [`statusfmt`]: session status and its text formatting.
//!
//! No GUI toolkit here: the desktop app is one client, headless drivers are others.

pub mod backend;
pub mod composer;
pub mod controller;
pub mod gateway;
pub mod host_remote;
pub mod inject;
pub mod lifecycle;
pub mod model_switch;
pub mod notifications;
pub mod session_actions;
pub mod settings;
pub mod status;
pub mod statusfmt;
