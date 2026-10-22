//! Headless tmux round trip: our encoder → tmux client (in our PTY) → inner app that requested
//! modifyOtherKeys 2. Socket `omp-deck-test-<pid>`; a Drop guard kills the server and removes
//! its socket file (tmux leaves it behind).
//! Skipped when tmux or python3 is absent.

mod common;

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use common::{TIMEOUT, spawn, wait_file};
use deck_term::SpawnSpec;
use deck_term::input::{self, Key, KeyInput, KeyRules, Mods};

/// docs/notes/tmux.md §Final conf.
const CONF: &str = "\
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
set -s extended-keys-format csi-u
set -s set-clipboard on
set -g allow-passthrough on
set -g focus-events on
set -g set-titles on
set -g set-titles-string '#T'
set -g default-terminal tmux-256color
set -s terminal-features[100] 'xterm-256color:RGB:extkeys:clipboard:hyperlinks:usstyle:sync:focus:title'
set -g allow-rename off
set -g exit-empty on
";

/// Inner app: raw tty, requests modifyOtherKeys 2, appends every stdin byte to argv[1].
const INNER: &str = r#"
import os, select, sys, time, tty
out = sys.argv[1]
fd = sys.stdin.fileno()
tty.setraw(fd)
os.write(1, b"\x1b[>4;2m")
open(out + ".ready", "w").close()
end = time.time() + 60
while time.time() < end:
    r, _, _ = select.select([fd], [], [], 0.1)
    if r:
        data = os.read(fd, 1024)
        if not data:
            break
        with open(out, "ab") as f:
            f.write(data)
"#;

struct TmuxServer {
    socket: String,
    /// `${TMUX_TMPDIR:-/tmp}/tmux-<uid>/<socket>`, as tmux resolves `-L <socket>`.
    socket_path: PathBuf,
}

impl TmuxServer {
    /// `owned` is any path created by this process; its owner uid is ours (tmux uses getuid()).
    fn new(owned: &Path) -> Self {
        let socket = format!("omp-deck-test-{}", std::process::id());
        let uid = std::fs::metadata(owned).expect("metadata").uid();
        let tmpdir = std::env::var_os("TMUX_TMPDIR")
            .filter(|d| !d.is_empty())
            .map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
        let socket_path = tmpdir.join(format!("tmux-{uid}")).join(&socket);
        Self {
            socket,
            socket_path,
        }
    }

    fn cmd(&self, args: &[&str]) -> Option<String> {
        let out = Command::new("tmux")
            .arg("-L")
            .arg(&self.socket)
            .args(args)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .output();
        if let Err(e) = std::fs::remove_file(&self.socket_path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("removing {}: {e}", self.socket_path.display());
        }
    }
}

fn available(prog: &str, arg: &str) -> bool {
    Command::new(prog)
        .arg(arg)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn tmux_modify_other_keys_round_trip() {
    if !available("tmux", "-V") || !available("python3", "--version") {
        eprintln!("skipping: tmux or python3 not available");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let conf = dir.path().join("tmux.conf");
    std::fs::write(&conf, CONF).expect("write conf");
    let inner = dir.path().join("inner.py");
    std::fs::write(&inner, INNER).expect("write inner");
    let log = dir.path().join("keys.log");
    let ready = dir.path().join("keys.log.ready");
    let server = TmuxServer::new(dir.path());

    let s = |p: &Path| p.display().to_string();
    let spec = SpawnSpec {
        program: "env".into(),
        args: [
            "-u",
            "TMUX",
            "-u",
            "WINDOWID",
            "-u",
            "ALACRITTY_WINDOW_ID",
            "tmux",
            "-L",
            &server.socket,
            "-f",
            &s(&conf),
            "new-session",
            "-A",
            "-s",
            "t1",
            "-c",
            &s(dir.path()),
            "--",
            "python3",
            &s(&inner),
            &s(&log),
        ]
        .iter()
        .map(|a| a.to_string())
        .collect(),
        cwd: dir.path().to_path_buf(),
        env: vec![
            ("TERM".into(), "xterm-256color".into()),
            ("COLORTERM".into(), "truecolor".into()),
            ("TERM_PROGRAM".into(), "omp-deck".into()),
        ],
        tmux_client: true,
    };
    let mut t = spawn(spec);
    wait_file(&mut t, &ready, "inner app ready", |_| ready.exists());

    // Wait until tmux applied the inner app's modifyOtherKeys request to the pane.
    let deadline = Instant::now() + TIMEOUT;
    loop {
        t.poll();
        let mode = server
            .cmd(&["display-message", "-p", "-t", "t1", "#{pane_key_mode}"])
            .unwrap_or_default();
        if mode.contains("Ext 2") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pane never entered modifyOtherKeys mode: {mode:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // The guard removes the computed path; it must be the file tmux actually created.
    let live = server
        .cmd(&["display-message", "-p", "#{socket_path}"])
        .unwrap_or_default();
    assert_eq!(Path::new(live.trim()), server.socket_path);
    // tmux releases its own extended-keys handling for the client after our DA reply.
    std::thread::sleep(Duration::from_millis(300));

    let mode = TermMode::empty();
    let steps: [(KeyInput<'_>, &[u8]); 6] = [
        (KeyInput::press(Key::Enter, Mods::SHIFT), b"\x1b[13;2u"),
        (KeyInput::press(Key::Enter, Mods::CTRL), b"\x1b[13;5u"),
        (KeyInput::press(Key::Enter, Mods::ALT), b"\x1b[13;3u"),
        (KeyInput::press(Key::Char('c'), Mods::CTRL), b"\x1b[99;5u"),
        (KeyInput::press(Key::Escape, Mods::NONE), b"\x1b"),
        (
            KeyInput::press(Key::Char('a'), Mods::NONE).with_text("a"),
            b"a",
        ),
    ];
    let mut expected = Vec::new();
    for (key, inner_bytes) in steps {
        let bytes = input::encode_key(&key, mode, KeyRules::Tmux);
        t.write(&bytes);
        expected.extend_from_slice(inner_bytes);
        let got = wait_file(&mut t, &log, &format!("{key:?}"), |d| {
            d.len() >= expected.len()
        });
        assert_eq!(
            String::from_utf8_lossy(&got),
            String::from_utf8_lossy(&expected),
            "after sending {key:?} as {:?}",
            String::from_utf8_lossy(&bytes)
        );
    }
    drop(t);
    drop(server);
}
