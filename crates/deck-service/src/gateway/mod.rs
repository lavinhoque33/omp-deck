//! The per-user gateway: one headless owner of every session controller, settings writer,
//! notification pipeline and long job, reached by native clients over private local IPC.
//!
//! - [`protocol`]: versioned handshake, typed requests/results/events and binary terminal frames.
//! - [`server`]: the singleton owner (`omp-deck-gateway serve`): listener, connections, lifetime.
//! - [`client`]: [`client::GatewayClient`], the asynchronous native client, and per-session
//!   frontend links for `deck_term::Terminal::frontend`.
//! - [`jobs`]: the typed executors of every operation family over the real core/service code.
//! - [`journal`]: the private durable record of accepted mutations (never replayed).
//! - [`lifetime`]: modes, OS supervision (`omp-deck-gateway supervise`) and the maintenance barrier.
//! - [`running`]: which backend sessions are alive and what they hold.
//! - [`desktop`]: the OS notification adapter (delivery and activation, never classification).
//!
//! Identity: one owner per user and base host namespace ([`GatewayId`]). The profile (settings
//! file, sessions root) is part of the handshake, not of the identity: a client with another
//! profile is refused instead of starting a second owner that would displace the same backend
//! sessions.

pub mod activation;
pub mod client;
pub mod desktop;
mod engine;
pub mod jobs;
pub mod journal;
pub mod lifetime;
pub mod protocol;
pub mod running;
pub mod server;
mod wire;

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Name of the gateway executable (without the Windows `.exe`).
pub const GATEWAY_BIN: &str = "omp-deck-gateway";
/// Environment override of the gateway executable (tests, sandboxes).
pub const GATEWAY_EXE_ENV: &str = "OMP_DECK_GATEWAY_EXE";
/// Version of this gateway build (handshakes compare it).
pub const GATEWAY_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Endpoint name of the owner inside the gateway namespace.
pub const OWNER_ENDPOINT: &str = "owner";
/// Endpoint name of the supervisor (its singleton lock) inside the gateway namespace.
pub const SUPERVISOR_ENDPOINT: &str = "supervisor";

/// The singleton identity: `gw-<24 hex of sha256(base host namespace)>`. Bounded (fits the host
/// namespace rule) and disjoint from every session-host namespace, so host enumeration never
/// probes the gateway and the gateway never shadows a session namespace.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GatewayId(String);

impl GatewayId {
    pub fn for_host_namespace(host_ns: &str) -> GatewayId {
        let digest = Sha256::digest(host_ns.as_bytes());
        let hex: String = digest[..12].iter().map(|b| format!("{b:02x}")).collect();
        GatewayId(format!("gw-{hex}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The gateway's endpoint namespace.
    pub fn namespace(&self) -> deck_host::Result<deck_host::Namespace> {
        deck_host::Namespace::new(&self.0)
    }
}

/// Which settings and sessions an owner serves. Compared in every handshake: an owner never
/// silently serves another profile, and a client never falls back to a default root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Base host namespace (the backend sessions this owner controls).
    pub host_namespace: String,
    /// The settings file (`state.json`).
    pub settings: PathBuf,
    /// omp's sessions directory.
    pub sessions_root: PathBuf,
}

impl Profile {
    /// The default profile of this user: the production host namespace (or the test override),
    /// the default settings file and omp's resolved sessions root.
    pub fn default_for_user() -> Profile {
        Profile {
            host_namespace: deck_core::mux::host_namespace(),
            settings: deck_core::config::config_dir().join("state.json"),
            sessions_root: deck_core::sessions::SessionsRoot::resolve().path,
        }
    }

    pub fn id(&self) -> GatewayId {
        GatewayId::for_host_namespace(&self.host_namespace)
    }

    /// The application root the settings file lives in (OmpDeck's config directory).
    pub fn app_root(&self) -> PathBuf {
        self.settings
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
    }

    /// The gateway's private persistent directory for this identity:
    /// `<app root>/gateway/<gateway id>`.
    pub fn state_dir(&self) -> PathBuf {
        gateway_root(&self.app_root()).join(self.id().as_str())
    }

    /// `true` when both name the same files (paths compared as given and canonicalized; a file
    /// that does not exist yet, like a fresh `state.json`, is compared through its directory).
    pub fn same_as(&self, other: &Profile) -> bool {
        fn resolved(p: &Path) -> Option<PathBuf> {
            std::fs::canonicalize(p).ok().or_else(|| {
                let name = p.file_name()?;
                let parent = p.parent().filter(|d| !d.as_os_str().is_empty())?;
                Some(std::fs::canonicalize(parent).ok()?.join(name))
            })
        }
        fn same(a: &Path, b: &Path) -> bool {
            a == b || matches!((resolved(a), resolved(b)), (Some(x), Some(y)) if x == y)
        }
        self.host_namespace == other.host_namespace
            && same(&self.settings, &other.settings)
            && same(&self.sessions_root, &other.sessions_root)
    }
}

/// `<app root>/gateway`: the parent of every identity's state directory and of the bundle-wide
/// maintenance marker.
pub fn gateway_root(app_root: &Path) -> PathBuf {
    app_root.join("gateway")
}

/// Creates `dir` (and parents) owner-only.
pub(crate) fn private_dir(dir: &Path) -> io::Result<()> {
    deck_core::platform::create_private_dir_all(dir)
}

/// Writes `bytes` to `path` owner-only and atomically.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        private_dir(parent)?;
    }
    deck_core::platform::write_private(path, bytes)
}

