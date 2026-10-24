# OmpDeck cross-crate contracts

Authoritative. Orchestrator-owned; implementers report needed changes instead of editing.
`deck-core` and `deck-term` never depend on each other; `omp-deck` depends on both.
Errors: typed `thiserror` in libs, `anyhow` in the bin. No `unwrap`/`expect` outside tests.

## deck_core (crate `deck-core`, no GUI deps)

### `deck_core::sessions`

```rust
pub struct SessionsRoot { pub path: PathBuf }
impl SessionsRoot {
    /// `$PI_CODING_AGENT_DIR/sessions` if set and non-empty, else `$HOME/$PI_CONFIG_DIR/agent/sessions`
    /// (`PI_CONFIG_DIR` default `.omp`). Never fails; existence checked by `exists()`.
    pub fn resolve() -> SessionsRoot;
    pub fn at(path: impl Into<PathBuf>) -> SessionsRoot;   // --sessions-root override, tests
    pub fn exists(&self) -> bool;                          // is_dir()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub id: String,                  // header "id"
    pub path: PathBuf,               // absolute .jsonl path
    pub cwd: PathBuf,                // header "cwd"
    pub title: Option<String>,       // title line "title" (trimmed, None if empty)
    pub first_prompt: Option<String>,// first user message text, whitespace-collapsed, ≤200 chars
    pub modified: SystemTime,        // file mtime
}
impl SessionEntry {
    /// title, else first_prompt, else "(untitled)".
    pub fn label(&self) -> &str;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectGroup {
    pub cwd: PathBuf,
    pub display: String,             // `~`-abbreviated cwd
    pub sessions: Vec<SessionEntry>, // newest-first by `modified`
}

/// Groups ordered by their newest session, newest first.
/// Missing root → empty Vec. Unreadable/garbage files are skipped (logged at debug).
/// Hides stubs: title None AND first_prompt None.
pub fn scan(root: &SessionsRoot) -> Vec<ProjectGroup>;

/// Incremental scanner: caches parsed headers keyed by (path, len, mtime) so repeat scans
/// only stat files and re-read changed ones. `scan()` above = `Scanner::new(root).scan()`.
pub struct Scanner { .. }
impl Scanner {
    pub fn new(root: SessionsRoot) -> Scanner;
    pub fn root(&self) -> &SessionsRoot;
    pub fn scan(&mut self) -> Vec<ProjectGroup>;
}

/// Background refresher: one thread, rescans every `interval` (≤5 s) or immediately after
/// `refresh_now()`. After each scan that changed the result it stores the snapshot and calls
/// `on_update` (the UI passes a closure calling `egui::Context::request_repaint`).
/// Drop stops and joins the thread (wake via channel/condvar; never sleeps through a stop).
pub struct SessionWatcher { .. }
impl SessionWatcher {
    pub fn spawn(root: SessionsRoot, interval: Duration, on_update: impl Fn() + Send + 'static) -> SessionWatcher;
    pub fn latest(&self) -> Option<Arc<Vec<ProjectGroup>>>;  // None until first scan finishes
    pub fn generation(&self) -> u64;                          // bumps on each changed snapshot
    pub fn refresh_now(&self);
}

pub fn abbreviate_home(path: &Path) -> String;  // "/home/u/x" -> "~/x"; HOME itself -> "~"
```

### `deck_core::tmux`

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec { pub program: String, pub args: Vec<String>, pub cwd: PathBuf, pub env: Vec<(String, String)> }

pub const APP_SOCKET: &str = "omp-deck";

#[derive(Clone, Debug)]
pub struct Tmux { /* program path, socket name, config path */ }
impl Tmux {
    /// tmux on PATH (runs `tmux -V` ok) → Some, socket APP_SOCKET, config `config_dir()/tmux.conf`.
    pub fn detect() -> Option<Tmux>;
    /// Tests: explicit socket (`omp-deck-test-<pid>`) and config path.
    pub fn with_socket(program: impl Into<PathBuf>, socket: impl Into<String>, config: impl Into<PathBuf>) -> Tmux;
    pub fn socket(&self) -> &str;
    /// Writes the generated config (create dirs), and if this socket's server is already running
    /// applies it with `source-file`. Returns the config path.
    pub fn ensure_config(&self) -> io::Result<PathBuf>;
    /// `env -u TMUX <tmux> -L <sock> -f <conf> new-session -A -s <name> -c <cwd'> -- <argv...>`
    /// cwd' = `effective_cwd(cwd)`. env = `terminal_env()`.
    pub fn attach_or_create(&self, name: &str, cwd: &Path, argv: &[String]) -> LaunchSpec;
    /// Names of live sessions on this socket; server not running → empty set (no error).
    pub fn alive_sessions(&self) -> HashSet<String>;
    pub fn has_session(&self, name: &str) -> bool;
    pub fn kill(&self, name: &str) -> Result<(), TmuxError>;
    /// For every live session: the top-level session .jsonl files (direct children of a project
    /// dir under `root`) that any process in its pane process tree holds open
    /// (`#{pane_pid}` → /proc descendants → /proc/<pid>/fd symlinks). omp keeps its current
    /// session file open for writing, so this maps tmux sessions ↔ omp session files even after
    /// omp switches sessions in place (/new, /resume). Linux-only; elsewhere empty map.
    pub fn session_files(&self, root: &Path) -> HashMap<String, Vec<PathBuf>>;
}

