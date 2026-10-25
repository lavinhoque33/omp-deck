//! GUI-free core of OmpDeck: omp session store scanning, session backends (tmux / OmpDeck host),
//! app config, and the per-OS glue in [`platform`].

pub mod agents;
pub mod changes;
pub mod config;
pub mod ctl;
pub mod fleet;
pub mod gdrive;
pub(crate) mod git;
pub mod inventory;
pub mod mux;
pub mod omp;
pub mod platform;
pub mod selfupdate;
pub mod sessions;
pub mod sharpshooter;
pub mod status;
pub mod tmux;
pub mod tree;
pub mod usage;
pub mod worktree;