/// Environment variables a fresh-login owner needs to find the same omp, roots and desktop
/// session the GUI saw. Values only, never credentials: tokens/API keys are not listed and stay
/// in omp's own credential store.
pub const STARTUP_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TMP",
    "TEMP",
    "SHELL",
    "PI_CONFIG_DIR",
    "PI_CODING_AGENT_DIR",
    "PI_CONFIG_FILES",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "XDG_DATA_DIRS",
    "XDG_CONFIG_DIRS",
    "XDG_CURRENT_DESKTOP",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "SYSTEMROOT",
    "PATHEXT",
    "COMSPEC",
    "OMP_DECK_TMUX_SOCKET",
    "OMP_DECK_HOST_NS",
    "OMP_DECK_HOST_EXE",
    GATEWAY_EXE_ENV,
];

/// A program plus its leading arguments. AppImage: the stable image file and `--gateway` (its
/// `AppRun` dispatches to the bundled gateway inside a mount of its own), never a path inside
/// another process's ephemeral mount.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Program {
    /// This program with `more` arguments after its own.
    pub fn command(&self, more: impl IntoIterator<Item = String>) -> (PathBuf, Vec<String>) {
        let mut args = self.args.clone();
        args.extend(more);
        (self.program.clone(), args)
    }
}

/// The running AppImage file, when this process runs from one (`$APPIMAGE` set by its runtime
/// and the executable inside `$APPDIR`).
fn appimage() -> Option<PathBuf> {
    let image = std::env::var_os("APPIMAGE").filter(|v| !v.is_empty())?;
    let appdir = std::env::var_os("APPDIR").filter(|v| !v.is_empty())?;
    let me = std::env::current_exe().ok()?;
    me.starts_with(&appdir).then(|| PathBuf::from(image))
}

/// How to run the gateway from this process: `$OMP_DECK_GATEWAY_EXE`; inside an AppImage
/// `$APPIMAGE --gateway`; else this executable when it is the gateway, else its sibling.
pub fn gateway_program() -> Option<Program> {
    if let Some(p) = std::env::var_os(GATEWAY_EXE_ENV).filter(|v| !v.is_empty()) {
        return Some(Program {
            program: PathBuf::from(p),
            args: Vec::new(),
        });
    }
    if let Some(image) = appimage() {
        return Some(Program {
            program: image,
            args: vec!["--gateway".into()],
        });
    }
    let me = std::env::current_exe().ok()?;
    let stem = me.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let program = if stem == GATEWAY_BIN {
        me
    } else {
        sibling(&me, GATEWAY_BIN)?
    };
    Some(Program {
        program,
        args: Vec::new(),
    })
}

