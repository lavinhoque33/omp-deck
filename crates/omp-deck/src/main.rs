//! OmpDeck: desktop wrapper for the omp coding-agent TUI.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod agent_ctl;
mod agent_hub;
mod agent_model;
mod agent_output;
mod agent_popup;
mod app;
mod backend;
mod cli;
mod clipboard_bridge;
mod composer;
mod config_writer;
mod confirm;
mod ctx_tip;
mod drive_view;
mod folder_scope;
mod host_remote;
mod icons;
mod inject;
mod model_order;
mod model_picker;
mod omp_rows;
mod omp_settings;
mod omp_worker;
mod running;
mod scope_bar;
mod session_actions;
mod settings_page;
mod sidebar;
mod statusbar;
mod statusfmt;
mod summary;
mod sync_view;
mod tabs;
mod theme;
mod titlebar;
mod toast;
mod transcript_view;
mod transfer;
mod tree_layout;
mod tree_view;
mod ui_fonts;
mod update;
mod usage_fmt;
mod usage_state;
mod usage_view;
mod widgets;

use anyhow::anyhow;
use deck_core::config::{AppConfig, config_dir};

use crate::app::{App, AppInit};
use crate::backend::Backends;
use crate::cli::Parsed;

const MIN_SIZE: [f32; 2] = [640.0, 400.0];

/// Writes the control extension into `$XDG_DATA_HOME/omp-deck/` (a small file write before the
/// window opens). Failure only costs live control: the hub stays read-only.
fn install_ctl_extension() -> Option<std::path::PathBuf> {
    let Some(data) = dirs::data_dir() else {
        log::warn!("no data directory: live subagent control is unavailable");
        return None;
    };
    match deck_core::ctl::install_extension(&data.join("omp-deck")) {
        Ok(path) => Some(path),
        Err(e) => {
            log::warn!("installing the control extension failed: {e}; live control is unavailable");
            None
        }
    }
}

/// The 256px window icon embedded at build time.
fn window_icon() -> egui::IconData {
    static PNG: &[u8] = include_bytes!("../assets/icon-256.png");
    eframe::icon_data::from_png_bytes(PNG).unwrap_or_default()
}

fn viewport(cfg: &AppConfig) -> egui::ViewportBuilder {
    let mut vb = egui::ViewportBuilder::default()
        .with_title("OmpDeck")
        .with_app_id("omp-deck")
        .with_icon(window_icon())
        .with_inner_size([1280.0, 800.0])
        .with_min_inner_size(MIN_SIZE)
        .with_decorations(cfg.native_decorations);
    if let Some(w) = cfg.window
        && w.width.is_finite()
        && w.height.is_finite()
    {
        vb = vb.with_inner_size([w.width.max(MIN_SIZE[0]), w.height.max(MIN_SIZE[1])]);
        if let (Some(x), Some(y)) = (w.x, w.y)
            && x.is_finite()
            && y.is_finite()
        {
            vb = vb.with_position([x, y]);
        }
        if w.maximized {
            vb = vb.with_maximized(true);
        }
    }
    vb
}

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let cli = match cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Run(cli)) => cli,
        Ok(Parsed::Help) => {
            print!("{}", cli::HELP);
            return Ok(());
        }
        Err(msg) => {
            eprintln!("omp-deck: {msg}\n\n{}", cli::HELP);
            std::process::exit(2);
        }
    };

    let cfg = AppConfig::load();
    let backends = Backends::detect(cfg.session_backend);
    backends.prepare();
    if backends.is_empty() {
        log::warn!("no session backend (tmux / omp-deck-host): running omp directly");
    }
    // Control endpoints of sessions that no longer exist. Before any tab launches, so a fresh
    // launch can't be pruned; the alive sessions of every backend are the keys still in use.
    // An empty answer may be a failed query: never prune on it (same rule as the poller).
    if !backends.is_empty() {
        let alive: std::collections::HashSet<String> = backends
            .all()
            .iter()
            .flat_map(|m| m.alive_sessions())
            .collect();
        if !alive.is_empty() {
            deck_core::ctl::prune_ctl_dirs(&backends.ctl_namespace(), &alive);
        }
    }
    log::debug!("config dir: {}", config_dir().display());

    let options = eframe::NativeOptions {
        viewport: viewport(&cfg),
        clipboard_shortcuts: false,
        // deck-term installs its own filtered system provider (it skips `term-primary`, which must stay primary-only).
        system_font_fallback: false,
        ..Default::default()
    };
    // Blocking filesystem checks are fine here: the window does not exist yet.
    let start_dir = std::env::current_dir().unwrap_or_default();
    let folder = cli.folder.as_ref().map(|f| {
        folder_scope::resolve(
            &f.to_string_lossy(),
            dirs::home_dir().as_deref(),
            &start_dir,
        )
    });
    let init = AppInit {
        cli,
        cfg,
        backends,
        ui_fonts: ui_fonts::UiFonts::load(),
        folder,
        start_dir,
        ctl_ext: install_ctl_extension(),
    };
    let result = eframe::run_native(
        "OmpDeck",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, init)))),
    );
    // Terminal drops hand tmux detaches to background threads; let them finish before exiting.
    deck_term::wait_detached(std::time::Duration::from_secs(2));
    result.map_err(|e| anyhow!("eframe: {e}"))
}
