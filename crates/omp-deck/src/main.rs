//! OmpDeck: desktop wrapper for the omp coding-agent TUI.

mod agent_ctl;
mod agent_hub;
mod agent_model;
mod agent_output;
mod agent_popup;
mod app;
mod cli;
mod clipboard_bridge;
mod composer;
mod config_writer;
mod confirm;
mod ctx_tip;
mod drive_view;
mod folder_scope;
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
use deck_core::tmux::Tmux;

use crate::app::{App, AppInit};
use crate::cli::Parsed;

const MIN_SIZE: [f32; 2] = [640.0, 400.0];

/// `^omp-deck-test-[A-Za-z0-9_-]+$`: never the user's default server, never a path.
fn valid_test_socket(name: &str) -> bool {
    name.strip_prefix("omp-deck-test-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

/// tmux on PATH; `OMP_DECK_TMUX_SOCKET` swaps in a different socket (tests/smoke runs).
fn detect_tmux() -> Option<Tmux> {
    let tmux = Tmux::detect()?;
    match std::env::var("OMP_DECK_TMUX_SOCKET") {
        Ok(sock) if valid_test_socket(&sock) => {
            let conf = std::env::temp_dir().join(format!("{sock}.conf"));
            Some(Tmux::with_socket("tmux", sock, conf))
        }
        Ok(sock) if !sock.is_empty() => {
            log::warn!(
                "ignoring OMP_DECK_TMUX_SOCKET={sock:?}: must match omp-deck-test-[A-Za-z0-9_-]+"
            );
            Some(tmux)
        }
        _ => Some(tmux),
    }
}

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

    let tmux = detect_tmux();
    match &tmux {
        Some(t) => {
            if let Err(e) = t.ensure_config() {
                log::error!("writing tmux config failed: {e}");
            }
        }
        None => log::warn!("tmux not found: running omp directly (no persistence)"),
    }
    // Control sockets of sessions that no longer exist. Before any tab launches, so a fresh
    // launch can't be pruned; needs tmux's session list as the set of keys still alive.
    // An empty answer may be a failed query: never prune on it (same rule as the poller).
    if let Some(t) = &tmux {
        let alive = t.alive_sessions();
        if !alive.is_empty() {
            deck_core::ctl::prune_ctl_dirs(t.socket(), &alive);
        }
    }
    log::debug!("config dir: {}", config_dir().display());
    let cfg = AppConfig::load();

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
        tmux,
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

#[cfg(test)]
mod tests {
    use super::valid_test_socket;

    #[test]
    fn socket_override_validation() {
        assert!(valid_test_socket("omp-deck-test-123"));
        assert!(valid_test_socket("omp-deck-test-a_B-9"));
        for bad in [
            "",
            "default",
            "omp-deck",
            "omp-deck-test-",
            "omp-deck-test-a/b",
            "omp-deck-test-..",
            "omp-deck-test-a\n",
            "/tmp/x",
            "omp-deck-test-é",
        ] {
            assert!(!valid_test_socket(bad), "{bad:?}");
        }
    }
}
