//! tmux integration on a dedicated socket. Every command carries `-L <socket> -f <config>`;
//! the user's default tmux server is never touched.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::config_dir;

pub const APP_SOCKET: &str = "omp-deck";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
}

#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("cannot run tmux: {0}")]
    Io(#[from] io::Error),
    #[error("tmux {command} failed: {stderr}")]
    Failed { command: String, stderr: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tmux {
    program: PathBuf,
    socket: String,
    config: PathBuf,
}

impl Tmux {
    /// tmux on PATH (`tmux -V` runs ok) → Some, socket [`APP_SOCKET`], config `config_dir()/tmux.conf`.
    pub fn detect() -> Option<Tmux> {
        let ok = Command::new("tmux")
            .arg("-V")
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        ok.then(|| Tmux::with_socket("tmux", APP_SOCKET, config_dir().join("tmux.conf")))
    }

    pub fn with_socket(
        program: impl Into<PathBuf>,
        socket: impl Into<String>,
        config: impl Into<PathBuf>,
    ) -> Tmux {
        Tmux {
            program: program.into(),
            socket: socket.into(),
            config: config.into(),
        }
    }

    pub fn socket(&self) -> &str {
        &self.socket
    }

    fn command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.env_remove("TMUX")
            .arg("-L")
            .arg(&self.socket)
            .arg("-f")
            .arg(&self.config)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> io::Result<Output> {
        self.command().args(args).output()
    }

    /// Writes the generated config (creating dirs) and, if this socket's server is already
    /// running (`-f` only applies at server start), applies it with `source-file`.
    pub fn ensure_config(&self) -> io::Result<PathBuf> {
        if let Some(dir) = self.config.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.config, config_text())?;
        if self.server_running() {
            let conf = self.config.to_string_lossy();
            let out = self.run(&["source-file", &conf])?;
            if !out.status.success() {
                return Err(io::Error::other(format!(
                    "tmux source-file failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
        }
        Ok(self.config.clone())
    }

    fn server_running(&self) -> bool {
        self.run(&["list-sessions", "-F", "#{session_name}"])
            .is_ok_and(|o| o.status.success())
    }

    /// `env -u TMUX <tmux> -L <sock> -f <conf> new-session -A -s <name> -c <cwd'> -- <argv...>`
    pub fn attach_or_create(&self, name: &str, cwd: &Path, argv: &[String]) -> LaunchSpec {
        let cwd = effective_cwd(cwd);
        let mut args: Vec<String> = vec![
            "-u".into(),
            "TMUX".into(),
            "-u".into(),
            "WINDOWID".into(),
            "-u".into(),
            "ALACRITTY_WINDOW_ID".into(),
            self.program.to_string_lossy().into_owned(),
            "-L".into(),
            self.socket.clone(),
            "-f".into(),
            self.config.to_string_lossy().into_owned(),
            "new-session".into(),
            "-A".into(),
            "-s".into(),
            name.into(),
            "-c".into(),
            cwd.to_string_lossy().into_owned(),
            "--".into(),
        ];
        args.extend(argv.iter().cloned());
        LaunchSpec {
            program: "env".into(),
            args,
            cwd,
            env: terminal_env(),
        }
    }

    /// Live session names; no server running → empty.
    pub fn alive_sessions(&self) -> HashSet<String> {
        match self.run(&["list-sessions", "-F", "#{session_name}"]) {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(str::to_owned)
                .collect(),
            _ => HashSet::new(),
        }
    }

    /// Output of `display -p -t =<session>: <format>` for the session's active pane; `None` when
    /// the command fails or prints nothing (no such session).
    fn pane_format(&self, session: &str, format: &str) -> Option<String> {
        let out = self
            .run(&["display", "-p", "-t", &format!("={session}:"), format])
            .ok()
            .filter(|o| o.status.success())?;
        let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        (!text.is_empty()).then_some(text)
    }

    /// tty of the session's active pane (`/dev/pts/N`), the key of omp's terminal breadcrumb.
    pub fn pane_tty(&self, session: &str) -> Option<String> {
        self.pane_format(session, "#{pane_tty}")
    }

    /// When the process that tmux started in the session's active pane began (`/proc`, Linux
    /// only). A breadcrumb older than this belongs to a previous owner of the same tty.
    pub fn pane_started(&self, session: &str) -> Option<SystemTime> {
        let pid: u32 = self.pane_format(session, "#{pane_pid}")?.parse().ok()?;
        process_start(pid)
    }

    pub fn has_session(&self, name: &str) -> bool {
        // `=` forces exact-name matching instead of prefix matching.
        self.run(&["has-session", "-t", &format!("={name}")])
            .is_ok_and(|o| o.status.success())
    }

    pub fn kill(&self, name: &str) -> Result<(), TmuxError> {
        let out = self.run(&["kill-session", "-t", &format!("={name}")])?;
        if out.status.success() {
            Ok(())
        } else {
            Err(TmuxError::Failed {
                command: "kill-session".into(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_owned(),
            })
        }
    }

    /// See contract: maps each live session to the top-level session `.jsonl` files (direct
    /// children of a project dir under `root`) held open by any process in its pane tree.
    pub fn session_files(&self, root: &Path) -> HashMap<String, Vec<PathBuf>> {
        let mut result = HashMap::new();
        if !cfg!(target_os = "linux") {
            return result;
        }
        let Ok(out) = self.run(&["list-panes", "-a", "-F", "#{session_name} #{pane_pid}"]) else {
            return result;
        };
        if !out.status.success() {
            return result;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let panes: Vec<(&str, u32)> = text
            .lines()
            .filter_map(|l| {
                let (name, pid) = l.rsplit_once(' ')?;
                Some((name, pid.trim().parse().ok()?))
            })
            .collect();
        if panes.is_empty() {
            return result;
        }
        let children = proc_children();
        let canon_root = std::fs::canonicalize(root).ok();
        for (name, pane_pid) in panes {
            let files: &mut Vec<PathBuf> = result.entry(name.to_owned()).or_default();
            let mut stack = vec![pane_pid];
            let mut visited = HashSet::new();
            while let Some(pid) = stack.pop() {
                if !visited.insert(pid) {
                    continue;
                }
                if let Some(kids) = children.get(&pid) {
                    stack.extend(kids);
                }
                for target in open_files(pid) {
                    if is_top_level_session(&target, root, canon_root.as_deref())
                        && !files.contains(&target)
                    {
                        files.push(target);
                    }
                }
            }
            files.sort();
        }
        result
    }
}

fn is_top_level_session(p: &Path, root: &Path, canon_root: Option<&Path>) -> bool {
    if p.extension().is_none_or(|e| e != "jsonl") {
        return false;
    }
    let Some(grand) = p.parent().and_then(Path::parent) else {
        return false;
    };
    grand == root || canon_root.is_some_and(|c| grand == c)
}

/// Wall-clock start of `pid`: `/proc/<pid>/stat` field 22 (clock ticks since boot, USER_HZ = 100 on
/// Linux) plus `btime` from `/proc/stat`.
fn process_start(pid: u32) -> Option<SystemTime> {
    const USER_HZ: u64 = 100;
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the last ')' start at field 3 (state); starttime is field 22.
    let ticks: u64 = stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()?;
    let btime: u64 = std::fs::read_to_string("/proc/stat")
        .ok()?
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(UNIX_EPOCH + std::time::Duration::from_millis(btime * 1000 + ticks * 1000 / USER_HZ))
}

/// ppid → children, from `/proc/*/stat`. Vanished/unreadable entries are ignored.
fn proc_children() -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return map;
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // "pid (comm) S ppid ..." — comm may contain spaces/parens, so split after the last ')'.
        let ppid = stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().nth(1))
            .and_then(|s| s.parse::<u32>().ok());
        if let Some(ppid) = ppid {
            map.entry(ppid).or_default().push(pid);
        }
    }
    map
}

fn open_files(pid: u32) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new(); // gone or permission denied
    };
    rd.flatten()
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .collect()
}

/// `"omp-"` + id with every char outside `[A-Za-z0-9_-]` replaced by `_`.
pub fn session_name_for(id: &str) -> String {
    let mut s = String::from("omp-");
    s.extend(id.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            c
        } else {
            '_'
        }
    }));
    s
}