/// How to open the native app from this process (notification activation without a window):
/// inside an AppImage the image itself, else the `omp-deck` next to this executable.
pub fn gui_program() -> Option<Program> {
    if let Some(image) = appimage() {
        return Some(Program {
            program: image,
            args: Vec::new(),
        });
    }
    let me = std::env::current_exe().ok()?;
    Some(Program {
        program: sibling(&me, "omp-deck")?,
        args: Vec::new(),
    })
}

/// Private startup context of an identity, written to its state directory (`startup.json`,
/// 0600): what a supervisor-started owner needs to serve the same profile the GUI enabled. The
/// owner a GUI launches directly inherits the GUI's own environment instead.
///
/// Bundle paths are stored only as stable launchers ([`Program`]); the host executable is
/// derived at owner startup from that owner's own executable unless it was overridden.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartupContext {
    pub profile: Profile,
    /// Allow-listed, non-secret environment ([`STARTUP_ENV`]).
    pub env: Vec<(String, String)>,
    /// Runs the gateway (`serve`, `supervise`).
    pub gateway: Program,
    /// Opens the native app.
    pub gui: Option<Program>,
    /// `$OMP_DECK_HOST_EXE` when it was set explicitly (kept across install repairs); `None` =
    /// the host next to the running gateway.
    pub host_override: Option<PathBuf>,
    /// omp as detected when the context was written; used only if omp is no longer found on the
    /// recorded `PATH`.
    pub omp: Option<PathBuf>,
}

impl StartupContext {
    pub const FILE: &'static str = "startup.json";

    /// The context of this process for `profile`.
    pub fn capture(profile: Profile) -> io::Result<StartupContext> {
        let env = STARTUP_ENV
            .iter()
            .filter(|k| **k != deck_host::HOST_EXE_ENV && **k != GATEWAY_EXE_ENV)
            .filter_map(|k| {
                let v = std::env::var(k).ok()?;
                (!v.is_empty()).then(|| ((*k).to_owned(), v))
            })
            .collect();
        let gateway = gateway_program().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "omp-deck-gateway executable not found",
            )
        })?;
        Ok(StartupContext {
            profile,
            env,
            gateway,
            gui: gui_program(),
            host_override: std::env::var_os(deck_host::HOST_EXE_ENV)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from),
            omp: deck_core::omp::Omp::detect().map(|o| o.program().to_path_buf()),
        })
    }

    pub fn path_in(state_dir: &Path) -> PathBuf {
        state_dir.join(Self::FILE)
    }

    pub fn load(path: &Path) -> io::Result<StartupContext> {
        let bytes = std::fs::read(path)?;
        serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(io::Error::other)?;
        write_private(path, &bytes)
    }

    /// Applies the context to this process before any thread starts (supervisor-started
    /// process): the recorded variables are set, nothing else is removed. An explicit host
    /// override is restored; otherwise the host is the one next to this executable.
    ///
    /// # Safety
    /// Mutates the process environment: call only while the process is single-threaded.
    pub unsafe fn apply_env(&self) {
        for (k, v) in &self.env {
            // SAFETY: the caller guarantees no other thread runs.
            unsafe { std::env::set_var(k, v) };
        }
        match &self.host_override {
            // SAFETY: as above.
            Some(host) => unsafe { std::env::set_var(deck_host::HOST_EXE_ENV, host) },
            // SAFETY: as above.
            None => unsafe { std::env::remove_var(deck_host::HOST_EXE_ENV) },
        }
    }

    /// Re-points the launchers at the installation this process belongs to (after an update or
    /// reinstall), keeping the profile, environment and explicit overrides.
    pub fn repair_for_install(&mut self, gateway: Program, gui: Option<Program>) {
        self.gateway = gateway;
        self.gui = gui;
        if let Some(omp) = deck_core::omp::Omp::detect() {
            self.omp = Some(omp.program().to_path_buf());
        }
    }

    /// The program and arguments an OS registration runs for this context.
    pub fn supervise_command(&self, context_file: &Path) -> (PathBuf, Vec<String>) {
        self.gateway.command([
            "supervise".to_owned(),
            "--startup".to_owned(),
            context_file.to_string_lossy().into_owned(),
        ])
    }

    /// The program and arguments that start the owner for this context.
    pub fn serve_command(&self, context_file: &Path) -> (PathBuf, Vec<String>) {
        self.gateway.command([
            "serve".to_owned(),
            "--startup".to_owned(),
            context_file.to_string_lossy().into_owned(),
        ])
    }
}