pub fn session_name_for(id: &str) -> String;     // "omp-" + id with every char not [A-Za-z0-9_-] → '_'
pub fn new_session_name(now: SystemTime) -> String; // "omp-new-<unix-ms>"
/// Degraded mode (no tmux): run argv directly; same env/cwd rules.
pub fn direct_launch(cwd: &Path, argv: &[String]) -> LaunchSpec;
/// cwd if it is an existing dir, else $HOME (else "/").
pub fn effective_cwd(cwd: &Path) -> PathBuf;
/// [("TERM","xterm-256color"),("COLORTERM","truecolor"),("TERM_PROGRAM","omp-deck"),
///  ("PI_FORCE_IMAGE_PROTOCOL","kitty"),("PI_KITTY_PLACEHOLDERS","1"),("PI_FORCE_HYPERLINKS","1")]
pub fn terminal_env() -> Vec<(String, String)>;
/// The generated tmux.conf text (see docs/notes/tmux.md); ends with `set-environment -g` of the
/// PI_* overrides above.
pub fn config_text() -> String;
```

### `deck_core::config`

```rust
pub fn config_dir() -> PathBuf;   // $XDG_CONFIG_HOME/omp-deck (dirs::config_dir()), fallback ~/.config/omp-deck

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct AppConfig {
    pub window: Option<WindowGeometry>,
    pub sidebar_width: f32,              // default 300.0
    pub font_size: f32,                  // default 14.0 (points)
    pub theme: Theme,                    // default Dark
    pub open_tabs: Vec<TabState>,
    pub active_tab: Option<usize>,
    pub collapsed_groups: Vec<PathBuf>,  // collapsed project cwds
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct WindowGeometry { pub x: Option<f32>, pub y: Option<f32>, pub width: f32, pub height: f32, pub maximized: bool }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Theme { #[default] Dark, Light }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct TabState {
    pub tmux_name: String,            // tmux session name (stable key)
    pub cwd: PathBuf,
    pub session_path: Option<PathBuf>,// resumed .jsonl (None for a fresh `omp`)
    pub title: String,                // last known tab label
}
impl AppConfig {
    pub fn load() -> AppConfig;                         // config_dir()/state.json; missing/corrupt → default (corrupt logged)
    pub fn load_from(path: &Path) -> AppConfig;
    pub fn save(&self) -> Result<(), ConfigError>;      // atomic: write tmp + rename
    pub fn save_to(&self, path: &Path) -> Result<(), ConfigError>;
}
```

## deck_term (crate `deck-term`: PTY + VT + egui widget; deps egui, epaint[color_fonts], alacritty_terminal 0.26, arboard)

Facts: docs/notes/alacritty-api.md, egui-input.md, egui-fonts.md, tmux.md (read them; do not re-derive).

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: String, pub args: Vec<String>, pub cwd: PathBuf, pub env: Vec<(String, String)>,
    /// true when `program` is a tmux client (omp-deck's normal mode). Selects key-encoding rules
    /// (see "Key encoding") because tmux, not the inner app, is the outer terminal's peer.
    pub tmux_client: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TermTheme { #[default] Dark, Light }

#[derive(Clone)]
pub struct TermOptions {
    pub scrollback: usize,          // default 10_000 (local scrollback; tmux keeps its own)
    pub theme: TermTheme,
    pub clipboard: Clipboard,       // shared system clipboard (one per app)
}

/// The app's single system clipboard (arboard, CLIPBOARD + PRIMARY). Clone = same instance.
/// Lazily created; if arboard fails (no display) every call is a logged no-op/None.
#[derive(Clone)]
pub struct Clipboard { .. }
impl Clipboard {
    pub fn new() -> Clipboard;
    pub fn get_text(&self) -> Option<String>;
    pub fn set_text(&self, text: &str);
    pub fn get_primary(&self) -> Option<String>;
    pub fn set_primary(&self, text: &str);
}

/// Install terminal font families into egui (call once, from the eframe creator). Starts from
/// `FontDefinitions::default()` (UI fonts unchanged) and adds families `term`, `term-bold`,
/// `term-primary` (primary face only, for has_glyph classification). Primary = `fc-match -f '%{file}' monospace`
/// (bold = `fc-match -f '%{file}' monospace:bold`), fallbacks = existing files among the chain in
/// egui-fonts.md (DejaVu Sans Mono, Source Code Pro, Noto Sans Symbols 2, Noto Sans Symbols [wght],
/// Noto Sans Mono CJK SC (.ttc idx 2), Noto Color Emoji COLRv1, Noto Emoji). Missing files skipped.
pub fn install_fonts(ctx: &egui::Context) -> FontReport;
#[derive(Clone, Debug)] pub struct FontReport { pub primary: Option<PathBuf>, pub loaded: Vec<PathBuf>, pub missing: Vec<String> }

pub struct Terminal { .. }
impl Terminal {
    /// Spawns the PTY child (alacritty tty + EventLoop thread). Initial size 80x24 until first show().
    pub fn spawn(spec: SpawnSpec, opts: TermOptions, ctx: egui::Context) -> std::io::Result<Terminal>;
    /// Drains pending terminal events (answers ColorRequest/TextAreaSizeRequest/ClipboardLoad,
    /// stores ClipboardStore, records title/bell/exit). The app calls this every frame for every
    /// tab (hidden tabs too); TerminalView::show calls it for the visible one. Never blocks on I/O
    /// except a clipboard read for OSC 52 load.
    pub fn poll(&mut self) -> TerminalEvents;
    pub fn title(&self) -> Option<&str>;        // last OSC 0/2 title (None after reset)
    pub fn exit_code(&self) -> Option<i32>;     // Some once the child exited (signal → -1)
    pub fn is_exited(&self) -> bool;
    pub fn set_theme(&mut self, theme: TermTheme);
    /// Ends the child: Msg::Shutdown → Pty drop SIGHUPs the child. For tmux tabs the app instead
    /// kills the tmux session via deck_core (the client then exits on its own).
    pub fn kill(&mut self);
    pub fn write(&self, bytes: &[u8]);          // raw input to the PTY
}
/// Drop: sends Msg::Shutdown without joining (detaches: SIGHUP to the tmux *client* only;
/// tmux server/session survive — verified).

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalEvents {
    pub title_changed: Option<String>,  // Some("") = title reset
    pub exited: Option<i32>,            // reported exactly once
    pub bell: bool,
}

pub struct TerminalView<'a> { .. }
impl<'a> TerminalView<'a> {
    pub fn new(term: &'a mut Terminal) -> Self;
    pub fn font_size(self, points: f32) -> Self;     // default 14.0
    pub fn show(self, ui: &mut egui::Ui) -> TerminalResponse;   // fills available rect
}
pub struct TerminalResponse {
    pub response: egui::Response,
    pub events: TerminalEvents,         // merged poll() result for this frame
    pub open_url: Option<String>,       // Ctrl+click on an OSC 8 hyperlink; app opens it (xdg-open)
}
```

### Input routing contract (egui side)
- App MUST set `NativeOptions.clipboard_shortcuts = false` and `ctx.options_mut(|o| o.zoom_with_keyboard = false)`
  (egui-input.md §0). There is no egui clipboard backend: all system clipboard I/O goes through `Clipboard`.
- App shortcuts are taken first: the app calls `ctx.input_mut(|i| i.consume_key(mods, key))` for its own
  shortcuts (Ctrl+PgUp/PgDn, Ctrl+Shift+T, …) BEFORE showing the terminal; the focused terminal processes
  every remaining `Event::Key`/`Event::Text`/`Event::Ime` and holds focus-lock (tab, arrows, escape).
- Inside the terminal: Ctrl+Shift+C copy selection; Ctrl+Shift+V and Shift+Insert paste CLIPBOARD
  (bracketed when mode set); Ctrl+C/X/V → 0x03/0x18/0x16; Shift+middle-click pastes PRIMARY;
  selection is mirrored to PRIMARY on release.

### Key encoding (input.rs; pure fns, unit-tested)
1. Kitty flags active in TermMode (DISAMBIGUATE_ESC_CODES…; direct mode only, `Config.kitty_keyboard = true`)
   → kitty CSI-u per active flags.
2. else `tmux_client` → tmux always requests modifyOtherKeys 2 from the outer terminal (tmux.md):
   keys legacy can't express (Shift/Ctrl/Alt+Enter, Ctrl+Tab, Ctrl+Shift+letter, Ctrl+digit,
   Ctrl+punctuation without a C0 mapping, Shift+Space/Backspace with mods…) → `CSI 27;<mod>;<code>~`.
   Lone Escape → `ESC O [` (tmux ?7727 application-escape form; avoids tmux's 500 ms ESC delay).
   Alt+char → ESC + char. Everything else xterm legacy.
3. else xterm legacy (Shift+Enter → `\r`, Alt+Enter → `ESC \r`).
xterm legacy: arrows/Home/End honour APP_CURSOR (`ESC O A` vs `ESC [ A`), modified `ESC [1;<mod>A`,
F1-F4 `ESC O P..S` / `ESC[1;<mod>P`, F5-F12 `ESC[15~…`, Insert/Delete/PgUp/PgDn `ESC[2~/3~/5~/6~`,
Backspace 0x7f (Ctrl+Backspace 0x08, Alt+Backspace ESC 0x7f), Tab 0x09, Shift+Tab `ESC[Z`, Enter `\r`.

### Mouse/wheel/paste/focus
- Mouse reporting when `mode & MOUSE_MODE` and Shift not held: SGR (`ESC[<b;x;yM/m`) if SGR_MOUSE, else
  UTF-8 (?1005) or legacy X10 (clamped 223). Click/drag/motion per mode. Wheel → buttons 64/65, one report per line.
- Wheel without mouse mode: ALT_SCREEN && ALTERNATE_SCROLL → arrow keys (3 per notch); else local scrollback.
- Shift+drag always local selection (double-click semantic, triple-click line).
- Paste: BRACKETED_PASTE → `ESC[200~`+text(with any `ESC[201~` removed)+`ESC[201~`; else `\r\n`/`\n` → `\r`.
- Focus in/out → `ESC[I`/`ESC[O` when FOCUS_IN_OUT (on widget focus change AND window focus change).
- Exited child → banner `[process exited: <code>]` painted over the bottom row; tab stays.

### deck_term additions (accepted)
- `pub mod input` — pure encoders (`encode_key`, `encode_mouse`, `encode_paste`, `encode_focus`,
  `encode_alt_scroll`, `encode_text` + their types), public so integration tests can drive them.
- `impl Default for TermOptions` — scrollback 10_000, `TermTheme::Dark`, `Clipboard::new()`.

### Fix-loop contract additions (P6)
- `deck_term::Terminal::set_focused(&mut self, focused: bool)` — records focus state and, on a change,
  writes `ESC[I`/`ESC[O` when FOCUS_IN_OUT is set. TerminalView uses the same state, so the app calls
  `set_focused(false)` on the previously active tab when switching tabs (hidden tabs are never shown).
- `deck_core::sessions::SessionWatcher::root_exists(&self) -> bool` — root availability observed by the
  scanner thread on its last pass (UI never stats the root itself).
- `SessionsRoot::at`/`resolve` absolutize the path lexically (no existence requirement).
- Key encoding rule 2 correction: Alt+Enter and Alt+char are sent as `ESC`+byte (tmux maps `ESC \r` to
  `CSI 13;3u` for omp); modifyOtherKeys `CSI 27;mod;code~` is used for Shift/Ctrl combos legacy can't express.
- Direct mode (no tmux): the terminal tracks the child's `CSI > 4 ; n m` modifyOtherKeys requests itself
  (alacritty ignores them) and encodes per rule 2 while level 2 is active and kitty flags are off.
- `Terminal::poll` never blocks on I/O: OSC 52 loads are answered asynchronously by the clipboard worker.
  `Clipboard` owns arboard on a worker thread: `set_*` enqueue; `get_*` wait ≤ 500 ms then `None`.
- `deck_term::input::encode_key(&KeyInput, TermMode, KeyRules)` with `KeyRules { Legacy, ModifyOtherKeys, Tmux }`
  (Tmux = modifyOtherKeys + Esc as `ESC O [`). Kitty REPORT_EVENT_TYPES follows kitty's encoder: text keys
  report releases as CSI-u; unmodified Enter/Tab/Backspace releases only with REPORT_ALL_KEYS_AS_ESC.
- Known platform limit: winit 0.30 X11 reports non-emulated button 4/5 wheel clicks (XTest) on press and
  release, doubling wheel steps under Xvfb only; real mice use XI2 smooth-scroll axes (one step per notch).

## Wave 2: chrome redesign, omp updates, omp settings, sidebar context menus

Facts: `docs/notes/omp-cli.md` (omp 18.12.3 CLI, verified in a sandbox) and `docs/notes/egui-style.md`
(+ compiled demo `docs/notes/assets/egui-style-demo.rs`, screenshot `assets/egui-style-demo.png`).
Policy: omp performs every write to its own store/config through its CLI; OmpDeck never writes
`~/.omp/**` itself. Every omp invocation is blocking and MUST run on a worker thread.

### `deck_core::config` additions
```rust
pub struct AppConfig { /* existing fields */
    pub native_decorations: bool,   // default false → OmpDeck draws its own title bar (CSD); true → KWin frame
    pub check_updates: bool,        // default true → `omp update --check` at startup and every 6 h
}
```
Old state.json files without these keys load with the defaults (`#[serde(default)]` already on the struct).

### `deck_core::omp` (new module)
```rust
#[derive(Debug, thiserror::Error)]
pub enum OmpError {
    #[error("failed to run omp: {0}")] Spawn(#[from] std::io::Error),
    /// Non-zero exit. `message` = trimmed stderr with a leading "Error: "/"error: " removed
    /// (falls back to stdout when stderr is empty). Never contains a secret value.
    #[error("{message}")] Failed { code: Option<i32>, message: String },
    #[error("unexpected omp output: {0}")] Parse(String),
}

#[derive(Clone, Debug)]
pub struct Omp { /* program path */ }
impl Omp {
    /// First executable `omp` on PATH, else `~/.local/bin/omp`.
    pub fn detect() -> Option<Omp>;
    pub fn at(program: impl Into<PathBuf>) -> Omp;          // tests
    pub fn program(&self) -> &Path;
    /// Every command below runs with `current_dir("/")` (no project `.omp/config.yml` layer),
    /// stdin null, stdout/stderr captured, NO_COLOR=1.
    pub fn version(&self) -> Result<String, OmpError>;      // "omp/18.12.3" → "18.12.3"
    pub fn check_update(&self) -> Result<UpdateStatus, OmpError>;   // `omp update --check`
    /// Merges `config list` (text: tab + enum choices + order), `config list --json` (type, value,
    /// description, redacted) and defaults (`config list --json` with `PI_CODING_AGENT_DIR` = an empty
    /// temp dir, cwd = an empty temp dir, all other `PI_*` vars removed; temp dirs deleted after).
    /// The three commands run concurrently.
    pub fn load_settings(&self) -> Result<SettingsCatalog, OmpError>;
    /// `omp config set <key> --json -- <encoded>` (value is ONE argv element after `--`, so values starting
    /// with '-' are not taken as flags; anything after `--` is value text, so `--json` must precede it).
    /// Returns the saved value from the JSON reply (secrets: `Value::Null`; their stdout is never logged).
    /// A set secret is listed with `value = Some("********")` (`SECRET_PLACEHOLDER`); never edit/echo it.
    /// `Omp::with_env(key, value)` adds env to every command (tests sandbox `PI_CODING_AGENT_DIR`).
    pub fn set_setting(&self, setting: &Setting, value: &serde_json::Value) -> Result<serde_json::Value, OmpError>;
    pub fn reset_setting(&self, key: &str) -> Result<(), OmpError>;   // `omp config reset <key> --json`
    /// Omp-native move: `omp session pack <src> <tmp.tar>` then `omp session unpack <tmp.tar> --cwd <dest>`;
    /// the tar is always removed. omp deletes the original when it is a prefix of the archive
    /// (`Replaced`), so this is copy-then-delete-original performed by omp. Fails with nothing changed
    /// while a live omp process writes the session ("another live omp process…").
    pub fn move_session(&self, src: &Path, dest_cwd: &Path) -> Result<MoveOutcome, OmpError>;
}

/// argv for a terminal tab (same form as `omp --resume`): `["omp","--fork",<src>,"--cwd",<dest>]`.
/// omp writes the copy (new id, artifacts copied, header cwd = dest) as it starts; source untouched.
pub fn fork_argv(src: &Path, dest_cwd: &Path) -> Vec<String>;
/// argv for an update tab: Omp → ["omp","update"], Plugins → ["omp","update","--plugins"],
/// Canary → ["omp","update","--canary"], Stable → ["omp","update","--stable"].
pub fn update_argv(kind: UpdateKind) -> Vec<String>;
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum UpdateKind { Omp, Plugins, Canary, Stable }
/// argv for the "Install omp" tab (TabRole::InstallOmp, direct, never persisted), offered while
/// `Omp::detect()` is None: `sh -c` fetching `INSTALL_URL` (https://omp.sh/install) with curl, else
/// wget, into a variable (a failed download fails the tab), then `sh -s -- --binary` (prebuilt binary
/// → `$PI_INSTALL_DIR` or `~/.local/bin`, no bun/root). Exit 0 → re-run `Omp::detect()` and adopt it
/// (version, models, usage) without a restart. All omp tab argv (sessions, resume, restart, fork,
/// update) use the detected binary as argv[0], since `~/.local/bin` may not be on PATH yet.
pub fn install_argv() -> Vec<String>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateStatus { pub current: String, pub latest: Option<String> } // latest None = up to date

#[derive(Clone, Debug, PartialEq)]
pub struct MoveOutcome { pub new_path: PathBuf, pub kind: MoveKind }
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveKind { Created, Replaced, Forked { new_id: String } }  // Forked: original left in place

#[derive(Clone, Debug, PartialEq)]
pub struct SettingsCatalog { pub tabs: Vec<SettingsTab> }   // omp's tab order; "internal" last
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsTab { pub id: String, pub label: String, pub settings: Vec<Setting> }
// labels: appearance Appearance, model Model, interaction Interaction, context Context, memory Memory,
// files Files, shell Shell, tools Tools, tasks Tasks, providers Providers, internal Advanced;
// unknown id → capitalized id.
#[derive(Clone, Debug, PartialEq)]
pub struct Setting {
    pub key: String,
    pub kind: SettingKind,
    pub value: Option<serde_json::Value>,    // None = unset (or secret)
    pub default: Option<serde_json::Value>,
    pub description: String,
    pub secret: bool,                        // JSON `redacted` or a known credential key (omp-cli.md A1.5)
}
#[derive(Clone, Debug, PartialEq)]
pub enum SettingKind { Bool, Number, Enum(Vec<String>), String, Array, Record }
impl Setting {
    pub fn label(&self) -> String;     // last key segment humanized: "compactThinkingLevel" → "Compact thinking level"
    pub fn section(&self) -> &str;     // key up to the last '.', "" if none ("statusLine.preset" → "statusLine")
    pub fn is_modified(&self) -> bool; // value differs from default (secret: value set/redacted)
}
/// Value → the single CLI argument: bool "true"/"false", number as JSON number text, enum/string raw
/// text (strings JSON-quoted when they have leading/trailing whitespace), array/record compact JSON.
pub fn encode_value(kind: &SettingKind, value: &serde_json::Value) -> Result<String, OmpError>;
/// Pure parsers (unit-tested against captured output):
pub fn parse_update_check(stdout: &str) -> Result<UpdateStatus, OmpError>;
pub fn parse_unpack(stdout: &str, stderr: &str) -> Result<MoveOutcome, OmpError>;
pub fn parse_settings(list_text: &str, list_json: &str, defaults_json: &str) -> Result<SettingsCatalog, OmpError>;
```

### omp-deck (app) behavior contract
- Chrome: "Midnight neon" theme (deep navy/graphite surfaces, violet→cyan accent gradient) + a matching
  light variant; UI font Adwaita Sans (fallback Cantarell / Noto Sans / egui default) added with
  `ctx.add_font` so emoji/fallback faces stay; icons from `egui_material_icons` (initialize AFTER
  `deck_term::install_fonts`). Terminal grid colors are unchanged.
- Client-side title bar unless `native_decorations`: logo, inline tab strip, update badge, settings
  button, app menu, minimize/maximize/close; drag → `ViewportCommand::StartDrag`, double-click toggles
  maximize, 6 px edge/corner zones → `BeginResize` with matching cursors (not when maximized).
- Settings page = a special non-terminal tab (at most one; not persisted) with omp's tabs as a nav
  rail plus an "OmpDeck" section (font size, theme, title bar style, update checks).
- Sidebar context menus (session row, project header); copy = new tab running `fork_argv` in a fresh
  `omp-new-<ms>` tmux session; move = confirm modal → stop the session if alive (tmux kill, close its tab)
  → `move_session` (retry ≤ 10 × 300 ms on the live-writer error) → rescan → reopen in a tab if it had one.
- Async results (settings writes, update checks, moves) surface as toasts; errors keep omp's message.

### Wave 2 fix-loop additions
```rust
// deck_core::sessions
/// The newest top-level session file under `root` (any project dir) with mtime >= `since` whose
/// header has `"parentSession": <parent_id>`; bounded header reads only (same 16 KiB rule as scan),
/// files older than `since` are skipped by metadata alone. None if not (yet) present.
pub fn find_fork(root: &SessionsRoot, parent_id: &str, since: SystemTime) -> Option<PathBuf>;
```
- Copy flow: the copy tab starts as "copy pending"; the app polls `find_fork` on a worker (≈1 s,
  ≤ 2 min). Found → tab.session_path = path (tab now counts as that session's tab), success toast.
  Process exits first → error toast pointing at the tab output.
- A session row counts as open/running if any tab's `session_path` equals it, in addition to the
  tmux fd mapping. Move's reopen uses the normal open path (`session_name_for(id)` + `--resume`).

## Wave 3: session status bar, model/thinking switcher, session actions

Facts: `docs/notes/omp-status.md` (status-line segments, jsonl schema + parsing pseudo-spec §3.2,
git/PR, breadcrumb, title glyph) and `docs/notes/omp-actions.md` (slash commands, injection bytes §3,
`/switch` + `/effort` §6, per-button recommendations §7). omp is now 18.12.5.
Decision: NO collab guest connection (it posts "joined/left" notices into the user's transcript,
bumps `collab:N`, and re-downloads the whole transcript on join). Live data = jsonl tail + pane
title glyph + git (+ `gh` for PR). Actions = slash-command/hotkey injection into the tab's PTY or a
new tab running an omp CLI.

### deck_term additions
```rust
impl Terminal {
    /// Instant the last 0x03 byte was written to the PTY (from key input or `write`). omp exits on
    /// two Ctrl+C within 500 ms; injectors must keep ≥ 650 ms distance.
    pub fn last_ctrl_c(&self) -> Option<Instant>;
    /// Visible text of the cursor's screen row (wide-char spacers skipped, trailing blanks trimmed);
    /// used to detect an unsent draft in omp's composer.
    pub fn cursor_line(&self) -> String;
}
```

### deck_core additions
```rust
// deck_core::sessions
impl SessionsRoot { pub fn agent_dir(&self) -> PathBuf; }  // parent of the sessions root

// deck_core::tmux
impl Tmux { pub fn pane_tty(&self, session: &str) -> Option<String>; } // `display -p -t <s> '#{pane_tty}'`

// deck_core::omp
impl Omp { pub fn models(&self) -> Result<Vec<ModelInfo>, OmpError>; }  // `omp models --json`
#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub selector: String,            // "provider/id" (what `/switch` takes)
    pub provider: String, pub id: String, pub name: String,
    pub context_window: Option<u64>,
    pub thinking: Vec<String>,       // supported effort levels in order; empty = no thinking
    pub cost_input: Option<f64>, pub cost_output: Option<f64>,   // $ per M tokens
}

// deck_core::status (new)
/// `<agent_dir>/terminal-sessions/<tty with "/dev/" stripped, "/"→"-">` (omp-actions.md §4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breadcrumb { pub cwd: PathBuf, pub session: PathBuf, pub fresh: bool }
pub fn read_breadcrumb(agent_dir: &Path, tty: &str) -> Option<Breadcrumb>;

/// Incremental jsonl reader (omp-status.md §2a, §3.2): keyed on (dev, ino); resets on inode change or
/// shrink; line 0 is the 256-byte title slot (re-read each refresh); appended bytes parsed from the
/// last complete line; prefilter lines by substring before JSON parsing.
pub struct SessionStatsReader { .. }
impl SessionStatsReader {
    pub fn new(path: PathBuf) -> Self;
    pub fn path(&self) -> &Path;
    /// Reads what changed since the last call. Ok(true) if the stats changed.
    pub fn refresh(&mut self) -> io::Result<bool>;
    pub fn stats(&self) -> &SessionStats;
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStats {
    pub title: Option<String>,
    pub model: Option<String>,               // last model_change "provider/id"
    pub thinking: Option<String>,            // effective level (None = model has no thinking)
    pub thinking_configured: Option<String>, // e.g. "auto"
    pub input_tokens: u64, pub output_tokens: u64, pub cache_read: u64, pub cache_write: u64,
    pub cost: f64,                           // Σ assistant usage.cost.total + model_usage (omp's rule)
    pub context_tokens: Option<u64>,         // last good assistant input+cacheRead+cacheWrite
    pub last_turn_failed: bool,              // last assistant stopReason is error (Retry button)
    pub last_assistant_text: Option<String>, // text parts of the last assistant message (≤ 256 KiB)
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GitInfo { pub branch: String, pub ahead: u32, pub behind: u32, pub staged: u32, pub unstaged: u32, pub untracked: u32, pub pr: Option<PrInfo> }
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrInfo { pub number: u64, pub url: String }
/// `git -C <cwd> status --porcelain=v2 --branch -unormal` (5 s timeout); None if not a repo / no git.
pub fn git_info(cwd: &Path) -> Option<GitInfo>;   // pr filled by the watcher only
pub fn parse_git_status_v2(out: &str) -> Option<GitInfo>;   // pure, tested

/// One background thread tracking the ACTIVE tab only.
#[derive(Clone, Debug, PartialEq)]
pub struct StatusTarget {
    pub tmux: Option<(Tmux, String)>,   // tab's tmux client + session name (None in direct mode)
    pub session: Option<PathBuf>,        // tab's known session file (fallback when no breadcrumb)
    pub cwd: PathBuf,
}
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionStatus {
    pub session: Option<PathBuf>,        // current file (breadcrumb wins; follows /new, /fork, /move)
    pub fresh: bool,                     // breadcrumb says not yet persisted
    pub cwd: PathBuf,
    pub stats: SessionStats,
    pub git: Option<GitInfo>,
}
pub struct StatusWatcher { .. }
impl StatusWatcher {
    /// jsonl/breadcrumb every 1 s, git every 3 s, `gh pr view --json number,url` once per (cwd, branch)
    /// when `gh` exists and branch isn't the default/detached; `on_update` after a changed snapshot.
    pub fn spawn(agent_dir: PathBuf, on_update: impl Fn() + Send + 'static) -> StatusWatcher;
    pub fn set_target(&self, target: Option<StatusTarget>);   // immediate refresh on change
    pub fn refresh_now(&self);
    pub fn latest(&self) -> Option<Arc<SessionStatus>>;        // for the current target only
}   // Drop stops + joins.
```

### omp-deck behavior
- Bottom status bar (only for terminal tabs running omp): busy indicator (from the tab title glyph:
  `π > …` idle, braille spinner busy, `!` attention), model chip + thinking glyph (dropdown), path,
  git branch + `*N +N ?N` + PR, context-usage meter (tokens / window from `ModelInfo`), ↑in ↓out
  tokens, cost; right side action buttons. Repaint only on data change (spinner animates only while busy).
- Model dropdown: models from `Omp::models()` (cached; refresh button), searchable, grouped by provider,
  current marked; thinking-level selector limited to `off`, `auto` + the model's levels.
  Apply with `/switch <selector>[:level]` and `/effort <level>` injection.
- Injection (omp-actions.md §3.7/§3.10): per-tab serialized queue driven by frame time (never sleeps on
  the UI thread): if the cursor line holds a draft → confirm modal (it will be cleared) → Ctrl+C
  (respect `last_ctrl_c` ≥ 650 ms) → wait 650 ms → `ESC[200~<cmd>ESC[201~` → 150 ms →
  (idle only) Esc → 150 ms → Enter. Idle-only actions are disabled while busy. Every key (Ctrl+C,
  Esc, Enter, hotkeys) goes through `Terminal::send_key`, never as raw bytes (ConPTY merges a raw
  trailing `ESC` with the next write once win32-input-mode is in use); only the paste is raw.
- Actions: Handoff (idle, optional focus modal), Compact (idle; soft/snapcompact/focus variants),
  Fork to new tab (`fork_argv(path, cwd)` + copy-pending linking), Rename (`/rename <title>`; auto-name),
  Plan mode toggle (Alt+Shift+P), Agent hub (Alt+A), Context (`/context`), Retry (F5, shown when
  `last_turn_failed`), Stop turn (Esc, busy only), Copy last response (clipboard), Export HTML
  (`omp --export <jsonl>` with cwd = `$XDG_CACHE_HOME/omp-deck/exports`, then xdg-open).
- The tab's `session_path` follows `SessionStatus.session` (keeps sidebar mapping right after /new, /fork).

### Wave 3 fix-loop additions
```rust
// deck_term — replaces `cursor_line` (removed).
pub struct ScreenText { pub lines: Vec<String>, pub cursor_row: usize, pub cursor_col: usize }
impl Terminal {
    /// Visible rows of the active screen (viewport at the bottom, i.e. ignoring local scrollback),
    /// wide spacers skipped, trailing blanks trimmed. Called only on user actions.
    pub fn screen_text(&self) -> ScreenText;
    /// While `Instant::now() < until`, ALL user keyboard/paste/IME input to this terminal is dropped
    /// (mouse wheel/selection unaffected; focus reports unaffected). `write()` is never held.
    /// Used by injectors so the user cannot interleave Enter / Ctrl+C into an injection.
    pub fn hold_input_until(&self, until: Instant);
    /// One synthetic key stroke (press + release) encoded like the user's own key under the
    /// child's current protocol (`input::encode_stroke`; win32 modifier records restored to what
    /// the child last saw). Never held; counts as Ctrl+C for `last_ctrl_c`.
    pub fn send_key(&self, key: input::Key, mods: input::Mods);
}
```
- Focus reports: a click on app chrome (status bar, menus, dialogs) while a terminal tab is active must
  NOT produce `ESC[O`/`ESC[I` pairs adjacent to injected bytes. An injected lone `ESC` must be followed
  by ≥ 100 ms without further PTY bytes from the app (omp merges `ESC` + `ESC[I` into one key → Stop fails).
- Injection: idle-only jobs require `Activity::Idle` exactly (not Attention/Unknown); a job aborts
  without sending more bytes if activity leaves Idle before its final Enter; user input is held from the
  first injected byte until 150 ms after the final byte; a tab close is deferred until its injector
  is at a job boundary (≤ ~1.5 s, then close).
- Draft detection reads the composer region (rows around the cursor matching the composer frame of
  the configured `composer.shape`), not arbitrary rows; if the cursor is not in a recognizable
  composer (an omp overlay/picker is open), slash jobs are refused with a toast ("Close omp's open
  dialog first"), hotkey jobs still run.
- Stop: `ESC`, then after 650 ms, if the title still shows busy, one more `ESC` (never two within 500 ms).
- Opening a sidebar session never attaches a tmux session that now runs another session file
  (after `/new`, `/fork`, `/resume`): consult `Tmux::session_files`; attach to whichever tmux session
  holds this file, else create `omp-<id>-<unix-ms>`.
- deck_core breadcrumb trust: if the pane start time is unknown, trust only a breadcrumb naming the
  target's known session.
- deck_term `Terminal::set_visible(&self, bool)`: output-driven repaints are coalesced (visible ≤ one
  per 16 ms, hidden ≤ one per 250 ms); the app marks only the active tab visible. Window title updates
  from omp's spinner are throttled to 1 Hz by the app.
- deck_term `pub fn wait_detached(timeout: Duration)`: joins in-flight detach-flush workers (from
  dropping a `Terminal` with unwritten input); omp-deck calls it after `eframe::run_native` returns.

## Wave 4: model picker favorites, native Agent Hub, per-folder mode

### deck_core::config additions (serde default, backward compatible)
```rust
pub struct AppConfig {
    // …existing…
    pub favorite_models: Vec<String>,          // `ModelInfo.selector`s, insertion order
    pub model_provider_filter: Option<String>, // None = "All providers"
    pub folder_scope: Option<PathBuf>,         // last sidebar scope (None = global view)
    pub recent_folders: Vec<PathBuf>,          // most-recent first, deduped, ≤ 10
    pub folder_tabs: BTreeMap<PathBuf, FolderTabs>, // per-folder open tabs (folder mode launch/restore)
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct FolderTabs { pub open_tabs: Vec<TabState>, pub active_tab: Option<usize> }
impl AppConfig {
    pub fn toggle_favorite(&mut self, selector: &str) -> bool;  // returns new state
    pub fn push_recent_folder(&mut self, dir: PathBuf);         // dedupe, front, cap 10
}
```

### Model picker (omp-deck)
- Below the search field: provider dropdown "All providers (N)" + each provider with its model count
  (selection persisted in `model_provider_filter`; unknown provider → All).
- "★ Favorites" section first (favorites within the current provider filter and search), then the
  provider groups (All) or the single provider's list. Every row has a star toggle (☆ empty / ★ filled,
  accent) that does NOT select the model; favorites persisted in `favorite_models`.
- Order inside a provider: newest first per `docs/notes/model-order.md` (series rank interleave,
  version tuple desc, alias before dated, date desc, id, name), implemented as a pure tested function
  using that note's 38-row test table.

### Per-folder mode (omp-deck)
- Enter: CLI `omp-deck [FOLDER]` (positional) or `--folder <dir>`; sidebar scope menu "Open folder…"
  (kdialog/zenity picker via the existing `transfer::picker_command`, plus a typed-path field) and recent
  folders; project-group context menu "Focus this folder". Leave: scope menu "All projects".
- Scope = sessions whose header cwd equals the folder or is inside it (path-component prefix,
  canonicalized/`~`-insensitive); grouping unchanged. Filter box works within the scope. Empty scope →
  empty state with "New session in <folder>". A "+ New session" for the folder is always visible.
- Scope persists in `folder_scope`; CLI folder overrides it for that run. Window title
  "OmpDeck — <folder name>" in folder mode. Running indicators / tabs of out-of-scope sessions stay
  (tabs are never closed by a scope change).
- Tabs: when launched with a CLI folder, restore `folder_tabs[folder]` instead of the global
  `open_tabs`; on exit save tabs to the scope the app was launched with (CLI folder → `folder_tabs`,
  else global `open_tabs`).

### Native Agent Hub
Facts: `docs/notes/agent-hub.md` (file layout §2, parent↔child mapping + status inference §3, live
tail rules §4, render shapes §6). Read-only from files; no collab guest; kill/chat/revive are NOT done
natively (only "Open in omp's Agent Hub" = `ESC a` injection via the existing injector).
UI dependency: `egui_commonmark` 0.26 (workspace dep, `default-features = false, features = ["pulldown_cmark"]`)
for assistant text / output markdown.

```rust
// deck_core::agents (new)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentStatus { Running, Done, Failed, Aborted, Interrupted }
#[derive(Clone, Debug, PartialEq)]
pub struct AgentSummary {
    pub id: String,                 // file stem, e.g. "CoreStatus" or "M2cCore.M2cSaveService"
    pub parent_id: Option<String>,  // None = spawned by the main session
    pub depth: usize,
    pub path: PathBuf,              // transcript .jsonl
    pub advisor: bool,
    pub agent: Option<String>,      // session_init.agent ("task", "scout", …)
    pub model: Option<String>,      // provider/id (latest model_change, else resolvedModel)
    pub thinking: Option<String>,
    pub task_gist: String,          // preamble stripped, `# Target` section preferred, ≤ 1000 chars
    pub task: String,               // full assignment text (≤ 64 KiB)
    pub status: AgentStatus,
    pub error: Option<String>,      // last errorMessage for Failed/Aborted
    pub current: Option<String>,    // running: pending tool ("bash · cargo test", intent preferred)
    pub created: Option<SystemTime>, pub last_activity: Option<SystemTime>,
    pub duration: Option<Duration>, // last entry − header timestamp
    pub requests: u32, pub tool_calls: u32, pub tokens: u64, pub cost: f64,
    pub context_tokens: Option<u64>,
    pub output_path: Option<PathBuf>, // `<id>.md` when size > 0
}
#[derive(Clone, Debug, PartialEq)]
pub struct AgentsTarget { pub session: PathBuf, pub owner_alive: bool } // owner_alive: the tab's omp runs
#[derive(Clone, Debug, Default)]
pub struct AgentsSnapshot { pub session: PathBuf, pub agents: Vec<AgentSummary> } // parent-before-child, newest first among siblings
pub struct AgentsWatcher { .. }
impl AgentsWatcher {
    /// Rescans `<stem>/` (recursively for nested dirs) + incrementally tails each transcript
    /// ((dev,ino) + offset + partial-line; reset on inode change/shrink); 1 s while any agent is
    /// Running, else 3 s; publishes only on change.
    pub fn spawn(on_update: impl Fn() + Send + 'static) -> AgentsWatcher;
    pub fn set_target(&self, target: Option<AgentsTarget>);
    pub fn latest(&self) -> Option<Arc<AgentsSnapshot>>;
}   // Drop stops + joins.

#[derive(Clone, Debug, PartialEq)]
pub enum TranscriptItem {
    User { text: String, from_agent: bool, at: Option<SystemTime> },
    Assistant { text: String, model: Option<String>, at: Option<SystemTime> },
    Thinking { text: String },
    ToolCall { call_id: String, name: String, intent: Option<String>, summary: String, args: String },
    ToolResult { call_id: String, name: String, text: String, is_error: bool, truncated: bool },
    Notice { kind: String, text: String },       // custom_message display:true, async-result, irc
    Compaction { tokens_before: Option<u64>, tokens_after: Option<u64>, summary: String },
    ModelChange { model: String },
    Error { message: String },                   // assistant stopReason error/aborted
    Exit { reason: String },
}
/// Long text fields capped (tool result 16 KiB, thinking 64 KiB, args 4 KiB) with `truncated`.
pub struct TranscriptWatcher { .. }
impl TranscriptWatcher {
    pub fn spawn(path: PathBuf, on_update: impl Fn() + Send + 'static) -> TranscriptWatcher; // 500 ms tail
    pub fn latest(&self) -> Arc<Vec<TranscriptItem>>;  // full list so far (append-only growth)
    pub fn revision(&self) -> u64;                     // bumps on change
}   // Drop stops + joins.
```

UI (omp-deck): status bar "Agents" chip with live counts (running spinner only while any Running) toggles a
right-side Agent Hub panel for the active tab's session: aggregate header, search, tree/flat,
cards (status, id, agent type, model, task gist, cost/duration/requests/tools/tokens, current tool,
age). Clicking a card opens a resizable transcript popup (header facts + Transcript / Output / Task
tabs; markdown via egui_commonmark; collapsible thinking & tool results; live follow while Running;
copy output; prev/next agent). The old "Agent hub" action becomes "Open omp's Agent Hub (TUI)" in ⋯.

## Wave 5: live subagent control, title strip, larger status bar

Facts: `docs/notes/omp-control-ext.md` (proven in-process extension: `omp -e <file.ts>` reaches
`AgentRegistry` / `AgentLifecycleManager`; kill/chat/revive semantics = omp's TUI hub; NDJSON unix
socket protocol; survives /new /fork /resume /restart; no visible side effects; internal module
specifiers may change between omp versions → extension stays inert, OmpDeck falls back to read-only).

### deck_core::ctl (new)
```rust
/// The extension source (crates/deck-core/assets/ompdeck-ctl.ts, `include_str!`): the note's final
/// version + `off` for the progress listener on stop + protocol `v:1`; inert unless OMPDECK_CTL_DIR set.
pub const EXTENSION_SOURCE: &str;
/// Writes the extension to `<dir>/ompdeck-ctl.ts` if missing/different (0600); returns the path.
pub fn install_extension(dir: &Path) -> io::Result<PathBuf>;
/// `$XDG_RUNTIME_DIR/omp-deck/ctl/<key>` (fallback `<temp>/omp-deck-<uid>/ctl/<key>`); key = tmux
/// session name (or a direct-mode tab key). Socket = `<dir>/ctl.sock`.
pub fn ctl_dir(key: &str) -> PathBuf;
/// `["env", "OMPDECK_CTL_DIR=<dir>", <omp>, "-e", <ext>, <rest of omp args…>]` — prefixes an omp argv.
pub fn with_ctl(omp_argv: &[String], ext: &Path, dir: &Path) -> Vec<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveStatus { Running, Idle, Parked, Aborted }
#[derive(Clone, Debug, PartialEq)]
pub struct LiveAgent { pub id: String, pub name: String, pub parent: Option<String>, pub status: LiveStatus,
    pub activity: Option<String>, pub live: bool, pub streaming: Option<bool>, pub session_file: Option<PathBuf>,
    pub model: Option<String> }
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LiveSnapshot { pub pid: u32, pub session_file: Option<PathBuf>, pub main_idle: Option<bool>, pub agents: Vec<LiveAgent> }
#[derive(Clone, Debug, PartialEq)]
pub enum CtlCommand { Kill { id: String }, Revive { id: String }, Chat { id: String, text: String } }
#[derive(Clone, Debug, PartialEq)]
pub struct CtlReply { pub ok: bool, pub error: Option<String> }
/// One thread per client: connects to `<dir>/ctl.sock`, reconnects with backoff (≤ 2 s) when the
/// socket is absent/closed (/restart, omp not yet started); never blocks the caller.
pub struct CtlClient { .. }
impl CtlClient {
    pub fn spawn(dir: PathBuf, on_update: impl Fn() + Send + 'static) -> CtlClient;
    pub fn connected(&self) -> bool;
    pub fn latest(&self) -> Option<Arc<LiveSnapshot>>;     // None until first snapshot / after disconnect
    /// Queued; the reply arrives via `take_replies` (and `on_update` fires). Returns a request id.
    pub fn send(&self, cmd: CtlCommand) -> u64;
    pub fn take_replies(&self) -> Vec<(u64, CtlReply)>;
}   // Drop closes + joins.
```

### omp-deck
- Every omp tab OmpDeck launches (open/resume, new, fork/copy, move-reopen, restore when it creates a
  session) runs `with_ctl(argv, ext, ctl_dir(tmux_name))`. Extension installed at startup into
  `$XDG_DATA_HOME/omp-deck/` (before the window; small write). `omp update` tabs: no extension.
- The Agent Hub keeps one `CtlClient` for the active session tab. When connected: live status
  (incl. Parked) and activity override file inference for matching ids; popup + cards gain Kill
  (confirm), Revive (parked), and a Chat composer (steers running, prompts idle, revives parked).
  Not connected: read-only as today with a hint; for tabs not launched with the extension, a
  "Restart session with live control" action (idle only, confirm): kill that tmux session, relaunch
  `--resume <current session file>` with the extension in the same tab.
- UI: model picker popup ~50 % wider; a centered title strip at the top of the CSD chrome showing the
  active tab's full title (omp `π <glyph> ` prefix stripped; tooltip with cwd/session path); native
  decorations mode sets the window title to "<tab title> — OmpDeck" instead. Status bar fonts, icons
  and height ~50 % larger.

### Wave 5 fix-loop additions
- `ctl_dir(key) -> PathBuf` stays pure but bounded: leaf = `<16 hex of a stable 64-bit hash of key>`;
  root = `$XDG_RUNTIME_DIR/omp-deck/ctl` when set, absolute, owned by the effective uid and not
  group/other-writable; otherwise `$XDG_CACHE_HOME/omp-deck/ctl` (never a predictable `/tmp` path).
- New `prepare_ctl_dir(dir: &Path) -> io::Result<()>`: creates every missing component 0700, then
  verifies with no-follow lstat that root and leaf are real dirs owned by the euid with no group/other
  write (leaf exactly 0700), and that `<dir>/ctl.sock` fits the 108-byte sun_path limit; error →
  caller launches WITHOUT control (logged). omp-deck calls it on a worker before every `with_ctl`.
- New `prune_ctl_dirs(alive: &HashSet<String>)`: removes leaf dirs (and a stale `ctl.sock` inside)
  whose key's hash isn't among `ctl_dir` leaves of `alive` keys; non-recursive beyond that, no-follow.
- `install_extension` enforces regular non-symlink file owned by euid with mode 0600 even when the
  content matches.
- Extension: commands authorized only for agents whose sessionFile belongs to the current root;
  `chat` replies once the prompt is accepted (not when the turn ends); per-dir ownership via an
  exclusive lock file (`ctl.lock`, `flock`-style or O_EXCL pid file with liveness check) — never
  unlink a socket owned by a live process; on close unlink only if the path still has our inode;
  setup failure after listen stops the server and unlinks its socket.
- Round 2: `ctl_dir(namespace, key)` / `prune_ctl_dirs(namespace, alive)` — namespace = the tmux
  socket name (`omp-deck`, `omp-deck-test-…`) or `direct`; leaves live under `<root>/<namespace>/`, so
  instances on different sockets never see each other's dirs. Prune refuses a symlinked root, and
  skips any leaf whose lock owner is live (lock records `pid starttime` from `/proc/<pid>/stat` field
  22; the extension writes the same and treats a lock as stale only if pid is dead or its start time
  differs) or whose socket accepts a connection. Every `CtlCommand` carries the expected root session
  file; the extension rejects commands whose root ≠ `rootFile()`; omp-deck ignores live snapshots whose
  `session_file` ≠ the tab's current session file.

## Wave 6: session tree view with fork-from-node; chrome/status-bar tweaks

Facts: `docs/notes/session-tree.md` (tree model, omp `/tree` visibility + labeling rules, navigate /
fork semantics, extension ops proven in a sandbox, perf numbers). Chrome/status-bar tweaks (two-row
CSD chrome, status bar ≈1.2×, wide model chip, picker popup back to 430 px) already landed.

### deck_core::tree (new; file-based, works for any session)
```rust
pub enum NodeKind {                     // mirrors omp entry types (session-tree.md §1 row-text table)
    User, Assistant, ToolResult, Developer, BashExecution, OtherRole(String),   // type == "message"
    CustomMessage(String), Compaction, BranchSummary, ModelChange, ModelUsage, ThinkingChange,
    Label, Custom(String), Setting(String) /* service_tier_change, title_change, mode_change, credential_pin, session_init, ttsr_injection, reset_boundary, other */,
}
pub struct TreeNode {
    pub id: String, pub parent: Option<String>, pub kind: NodeKind,
    pub timestamp: Option<String>,     // ISO as written
    pub text: String,                  // omp row text WITHOUT the role prefix, ≤200 chars, \n\t→space, trimmed
                                       // (toolResult → "[read: path]" etc. from the matching toolCall; assistant w/o text → error/“(aborted)”/“(no content)”)
    pub has_text: bool,                // assistant visibility rule input (non-whitespace text block)
    pub stop_reason: Option<String>,
    pub active_leaf: Option<Option<String>>,  // `activeLeafId` if present (Some(None) = explicit null)
}
pub struct SessionTree {
    pub nodes: Vec<TreeNode>,                 // file order
    pub index: HashMap<String, usize>,
    pub children: Vec<Vec<usize>>,            // sorted by timestamp asc
    pub roots: Vec<usize>,                    // parentId null/missing/self
    pub labels: HashMap<String, String>,      // target id → latest label text (cleared labels removed)
    pub file_leaf: Option<usize>,             // omp load rule (activeLeafId fallback, session-tree.md §1)
}
impl SessionTree {
    pub fn active_path(&self, leaf: Option<usize>) -> Vec<bool>;   // per node: on root→leaf path
    pub fn is_forkable(&self, i: usize) -> bool;                  // kind is a `message` (User needs a parent)
    pub fn fork_target(&self, i: usize) -> Option<&str>;          // User → parent id; other message kinds → own id
}
/// Streams the file on its own thread: header/title lines skipped, prefix-only parse for hidden types,
/// bodies parsed only for visible ones; truncated last line tolerated; tails appends (≤1 s poll or
/// notify) and rebuilds incrementally; full reload when the file is replaced/shrinks (/move).
pub struct TreeWatcher;
impl TreeWatcher {
    pub fn spawn(path: PathBuf, on_update: impl Fn() + Send + 'static) -> TreeWatcher;
    pub fn latest(&self) -> Option<Arc<SessionTree>>;
    pub fn revision(&self) -> u64;
    pub fn error(&self) -> Option<String>;
}  // Drop stops + joins promptly.
```

### deck_core::ctl additions
- `LiveSnapshot.leaf: Option<String>` (main session's in-memory leaf, `ctx.sessionManager.getLeafId()`).
- `CtlCommand::ForkAt { root, entry_id }` → `CtlReply.new_file: Option<PathBuf>`: extension opens a second
  `SessionManager` on the file (`flush()` first) and `createBranchedSession(entryId, {copyArtifacts:true})`;
  rejects non-`message` entries; when the target is an assistant message with toolCalls, extends the cut
  to cover the matching toolResults (like omp's `fork(entryId)`), never leaving unpaired calls. Live tab
  is untouched.
- `CtlCommand::Navigate { root, entry_id }` → `CtlReply.leaf: Option<String>`: idle only (`busy` error
  otherwise); `Main.session.extensionRunner.createCommandContext().navigateTree(id, {summarize:false})`
  (TUI re-renders; user target → leaf = parent, text goes to the editor — omp semantics).
- Every op keeps the root check.

### deck_core::omp addition (sessions without live control)
`Omp::fork_at(&self, session_file: &Path, entry_id: &str) -> Result<PathBuf, OmpError>`: headless
`omp --mode rpc --resume <file>` (cwd = header cwd or $HOME), sends `{"type":"fork","entryId"}` then
`get_state` for the new `sessionFile`, closes stdin; 30 s timeout; error text surfaced (e.g. lease held
by a live omp). Blocking — callers run it on a worker.

### omp-deck: Session tree view
- Opens from: status-bar tree button (active tab), ⋯ menu "Session tree", Ctrl+Shift+B (same gating as
  Ctrl+Shift+A), sidebar row context menu "Session tree…" (any session, file-only).
- Large centered window (like the agent popup): header (session title, node counts, filter chips
  Conversation [user + assistant text, default] / Default [omp's /tree default] / All / Labeled,
  search), body = virtualized git-graph style rows: lanes with smooth branch curves, active path in
  accent, other branches muted, leaf marked "you are here", role glyph + text, label pills, compaction
  / branch-summary rows styled distinctly, relative time on the right. Hidden nodes lift visible
  descendants (omp rule); linear runs stay compact. Hover: full text tooltip + timestamp.
- Per row actions (on hover, plus right-click menu): **Fork from here** (enabled iff `is_forkable`;
  tooltip "Fork after this reply" / "Fork before this prompt"): live control → `ForkAt`; otherwise
  `Omp::fork_at` on a worker; result opens a new tab `--resume <new file>` with control. **Go here**
  (live control + idle only): `Navigate`. Copy text / copy id in the context menu.
- Live tab: leaf from `LiveSnapshot.leaf`, else `file_leaf`. Tree follows appends live (keeps scroll
  unless pinned to the leaf). Keyboard: ↑/↓ select, Enter = Go here, F = fork, Esc closes.

## Wave 7: sidebar Restart / Delete session; packaging

Facts: `docs/notes/omp-delete.md` (no omp CLI/RPC delete; omp's own deletion =
`FileSessionStorage.deleteSessionWithArtifacts` — removes `.jsonl`, sibling `<stem>/`, `.bak` leftovers;
lease probe `tryAcquireSessionLease(id)`; both exported from `@oh-my-pi/pi-coding-agent/session/session-storage`;
`/session delete` over RPC leaves a stub — never use; `/delete` injection leaves an empty replacement — not used).

### deck_core::omp addition
```rust
/// Embedded `assets/ompdeck-del.ts`; run as
/// `env OMPDECK_DEL_TARGET=<abs jsonl> OMPDECK_DEL_OUT=<tmp result file> omp --mode rpc --no-session --no-extensions -e <del.ts> </dev/null`
/// (cwd = $HOME). The extension reads the header id, takes the lease (null → `live`), calls
/// `deleteSessionWithArtifacts`, releases, writes one JSON result line. Missing result line = failure.
/// 20 s timeout; child killed on timeout/drop. Blocking — call from a worker.
pub fn delete_session(&self, file: &Path) -> Result<(), DeleteError>;
#[derive(Debug, thiserror::Error)]
pub enum DeleteError { Live, NotASession, Omp(String), Io(io::Error), Timeout }
```
Never touches anything outside what omp's own delete removes. Extension installed next to
`ompdeck-ctl.ts` (same hardening as `install_extension`).

### omp-deck
- Sidebar row menu: **Restart session** (running only; in place for a tab, else new tab; confirm, busy
  warning) and **Delete session…** (destructive, red; confirm modal naming title, project, file size,
  "subagent transcripts and artifacts are deleted too; this cannot be undone"). Running in a tab/tmux →
  the confirm says it stops the session first: close the tab, kill the tmux session (app socket), then
  retry `delete_session` while it reports `Live` for up to ~5 s (lease release after exit). `Live` after
  that → toast "open in another omp process". Success → toast + immediate sidebar rescan; recently-closed
  (Ctrl+Shift+T) and persisted tab lists drop the deleted path.

## Wave 8: "Neutral" modern look (default), Compact density, lucide icons, sidebar auto-expand

User request: their-style modern look (oh-my-pi-gui: neutral near-black, hairline borders, Geist, lucide
icons, flat, compact) added **and made the default**; lucide icons everywhere; sidebar expands only the
active tab's folder, the rest collapsed by default.

### deck_core::config
```rust
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum UiStyle { #[default] Neutral, Neon }
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum Density { #[default] Compact, Comfortable }
// AppConfig gains (both #[serde(default)], so existing config files switch to Neutral + Compact):
pub style: UiStyle,
pub density: Density,
// AppConfig loses `collapsed_groups` (expansion is no longer persisted); old files carrying the key
// still load.
```
`Theme { Dark, Light }` unchanged (light/dark axis; style is the second axis).

### deck_term
`TermTheme` gains `NeutralDark` and `NeutralLight` (existing `Dark`/`Light` = Breeze stay for Neon).
NeutralDark: background `0x0b0b0d` (must equal the chrome `bg`), foreground `0xe4e4e7`, cursor `0xfafafa`;
ANSI 0–15: `18181b f87171 4ade80 facc15 60a5fa c084fc 22d3ee d4d4d8 / 52525b fca5a5 86efac fde047 93c5fd
d8b4fe 67e8f9 fafafa`. NeutralLight: background `0xffffff`, foreground `0x18181b`, cursor `0x18181b`, ANSI
tuned for contrast on white (zinc/tailwind-600 family). OSC 10/11/4 replies follow the palette.

### omp-deck
- Neutral palettes (theme.rs): dark bg `0b0b0d`, panel/bar `0f0f11`, surface `17171a`, raised `1f1f23`,
  stroke `232327`, text `d4d4d8`, text_strong `fafafa`, weak `71717a`, accent teal `2dd4bf` (accent_a =
  accent_b = accent: no gradients), ok `4ade80`, warn `fbbf24`, danger `f87171`. Light: bg `ffffff`, panel/bar
  `fafafa`, surface `f4f4f5`, raised/stroke `e4e4e7`, text `3f3f46`, strong `09090b`, weak `71717a`, accent
  `0d9488`, ok `16a34a`, warn `ca8a04`, danger `dc2626`. Neutral rules: flat (no gradient/glow), 1 px hairline
  borders, radius 8 (popups 10), hover = subtle surface fill, selection = surface fill + strong text with a
  thin accent indicator, accent only for focus/active/primary/activity.
- Fonts: Geist (bundled, OFL; license file shipped) is the UI font in Neutral; Neon keeps Adwaita Sans.
- Icons: lucide icon font (ISC; license shipped) replaces Material Symbols everywhere (both styles);
  `egui_material_icons` dependency removed.
- Density Compact: tighter sidebar rows/headers, tab row, title row, menus, tree/picker rows, paddings.
  The status bar keeps its user-tuned size (41 px bar, model chip 1.3 × max(path, 213 px)).
- Settings → Appearance: Theme (Dark/Light), Style (Neutral/Midnight neon), Density (Compact/Comfortable);
  live switch, persisted.
- Sidebar expansion: open(group) = filtering || override(group) ?? (group == active tab's project group).
  User toggles set an in-memory override; activating a tab clears the override of its group (it opens);
  other groups without an override collapse. Collapse-all / Expand-all set overrides for every group. Not
  persisted (startup: everything collapsed except the restored active tab's group).

## Wave 9: native Usage view, detailed context tooltip

Facts: `docs/notes/omp-usage-context.md` (omp 18.12.6 `/usage` + `/context` internals, `omp usage --json`
shape, stats.db schema, extension imports, timings).

### deck-core

- `deck_core::usage` (new module; workspace dep `rusqlite` 0.40 `bundled`, read-only opens only):
  - Serde mirror of `omp usage --json` (`raw` ignored, unknown fields ignored, every optional field
    `Option`/default): `UsageSnapshot { generated_at_ms: i64, reports: Vec<UsageReport>,
    accounts_without_usage: Vec<UnavailableAccount>, disabled_credentials: Vec<UnavailableAccount> }`,
    `UsageReport { provider, fetched_at_ms, limits: Vec<UsageLimit>, reset_credits: Option<ResetCredits>,
    notes: Vec<String>, metadata: UsageMetadata { email, account_id, org_name, plan_type, tier, project_id } }`,
    `UsageLimit { id, label, scope: UsageScope { account_id, model_id, tier, window_id, shared }, window:
    Option<UsageWindow { id, label, duration_ms, resets_at_ms, reset_label }>, amount: UsageAmount { used,
    limit, remaining, used_fraction, remaining_fraction, unit: String }, status: LimitStatus
    { Ok, Warning, Exhausted, Unknown }, notes }`, `ResetCredits { available_count, redeemable_count,
    cooldown_until, credits: Vec<ResetCredit { title, remaining_count, usable, expires_at }> }`,
    `UnavailableAccount { provider, label: Option<String>, reason: Option<String> }`.
  - `fetch_usage(omp: &OmpCli-or-program, invalidate: bool, timeout: Duration) -> Result<UsageSnapshot, UsageError>`:
    runs `omp usage --json --no-extensions` (preceded by `omp usage invalidate` when `invalidate`), kills
    on timeout (default 30 s), typed errors (spawn / timeout / exit status + stderr tail / parse).
    Never runs `usage reset`.
  - `provider_cards(&UsageSnapshot, now_ms) -> Vec<ProviderCard>` — pure port of omp's
    `buildProviderCards`: group windows by `label|window.id`, mean fraction across accounts,
    `resets_in_ms` from the fullest window, window tag (`5h`/`7d`/`mo`…) only when labels collide,
    effective status (explicit, else used ≥1 exhausted / ≥0.9 warning / ok), `idle` when every window
    < 0.5 % used and no credits/notes, sort active by max fraction desc then name, display names
    (`anthropic` → `Anthropic`, `openai-codex` → `Codex`, …), accounts count + unavailable account labels,
    reset-credit summary text. `tightest(&[ProviderCard]) -> Option<(provider name, window label, used_fraction, status)>`.
  - `stats_db_path() -> PathBuf` (`~/$PI_CONFIG_DIR/stats.db`, `PI_CONFIG_DIR` default `.omp`).
  - `activity(db, days) -> Result<Activity, UsageError>`: read-only open (`SQLITE_OPEN_READ_ONLY`, WAL
    tolerated, busy timeout); per local day `cost, requests, tokens` (omp's query) and per model
    `{provider, model, cost, requests, tokens}` for the last 30 days. Missing db → empty `Activity`.
  - `Heatmap::build(&Activity, today, weeks = 53)`: 7×weeks cells `Option<u8>` 0–4 (`ceil(sqrt(v/max)·4)`,
    value = cost if any cost else requests), month labels, totals (past year cost/requests/tokens),
    last-30-days totals + daily series, busiest day, active days, longest streak.
- `deck_core::ctl`: `LiveSnapshot.context: Option<ContextBreakdown>` (absent from older extensions →
  `None`). `ContextBreakdown { model: Option<String> /* display name */, context_window: u64,
  used_tokens: u64, anchored: bool, categories: Vec<ContextCategory { id, label, tokens }>,
  auto_compact_buffer: u64, free_tokens: u64, threshold_tokens: Option<u64>, snapcompact_saved: Option<u64>,
  tools: Vec<(String, u64)> /* per-tool schema tokens, desc */, skills: u32, computed_at_ms: i64 }`.
  Extension (`ompdeck-ctl.ts`) computes it with `computeSessionContextBreakdown(session,
  {snapcompactSavings:true})` (+ per-tool schema tokens) on `session_start`, `session_switch`,
  `agent_end`, `turn_end`, model change, and caches it; snapshots carry the cached value (no model blob).
  Any failure → field omitted.

### omp-deck

- Usage state owned by the app: background worker runs `fetch_usage` ~3 s after startup, every 5 min,
  when the Usage view opens and data is > 60 s old, and on manual refresh (`invalidate = true`); and
  `activity` when the view opens (≥ 10 min apart). UI thread never blocks.
- Status bar **usage chip** (gauge icon + tightest `NN% left`, coloured by status; dropped first when
  narrow; hidden when there are no reports). Hover: compact per-provider cards (bars = remaining,
  `resets in …`). Click, ⋯ menu "Usage…", app menu, Ctrl+Shift+U → **Usage view**.
- **Usage view** (modal like the session tree): header "Usage" + "Updated Xs ago" + refresh;
  tabs **Overview** (stats strip: Tightest quota, Past year, Last 30 days + sparkline, Busiest day;
  provider cards with remaining bars, `NN% left`, `resets in`, resets banked, unavailable accounts,
  "untouched: …" footnote; activity heatmap 53×7 with month labels + "N active days · longest streak M
  days") and **Details** (per provider table Limit | Account | Left | Resets; top models last 30 days).
  Empty states: no accounts (hint `omp /login`), fetch error inline with Retry, omp missing.
- **Context tooltip**: rich hover on the status-bar context chip: model + window header,
  `used / window (pct)` (+ "estimated" when not anchored), stacked category meter with the
  auto-compact threshold mark, legend rows (swatch, label, tokens, % of window) for categories, free
  space, autocompact buffer, "N left before auto-compaction at P %" / "past auto-compaction", snapcompact
  savings, top tool schemas (≤ 8 + "N more"), skills count. Without a breakdown (no live control /
  old extension): current numbers + note that the full breakdown needs live control.
- Both styles (Neutral default, Neon) and Compact/Comfortable density; lucide icons; no per-frame allocation.

## Tab activity indicators, "Open tabs" sidebar section

- `tabs::TabStatus::of(exit_code, Activity::from_title(title), bell && !active)`: precedence
  Exited > Attention (`π !`) > Busy (spinner glyph) > Bell > Idle. `tabs::paint_status` draws it (check/x,
  amber `!` badge, `widgets::Spin`, warn dot; Idle → caller's own icon/LED) for the tab strip, the
  "Open tabs" rows and session rows; `tabs::paint_attention_pill` tints Attention pills/rows. No pulse:
  idle frames stay free (the spinner repaints at 10 fps only while visible).
- Tab labels use `Tab::label` (omp's `π <glyph>` prefix stripped, also from stored fallback titles);
  `Tab::title` stays raw for persistence/kill labels.
- Background tabs: `Terminal::poll` runs every frame for every tab; title events repaint through the
  hidden-tab gate (≤ 1 per 250 ms), so indicators update without input.
- Sidebar: "Open tabs" (collapsible, count + amber badge when any tab waits, ≤ 6 rows then scrolls) pinned
  above the scope chip; click → `SidebarAction::ActivateTab(i)`, ✕/middle-click → `CloseTab(i)` (app routes
  both through `apply_tab_action`, i.e. `request_close`). Session rows whose file is open in a tab show
  that tab's Busy/Attention (`SidebarInput::tab_activity`, built per frame) instead of the running LED.

## Wave 10: project sync (per-project `.omp` + sessions to Google Drive)

Facts: `docs/notes/project-sync.md` (omp pack/unpack format + conflict rules, project `.omp`, Drive resumable
upload, ureq traps). A **separate workflow** from the config backup (`google-drive-backup.md`): the config
backup's files, archive, list query (`omp-backup-` prefix), retention, card and auto timer stay unchanged.
Sessions are restored only through `omp session unpack` (omp performs the store writes).

### deck_core::config (additions, `#[serde(default)]`, old state files load)

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncRow { pub path: PathBuf, pub omp: bool, pub sessions: bool }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncInterval { Off, Hourly, #[default] Every6Hours, Daily }
impl SyncInterval { pub fn period(self) -> Option<Duration>; pub const ALL: [SyncInterval; 4]; pub fn label(self) -> &'static str }
// AppConfig gains: pub project_sync: Vec<SyncRow> (default empty), pub project_sync_interval: SyncInterval.
```

### deck_core::gdrive::project (new; re-exported from `gdrive`)

```rust
pub const PROJECT_PREFIX: &str = "omp-project-";
pub const KEEP_PROJECT_BACKUPS: usize = 3;          // per (host, path_id)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parts { pub omp: bool, pub sessions: bool }
impl Parts { pub fn any(self) -> bool }

pub enum OmpDir { Missing, Global, Present { files: u64, bytes: u64 } }   // Global: folder is $HOME
pub struct ProjectSizes { pub omp: OmpDir, pub sessions: u64, pub session_bytes: u64 } // jsonl + artifacts
/// Blocking (worker). Sessions = `sessions::scan(root)` group whose `cwd` equals `project`.
pub fn measure(root: &SessionsRoot, project: &Path) -> std::io::Result<ProjectSizes>;
/// Lexical identity of a project path: sha256(path bytes) → first 16 hex.
pub fn path_id(project: &Path) -> String;

pub enum Stage { Scanning, Packing, Uploading, Downloading, Verifying, RestoringOmp, RestoringSessions }
pub struct Progress { pub stage: Stage, pub done: u64, pub total: u64 }  // bytes or items per stage

/// Per-path local sync state, `config_dir()/project-sync.json` (0600, atomic write); separate from
/// google-drive.json so the two Drive lanes never race on one file.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectSyncState { pub projects: BTreeMap<PathBuf, ProjectSynced> }
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ProjectSynced { pub fingerprint: String, pub parts: Parts, pub last: LastBackup }

pub struct ProjectBackup {            // one Drive file, parsed from appProperties (no download)
    pub file: BackupFile, pub host: String, pub path_id: String, pub path: String, pub name: String,
    pub omp_bytes: Option<u64>, pub sessions: Option<u64>, pub session_bytes: Option<u64>,
}
pub struct ProjectBackupOutcome { pub backup: ProjectBackup, pub synced: ProjectSynced, pub pruned: usize }

pub struct SessionPlan { pub id: String, pub title: String, pub modified_unix: i64, pub bytes: u64, pub new_id: bool }
pub struct ProjectRestorePlan {
    pub target: PathBuf, pub create_target: bool,        // target missing → created on apply
    pub omp_exists: bool, pub omp_overwritten: Vec<String>, pub omp_added: Vec<String>, // rel to .omp
    pub sessions: Vec<SessionPlan>,
}
pub struct PreparedProjectRestore { pub backup: ProjectBackup, pub plan: ProjectRestorePlan, /* staging */ }
pub struct ProjectRestoreReport {
    pub omp_written: usize, pub omp_overwritten: usize, pub safety_dir: Option<PathBuf>,
    pub sessions_added: Vec<PathBuf>, pub sessions_failed: Vec<(String /*title or id*/, String /*why*/)>,
}

impl Service {
    pub fn project_state(&self) -> ProjectSyncState;   // small file; missing/corrupt → default (+warn)
    /// None = unchanged since the last backup of this path with these parts (skipped unless `force`).
    /// `!parts.any()` or nothing present → Err(NothingToBackUp).
    pub fn backup_project(&self, root: &SessionsRoot, project: &Path, parts: Parts, force: bool,
        cancel: &AtomicBool, progress: &(dyn Fn(Progress) + Sync)) -> Result<Option<ProjectBackupOutcome>>;
    pub fn list_projects(&self) -> Result<Vec<ProjectBackup>>;   // newest first, kind=project only
    /// Downloads + verifies into staging; touches nothing outside the cache dir.
    pub fn prepare_project_restore(&self, backup: &ProjectBackup, target: &Path, parts: Parts,
        root: &SessionsRoot, cancel: &AtomicBool, progress: &(dyn Fn(Progress) + Sync)) -> Result<PreparedProjectRestore>;
    /// `omp: None` (omp not installed) allows a `.omp`-only restore; sessions then fail with `Error::Io`.
    pub fn apply_project_restore(&self, prepared: PreparedProjectRestore, omp: Option<&Omp>,
        cancel: &AtomicBool, progress: &(dyn Fn(Progress) + Sync)) -> Result<ProjectRestoreReport>;
}
```

- **Service concurrency**: the project lane and the config lane may run at once; each job builds its own
  `Drive` (own access token). Only the project lane writes `project-sync.json`.
- **Snapshot**: `.omp` = regular files under `<project>/.omp` (skip `node_modules`, `.git`, symlinks,
  non-regular; depth ≤ 16; not offered when `project == $HOME`). Sessions = scan group with `cwd == project`;
  each jsonl cut at its last `\n` at snapshot time, its `<stem>/` artifacts, and every blob referenced (regex in
  facts) from `<agent>/blobs/<hex>` (missing → skipped + logged). **Fingerprint** = sha256 over parts flags and
  each included file's (part, rel path, len, mtime ns) — metadata only, no content reads.
- **Archive** `omp-project-<YYYYMMDDTHHMMSSZ>-<host>-<slug>.tar.gz` (slug = sanitized folder name ≤ 40),
  streamed to `$XDG_CACHE_HOME/omp-deck/project-sync/<random>/` (never /tmp; removed after; stale > 24 h
  dirs swept at job start). Members: `omp/<rel>`, `sessions/<id>/session.jsonl`, `sessions/<id>/artifacts/<rel>`,
  `blobs/<hex>` (deduped), then **`manifest.json` last**: `{format:1, kind:"project", created, host,
  app_version, omp_version, project (abs path), name, fingerprint, omp: Option<{files:[{path,size,sha256,mode}]}>,
  sessions: Option<{items:[{id, file_name, title, modified_unix, size, sha256, artifacts:[ManifestFile],
  blobs:[hex]}], blobs:[{sha256,size}]}>}`.
- **Upload**: resumable, 8 MiB chunks (256 KiB multiple), 308 handled (`max_redirects(0)`), resume via
  `bytes */total` on network/5xx (≤ 5 attempts, backoff), 404/410 → one fresh session; no global timeout over
  the transfer (stall timeout ≥ 60 s). appProperties (each ≤ 124 B): `kind=project`, `host`, `pid` (path_id),
  `fp` (fingerprint), `name`, `path` (if too long: `…` + tail), `omp` (bytes), `sess` (count), `sessb` (bytes).
  After upload: keep newest `KEEP_PROJECT_BACKUPS` per (host, pid); prune failure only logged. List query
  `appProperties has { key='kind' and value='project' } and trashed = false`, `spaces=appDataFolder`, paginated.
- **Prepare restore**: stream-download to the cache dir, extract to staging with validation (regular files only,
  safe relative paths under the four prefixes, manifest present exactly once, every member listed with matching
  size + sha256, no extras, `format==1`, `kind=="project"`). Plan: `.omp` overwritten/added relative paths
  (when `parts.omp`); sessions with `new_id = true` when any `*_<id>.jsonl` exists in a project dir of the
  sessions root. `parts` not present in the backup → `InvalidBackup`. `.omp` into `$HOME` → error.
- **Apply**: create target if missing. `.omp`: safety copy of every overwritten file to
  `$XDG_DATA_HOME/omp-deck/restore-safety/project-<stamp>/` (newest 5 project copies kept), then atomic writes
  (exec bit kept); **never deletes** local files. Sessions: per item build a pack-format tar in staging
  (`manifest.json {version:1,id,cwd:<original project>,omp:<omp_version or "unknown">}`, `session.jsonl` with the
  header id rewritten to a fresh UUIDv7-shaped id when `new_id`, `artifacts/**`, its `blobs/<hex>`), then
  `omp session unpack <tar> --cwd <target>` (stdin null, terminal env vars removed per facts, 120 s timeout,
  process killed on timeout). stdout path → `sessions_added`; non-zero exit or unexpected stderr mode →
  `sessions_failed` entry (continue with the rest). Cancel between items.

### omp-deck

- Settings nav gains **Sync** (cloud icon) right under OmpDeck; omp catalog tabs shift down. Page:
  account strip (email / Sign in / "not available in this build", reusing `DriveAction`), **Projects** table
  (rows from `cfg.project_sync`): folder (`~`-abbreviated, tooltip full path, Change…), `.omp` switch + size
  (disabled with reason when Missing/Global), Sessions switch + `N sessions · size`, last backup (relative age,
  Never, or error with tooltip), row actions Back up now (progress while running) and Remove. "Add folder…"
  (native picker when kdialog/zenity exists, else typed path). Interval selector (`SyncInterval`). Short note:
  stored in the private Drive app folder, counts toward Drive storage, `.omp` may contain secrets.
  **Restore…** opens the restore dialog.
- **Restore dialog** (modal): cloud projects grouped by (host, pid): name, path, host ("this PC" when equal),
  newest date, parts + sizes, older versions selectable. Target folder (preset to the original path when it
  exists here, else empty; Browse…/typed; missing → "will be created"), `.omp` / Sessions checkboxes (only parts
  in the backup). Continue → prepare (progress, Cancel) → confirm with the plan: warning (danger) when the
  target `.omp` exists and N files will be overwritten (first few listed); "K sessions will be added alongside
  existing ones (J get a new id)". Restore → progress → toast summary; sessions failures listed; sidebar
  refresh (`SessionWatcher::refresh_now`).
- **Sidebar**: project header menu gains "Back up to Google Drive…" and "Restore from Google Drive…"; the
  shared session menu (rows + tabs) gains a "Google Drive" submenu with both for `entry.cwd`. Hidden when the
  build has no credentials; disabled with a reason when signed out or the project lane is busy. Backup dialog:
  folder, both switches with sizes (measured on a worker), "Keep this folder in Sync" (default on: adds/updates
  the row), "Back up now" (forced). Restore from the sidebar = restore dialog with the target fixed.
- **Lane**: its own job phase, one project job at a time, independent of the config lane; progress throttled
  (≤ 10 repaints/s). Auto: first pass 2 min after startup/sign-in, then every `SyncInterval::period()`, rows in
  order, unchanged rows skipped by fingerprint; first automatic failure per pass toasts, the rest log. Sizes are
  measured on a worker when the page/dialog opens, after row changes and after jobs — never per frame.

### Wave 10 fix-loop additions

- `OmpDir::Symlink`: a symlinked `<folder>/.omp` is never followed or backed up (switch disabled with a reason);
  symlinked artifact dirs and skipped `.omp` entries (symlinks, special files, non-UTF-8 or `\` names, too deep,
  unreadable) are listed in `ProjectBackupOutcome.skipped: Vec<String>` (`"<what>: <reason>"`).
- `ProjectSynced.unresolved_blobs: BTreeMap<String, String>` (`#[serde(default)]`): missing/damaged blob state
  observed while packing; any later change makes the row count as changed.
- Sessions with a jsonl/artifact/blob member > 2 GiB (omp's pack limit) are excluded at backup (listed in
  `skipped`) and flagged at prepare in `SessionPlan.problem: Option<String>` (reported failed, omp not run).
- Restore never writes through symlinks: `<target>/.omp` that is a symlink or file, or a target that is a file,
  fails at prepare; a symlinked file/intermediate dir or a file/folder clash inside `.omp` is listed in
  `ProjectRestorePlan.omp_skipped` / `ProjectRestoreReport.omp_skipped` and not written (re-checked at apply).
  A target folder that is itself a symlinked path is allowed (user-chosen). Concurrent symlink swaps between
  check and write are out of the threat model (needs write access to the user's own folders).
- Prepare checks each archived session header id equals its item id. Apply runs omp with
  `PI_CODING_AGENT_DIR = <prepared root>.agent_dir()` and `TMPDIR`/`TMP`/`TEMP` inside the staging dir; omp
  failure text is reduced to its `error:` line (≤ 240 chars).
- Cancel after the first write returns `Ok(ProjectRestoreReport { cancelled: true, .. })` with what was done;
  `Err(Cancelled)` (displays "cancelled") only when nothing was written.
- Transfers: initiation, chunks and downloads retry network/429/5xx with bounded backoff; a 308 that does not
  advance, or a download that does not pass its high-water mark, counts as a failure. The access token is
  fetched before packing (offline fails at once). `Service.cache_root` (pub) = `$XDG_CACHE_HOME/omp-deck/project-sync`.

## Wave 11: cloud backup management, deliberate add, session opt-out, config backup interval

### deck-core
- `config::SyncInterval` gains `Every10Minutes` ("Every 10 minutes") and `Every30Minutes` ("Every 30 minutes");
  `ALL` = `[Off, Every10Minutes, Every30Minutes, Hourly, Every6Hours, Daily]`. Existing serialized names unchanged.
  `#[default]` stays `Every6Hours` (project sync).
- `AppConfig.drive_auto_backup: bool` is replaced by `AppConfig.drive_backup_interval: SyncInterval`
  (config backup lane; default `Every10Minutes`). Migration on load: a stored `"drive_auto_backup": false` with no
  `drive_backup_interval` key → `Off`; `true`/absent → `Every10Minutes`. The old key is never written again.
- `config::SyncRow` gains `#[serde(default)] excluded_sessions: BTreeSet<String>` (session ids opted out; every
  other session, including ones created later, is backed up).
- `gdrive::ProjectSizes` (now `Clone`, not `Copy`) gains `session_list: Vec<SessionSize>`, newest first, for every
  session of the project (exclusions not applied; `sessions`/`session_bytes` stay totals over all of them).
  `SessionSize { id: String, title: String /* title, else first-prompt snippet, else "(untitled)" */,
  modified: SystemTime, bytes: u64 /* jsonl + artifacts */ }`.
- `Service::backup_project(root, project, parts, excluded: &BTreeSet<String>, force, cancel, progress)`: sessions
  whose id is in `excluded` are left out silently (not in `skipped`); the fingerprint covers the excluded set, so
  changing it counts as a change. Selected parts with nothing left (all sessions excluded, no `.omp`) →
  `Error::NothingToBackUp`.
- `Service::delete_project_backup(&self, backup: &ProjectBackup) -> Result<()>`: Drive `files.delete` (permanent,
  bypasses trash; appDataFolder files cannot be trashed anyway); 404 = already gone = `Ok`. Refuses (typed error)
  a file whose appProperties `kind` is not `project`. If the deleted file's name equals the `last.name` of a
  `ProjectSyncState` entry, that entry is removed (state saved) so the next pass uploads that project again.

### omp-deck
- Add folder / Change folder only adds/changes the row: new rows start with `.omp` and Sessions **off** and no
  exclusions; nothing is packed or uploaded until the user clicks Back up now (or the row is later due in an
  automatic pass with something selected). Adding a row never makes a pass due immediately (if `next_auto` is in
  the past, push it to now + period). Sizes are still measured on a worker.
- Sessions cell: count/size of the **included** sessions (`9 of 11 sessions · 80 MB` when some are excluded,
  computed on measure/exclusion change, never per frame) + a "Choose…" button → dialog "Sessions to back up":
  filter box, All / None, checkbox list (checked = backed up; title, age, size), note "New sessions are backed
  up automatically", Save / Cancel. Save stores the unchecked ids (only ids in the current list).
- Every backup of a row (manual, automatic, sidebar dialog for a folder in the table) passes the row's
  exclusions; the sidebar backup dialog shows "N sessions excluded on the Sync page" when nonzero.
- Delete cloud backups (project lane, one job at a time, never while another project job runs):
  - Restore dialog: each version has a trash button → confirm "Delete permanently" (project, PC, date, size,
    "This can't be undone"), then the list refreshes.
  - Sync page: "Manage cloud backups…" next to Restore… → dialog listing every project backup grouped by project
    → PC ("this PC" for ours) → versions (date, size, parts), version checkboxes + group select-all, Refresh,
    footer "N selected · size", "Delete permanently…" → confirm (count, total size, can't be undone) → worker
    deletes sequentially with progress "Deleting i of n", stops on cancel, reports deleted/failed, refreshes.
- Settings → OmpDeck → Google Drive backup: the "Back up automatically" switch becomes a dropdown over
  `SyncInterval::ALL` ("Off" disables; others = period between checks; unchanged config is still skipped). The
  first automatic check after startup/sign-in/turning it on stays 45 s.

## Wave 12: native Windows + OmpDeck host (tmux replacement)

Facts: `docs/notes/windows-host.md`. Goal: OmpDeck builds, tests and runs natively on Windows 10/11 x64
(arm64 best effort) with sessions that survive closing a tab or the app, via our own per-session host daemon.
Linux keeps tmux as the default backend; the host is the Linux fallback when tmux is missing (replacing the
no-persistence "direct" mode there) and can be chosen explicitly. Every crate compiles for
`x86_64-pc-windows-msvc` with clippy `-D warnings` (cargo-xwin locally, CI natively).

### New crate `crates/deck-host` (lib + bin `omp-deck-host`)
Depends on nothing of ours. Deps: alacritty_terminal (headless Term), serde/serde_json, thiserror, log,
getrandom, sha2, dirs; `libc` (unix); `windows-sys` 0.61 (windows). No portable-pty, no tokio.

**Namespaces and endpoints.** `Namespace::new(ns)` with `ns` = `omp-deck` (app) or `omp-deck-test-<unique>`
(tests; `[A-Za-z0-9_-]{1,40}`). Session names `[A-Za-z0-9_-]{1,64}` (`session_name_for` already conforms).
- Unix: socket `<base>/<ns>/<name>.sock`, `<base>` = `$XDG_RUNTIME_DIR/omp-deck-host` else
  `<temp_dir>/omp-deck-host-<uid>`; base and ns dirs 0700 and owned by us (refuse otherwise); sun_path length
  checked (typed error).
- Windows: named pipe `\\.\pipe\omp-deck-host.<user>.<ns>.<name>`, `<user>` = 16 hex of sha256(current user
  SID string). Pipe DACL = current user SID only (+SYSTEM), `PIPE_REJECT_REMOTE_CLIENTS`, first instance created
  with `FILE_FLAG_FIRST_PIPE_INSTANCE` (a second daemon for the same name fails). Listing = enumerate
  `\\.\pipe\` entries with the `omp-deck-host.<user>.<ns>.` prefix.

**Wire protocol** (version `PROTOCOL = 1`; both sides send `Hello{protocol, version}` first; a major mismatch is
a typed error, the GUI can still `Kill`). Frames: `u32 LE length` (≤ 1 MiB) + `u8 kind` + payload. Byte
payloads raw (`Input`, `Output`, `Snapshot` chunks), control payloads JSON (unknown fields ignored).
- client -> daemon: `Hello`, `Attach{cols, rows, cell_width, cell_height, history_lines, palette}`, `Input(bytes)`,
  `Resize{cols, rows, cell_width, cell_height}`, `Palette(Palette)`, `Detach`, `Kill`, `Info`, `Ping`.
- daemon -> client: `Hello`, `Info(HostInfo)`, `Snapshot(bytes)`… `SnapshotEnd`, `Output(bytes)`,
  `Exited{code: Option<i32>}`, `Displaced` (another client attached), `Error(String)`, `Pong`.
- `Palette`: 16 ANSI + fg + bg + cursor as `[u8;3]` (answers OSC 4/10/11/12 queries).
- `HostInfo { name, protocol, host_version, daemon_pid, child_pid, child_started_unix_ms, cwd, argv,
  tty: Option<String> /* unix slave path */, term_session_id: Option<String> /* windows */,
  attached: bool, cols, rows }`.
- One attached client: a new `Attach` sends `Displaced` to the old one and closes it. Non-attach connections
  (Info/Kill/Ping) are served concurrently.

**Client API** (`deck_host::Client`, blocking; callers run it on workers):
- `Namespace::list(&self) -> Vec<String>` (alive = endpoint accepts + Hello within 500 ms), `info(name) ->
  Result<HostInfo>`, `has(name) -> bool`, `kill(name) -> Result<()>` (returns after the daemon confirmed or the
  endpoint vanished, ≤ 5 s).
- `create_or_attach(&Namespace, &SessionSpec, AttachParams) -> Result<Attached>`: attach if alive (never
  respawn), else spawn the daemon detached (`daemon_exe()` = `omp-deck-host[.exe]` next to `current_exe()`,
  env override `OMP_DECK_HOST_EXE`) with the spec passed through a 0600 file the daemon deletes, wait ≤ 5 s
  for the endpoint (daemon startup errors are reported from its log), then attach.
- `SessionSpec { name, program, args: Vec<String>, cwd, env_set: Vec<(String,String)>, env_remove: Vec<String> }`.
- `Attached` splits into `AttachedRx` (blocking `recv() -> Result<HostEvent>`; `HostEvent::{Snapshot(Vec<u8>)
  /* whole replay */, Output(Vec<u8>), Exited(Option<i32>), Displaced, Closed}`) and a cloneable
  `AttachedTx` (`input(&[u8])`, `resize(..)`, `palette(..)`, `kill()`, `detach()`); drop = detach.

**Daemon** (`omp-deck-host serve --ns <ns> --name <name> --spec <file>`; also `ls`, `info <name>`,
`kill <name>`, `dump <name>` (attach, print the snapshot as plain text, detach) for debugging and CI):
- Detached from the GUI: Unix `setsid` + stdio to null; Windows `DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP |
  CREATE_NO_WINDOW` (+ `CREATE_BREAKAWAY_FROM_JOB`, retried without it on access denied). Release builds use the
  Windows GUI subsystem (no console window). Logs to `<cache_dir>/omp-deck/host/<ns>/<name>.log` (≤ 1 MiB).
- Child PTY, own module `deck_host::pty`: Unix `openpty` + `Command` (`setsid`, `TIOCSCTTY`, slave as stdio);
  Windows ConPTY via `conpty.dll` sideload (exe dir) with kernel32 fallback, flags chosen by CI byte probes
  (start with `RESIZE_QUIRK | WIN32_INPUT_MODE`; PASSTHROUGH only with the sideloaded DLL on build ≥ 22621 if
  the probe proves it helps), child created `CREATE_SUSPENDED` with `EXTENDED_STARTUPINFO_PRESENT`, assigned to a
  `KILL_ON_JOB_CLOSE` job, then resumed; MSVCRT argv quoting; `.cmd`/`.bat` via `cmd.exe /d /c`. No panics on
  ConPTY errors (typed errors).
- Output loop: one reader thread; each chunk goes to `vte::ansi::Processor` -> headless `Term`
  (`scrolling_history = 50_000`), to a side-state scanner, and to the attached client as `Output`. The daemon's
  `Term` answers every query (DA/DSR/CPR/kitty `CSI ? u`/OSC 4/10/11/12 with the last client palette) by writing
  to the PTY, attached or not. Windows Term config: `kitty_keyboard = false`; Unix: `true`.
- Side-state the snapshot needs beyond `Term`: title (from `Event::Title`), `CSI > 4;n m`, `?9001`, `?2031`,
  `?2026` state, scroll region (DECSTBM). Images (APC) are not retained (same as tmux today).
- Snapshot on attach (taken under the Term lock between reads, then live `Output` follows): VT replay that a fresh
  alacritty `Term` turns into the same state: reset; up to `history_lines` scrollback rows then screen rows of the
  primary grid (SGR, zero-width, OSC 8, WRAPLINE joins); if alt screen active: `?1049h` + alt grid; cursors,
  DECSC saved cursor, DECSCUSR, every `TermMode` bit, kitty flags (`CSI = flags ; 1 u`), modifyOtherKeys,
  `?9001`, `?2031`, OSC 4/10/11 overrides, title, scroll region. Pure function
  `snapshot::encode(&Term, &SideState, history_lines) -> Vec<u8>`, tested by round-tripping through a fresh Term.
- Resize: PTY + Term resize (cell pixel size kept for `CSI 14t`). Child exit: `Exited{code}` to the client,
  endpoint removed, daemon exits. `Kill`: Unix `kill(-pgid, SIGHUP)`, `SIGKILL` after 2 s; Windows
  `TerminateJobObject`; then exit.

### deck-core
- Depends on deck-host. New `deck_core::mux`:
  - `BackendChoice { Auto (default), Tmux, Host }` persisted as `AppConfig.session_backend` (`#[serde(default)]`);
    Windows always resolves to Host.
  - `enum Mux { Tmux(Tmux), Host(HostMux) }`; `Mux::detect(choice) -> Option<Mux>` (Auto: tmux if installed else
    host if `daemon_exe()` exists; env `OMP_DECK_TMUX_SOCKET` / `OMP_DECK_HOST_NS` select test namespaces).
  - `kind()`, `namespace() -> &str`, `launch(name, cwd, argv) -> Launch` with `Launch::Pty(LaunchSpec)` (tmux,
    unchanged command) | `Launch::Host(deck_host::SessionSpec)` (env per windows-host.md "Env for omp
    children", Windows adds `TERM_SESSION_ID=<16 hex>` derived from the session name),
    `alive_sessions()`, `has_session()`, `kill()`, `session_files(root)` (host: breadcrumb of each session via
    `HostInfo.tty` / `term_session_id`), `pane(name) -> Option<Pane { breadcrumb_key: String, started:
    SystemTime }>` (replaces `pane_tty` + `pane_started`; tmux keeps its `/proc` path on Linux).
  - `tmux::direct_launch` stays only as the last-resort Unix fallback when neither backend is available.
- `deck_core::platform` (one home for OS differences): `hostname()`, `process_start(pid)`, `find_on_path(name)`
  (PATHEXT on Windows), `is_executable`, `create_private_dir`, `write_private`, `open_private_file`,
  `file_identity(&fs::Metadata/&File) -> Option<FileId>` (Unix dev/ino, Windows volume serial + file index),
  `KillableCommand`/`spawn_killable` (Unix process group, Windows job object; `kill_tree()`), `open_path(path)`
  (xdg-open / ShellExecuteW), `reveal(file)` (file manager selecting the file; `explorer /select,` on Windows),
  `user_dirs` fallbacks via `dirs` only (no `~/.config`-style hardcoding). Every `std::os::unix` use outside
  `platform` and `cfg(unix)` tests is gone.
- ctl on Windows: pipe `\\.\pipe\ompdeck-ctl.<user>.<key>` passed as `OMPDECK_CTL_PIPE` (Rust computes, TS never
  derives); `ompdeck-ctl.ts` uses `node:net` on win32, no lock file, no mode check, no socket unlink; Unix path
  unchanged. `Command::env` replaces the `env` wrapper argv on both OSes.
- omp on Windows: `Omp::detect` searches PATH (PATHEXT), `%LOCALAPPDATA%\omp\omp.exe`,
  `%USERPROFILE%\.bun\bin\omp.exe`; `.cmd` runs through `cmd.exe /d /c`; install argv = PowerShell
  `& ([scriptblock]::Create((irm https://omp.sh/install.ps1))) -Binary` (`-NoProfile -ExecutionPolicy Bypass`).
- gdrive on Windows: no POSIX modes (stored as 0o644/0o755 by origin), `path_id` over `as_encoded_bytes()`,
  restore skips (and lists in `omp_skipped`) names invalid on Windows (`<>:"|?*`, control chars, reserved
  CON/PRN/AUX/NUL/COM1-9/LPT1-9, trailing dot/space) and never writes through reparse points.
- `status.rs` breadcrumb trust uses `Mux::pane`; a bare `omp` title is noise on Windows.

### deck-term
- Compiles and works on Windows: clipboard without PRIMARY there; local-PTY path (`TrackedPty`, alacritty
  `EventLoop`) is `cfg(unix)`; Windows uses only remote terminals. Windows font discovery (Cascadia Mono ->
  Consolas -> Courier New; fallbacks Segoe UI Symbol, Segoe UI Emoji, Segoe UI) with `fc-match` kept on Unix.
- `TermOptions.kitty_keyboard: bool` (default true).
- Remote terminals: `pub trait Remote: Send` with `fn split(self: Box<Self>) -> (Box<dyn RemoteRx>,
  Box<dyn RemoteTx>)`; `RemoteRx::recv(&mut self) -> io::Result<RemoteEvent>` (`Snapshot(Vec<u8>)`,
  `Output(Vec<u8>)`, `Exited(Option<i32>)`, `Displaced`, `Closed`); `RemoteTx: Send + Sync` with `input`,
  `resize(cols, rows, cell_w, cell_h)`, `palette([[u8;3]; 19])`, `kill`, `detach`.
  `Terminal::connect(Box<dyn Remote>, TermOptions, egui::Context) -> Terminal` (same `TerminalView`, events,
  title, bell, exit, screen_text). In remote mode the local `Term` never answers queries (the host does) except
  OSC 52 clipboard loads; a `Snapshot` resets the local Term and replays; `Displaced` shows
  `[attached elsewhere]` and stops input; drop = detach.
- win32-input-mode: track `CSI ? 9001 h/l` in output (local and remote, like modifyOtherKeys). While on, keys are
  encoded as `CSI Vk;Sc;Uc;Kd;Cs;Rc _` (key down and up; `Uc` from the text event; `Cs` = Windows control-key
  state; `Sc` from `MapVirtualKeyW` on Windows, 0 elsewhere); mouse stays VT; paste per Windows Terminal's
  behavior. Pure encoders, exact-byte tests.

### omp-deck
- Uses `Mux` everywhere (no direct `Tmux` outside deck-core): launch -> `Terminal::spawn` for `Launch::Pty`,
  worker `create_or_attach` -> `Terminal::connect` for `Launch::Host` (tab shows "Starting…" meanwhile; errors in
  the tab); running indicators, Stop, Restart, Delete/Move kill, tab restore, status-bar breadcrumb via `Mux`.
- Settings → OmpDeck (Unix only): "Session backend" Auto / tmux / OmpDeck host (applies to new tabs; live
  sessions keep their backend until closed). No-tmux banner replaced: Auto with host available needs none;
  direct fallback keeps the banner.
- Windows app: `windows_subsystem = "windows"` (release), embedded icon + version info, client-side title bar
  (unchanged look), folder picker via `rfd` (Windows only; kdialog/zenity stay on Linux), `platform::open_path`
  / `reveal`, UI fonts fallback (Segoe UI Variable / Segoe UI), config in `dirs::config_dir()/omp-deck`.

### Packaging / CI
- CI: add `windows-latest` job (clippy `-D warnings`, `cargo test --workspace`, Windows-only host tests incl.
  ConPTY byte probes with a test child that echoes what it reads), keep Linux job (+ host backend tests).
- Release: `omp-deck-<v>-x86_64-windows.zip` (omp-deck.exe, omp-deck-host.exe, conpty.dll, OpenConsole.exe,
  licenses incl. ConPTY MIT notice) + sha256; arm64 zip if its runner builds green. `packaging/install.ps1`
  (latest release, sha256 check, `%LOCALAPPDATA%\Programs\OmpDeck`, Start Menu shortcut, rename-aside of locked
  exes, installs omp via omp.sh/install.ps1 unless `-NoOmp`, `-Uninstall`). Linux tarball/AppImage/deb/rpm also
  ship `omp-deck-host`.

### Wave 12 as built (Wave A; supersedes the sketch above where they differ)
- deck-host: no `Client` struct. `Namespace::{new, as_str, log_path, list, has, info, kill}`; free fns
  `daemon_exe() -> Option<PathBuf>` (env `OMP_DECK_HOST_EXE`), `start(ns, spec, cols, rows)` (create, no attach),
  `create_or_attach(ns, spec, AttachParams) -> Result<Attached>`, `attach(ns, name, params)`,
  `peek(ns, name, history_lines) -> (HostInfo, Vec<u8>)` (frame `Peek` 0x0a: snapshot without displacing/resizing).
  `Attached::split() -> (AttachedRx, AttachedTx)`; `AttachedRx::recv() -> Result<HostEvent>`; `AttachedTx: Clone`
  `input/resize/palette(&Palette)/kill/detach -> Result<()>`. AttachParams cols/rows 0 = keep size.
  `snapshot::encode(&mut Term, ..)`; kitty flags replayed as `CSI > f u`; title tracked by the output scanner.
  Env `OMP_DECK_CONPTY_FLAGS` (default 0x6 = RESIZE_QUIRK | WIN32_INPUT_MODE). CLI `new|ls|info|dump|kill`,
  `--ns` default `omp-deck`.
- deck-core: `platform::{hostname, process_start, home_or_root, root_dir, no_window, path_extensions,
  is_executable, find_in, find_on_path, create_private_dir(_all), open_private_file, write_private, FileId,
  file_identity, file_mode, set_file_mode, open_regular_file, is_link, path_bytes, tree_open_files,
  KillableCommand, spawn_killable, KillableChild::kill_tree, open_path, reveal}`; `mux::{BackendChoice, MuxKind,
  MuxError, Pane, Launch, LaunchSpec, Mux, HostMux, host_env, term_session_id, is_noise_title, title_is_noise,
  session_name_for, new_session_name, direct_launch, effective_cwd, terminal_env, HOST_NAMESPACE,
  TMUX_SOCKET_ENV, HOST_NS_ENV, HOST_ENV_REMOVE}` (moved from `tmux`, no re-exports; the env overrides accept only
  `omp-deck-test-…` names that are valid host namespaces, ≤ 40 bytes); `Mux::{detect, kind, namespace, launch,
  launch_with_env, alive_sessions, has_session, kill, session_files, pane}`; `ctl::with_ctl(omp_argv, ext)` (no
  dir/env wrapper) + `ctl_env(dir) -> io::Result<Vec<(&'static str, OsString)>>` (`OMPDECK_CTL_DIR`; Windows
  `OMPDECK_CTL_PIPE` + `OMPDECK_CTL_TOKEN` = sha256(secret ‖ pipe), secret in `default_key_file()`);
  `ctl_env_with_key(dir, key_file)` / `CtlClient::spawn_with_key` inject the key file (tests).
  `TabState.session_name` (serde alias `tmux_name`).
  `StatusTarget.mux: Option<(Mux, String)>`; `Tmux::pane`, `Tmux::detect_on`; `AppConfig.session_backend`.
- deck-term: `Remote`/`RemoteRx`/`RemoteTx` (`&self` methods returning `io::Result<()>`; `palette([[u8;3];19])`),
  `RemoteEvent`, `Terminal::connect(Box<dyn Remote>, TermOptions, Context) -> Terminal`, `Terminal::is_displaced`,
  `TermOptions.kitty_keyboard`, `KeyRules::Win32`, `encode_win32_key`, `encode_win32_modifiers`,
  `encode_paste_win32`; `Terminal::spawn`/`SpawnSpec` are `cfg(unix)`. After `detach()`/`kill()` the Remote's
  `recv` must return `Closed`/error promptly.

## Wave 13: git worktree sessions

Facts + user decisions: `docs/notes/worktrees.md` (authoritative for behavior). omp never gets a worktree flag;
OmpDeck composes `omp worktree add` + plain `omp`, omp's `/wt`, and git. OmpDeck never writes `~/.omp/**` itself
(`omp worktree add` creates the dir under the managed base).

### deck-core
- New module `worktree` (pub; no GUI deps; all git/omp runs blocking — callers use workers):
  - `pub struct Worktree { pub path: PathBuf /* worktree root */, pub primary: PathBuf /* main checkout root */,
    pub branch: Option<String> /* None = detached */ }` (Clone, Debug, PartialEq, Eq).
  - `pub fn resolve(dir: &Path) -> Option<Worktree>`: filesystem only, no process (cheap enough for the scanner):
    walk up from `dir` (bounded) to the first `.git`; a **file** `gitdir: <x>` (relative to the file's dir allowed)
    → read `<x>/commondir` (relative to `<x>`) → common dir; primary = parent of a common dir named `.git`
    (bare/other layouts → None); branch from `<x>/HEAD` (`ref: refs/heads/<b>`). A `.git` directory → None.
  - `pub fn managed_base(setting: Option<&str>) -> PathBuf`: `OMP_WORKTREE_DIR` (absolute or `~`) > `setting`
    (absolute or `~`) > `<home>/.omp/wt` (same precedence as omp's `getWorktreesDir`; relative values ignored).
  - `pub fn slug(branch: &str) -> String` (path-safe, lowercase-free-preserving: `[A-Za-z0-9._-]`, others → `-`,
    collapsed, trimmed, ≤ 60 bytes, never empty/`.`/`..`).
  - `pub fn default_location(base: &Path, primary: &Path, branch: &str) -> PathBuf`:
    `<base>/<slug(branch)>-<slug(primary file name)>`, then `-2`, `-3`, … while the path exists.
  - `pub struct RepoInfo { pub root: PathBuf /* toplevel of dir */, pub primary: PathBuf, pub current_branch:
    Option<String>, pub branches: Vec<String> /* local first, then remote-tracking `origin/x`; no HEAD aliases */,
    pub has_commits: bool }`; `pub fn repo_info(dir: &Path) -> Result<RepoInfo, WorktreeError>`
    (`NotARepo` typed error when outside git).
  - `pub fn check_branch_name(repo: &Path, name: &str) -> Result<(), WorktreeError>` (`git check-ref-format
    --branch`, plus "already exists" check).
  - `pub fn create(omp: &Omp, repo: &Path, branch: &str, base_ref: &str, dest: &Path) -> Result<(), WorktreeError>`:
    `omp worktree add -q -C <repo> -b <branch> <dest> <base_ref>` via the bounded/tree-killing runner (120 s),
    with `PI_CONFIG_FILES=<overlay>` where the overlay (written under OmpDeck's data dir, not `~/.omp`) sets
    `worktree.clone: false` so ignored files are never copied (user decision: clean checkout). Verify the overlay
    key format and that `omp worktree add` honours it (v18.12.6 source + sandbox: the "clone fell back" warning
    must disappear). Errors carry omp's stderr.
  - `pub struct WorktreeStatus { pub branch: Option<String>, pub dirty: bool /* staged|unstaged|untracked,
    ignored excluded */, pub merged: bool /* branch tip is an ancestor of base */, pub ahead: u32 /* commits not
    in base */ }`; `pub fn status(wt: &Worktree, base: &str) -> Result<WorktreeStatus, WorktreeError>`.
  - `pub fn infer_base(wt: &Worktree) -> Option<String>`: the primary checkout's current branch, else the remote
    default branch (`origin/HEAD` target), else None.
  - `pub enum Unsafe { Dirty, NotMerged { ahead: u32 }, Detached, Missing }`;
    `pub fn remove(wt: &Worktree, base: &str, delete_branch: bool) -> Result<(), WorktreeError>`: re-checks
    status first and returns `WorktreeError::Unsafe(Vec<Unsafe>)` without touching anything when not clean+merged;
    then `git -C <primary> worktree remove <path>` (never `--force`) and, if asked, `git -C <primary> branch -d
    <branch>` (never `-D`). `remove_unchecked` must not exist.
  - `WorktreeError` (thiserror): `NotARepo`, `Git { command, stderr }`, `Omp { stderr }`, `Unsafe(Vec<Unsafe>)`,
    `Io`, `Timeout`.
- `sessions::ProjectGroup` gains `pub worktree: Option<worktree::Worktree>` (from `resolve(cwd)` at scan time;
  every constructor updated).
- `status::GitInfo` gains `pub worktree_of: Option<PathBuf>` (primary root when the cwd is a linked worktree).
- `config::AppConfig` gains `pub worktree_bases: BTreeMap<PathBuf, String>` (worktree path → base branch
  recorded by OmpDeck at create / move time; serde default; pruned on removal).

### omp-deck
- Sidebar: groups with `worktree: Some(w)` nest under the group whose cwd == `w.primary` (a synthetic, session-less
  parent header is shown when the primary has no sessions); child header shows the branch (fallback: folder
  name); parent sort key = newest of itself and children; expanding follows the active tab (parent + child);
  filter matches children; per-folder scope includes worktrees whose primary is in scope.
- Project header menu (non-worktree groups): "New session in worktree…" → dialog (branch name prefilled
  `wt/<yyyymmdd-hhmm>`, base ref dropdown from `repo_info` defaulting to the current branch, location defaulting
  to `default_location(managed_base(omp worktree.base setting), primary, branch)` + Browse/edit, inline
  validation via `check_branch_name` on a worker) → worker `worktree::create` → record `worktree_bases` → open a
  tab running plain `omp` in the worktree (same path as "New session here").
- Running-tab / session-row menu: "Move to new worktree…" (tab running, not already a linked worktree): branch
  dialog → record base (tab's current branch) → `Job::slash("Move to worktree", "/wt <branch>",
  Requires::Idle)`; the tab follows omp's relocation.
- Worktree group header menu: "Finish worktree" and "Remove worktree…" (+ existing items).
  - Finish: target = the worktree's running tab (else open its newest session in a tab first); inject
    `Job::slash("Finish worktree", <finish prompt>, Requires::Idle)`. Prompt (one line) tells the agent: commit
    its work on `<branch>`, merge it into `<base>` in the main checkout at `<primary>` (e.g. `git -C <primary>
    merge <branch>`), follow this project's rules on committing/merging and ask the user first if they don't
    allow doing it unprompted, don't delete the worktree or branch (OmpDeck does), report the result.
    Mark the worktree "finishing" (chip/badge on its header + cancel). On every busy→idle transition of that tab
    while finishing: worker `status`; clean+merged → stop **only** the tabs whose cwd is inside the worktree
    (`Backends::stop` per tab), `worktree::remove(.., delete_branch: true)`, toast; otherwise keep waiting
    (one info toast with the reason the first time). Finishing state is in-memory only.
  - Remove…: confirm dialog listing branch/base/status; refuses with the reasons from `Unsafe`; stops only that
    worktree's tabs first.
  - After any removal: drop `worktree_bases`, `folder_tabs`, `recent_folders` entries for the path; if a Drive
    project-sync row exists for it, ask whether to drop the row (cloud backups untouched).
- Status bar git chip: worktree icon + tooltip "Worktree of <primary>" when `worktree_of` is set.
- Housekeeping: whenever a tab's `session_path` changes (breadcrumb/`/proc` follow), set `tab.cwd` to the scanned
  entry's header cwd (or the breadcrumb cwd) so restore/reopen/sync use the relocated folder.
- (Wave 13 follow-up) `config::AppConfig.removed_worktrees: BTreeMap<PathBuf, RemovedWorktree>` +
  `RemovedWorktree { primary: PathBuf, branch: Option<String> }`: recorded by omp-deck on every worktree it removes;
  the sidebar nests groups whose cwd is a key and no longer exists under the primary as "<branch> (removed)", and
  resuming such a session launches with the primary as cwd (omp relocates it there). Entries whose primary is gone
  are pruned.

### Wave 13 as built (supersedes the sketch above where they differ)
- `worktree::remove(wt, base, delete_branch) -> Result<Removed, WorktreeError>`, `Removed { branch_deleted: bool,
  branch_note: Option<String> }`: verifies clean + tip ancestor of `base`, `git worktree remove -- <path>` (no
  force), then `git update-ref -d refs/heads/<b> <verified tip>` (+ best-effort `branch.<b>` config removal) —
  works when `base` isn't checked out (`branch -d` would refuse); a moved branch is kept with a note (partial
  success, not an error). A clean detached HEAD already in base is removable; missing dir → `Unsafe([Missing])`;
  `status` rejects a base that is empty, starts with `-`, or doesn't resolve. Paths are lexically normalized.
- `create` reads `PI_CONFIG_FILES` and the data dir from the `Omp` command env first (sandboxable), rejects an
  option-like base and a relative dest before running omp.
- `Tmux::attach_or_create` returns `LaunchSpec.cwd = platform::root_dir()`; the pane dir only travels as `-c`
  (the tmux server inherits its first client's cwd; a removed worktree stranded it).
- omp-deck removal: a worker classifies tabs (`worktree_ui::classify`) by live cwd (breadcrumb when fresh for the
  pane / matching session, else launch cwd for direct tabs; canonical paths, Windows case-insensitive, component
  prefix); the UI stops exactly those, a second worker re-classifies, refuses if one still runs inside, then
  removes. A direct tab with unknown live cwd launched in the primary blocks removal (named in the message).
- Finish: merge target computed on a worker from `git worktree list --porcelain` (primary on base → merge there;
  base in another worktree → merge there; base checked out nowhere → merge base into the worktree and
  `push . HEAD:refs/heads/<base>`); all values single-quoted + code-spanned; injection retried every 1.5 s until
  `FINISH_TAB_TIMEOUT`; exited tabs are replaced by a resumed session.

## Wave 14: notifications, changes panel, command palette, OmpDeck self-update

Facts + user decisions: `docs/notes/wave14.md`. New workspace deps (root `Cargo.toml`): `zip` (deflate-flate2),
`zbus` 5 (async-io), `tauri-winrt-notification` 0.8, `wayland-client` 0.31 (system), `wayland-protocols` 0.32
(client, staging). Platform-gate them per target in the crate manifests.

### deck-core
- `config::AppConfig` gains (serde default; old configs load):
  - `notify: NotifyConfig { enabled: bool = true, turn_finished: bool = true, attention: bool = true,
    exited: bool = true, only_when_unfocused: bool = true }`
    (`only_when_unfocused = false` also notifies for background tabs while the window has focus).
  - `check_app_updates: bool = true`; `skipped_app_version: Option<String>` ("Skip this version").
  - `palette_recent: Vec<String>` (command ids, newest first, ≤ 20).
  - `changes_panel_width: f32` (0 = default).
- New `pub(crate) mod git`: the shared git runner moved out of `worktree.rs` (`git_command`, raw-bytes and string
  variants, exit code + stderr, timeouts, `LC_ALL=C`, `GIT_TERMINAL_PROMPT=0`, `no_window`, tree kill);
  `worktree.rs` uses it (behavior unchanged).
- New `pub mod changes` (blocking; callers use workers):
  - `pub enum Change { Modified, Added, Deleted, Renamed, Copied, TypeChanged }`
  - `pub struct ChangedFile { pub path: String /* display, repo-relative, lossy */, pub os_path: OsString /* exact,
    for commands */, pub orig_path: Option<String>, pub staged: Option<Change>, pub unstaged: Option<Change>,
    pub untracked: bool, pub conflicted: bool, pub is_dir: bool }`
  - `pub struct Changes { pub root: PathBuf, pub branch: Option<String>, pub files: Vec<ChangedFile> }`
  - `pub fn list(cwd: &Path) -> Result<Option<Changes>, ChangesError>` (`None` = not inside a work tree).
  - `pub enum DiffSide { Staged, Unstaged, Untracked }`;
    `pub struct FileDiff { pub text: String, pub binary: bool, pub truncated: bool, pub added: u32, pub removed: u32 }`;
    `pub fn diff(root: &Path, file: &ChangedFile, side: DiffSide) -> Result<FileDiff, ChangesError>` (cap 2 MiB /
    20 000 lines → `truncated`; untracked synthesized in Rust; empty text for mode/EOL-only changes is valid).
  - `pub fn patch(root: &Path) -> Result<String, ChangesError>`: staged + unstaged unified diff (for Copy diff).
  - Writes, each re-verifying `rev-parse --show-toplevel == root` first (else `ChangesError::RootChanged`):
    `stage(root, &[&ChangedFile])` (`add --`), `unstage` (`restore --staged --`; unborn HEAD → `rm --cached --`),
    `discard(root, &[&ChangedFile])` (tracked unstaged changes only: `restore --`; refuses conflicted),
    `delete_untracked(root, &[&ChangedFile])` (`clean -f[d] --` per path after a `clean -n` dry run lists exactly
    that path; never `-x`, never without a pathspec). Every path operand after `--` with `GIT_LITERAL_PATHSPECS=1`.
  - `ChangesError` (thiserror): `Git { command, stderr }`, `RootChanged`, `Conflicted`, `Io`, `Timeout`.
- New `pub mod selfupdate` (blocking; callers use workers):
  - `pub enum InstallKind { UserDir { dir: PathBuf } /* install.sh / tarball, writable */, AppImage { file: PathBuf },
    Deb, Rpm, WindowsDir { dir: PathBuf }, Dev /* cargo target dir */, Unsupported { reason: String } }`;
    `pub fn detect() -> InstallKind` (`$APPIMAGE`, `current_exe` location, `dpkg -S`/`rpm -qf` for `/usr`, dir
    writability probe; read-only/unknown → Unsupported).
  - `pub struct Release { pub version: String /* "0.9.0" */, pub tag: String, pub page_url: String }`;
    `pub fn latest() -> Result<Release, UpdateError>` (redirect of `<base>/releases/latest`, no API; base =
    `OMP_DECK_UPDATE_BASE` env or `https://github.com/lavinhoque33/omp-deck`, so tests/sandboxes can serve a fake
    release over local HTTP); `pub fn is_newer(latest: &str, current: &str) -> bool` (numeric semver, pre-release
    tags never newer than their release).
  - `pub fn asset_for(kind: &InstallKind, version: &str) -> Option<String>` (arch from `std::env::consts::ARCH`).
  - `pub fn download(release: &Release, asset: &str, dir: &Path, progress: &dyn Fn(u64, Option<u64>),
    cancel: &AtomicBool) -> Result<PathBuf, UpdateError>`: streams to `dir`, verifies against `<asset>.sha256`
    (mismatch → error, file deleted).
  - `pub fn apply(kind: &InstallKind, downloaded: &Path) -> Result<Applied, UpdateError>`:
    UserDir: unpack, write `omp-deck-host` then `omp-deck` as temp files in the same dir (0755), `rename` over;
    AppImage: chmod + `rename` over `$APPIMAGE`; WindowsDir: unzip, for each of `omp-deck-host.exe`, `conpty.dll`,
    `OpenConsole.exe`, `omp-deck.exe` rename the existing file to `<name>.old-<yyyyMMddHHmmss>` then write the new
    one (roll back renamed files on failure); Deb/Rpm: `pkexec dnf|apt-get install -y <file>` (or `rpm -U`/
    `dpkg -i` when dnf/apt are missing) with a 15 min timeout; pkexec missing or auth dismissed →
    `UpdateError::NeedsManual { command: String }`. `Applied { restart: (PathBuf, Vec<OsString>) }` = what to exec
    after exit (AppImage → `$APPIMAGE`; others → the installed `omp-deck` path) with the original args.
  - `pub fn sweep_leftovers()`: Windows: delete `*.old-*` next to the exe (best effort, ignores locked files).
  - `UpdateError` (thiserror): `Http`, `Checksum`, `NoAsset`, `Unsupported(String)`, `NeedsManual { command }`,
    `Io`, `Timeout`, `Cancelled`.

### omp-deck
- **Notifications** (`notify.rs` + platform files): a notifier thread (Linux: one zbus session connection,
  `Notify` with `actions ["default","Open"]`, hints `desktop-entry=omp-deck`, app icon `omp-deck`, replaces_id per
  tab; listens to `ActivationToken` + `ActionInvoked` + `NotificationClosed`; Windows: WinRT toast with AUMID
  `io.github.lavinhoque33.OmpDeck` + `SetCurrentProcessExplicitAppUserModelID` at startup, PowerShell AUMID
  fallback when the Start-menu shortcut lacks it). Triggers from `tabs::TabStatus` transitions of a tab that is not
  (active and window focused) — or only when the window is unfocused, per `notify.only_when_unfocused`: Busy→Idle
  (turn finished; only after ≥ 3 s busy), →Attention, →Exited (`[code]`), each gated by its toggle. Title = tab
  title; body = event + project. Activating the tab closes its notification. Click → activate that tab and raise
  the window (Wayland: xdg-activation with the received token; else/fallback `Focus` + `RequestUserAttention`;
  Windows `Minimized(false)` + `Focus`). No D-Bus/notification server → silently disabled (log once). Settings →
  OmpDeck → Notifications (toggles + "Send test notification"). `packaging/install.ps1` sets
  `System.AppUserModel.ID` on the `OmpDeck.lnk` shortcut (Wave 14 owner of that file: the notifications agent).
- **Command palette** (`palette.rs`): Ctrl+Shift+K = commands; Ctrl+Shift+E = quick switcher (open tabs, sessions,
  projects; newest first on empty query). One modal overlay (search box, ranked rows with highlighted matches,
  shortcut hints, disabled rows greyed with the reason as tooltip, ↑↓/Enter/Esc, mouse). In-house fuzzy scorer
  (case-folded subsequence, bonuses for consecutive/word-start/path-segment matches, length penalty) unit-tested.
  Commands cover every action in the title-bar app menu, ⋯ menu, status-bar actions, active tab / session / project
  context menus (target = active tab's session / project), settings pages, Usage, Agent hub, Session tree,
  worktree actions, Drive actions (open dialogs only), plus the new `App::toggle_changes_panel()`,
  `App::check_app_update_manually()`, `App::start_app_update()`, notifications toggles. Dispatch through the same
  functions as the menus; availability via `session_actions::availability` and the existing gates.
  `palette_recent` boosts recent commands.
- **Changes panel** (`changes_view.rs`, `app/changes.rs`): right panel `changes` (exclusive with the agent hub),
  toggled by Ctrl+Shift+G (verify unbound in omp v18.12.6; else report), the status-bar git chip ("Review
  changes"), palette. Shows the active tab's live cwd repo (`SessionStatus.cwd`, fallback `tab.cwd`): branch, root,
  sections Conflicted / Staged / Changes / Untracked with counts, per-file + section actions Stage/Unstage, Discard
  (confirm, danger, lists paths), Delete untracked (confirm), Copy diff (whole patch), open file's folder. Selecting
  a file shows its diff (virtualised rows, hunk headers, old/new line numbers, add/remove colours from the theme,
  binary/too-large/EOL-only placeholders). Refresh on open, tab switch, after actions, every 3 s while open (worker,
  cancellable). Not a repo → empty state. Width persisted in `changes_panel_width`.
- **Self-update** (`app_update.rs`, `app/app_update.rs`): own tracker (startup + every 6 h when
  `check_app_updates`, manual "Check for OmpDeck updates"), independent of omp being installed. Title-bar pill
  "OmpDeck X.Y.Z" (distinct from the omp pill) with popover: release page link, **Update and restart**
  (progress, cancel; download → verify → apply), deb/rpm "Install (asks for your password)"; `NeedsManual` →
  copyable command + release page; Dev/Unsupported → explanation, no button; "Skip this version". Restart =
  flush state (same path as `on_exit`), spawn the successor after the flush, exit; tabs re-attach. Startup calls
  `selfupdate::sweep_leftovers()` on a worker. Settings → OmpDeck: toggle + "Check now" + current version.

### Wave 14 as built (supersedes the sketch above where they differ)
- `selfupdate::UpdateError::{Checksum, NoAsset}` carry a `String`. `latest()` has its own 15 s deadline, no redirect
  following; downloads keep the stall-based client. `detect()` honours the verification override
  `OMP_DECK_UPDATE_KIND=deb|rpm|appimage:<file>|dir:<dir>|dev` (overridden deb/rpm restart the current exe);
  under `/usr`, only dpkg/rpm-owned exes are Deb/Rpm, others fall through to the writability probe.
  `download` fetches `<asset>.sha256` first and refuses a release without one; the caller owns the temp dir
  (kept while a NeedsManual command is shown). Startup sweeps `omp-deck-update-*` temp dirs older than a day.
- Update UI: checks wait for install detection and never run on Dev; a later failed check keeps a known release's
  Update button; a window close during download cancels and waits, during apply defers ("Finishing the OmpDeck
  update…") and then closes without relaunching; a successful Update and restart flushes `state.json` before
  spawning the successor (observed ~30 ms after the flush, the old GUI exits ~90 ms later).
- `changes`: binary diffs have empty text; untracked directories share one byte budget; `discard` skips untracked,
  unchanged and intent-to-add entries; `stage` on a conflicted file marks it resolved; unstaging a staged rename also
  unstages its source; `delete_untracked` accepts only `Would remove` lines for the requested path and refuses when
  git would keep something that still exists (nested repo). Panel: `ChangesUi::retarget` clears the listing on every
  tab/cwd change; destructive confirms re-check the repo at confirm time; file list rendered with `show_rows`.
- Notifications: zbus connection with a 5 s method timeout; the notifier keeps only the newest pending `Show` per tab.
  Palette: per-model (`Model: <provider> / <name>`, favourites first) and `Thinking: <level>` rows through
  `queue_change`; "Tab: Kill session process" disabled for session-backed tabs (Stop with confirm instead);
  worktree rows gate on the sidebar group or `GitInfo.worktree_of` and resolve the real root at run time.
  `install.ps1` stamps `System.AppUserModel.ID` on `OmpDeck.lnk`; the release workflow's installer round trip
  asserts it.

## Wave 15: split view, agents dashboard, review comments

User decisions (2026-10-24): panes show existing tabs (tab = session stays true); the dashboard covers open tabs
plus every running session without a tab; it is a full view over the terminal area that replaces the right-hand
Agent Hub panel (no side panel any more); review comments offer both **Insert into omp** and **Send**.
Recon facts with file:line evidence: `docs/notes/wave15.md`.

### deck-host (owned by the deck-core implementer this wave)
- `protocol::HostInfo` gains `title: Option<String>` (`#[serde(default)]`; old daemons → `None`), filled by the
  daemon from its headless snapshot's current OSC 0/2 title. No protocol version bump (additive field).

### deck-core
- `Mux::pane_titles(&self) -> HashMap<String, String>`: session name → current pane title for every live session
  of that backend in one pass (tmux: a single `list-panes -a -F '#{session_name}<TAB>#{pane_title}'` on the
  dedicated socket, active pane only; host: `info` per live session, `title` field). Never blocks longer than the
  existing alive query timeouts; errors → empty map.
- `pub mod fleet` (new; pattern of `status::StatusWatcher` / `agents::AgentsWatcher`: one background thread,
  generation counter, Condvar wake, publish-on-change `Arc`, `on_update` callback):
  - `FleetTarget { session: PathBuf, cwd: PathBuf, owner_alive: bool }`.
  - `FleetWatcher::spawn(on_update) -> FleetWatcher`; `set_targets(Vec<FleetTarget>)` (empty = idle, drops all
    readers); `latest() -> Option<Arc<FleetSnapshot>>`.
  - Per target it keeps one incremental `status::SessionStatsReader` and one `agents::AgentsScanner`
    (dropped when the target goes); per distinct cwd one `git_info` (dedup by cwd). Cadence: stats + agents every
    1 s while any agent of that session runs, else 3 s; git every 5 s. First pass of a huge file must not delay
    other sessions' updates by more than one cycle (process targets round-robin; publish after each target).
  - `FleetSnapshot { sessions: Vec<FleetSession> }` in target order; `FleetSession { session: PathBuf, model:
    Option<String>, thinking: Option<String>, context_tokens: Option<u64>, input_tokens: u64, output_tokens: u64,
    cost: f64, last_reply: Option<String> /* ≤ 400 chars, whitespace collapsed */, last_turn_failed: bool,
    agents: Vec<AgentSummary>, git: Option<GitInfo> }` (reuse the existing types; field names may follow them).
- No writes anywhere; real sessions root read-only as always.

### deck-term
- `TerminalView::id_salt(impl Hash) -> Self`: the widget's interaction id derives from the salt (stable per tab
  uid), so several views in one frame keep their own focus/drag state regardless of draw order.
- `Terminal::paste_bytes(&self, text: &str) -> Option<Vec<u8>>`: the exact bytes the widget's own paste would
  send for `text` under the current modes (bracketed `encode_paste` or win32-input-mode `encode_paste_win32`),
  `None` when the application has not enabled bracketed paste (?2004). Pure apart from reading modes.

### omp-deck — split view
- Layouts: Single, Two columns, Two rows, Three columns, Grid 2×2. `App.active` stays "the tab in the focused
  pane", so everything already bound to the active tab (status bar, Changes panel, ctl link, model picker,
  palette, session tree) follows pane focus. New state: `panes: Vec<Option<u64 /*tab uid*/>>` (one per slot),
  `focused_pane`, divider ratios. Invariant: a tab is shown in at most one pane; `panes[focused_pane]` is the
  active tab whenever the active tab is a terminal tab.
- Each visible pane renders its tab via `TerminalView::id_salt(uid)` in its own rect (with the same
  `surrender_focus_on` override), `set_visible(true)`; every non-drawn terminal tab is polled by the hidden loop.
  Focus reclaim / `focus_pending` target the focused pane. Click in a pane focuses it. Thin pane header when
  split: status glyph, title, close-pane (×, removes the pane, the tab stays open); focused pane gets the accent
  border. Draggable dividers.
- Tab strip: clicking a tab visible in a pane focuses that pane; otherwise the tab replaces the focused pane's tab.
  Tabs visible in a pane get a subtle marker. Tab/"Open tabs"/session context menus: "Open in split right",
  "Open in split down" (adds a pane; at 4 panes replaces the least recently focused non-focused pane).
  Palette: Split right/down, Grid, Unsplit (keep focused pane), Focus next/previous pane. Keyboard: focus pane
  left/right/up/down with Ctrl+Alt+Arrow — implementer verifies in omp's source (`~/PersProject/oh-my-pi`) that
  omp does not bind these and reports; Ctrl+PgUp/PgDn keep cycling tabs (into the focused pane).
- Closing/detaching a tab shown in a pane removes that pane (layout shrinks; 1 left → Single). Settings/update/
  install tabs never enter a pane; activating one shows it full width and the split comes back on the next
  terminal tab activation.
- Per-tab rules switch from "active" to "visible": bell suppression, notification `Sighting.active` and
  `take_seen` (a visible pane counts as looked at), the modal input hold (all visible panes).
- Persistence: `AppConfig.split: Option<SplitState { layout, panes: Vec<Option<usize> /*index into the persisted
  open_tabs*/>, focused: usize, ratios: Vec<f32> }>` and the same in `FolderTabs` (`serde(default)`); restored
  after tab restore; dangling indices dropped.

### omp-deck — agents dashboard (replaces the right-hand Agent Hub panel)
- Ctrl+Shift+A (no longer needs an active session tab), status-bar Agents chip (opens with the active session's
  card expanded), palette, ⋯ menu: toggle a full view over the terminal area (panes not drawn while it is open;
  Esc / Ctrl+Shift+A closes). Changes panel hidden while it is shown.
- Cards = open session tabs + sessions held by a live backend session without a tab (`RunningMonitor`), driven
  by `FleetWatcher` only while the dashboard is open. Card: title, project (`~`-abbreviated, worktree branch),
  status (open tab: `TabStatus`; no tab: `Activity::from_title` of `Mux::pane_titles`, else "Running"),
  model + thinking, context meter % (window from `ModelsCache`; live ctl context for the active tab), cost,
  ↑in ↓out tokens, changed files (staged/unstaged/untracked from `GitInfo`), last reply snippet (3 lines),
  subagents summary (running/total) expandable into the existing subagent card rows (tree/flat). Responsive grid;
  search (title, project, id prefix, subagent id/task) and status filter (All / Needs attention / Busy / Idle);
  order: attention, busy, then most recent activity.
- Click card → focus/open the session (existing `open_session`/`activate`, into the focused pane) and close the
  dashboard. Click a subagent → transcript popup (existing). Kill/Revive/Chat stay available when that session's
  tab is the active one with live control (activate it first if it is open); for a session without a tab the
  popup is read-only with "Open session for live control".
- The old right-panel hub UI is removed (dead code deleted); its subagent card renderer and view model are reused.

### omp-deck — review comments (Changes panel)
- Diff rows become clickable: hover shows a "+" in the gutter; click opens an inline multi-line comment editor
  under the row; Shift+click extends to a line range in the same file and side. Comments: `{ path (repo-relative),
  side (new for added/context, old for deleted), lines start..=end, quoted lines (≤ 12, "…" beyond), body }`, kept
  in memory per repo root (not persisted), survive refreshes, listed in a "Review (N)" section with edit/delete
  and inline markers on matching rows. A general comment (no line) is allowed.
- Prompt text: a short header, then numbered items `path:start[-end]` + quoted lines (`> `) + body, paths relative
  to the repository root (header says so). Cap 32 KiB → refuse with a clear message.
- `inject::Payload::Text { text, submit: bool }`: text keeps `\n`/`\t`, strips every other C0/C1 control (incl.
  ESC) and is written with `Terminal::paste_bytes` (refused when `None`). `submit=false` (**Insert into omp**):
  Requires::Any except exited, no draft confirm, no clear, no Esc/Enter — pasted at omp's cursor. `submit=true`
  (**Send**): Requires::Idle, existing draft confirm + Ctrl+C clear, paste, Esc, Enter; abort rules as Slash.
  Target = the panel's tab uid, re-resolved at click time. Success clears the review (toast); refusal keeps it.

### Wave 15 as built (supersedes the sketch above where they differ)
- deck-core: `SplitLayout { Single, Columns2, Rows2, Columns3, Grid4 }` + `SplitState` on `AppConfig.split` and
  `FolderTabs.split`. `Mux::pane_titles`: tmux `list-panes -a` with the title as the last field (tmux/alacritty
  strip tabs from titles); host queries daemons concurrently. `fleet`: `FleetSnapshot { sessions:
  Vec<Arc<FleetSession>> }` (an unchanged session is the same `Arc` across snapshots), `FleetSession.agents:
  Arc<Vec<AgentSummary>>`; git runs on its own thread (a slow git never delays session updates) with a cancel flag
  set by `set_targets`/`Drop`; snapshots are built only on a visible change; a session appears after its first read.
- deck-term: salted views take their id from `make_persistent_id(("deck_term::TerminalView", hash))`; salts must be
  unique within the parent scope. `paste_bytes` returns `Some(vec![])` for empty text; the widget's own paste paths
  share the same encoder selection.
- Split view: panes hold tab uids (no index shifting); a split without a spare tab leaves an empty pane that the next
  opened session fills; focus never moves to an empty or reserved pane (clicking one releases egui focus).
  Launches into a pane carry a reservation ticket (`PaneClaim`, keyed by session path / mux name) so they fill
  exactly their pane; a full grid replaces the least recently focused non-reserved pane. Closing a tab also drops
  empty panes. Ctrl+Alt+Arrow verified unbound in omp (`pi-natives/src/keys.rs` exact modifier match; omp binds only
  `alt+arrow` and `ctrl+alt+]`); desktops may still grab it.
- Dashboard: `dashboard.rs` + `dashboard_model.rs`, glue in `app/agents.rs`; `agent_hub.rs` is now only the
  embeddable subagent rows. Tab-less sessions map to files via the backend snapshot or their canonical `omp-<id>`
  name. Rebuilds are driven by an allocation-free signature (age bucket, scan generation, running/fleet `Arc`
  pointers, models generation, live context, per-tab facts). Bells are cleared only when a terminal is drawn.
  Entry points: Ctrl+Shift+A, status-bar Agents chip (expands the active card), palette, title-bar app menu.
- Review comments: `review.rs` (model + prompt builder), injector `Payload::Text` gets its bytes at write time via
  `Env::paste` (the injector stays pure; `None` → refused, `Poll::no_paste`); completions reported in
  `Poll::completed`; pending hand-offs tracked per tab uid. Editor: Ctrl+Enter saves, Esc cancels; while it takes
  input it cancels `focus_pending`. The Review section shows for every ready repository (also a clean tree); the
  file list is sized to its rows (≤ 45 %), the file/diff divider is fixed.

## Wave 16: theme gallery (16 dark styles, Blueprint default)

Design + decisions: `docs/notes/wave16.md`; exact values: `docs/notes/theme-mockups.html`.

### deck-core (`config.rs`)

```rust
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum UiStyle {
    Neutral, Neon,
    ZincContrast, LayeredDepth, TealSignal, SlateInk, MonoWire, GraphiteIslands,
    Aurora, ObsidianGlass, #[default] Blueprint, Ember, IndigoNight, Phosphor,
    NeoBrutal, RoseNoir, Spotlight, TwoToneFrame,
}
impl UiStyle {
    /// Gallery / palette order: Blueprint first, then Neutral, Neon, then mockups 1–16 minus Blueprint.
    pub const ALL: [UiStyle; 18];
    pub fn label(self) -> &'static str;   // "Blueprint", "Zinc Contrast", "Rosé Noir", …
    pub fn key(self) -> &'static str;     // "blueprint", "zinc_contrast", … (palette ids `style.<key>`)
    pub fn dark_only(self) -> bool;       // every style except Neutral and Neon
}
pub const STYLE_REV: u32 = 1;
pub struct AppConfig { …, pub style_rev: u32 /* default STYLE_REV */, … }
impl AppConfig {
    /// Effective dark mode: `theme == Dark || style.dark_only()`.
    pub fn dark(&self) -> bool;
}
```

Serde names are the variant names (`"Blueprint"`, `"RoseNoir"`, …). `parse_config` migration: when the JSON has
no `style_rev`, a `style` of `"Neutral"` or absent becomes `"Blueprint"` unless `theme` is `"Light"`; then
`style_rev` = `STYLE_REV`. Unknown style strings keep failing the parse as today (no silent fallback beyond the
existing behavior).

### deck-term (`theme.rs`, re-exported from the crate root)

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermColors { pub foreground: [u8; 3], pub background: [u8; 3], pub cursor: [u8; 3], pub selection_bg: [u8; 3] }
pub enum TermTheme { Dark, Light, NeutralDark, NeutralLight, Custom(TermColors) }
```

`Palette::for_theme(Custom(c))` = those four colors + Neutral dark's 16 ANSI colors. Everything that already takes
a `TermTheme` (spawn options, `set_theme`, host color push, OSC 10/11/4 answers) works unchanged with `Custom`.

### omp-deck

- `theme::apply(ctx, style, density, dark)` builds per-style palettes + a per-style structural look; `dark` is
  `AppConfig::dark()`. `theme::neutral()` is replaced by `theme::flat()` (true for every style but Neon).
- `term_theme(theme, style)`: Neutral/Neon unchanged; new styles → `TermTheme::Custom` (canvas bg, text, text_strong,
  mix(bg, accent, 0.35)).
- Settings → Appearance: Style gallery (18 tiles); Light disabled with "<Style> is dark-only" for dark-only styles;
  same gating in the title-bar menu and the command palette (`theme.light`). Palette: `style.<key>` per style
  (replaces `style.neutral` / `style.neon`).

### Wave 16 as built

- deck-core: as specified. Extra migration rule: a legacy file with `theme: "Light"` and no `style` gets
  `style: "Neutral"` written explicitly (otherwise serde's new default Blueprint, dark-only, would silently turn a
  light-mode user dark).
- deck-term: as specified; the NeutralDark ANSI table is one private `NEUTRAL_DARK_ANSI` shared by `NeutralDark`
  and `Custom`.
- omp-deck: `theme.rs` keeps the egui style build + shared helpers; new submodules `theme/palettes.rs` (one palette
  per style), `theme/look.rs` (`Look`: per-style structure knobs: regions, card frame, hover/active effects,
  canvas, tabs, selection, accordion kind/chevron/rail/segment bar, fonts), `theme/skin.rs` (window regions,
  canvas decorations with cached meshes, card/tab/row painters) and `theme/acc.rs` (accordion geometry +
  painting). Accessors: `theme::style()`, `look()`, `look_of(style)`, `palette(style, dark)`, `is_gallery()`
  (one of the 16 new styles), `flat()`, `label_font(size)` (mono for Blueprint labels), `ui_rad(base)`.
- Central panels go through `skin::central()` (all 5 call sites: terminal, split, dashboard, Settings, empty
  state) so region decorations paint everywhere; it turns egui's scroll fade off only inside gallery styles'
  filled central panel (Neutral/Neon unchanged, pixel-identical to v0.10.1). Cached meshes are keyed per style.
- Known differences from the mockups: Graphite Islands' title bar is flush (not inset into the backdrop frame);
  egui has one weight per font, so Neo-Brutal's heavy titles are approximated; glows are mesh gradients, not CSS
  blur.