/// `omp-new-<unix-ms>`.
pub fn new_session_name(now: SystemTime) -> String {
    let ms = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    format!("omp-new-{ms}")
}

/// Degraded mode (no tmux): `env -u WINDOWID -u ALACRITTY_WINDOW_ID <argv...>`; same env/cwd rules.
pub fn direct_launch(cwd: &Path, argv: &[String]) -> LaunchSpec {
    // alacritty forces WINDOWID/ALACRITTY_WINDOW_ID into the child env; strip them via env(1).
    let mut args: Vec<String> = ["-u", "WINDOWID", "-u", "ALACRITTY_WINDOW_ID"]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    args.extend(argv.iter().cloned());
    LaunchSpec {
        program: "env".into(),
        args,
        cwd: effective_cwd(cwd),
        env: terminal_env(),
    }
}

/// `cwd` if it is an existing dir, else `$HOME`, else `/`.
pub fn effective_cwd(cwd: &Path) -> PathBuf {
    if cwd.is_dir() {
        cwd.to_path_buf()
    } else {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    }
}

/// Overrides omp honours: inside tmux it would otherwise disable images (OmpDeck renders kitty
/// Unicode-placeholder images) and never emit OSC 8 hyperlinks.
const OMP_ENV: [(&str, &str); 3] = [
    ("PI_FORCE_IMAGE_PROTOCOL", "kitty"),
    ("PI_KITTY_PLACEHOLDERS", "1"),
    ("PI_FORCE_HYPERLINKS", "1"),
];

