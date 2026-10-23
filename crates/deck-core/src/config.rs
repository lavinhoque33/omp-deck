//! Persistent app configuration (`$XDG_CONFIG_HOME/omp-deck/state.json`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::mux::BackendChoice;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config I/O error at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("config serialization error: {0}")]
    Json(#[from] serde_json::Error),
}

const MAX_RECENT_FOLDERS: usize = 10;

/// `omp-deck` under the platform config directory (`dirs::config_dir()`: `$XDG_CONFIG_HOME` or
/// `~/.config`, `%APPDATA%`). Without any config directory the path is relative to the cwd.
pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("omp-deck")
}

fn state_path() -> PathBuf {
    config_dir().join("state.json")
}

/// Parses the stored JSON, migrating the retired `drive_auto_backup` bool: `false` without a
/// `drive_backup_interval` becomes `Off`; `true` or absent keeps the default. The old key is dropped.
fn parse_config(bytes: &[u8]) -> Result<AppConfig, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes)?;
    if let Some(map) = value.as_object_mut() {
        let legacy = map.remove("drive_auto_backup");
        if legacy == Some(serde_json::Value::Bool(false))
            && !map.contains_key("drive_backup_interval")
        {
            map.insert("drive_backup_interval".into(), "Off".into());
        }
    }
    serde_json::from_value(value)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    pub window: Option<WindowGeometry>,
    pub sidebar_width: f32,
    pub font_size: f32,
    pub theme: Theme,
    pub open_tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
    pub style: UiStyle,
    pub density: Density,
    /// false → OmpDeck draws its own title bar; true → the window manager's frame.
    pub native_decorations: bool,
    /// Run `omp update --check` at startup and every 6 h.
    pub check_updates: bool,
    /// `ModelInfo.selector`s, insertion order.
    pub favorite_models: Vec<String>,
    /// `None` = "All providers".
    pub model_provider_filter: Option<String>,
    /// Last sidebar scope (`None` = global view).
    pub folder_scope: Option<PathBuf>,
    /// Most-recent first, deduped, at most 10.
    pub recent_folders: Vec<PathBuf>,
    /// Per-folder open tabs (folder mode launch/restore).
    pub folder_tabs: BTreeMap<PathBuf, FolderTabs>,
    /// Back up the omp config to Google Drive automatically when it changed (while signed in).
    pub drive_backup_interval: SyncInterval,
    /// Project folders backed up to Google Drive (Settings → Sync), in table order.
    pub project_sync: Vec<SyncRow>,
    /// How often the rows are backed up automatically.
    pub project_sync_interval: SyncInterval,
    /// Which session backend new tabs use (live sessions keep theirs until closed).
    pub session_backend: BackendChoice,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            window: None,
            sidebar_width: 300.0,
            font_size: 14.0,
            theme: Theme::Dark,
            open_tabs: Vec::new(),
            active_tab: None,
            style: UiStyle::default(),
            density: Density::default(),
            native_decorations: false,
            check_updates: true,
            favorite_models: Vec::new(),
            model_provider_filter: None,
            folder_scope: None,
            recent_folders: Vec::new(),
            folder_tabs: BTreeMap::new(),
            drive_backup_interval: SyncInterval::Every10Minutes,
            project_sync: Vec::new(),
            project_sync_interval: SyncInterval::default(),
            session_backend: BackendChoice::default(),
        }
    }
}

/// One project folder of the Sync page: which parts of it are kept on Drive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRow {
    pub path: PathBuf,
    /// The project's `<folder>/.omp` directory.
    pub omp: bool,
    /// The project's omp sessions.
    pub sessions: bool,
    /// Session ids opted out of the backup; every other session (including ones created later) is
    /// backed up.
    #[serde(default)]
    pub excluded_sessions: BTreeSet<String>,
}

/// How often the Sync rows are backed up automatically.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncInterval {
    Off,
    Every10Minutes,
    Every30Minutes,
    Hourly,
    #[default]
    Every6Hours,
    Daily,
}

