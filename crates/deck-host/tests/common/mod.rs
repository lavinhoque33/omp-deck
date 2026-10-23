//! Helpers shared by the end-to-end host tests: per-test namespaces under temporary
//! directories and a client that mirrors the session into a local terminal like the GUI.
// Each test binary uses a different subset.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Config;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::Processor;
use deck_host::{AttachParams, AttachedTx, HostEvent, Namespace, Palette, SessionSpec};

pub const WAIT: Duration = Duration::from_secs(10);

pub struct Env {
    pub root: PathBuf,
    #[cfg(unix)]
    pub runtime: PathBuf,
}

pub fn env() -> &'static Env {
    static ENV: OnceLock<Env> = OnceLock::new();
    ENV.get_or_init(|| {
        // Roots of earlier runs whose test process is gone.
        #[cfg(target_os = "linux")]
        for entry in std::fs::read_dir(std::env::temp_dir())
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = entry.file_name();
            let pid = name
                .to_str()
                .and_then(|n| n.strip_prefix("omp-deck-host-test-"));
            if pid.is_some_and(|p| !std::path::Path::new(&format!("/proc/{p}")).exists()) {
                let _ = std::fs::remove_dir_all(entry.path());
            }
        }
        let root = std::env::temp_dir().join(format!("omp-deck-host-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        // SAFETY: runs once, before any test thread reads the environment (all tests call
        // `env()` first and block on this initializer).
        unsafe { std::env::set_var("OMP_DECK_HOST_EXE", env!("CARGO_BIN_EXE_omp-deck-host")) };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let private = |dir: &std::path::Path| {
                std::fs::create_dir_all(dir).unwrap();
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
            };
            let runtime = root.join("run");
            private(&runtime);
            // Created up front so tests that make namespace dirs by hand cannot race it to 0755.
            private(&runtime.join("omp-deck-host"));
            std::fs::create_dir_all(root.join("cache")).unwrap();
            // SAFETY: as above.
            unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", &runtime);
                std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
            }
            Env { root, runtime }
        }
        #[cfg(windows)]
        Env { root }
    })
}

/// A test namespace whose sessions are killed on drop.
pub struct TestNs(pub Namespace);

impl TestNs {
    pub fn new() -> TestNs {
        env();
        static N: AtomicUsize = AtomicUsize::new(0);
        let ns = format!(
            "omp-deck-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        );
        TestNs(Namespace::new(&ns).unwrap())
    }

    #[cfg(unix)]
    pub fn dir(&self) -> PathBuf {
        env().runtime.join("omp-deck-host").join(self.0.as_str())
    }
}

impl Drop for TestNs {
    fn drop(&mut self) {
        for name in self.0.list() {
            let _ = self.0.kill(&name);
        }
    }
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = env().root.join("scratch");
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

pub fn params(cols: u16, rows: u16) -> AttachParams {
    AttachParams {
        cols,
        rows,
        cell_width: 8,
        cell_height: 16,
        history_lines: 1000,
        palette: Palette::default(),
    }
}

/// A client that mirrors the session into a local terminal, like the GUI does.
pub struct Client {
    pub tx: AttachedTx,
    pub events: Receiver<deck_host::Result<HostEvent>>,
    pub term: Term<VoidListener>,
    pub processor: Processor,
    pub output: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
}

impl Client {
    pub fn new(attached: deck_host::Attached, cols: u16, rows: u16) -> Client {
        let (mut rx, tx) = attached.split();
        let (events_tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let event = rx.recv();
                let end = matches!(event, Err(_) | Ok(HostEvent::Closed));
                if events_tx.send(event).is_err() || end {
                    break;
                }
            }
        });
        Client {
            tx,
            events,
            term: fresh_term(cols, rows),
            processor: Processor::new(),
            output: Vec::new(),
            cols,
            rows,
        }
    }

    pub fn create(ns: &TestNs, spec: &SessionSpec, cols: u16, rows: u16) -> Client {
        let attached = deck_host::create_or_attach(&ns.0, spec, params(cols, rows)).unwrap();
        let mut c = Client::new(attached, cols, rows);
        c.expect_snapshot();
        c
    }

    pub fn attach(ns: &TestNs, name: &str, cols: u16, rows: u16) -> Client {
        let attached = deck_host::attach(&ns.0, name, params(cols, rows)).unwrap();
        let mut c = Client::new(attached, cols, rows);
        c.expect_snapshot();
        c
    }

    pub fn next(&mut self) -> HostEvent {
        let event = self
            .events
            .recv_timeout(WAIT)
            .expect("host event")
            .expect("recv");
        match &event {
            HostEvent::Snapshot(bytes) => {
                self.term = fresh_term(self.cols, self.rows);
                self.processor = Processor::new();
                self.processor.advance(&mut self.term, bytes);
            }
            HostEvent::Output(bytes) => {
                self.processor.advance(&mut self.term, bytes);
                self.output.extend_from_slice(bytes);
            }
            _ => {}
        }
        event
    }

    pub fn expect_snapshot(&mut self) {
        assert!(
            matches!(self.next(), HostEvent::Snapshot(_)),
            "first event is the snapshot"
        );
    }

    pub fn wait_output(&mut self, needle: &str) {
        let deadline = Instant::now() + WAIT;
        while !String::from_utf8_lossy(&self.output).contains(needle) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {needle:?} in {:?}",
                String::from_utf8_lossy(&self.output)
            );
            match self.next() {
                HostEvent::Output(_) | HostEvent::Snapshot(_) => {}
                other => panic!("unexpected {other:?} while waiting for {needle:?}"),
            }
        }
    }

    /// Consumes output until none arrives for `quiet`.
    pub fn settle(&mut self, quiet: Duration) {
        while let Ok(event) = self.events.recv_timeout(quiet) {
            let event = event.expect("recv");
            if let HostEvent::Output(bytes) = &event {
                self.processor.advance(&mut self.term, bytes);
                self.output.extend_from_slice(bytes);
            }
        }
    }

    pub fn type_line(&self, line: &str) {
        self.tx.input(format!("{line}\r").as_bytes()).unwrap();
    }

    pub fn cursor(&self) -> Point {
        self.term.grid().cursor.point
    }

    pub fn screen(&self) -> String {
        screen_text(&self.term)
    }
}

pub fn fresh_term(cols: u16, rows: u16) -> Term<VoidListener> {
    let config = Config {
        scrolling_history: 10_000,
        kitty_keyboard: true,
        ..Config::default()
    };
    Term::new(
        config,
        &TermSize::new(cols.into(), rows.into()),
        VoidListener,
    )
}

pub fn screen_text(term: &Term<VoidListener>) -> String {
    let grid = term.grid();
    (grid.topmost_line().0..grid.screen_lines() as i32)
        .map(|l| {
            let row: String = (0..grid.columns())
                .map(|c| grid[Line(l)][Column(c)].c)
                .collect();
            row.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
