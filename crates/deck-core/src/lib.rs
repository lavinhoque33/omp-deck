//! GUI-free core of OmpDeck: omp session store scanning, session backends (tmux / OmpDeck host),
//! app config, and the per-OS glue in [`platform`].

pub mod agents;
pub mod config;
pub mod ctl;
pub mod gdrive;
pub mod mux;
pub mod omp;
pub mod platform;
pub mod sessions;
pub mod status;
pub mod tmux;
pub mod tree;
pub mod usage;
pub mod worktree;