/// `<dir of exe>/<name>[.exe]` when it exists.
fn sibling(exe: &Path, name: &str) -> Option<PathBuf> {
    let p = exe
        .parent()?
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    p.is_file().then_some(p)
}

/// The omp to run: found on the current `PATH`, else the one a startup context recorded.
pub fn resolve_omp(recorded: Option<&Path>) -> Option<deck_core::omp::Omp> {
    deck_core::omp::Omp::detect().or_else(|| {
        recorded
            .filter(|p| deck_core::platform::is_executable(p))
            .map(deck_core::omp::Omp::at)
    })
}

/// The terminal colours for a UI theme and style: the one mapping the gateway (which answers
/// colour queries) and native renderers share.
pub fn terminal_theme(
    theme: deck_core::config::Theme,
    style: deck_core::config::UiStyle,
) -> deck_term::TermTheme {
    use deck_core::config::{Theme, UiStyle};
    use deck_term::TermTheme;
    match (style, theme) {
        (UiStyle::Neutral, Theme::Dark) => TermTheme::NeutralDark,
        (UiStyle::Neutral, Theme::Light) => TermTheme::NeutralLight,
        (UiStyle::Neon, Theme::Dark) => TermTheme::Dark,
        (UiStyle::Neon, Theme::Light) => TermTheme::Light,
        // Gallery styles are dark-only; the terminal follows the style's canvas colours.
        (style, _) => {
            let p = deck_core::theme::palette(style, true);
            let rgb = |c: deck_core::theme::Rgba| [c.r, c.g, c.b];
            let sel = deck_core::theme::mix(p.bg, p.accent, 0.35);
            TermTheme::Custom(deck_term::TermColors {
                foreground: rgb(p.text),
                background: rgb(p.bg),
                cursor: rgb(p.text_strong),
                selection_bg: rgb(sel),
            })
        }
    }
}

/// The bundle-wide maintenance marker: `<app root>/gateway/maintenance`. Persistent (not runtime
/// state), so a failed install or a reboot mid-update never auto-starts a mixed bundle.
pub fn maintenance_marker(app_root: &Path) -> PathBuf {
    gateway_root(app_root).join("maintenance")
}

/// Why the barrier is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MaintenancePurpose {
    Update,
    Uninstall,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintenanceMarker {
    pub purpose: MaintenancePurpose,
    pub since_unix_ms: u64,
    pub by_version: String,
}

/// The barrier, if up. No GUI, supervisor or client autostart may launch an owner meanwhile.
pub fn maintenance_active(app_root: &Path) -> Option<MaintenanceMarker> {
    let bytes = std::fs::read(maintenance_marker(app_root)).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(m) => Some(m),
        // An unreadable marker still blocks: it was written by someone.
        Err(_) => Some(MaintenanceMarker {
            purpose: MaintenancePurpose::Update,
            since_unix_ms: 0,
            by_version: String::new(),
        }),
    }
}

