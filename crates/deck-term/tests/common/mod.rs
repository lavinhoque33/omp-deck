#![allow(dead_code)]

use std::path::Path;
use std::time::{Duration, Instant};

use deck_term::{SpawnSpec, TermOptions, Terminal, TerminalEvents};

pub const TIMEOUT: Duration = Duration::from_secs(10);

pub fn spawn_sh(script: &str, tmux_client: bool) -> Terminal {
    spawn(SpawnSpec {
        program: "sh".into(),
        args: vec!["-c".into(), script.into()],
        cwd: std::env::temp_dir(),
        env: vec![("TERM".into(), "xterm-256color".into())],
        tmux_client,
    })
}

pub fn spawn(spec: SpawnSpec) -> Terminal {
    Terminal::spawn(spec, TermOptions::default(), egui::Context::default())
        .expect("spawn pty child")
}

/// Polls `t` until `f` returns Some, failing after `TIMEOUT`. Every poll result is fed to `f`.
pub fn poll_until<T>(
    t: &mut Terminal,
    what: &str,
    mut f: impl FnMut(&TerminalEvents, &Terminal) -> Option<T>,
) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let ev = t.poll();
        if let Some(v) = f(&ev, t) {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Polls the terminal (answering queries) until `path` contains `pred`-satisfying bytes.
pub fn wait_file(
    t: &mut Terminal,
    path: &Path,
    what: &str,
    pred: impl Fn(&[u8]) -> bool,
) -> Vec<u8> {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        t.poll();
        let data = std::fs::read(path).unwrap_or_default();
        if pred(&data) {
            return data;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; file has {:?}",
            String::from_utf8_lossy(&data)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Single-quotes `s` for sh.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}