/// Environment of the PTY child (tmux client or omp itself); a tmux server started by it
/// inherits the omp overrides, and the generated config sets them for running servers too.
pub fn terminal_env() -> Vec<(String, String)> {
    [
        ("TERM", "xterm-256color"),
        ("COLORTERM", "truecolor"),
        ("TERM_PROGRAM", "omp-deck"),
    ]
    .into_iter()
    .chain(OMP_ENV)
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

const CONFIG_TEXT: &str = r#"# OmpDeck generated tmux.conf — rewritten at startup; do not edit
# -q on options newer than tmux 3.2 (extended-keys-format 3.5, allow-passthrough 3.3): older tmux ignores them
set -g status off
set -g prefix None
set -g prefix2 None
unbind -aq -T prefix
set -s escape-time 0
set -g mouse on
unbind -nq MouseDown3Pane
unbind -nq M-MouseDown3Pane
unbind -nq MouseDown3Status
unbind -nq MouseDown3StatusLeft
unbind -nq M-MouseDown3Status
unbind -nq M-MouseDown3StatusLeft
set -g history-limit 50000
set -s extended-keys on
set -sq extended-keys-format csi-u
set -s set-clipboard on
set -gq allow-passthrough on
set -g focus-events on
set -g set-titles on
set -g set-titles-string '#T'
set -g default-terminal tmux-256color
set -s terminal-features[100] 'xterm-256color:RGB:extkeys:clipboard:hyperlinks:usstyle:sync:focus:title'
set -g allow-rename off
set -g exit-empty on
# copy-mode: wheel scroll + mouse selection only; any key cancels copy-mode and is replayed to the pane
set -g mode-keys emacs
unbind -aq -T copy-mode
unbind -aq -T copy-mode-vi
bind -T copy-mode MouseDown1Pane select-pane
bind -T copy-mode MouseDrag1Pane select-pane \; send-keys -X begin-selection
bind -T copy-mode MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel
bind -T copy-mode WheelDownPane select-pane \; send-keys -X -N 5 scroll-down
bind -T copy-mode WheelUpPane select-pane \; send-keys -X -N 5 scroll-up
bind -T copy-mode DoubleClick1Pane select-pane \; send-keys -X select-word \; run-shell -d 0.3 \; send-keys -X copy-pipe-and-cancel
bind -T copy-mode TripleClick1Pane select-pane \; send-keys -X select-line \; run-shell -d 0.3 \; send-keys -X copy-pipe-and-cancel
bind -T copy-mode Any if -F '#{!=:#{mouse_x},}' '' { send-keys -X cancel ; send-keys }
"#;

/// The generated tmux.conf: makes tmux invisible (no status bar, no prefix), passes through
/// everything omp needs (extended keys, clipboard, hyperlinks, focus, titles) and puts omp's
/// image/hyperlink overrides into the global environment (new sessions of an already running
/// server get them). Idempotent under repeated `source-file` (`-q` unbinds, indexed
/// terminal-features, `set-environment` overwrites).
pub fn config_text() -> String {
    let mut text = CONFIG_TEXT.to_owned();
    for (k, v) in OMP_ENV {
        text.push_str(&format!("set-environment -g {k} {v}\n"));
    }
    text
}