pub(crate) fn unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Every identity state directory under `app_root` with a startup context (bundle-wide drain).
pub fn known_contexts(app_root: &Path) -> Vec<(PathBuf, StartupContext)> {
    let Ok(dir) = std::fs::read_dir(gateway_root(app_root)) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, StartupContext)> = dir
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("gw-"))
        .filter_map(|e| {
            let file = StartupContext::path_in(&e.path());
            StartupContext::load(&file).ok().map(|c| (file, c))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Arguments that start the owner for `profile` without a startup file (the GUI-launched owner
/// inherits the GUI's environment).
pub(crate) fn serve_args(profile: &Profile) -> Vec<String> {
    vec![
        "serve".into(),
        "--ns".into(),
        profile.host_namespace.clone(),
        "--settings".into(),
        profile.settings.to_string_lossy().into_owned(),
        "--sessions-root".into(),
        profile.sessions_root.to_string_lossy().into_owned(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_bounded_and_depends_only_on_the_host_namespace() {
        let a = GatewayId::for_host_namespace("omp-deck");
        assert_eq!(a.as_str().len(), 27);
        assert!(deck_host::valid_namespace(a.as_str()));
        let long = "omp-deck-test-0123456789012345678901234";
        assert!(deck_host::valid_namespace(
            GatewayId::for_host_namespace(long).as_str()
        ));
        assert_ne!(a, GatewayId::for_host_namespace("omp-deck-test-1"));
        let p = |settings: &str| Profile {
            host_namespace: "omp-deck".into(),
            settings: PathBuf::from(settings),
            sessions_root: PathBuf::from("/r"),
        };
        // Another profile on the same backend namespace is the same singleton: it is refused
        // at the handshake rather than becoming a second owner.
        assert_eq!(p("/a/state.json").id(), p("/b/state.json").id());
        assert!(!p("/a/state.json").same_as(&p("/b/state.json")));
        assert_eq!(
            p("/a/state.json").state_dir(),
            PathBuf::from("/a/gateway").join(a.as_str())
        );
    }

    #[test]
    fn a_settings_file_that_does_not_exist_yet_matches_through_its_directory() {
        let dir = tempfile::tempdir().expect("temp dir");
        let sessions = dir.path().join("sessions");
        std::fs::create_dir_all(&sessions).expect("sessions");
        std::fs::create_dir_all(dir.path().join("cfg")).expect("cfg");
        let at = |settings: PathBuf| Profile {
            host_namespace: "omp-deck".into(),
            settings,
            sessions_root: sessions.clone(),
        };
        let plain = at(dir.path().join("cfg/state.json"));
        // The same not-yet-written file, spelled through `.` and `..`.
        let spelled = at(dir.path().join("cfg/../cfg/./state.json"));
        assert!(plain.same_as(&spelled));
        assert!(!plain.same_as(&at(dir.path().join("other/state.json"))));
        assert!(!plain.same_as(&at(dir.path().join("cfg/settings.json"))));
    }

    #[test]
    fn startup_context_round_trips_and_install_repair_keeps_overrides_and_profile() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut ctx = StartupContext {
            profile: Profile {
                host_namespace: "omp-deck-test-ctx".into(),
                settings: dir.path().join("state.json"),
                sessions_root: dir.path().join("sessions"),
            },
            env: vec![("PATH".into(), "/bin".into())],
            gateway: Program {
                program: PathBuf::from("/old/omp-deck.AppImage"),
                args: vec!["--gateway".into()],
            },
            gui: None,
            host_override: Some(PathBuf::from("/custom/host")),
            omp: None,
        };
        let file = dir.path().join("startup.json");
        ctx.save(&file).expect("save");
        assert_eq!(StartupContext::load(&file).expect("load"), ctx);
        let (prog, args) = ctx.supervise_command(&file);
        assert_eq!(prog, PathBuf::from("/old/omp-deck.AppImage"));
        assert_eq!(args[..2], ["--gateway".to_owned(), "supervise".to_owned()]);
        let profile = ctx.profile.clone();
        ctx.repair_for_install(
            Program {
                program: PathBuf::from("/new/omp-deck-gateway"),
                args: Vec::new(),
            },
            None,
        );
        assert_eq!(ctx.gateway.program, PathBuf::from("/new/omp-deck-gateway"));
        assert_eq!(ctx.host_override, Some(PathBuf::from("/custom/host")));
        assert_eq!(ctx.profile, profile);
        assert!(
            STARTUP_ENV
                .iter()
                .all(|k| !k.contains("TOKEN") && !k.contains("KEY"))
        );
    }
}