impl SyncInterval {
    pub const ALL: [SyncInterval; 6] = [
        SyncInterval::Off,
        SyncInterval::Every10Minutes,
        SyncInterval::Every30Minutes,
        SyncInterval::Hourly,
        SyncInterval::Every6Hours,
        SyncInterval::Daily,
    ];

    /// `None` = automatic backup is off.
    pub fn period(self) -> Option<Duration> {
        let minutes = match self {
            SyncInterval::Off => return None,
            SyncInterval::Every10Minutes => 10,
            SyncInterval::Every30Minutes => 30,
            SyncInterval::Hourly => 60,
            SyncInterval::Every6Hours => 6 * 60,
            SyncInterval::Daily => 24 * 60,
        };
        Some(Duration::from_secs(minutes * 60))
    }

    pub fn label(self) -> &'static str {
        match self {
            SyncInterval::Off => "Off",
            SyncInterval::Every10Minutes => "Every 10 minutes",
            SyncInterval::Every30Minutes => "Every 30 minutes",
            SyncInterval::Hourly => "Every hour",
            SyncInterval::Every6Hours => "Every 6 hours",
            SyncInterval::Daily => "Every day",
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowGeometry {
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub width: f32,
    pub height: f32,
    pub maximized: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Dark,
    Light,
}

/// Visual style axis (second to light/dark `Theme`).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum UiStyle {
    #[default]
    Neutral,
    Neon,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Density {
    #[default]
    Compact,
    Comfortable,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TabState {
    pub tmux_name: String,
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,
    pub title: String,
}

/// Open tabs remembered per folder (folder-mode launch/restore).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FolderTabs {
    pub open_tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
}

impl AppConfig {
    pub fn load() -> AppConfig {
        Self::load_from(&state_path())
    }

    /// Missing file → default silently; unreadable or corrupt → default, logged.
    pub fn load_from(path: &Path) -> AppConfig {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    log::warn!("cannot read config {}: {e}", path.display());
                }
                return AppConfig::default();
            }
        };
        match parse_config(&bytes) {
            Ok(cfg) => cfg,
            Err(e) => {
                log::warn!("corrupt config {}: {e}; using defaults", path.display());
                AppConfig::default()
            }
        }
    }

    pub fn save(&self) -> Result<(), ConfigError> {
        self.save_to(&state_path())
    }

    /// Atomic: write a sibling temp file, then rename over the target.
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError> {
        let io_err = |source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        };
        let json = serde_json::to_vec_pretty(self)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            fs::create_dir_all(dir).map_err(io_err)?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        fs::write(&tmp, json).map_err(io_err)?;
        fs::rename(&tmp, path).map_err(|e| {
            let _ = fs::remove_file(&tmp);
            io_err(e)
        })
    }

    /// Toggles `selector` in `favorite_models`; returns the new state (true = favorite).
    pub fn toggle_favorite(&mut self, selector: &str) -> bool {
        if let Some(i) = self.favorite_models.iter().position(|s| s == selector) {
            self.favorite_models.remove(i);
            false
        } else {
            self.favorite_models.push(selector.to_owned());
            true
        }
    }

    /// Moves `dir` to the front of `recent_folders`, deduped, capped at 10.
    pub fn push_recent_folder(&mut self, dir: PathBuf) {
        self.recent_folders.retain(|p| *p != dir);
        self.recent_folders.insert(0, dir);
        self.recent_folders.truncate(MAX_RECENT_FOLDERS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AppConfig {
        AppConfig {
            window: Some(WindowGeometry {
                x: Some(10.0),
                y: None,
                width: 1200.0,
                height: 800.0,
                maximized: true,
            }),
            sidebar_width: 321.0,
            font_size: 16.5,
            theme: Theme::Light,
            open_tabs: vec![TabState {
                tmux_name: "omp-abc".into(),
                cwd: "/home/u/x".into(),
                session_path: Some("/s/a.jsonl".into()),
                title: "T".into(),
            }],
            active_tab: Some(0),
            style: UiStyle::Neon,
            native_decorations: true,
            check_updates: false,
            ..AppConfig::default()
        }
    }

    #[test]
    fn old_state_without_wave2_keys_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        fs::write(&path, r#"{"sidebar_width": 250.0, "font_size": 12.0}"#).unwrap();
        let cfg = AppConfig::load_from(&path);
        assert_eq!(cfg.sidebar_width, 250.0);
        assert!(!cfg.native_decorations);
        assert!(cfg.check_updates);
    }

    #[test]
    fn roundtrip_creates_dirs_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/state.json");
        sample().save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path), sample());
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("state.json")]);
    }

    #[test]
    fn missing_or_corrupt_gives_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            AppConfig::load_from(&dir.path().join("nope.json")),
            AppConfig::default()
        );
        let bad = dir.path().join("bad.json");
        fs::write(&bad, b"{ not json").unwrap();
        assert_eq!(AppConfig::load_from(&bad), AppConfig::default());
    }

    #[test]
    fn partial_json_fills_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        fs::write(
            &p,
            br#"{"font_size": 20.0, "theme": "Light", "future_field": 1}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(&p);
        assert_eq!(cfg.font_size, 20.0);
        assert_eq!(cfg.theme, Theme::Light);
        assert_eq!(cfg.sidebar_width, 300.0);
        assert!(cfg.open_tabs.is_empty());
    }

    #[test]
    fn old_state_without_wave4_keys_gets_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        fs::write(&p, br#"{"open_tabs": [], "check_updates": false}"#).unwrap();
        let cfg = AppConfig::load_from(&p);
        assert!(!cfg.check_updates);
        assert!(cfg.favorite_models.is_empty());
        assert_eq!(cfg.model_provider_filter, None);
        assert_eq!(cfg.folder_scope, None);
        assert!(cfg.recent_folders.is_empty());
        assert!(cfg.folder_tabs.is_empty());
    }

    #[test]
    fn old_state_gets_neutral_compact_and_ignores_collapsed_groups() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        fs::write(
            &p,
            br#"{"theme": "Light", "collapsed_groups": ["/home/u/y"], "font_size": 15.0}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(&p);
        assert_eq!(cfg.style, UiStyle::Neutral);
        assert_eq!(cfg.density, Density::Compact);
        assert_eq!(cfg.theme, Theme::Light);
        assert_eq!(cfg.font_size, 15.0);
    }

    #[test]
    fn style_and_density_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("s.json");
        let cfg = AppConfig {
            style: UiStyle::Neon,
            density: Density::Comfortable,
            ..AppConfig::default()
        };
        cfg.save_to(&p).unwrap();
        assert_eq!(AppConfig::load_from(&p), cfg);
    }

    #[test]
    fn toggle_favorite_twice_restores() {
        let mut cfg = AppConfig::default();
        assert!(cfg.toggle_favorite("a/x"));
        assert!(cfg.toggle_favorite("b/y"));
        assert_eq!(cfg.favorite_models, vec!["a/x", "b/y"]);
        assert!(!cfg.toggle_favorite("a/x"));
        assert_eq!(cfg.favorite_models, vec!["b/y"]);
        assert!(cfg.toggle_favorite("a/x"));
        assert_eq!(cfg.favorite_models, vec!["b/y", "a/x"]);
    }

    #[test]
    fn recent_folders_dedupe_front_cap() {
        let mut cfg = AppConfig::default();
        for i in 0..12 {
            cfg.push_recent_folder(PathBuf::from(format!("/d/{i}")));
        }
        assert_eq!(cfg.recent_folders.len(), 10);
        assert_eq!(cfg.recent_folders[0], PathBuf::from("/d/11"));
        assert_eq!(cfg.recent_folders[9], PathBuf::from("/d/2"));
        cfg.push_recent_folder(PathBuf::from("/d/5"));
        assert_eq!(cfg.recent_folders.len(), 10);
        assert_eq!(cfg.recent_folders[0], PathBuf::from("/d/5"));
        assert_eq!(
            cfg.recent_folders
                .iter()
                .filter(|p| p.as_path() == Path::new("/d/5"))
                .count(),
            1
        );
    }

    #[test]
    fn folder_tabs_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut cfg = sample();
        cfg.folder_tabs.insert(
            "/home/u/x".into(),
            FolderTabs {
                open_tabs: cfg.open_tabs.clone(),
                active_tab: Some(0),
            },
        );
        cfg.folder_scope = Some("/home/u/x".into());
        cfg.model_provider_filter = Some("openai".into());
        cfg.save_to(&path).unwrap();
        assert_eq!(AppConfig::load_from(&path), cfg);
    }

    #[test]
    fn old_state_without_sync_keys_gets_defaults_and_sync_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(&p, br#"{"open_tabs": []}"#).unwrap();
        let cfg = AppConfig::load_from(&p);
        assert!(cfg.project_sync.is_empty());
        assert_eq!(cfg.project_sync_interval, SyncInterval::Every6Hours);

        let mut cfg = sample();
        cfg.project_sync = vec![SyncRow {
            path: "/home/u/x".into(),
            omp: true,
            sessions: false,
            excluded_sessions: ["a".to_owned(), "b".to_owned()].into(),
        }];
        cfg.project_sync_interval = SyncInterval::Daily;
        cfg.save_to(&p).unwrap();
        assert_eq!(AppConfig::load_from(&p), cfg);
    }

    #[test]
    fn sync_row_without_exclusions_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(
            &p,
            br#"{"project_sync": [{"path": "/x", "omp": true, "sessions": true}]}"#,
        )
        .unwrap();
        let cfg = AppConfig::load_from(&p);
        assert!(cfg.project_sync[0].excluded_sessions.is_empty());
    }

    #[test]
    fn legacy_drive_auto_backup_migrates_and_is_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        let load = |json: &str| {
            fs::write(&p, json).unwrap();
            AppConfig::load_from(&p)
        };
        assert_eq!(
            load(r#"{"drive_auto_backup": false}"#).drive_backup_interval,
            SyncInterval::Off
        );
        assert_eq!(
            load(r#"{"drive_auto_backup": true}"#).drive_backup_interval,
            SyncInterval::Every10Minutes
        );
        assert_eq!(
            load("{}").drive_backup_interval,
            SyncInterval::Every10Minutes
        );
        // An explicit new key wins over the legacy one.
        assert_eq!(
            load(r#"{"drive_auto_backup": false, "drive_backup_interval": "Hourly"}"#)
                .drive_backup_interval,
            SyncInterval::Hourly
        );
        let cfg = load(r#"{"drive_auto_backup": false}"#);
        cfg.save_to(&p).unwrap();
        let written = fs::read_to_string(&p).unwrap();
        assert!(!written.contains("drive_auto_backup"));
        assert_eq!(
            AppConfig::load_from(&p).drive_backup_interval,
            SyncInterval::Off
        );
    }

    #[test]
    fn session_backend_defaults_for_old_state_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("state.json");
        fs::write(&p, r#"{"sidebar_width": 250.0}"#).unwrap();
        assert_eq!(
            AppConfig::load_from(&p).session_backend,
            BackendChoice::Auto
        );
        for choice in [
            BackendChoice::Auto,
            BackendChoice::Tmux,
            BackendChoice::Host,
        ] {
            let cfg = AppConfig {
                session_backend: choice,
                ..AppConfig::default()
            };
            cfg.save_to(&p).unwrap();
            assert_eq!(AppConfig::load_from(&p).session_backend, choice);
        }
        fs::write(&p, r#"{"session_backend": "Host"}"#).unwrap();
        assert_eq!(
            AppConfig::load_from(&p).session_backend,
            BackendChoice::Host
        );
    }

    #[test]
    fn sync_intervals_have_periods_and_labels() {
        let periods: Vec<_> = SyncInterval::ALL.iter().map(|i| i.period()).collect();
        assert_eq!(
            periods,
            [
                None,
                Some(Duration::from_secs(600)),
                Some(Duration::from_secs(1800)),
                Some(Duration::from_secs(3600)),
                Some(Duration::from_secs(6 * 3600)),
                Some(Duration::from_secs(24 * 3600)),
            ]
        );
        let labels: std::collections::BTreeSet<_> =
            SyncInterval::ALL.iter().map(|i| i.label()).collect();
        assert_eq!(labels.len(), 6);
    }
}
