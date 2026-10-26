//! Terminal state: alacritty `Term` + the event listener, fed either by a local PTY child
//! (Unix: alacritty tty + EventLoop) or by a remote session host ([`crate::Remote`]). No egui
//! types except `egui::Context` (used only to wake the UI).

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::Line;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{ClipboardType, Config, Osc52, Term, TermMode};

use crate::clipboard::Clipboard;
use crate::graphics::Graphics;
use crate::input::KeyRules;
use crate::out::{PtyOut, Sink, WriteProgress, register_detaching};
use crate::remote::{self, Feeder, Remote, ScrollKeep};
use crate::scan::{KeyModes, OutputScan};
use crate::theme::{self, Palette, TermTheme};
use crate::wake::{Wake, WakeCallback};
use crate::widget::ViewState;

/// What to run in a local PTY (Unix).
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpawnSpec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: std::path::PathBuf,
    pub env: Vec<(String, String)>,
    /// true when `program` is a tmux client (omp-deck's normal mode). Selects key-encoding rules
    /// because tmux, not the inner app, is the outer terminal's peer.
    pub tmux_client: bool,
}

#[derive(Clone)]
pub struct TermOptions {
    /// Local scrollback lines (tmux and the session host keep their own).
    pub scrollback: usize,
    pub theme: TermTheme,
    /// Shared system clipboard (one per app).
    pub clipboard: Clipboard,
    /// Honor kitty keyboard protocol requests (`CSI > flags u`). A local terminal also answers
    /// `CSI ? u` with it; for a remote one the host answers and this only selects the encoder.
    pub kitty_keyboard: bool,
}

impl Default for TermOptions {
    fn default() -> Self {
        TermOptions {
            scrollback: 10_000,
            theme: TermTheme::Dark,
            clipboard: Clipboard::new(),
            kitty_keyboard: true,
        }
    }
}

/// Events drained by [`Terminal::poll`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalEvents {
    /// Some("") = title reset.
    pub title_changed: Option<String>,
    /// Reported exactly once.
    pub exited: Option<i32>,
    pub bell: bool,
}

/// Visible text of the active screen, see [`Terminal::screen_text`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScreenText {
    /// One entry per screen row, top to bottom.
    pub lines: Vec<String>,
    /// Cursor row, index into `lines`.
    pub cursor_row: usize,
    /// Cursor grid column (cells, 0-based). Equals the char index into `lines[cursor_row]`
    /// only while the row left of the cursor holds no wide chars or combining marks.
    pub cursor_col: usize,
}

/// Minimum spacing of output-driven repaints while the terminal is visible (one 60 Hz frame).
const VISIBLE_REPAINT: Duration = Duration::from_millis(16);
/// Spacing of output-driven repaints while hidden: enough for title/bell/exit via `poll`.
const HIDDEN_REPAINT: Duration = Duration::from_millis(250);

/// Repaint slots, µs since [`RepaintGate::epoch`].
#[derive(Default)]
struct Slots {
    /// Earliest time the next repaint may happen.
    next: u64,
    /// Time of the last scheduled (delayed) repaint; pending while in the future.
    scheduled: u64,
}

/// Coalesces repaint requests caused by PTY output (shared by the PTY thread's listener
/// and [`Terminal::set_visible`]): at most one repaint per interval. After a quiet interval
/// the repaint is immediate (visible) so typing echo is instant; output within an interval
/// schedules one delayed repaint at the interval's end, which occupies that slot.
pub(crate) struct RepaintGate {
    epoch: Instant,
    visible: AtomicBool,
    slots: Mutex<Slots>,
}

impl RepaintGate {
    fn new() -> Self {
        RepaintGate {
            epoch: Instant::now(),
            visible: AtomicBool::new(true),
            slots: Mutex::new(Slots::default()),
        }
    }

    fn slots(&self) -> MutexGuard<'_, Slots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn request(&self, wake: &Wake) {
        let now = u64::try_from(self.epoch.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.request_at(now, wake);
    }

    /// [`Self::request`] at `now` (µs since `epoch`).
    fn request_at(&self, now: u64, wake: &Wake) {
        let visible = self.visible.load(Ordering::Relaxed);
        let interval = if visible {
            VISIBLE_REPAINT
        } else {
            HIDDEN_REPAINT
        };
        let step = interval.as_micros() as u64;
        let mut s = self.slots();
        if s.scheduled > now {
            return;
        }
        if now >= s.next {
            s.next = now.saturating_add(step);
            if visible {
                drop(s);
                wake.now();
                return;
            }
            s.scheduled = s.next;
        } else {
            s.scheduled = s.next;
            s.next = s.next.saturating_add(step);
        }
        let delay = Duration::from_micros(s.scheduled - now);
        drop(s);
        wake.after(delay);
    }

    /// Shown again: a hidden-mode slot must not delay the next output repaint.
    fn reset(&self) {
        *self.slots() = Slots::default();
    }
}

/// What the output side tells [`Terminal::poll`].
pub(crate) enum Notice {
    /// An event from the `Term` (or, locally, the PTY event loop).
    Term(Event),
    /// Remote: the child exited.
    Exited(Option<i32>),
    /// Remote: another client attached; input has stopped.
    Displaced,
    /// Remote: the connection ended.
    Closed,
    /// Remote: a snapshot replaced the `Term` (the title starts over).
    Reset,
    /// Authoritative geometry received by an output observer.
    Resized(WindowSize),
}

/// Called from the output thread (and from `Term` methods on the UI thread while the Term is
/// locked), so it never locks the Term: PtyWrite goes to [`PtyOut`], everything else to the
/// channel + a coalesced repaint request (query events needing an answer from `poll` repaint
/// at once). Remote terminals never answer queries (the host does), except clipboard loads.
#[derive(Clone)]
pub(crate) struct Listener {
    tx: Sender<Notice>,
    out: Arc<PtyOut>,
    wake: Wake,
    gate: Arc<RepaintGate>,
    /// Local terminal: this side answers terminal queries.
    answers: bool,
}

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(s) => {
                if self.answers {
                    self.out.reply(s.into_bytes());
                }
            }
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {
                self.gate.request(&self.wake);
            }
            Event::ColorRequest(..) | Event::TextAreaSizeRequest(_) if !self.answers => {}
            e @ (Event::ColorRequest(..)
            | Event::TextAreaSizeRequest(_)
            | Event::ClipboardLoad(..)) => {
                let _ = self.tx.send(Notice::Term(e));
                self.wake.now();
            }
            e => {
                let _ = self.tx.send(Notice::Term(e));
                self.gate.request(&self.wake);
            }
        }
    }
}

pub(crate) type SharedTerm = Arc<FairMutex<Term<Listener>>>;

/// Shared construction of both terminal kinds.
struct Parts {
    term: SharedTerm,
    config: Config,
    listener: Listener,
    out: Arc<PtyOut>,
    progress: Arc<WriteProgress>,
    tx: Sender<Notice>,
    rx: Receiver<Notice>,
    gate: Arc<RepaintGate>,
    graphics: Arc<Graphics>,
    key_modes: Arc<KeyModes>,
    window_size: WindowSize,
    scroll_keep: Arc<ScrollKeep>,
}

impl Parts {
    /// Initial size 80x24 until the first `TerminalView::show`.
    fn new(opts: &TermOptions, ctx: &egui::Context, answers: bool, wake: Wake) -> Parts {
        let (cols, rows) = (80u16, 24u16);
        let window_size = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 8,
            cell_height: 16,
        };
        let (tx, rx) = mpsc::channel();
        let progress = Arc::new(WriteProgress::default());
        let out = Arc::new(PtyOut::new(progress.clone(), wake.clone()));
        let gate = Arc::new(RepaintGate::new());
        let listener = Listener {
            tx: tx.clone(),
            out: out.clone(),
            wake: wake.clone(),
            gate: gate.clone(),
            answers,
        };
        let config = Config {
            kitty_keyboard: opts.kitty_keyboard,
            osc52: Osc52::CopyPaste,
            scrolling_history: opts.scrollback,
            ..Config::default()
        };
        let term = Term::new(
            config.clone(),
            &TermSize::new(usize::from(cols), usize::from(rows)),
            listener.clone(),
        );
        Parts {
            term: Arc::new(FairMutex::new(term)),
            config,
            listener,
            out,
            progress,
            tx,
            rx,
            gate,
            graphics: Arc::new(Graphics::new(ctx.clone(), wake)),
            key_modes: Arc::new(KeyModes::default()),
            window_size,
            scroll_keep: Arc::default(),
        }
    }

    fn scan(&self) -> OutputScan {
        OutputScan::new(self.key_modes.clone(), self.graphics.clone())
    }

    fn into_terminal(
        self,
        opts: TermOptions,
        tmux_client: bool,
        remote: bool,
        writer: Option<JoinHandle<()>>,
    ) -> Terminal {
        Terminal {
            term: self.term,
            out: self.out,
            rx: self.rx,
            window_size: self.window_size,
            size_sent: !remote,
            clipboard: opts.clipboard,
            palette: Palette::for_theme(opts.theme),
            tmux_client,
            key_modes: self.key_modes,
            scroll_keep: self.scroll_keep,
            title: None,
            exit_code: None,
            exit_reported: false,
            banner: None,
            displaced: false,
            shut_down: false,
            read_only: false,
            writer,
            last_ctrl_c: Cell::new(None),
            input_hold: Cell::new(None),
            gate: self.gate,
            graphics: self.graphics,
            view: ViewState::default(),
        }
    }
}

/// One child with its terminal state: a local PTY child ([`Terminal::spawn`], Unix) or a
/// session on a remote host ([`Terminal::connect`]). Both behave alike towards the widget and
/// the app.
pub struct Terminal {
    pub(crate) term: SharedTerm,
    /// Every byte (and resize) to the child goes through here.
    out: Arc<PtyOut>,
    rx: Receiver<Notice>,
    window_size: WindowSize,
    /// The child knows `window_size` (false for a remote terminal until the first resize: the
    /// host's size is unknown here).
    size_sent: bool,
    clipboard: Clipboard,
    palette: Palette,
    tmux_client: bool,
    /// Keyboard modes requested by the child that `TermMode` lacks (set by the output thread).
    key_modes: Arc<KeyModes>,
    /// Holds a scrolled-back view across the redraw an app answers a resize with.
    pub(crate) scroll_keep: Arc<ScrollKeep>,
    title: Option<String>,
    exit_code: Option<i32>,
    exit_reported: bool,
    /// Text drawn over the bottom row once the session ended here (exit, displaced, lost).
    pub(crate) banner: Option<String>,
    displaced: bool,
    shut_down: bool,
    read_only: bool,
    /// Remote writer thread (joined via [`crate::wait_detached`] after drop).
    writer: Option<JoinHandle<()>>,
    /// When the last byte 0x03 was sent to the PTY (see [`Terminal::last_ctrl_c`]).
    last_ctrl_c: Cell<Option<Instant>>,
    /// User keyboard/paste/IME input is dropped before this instant.
    input_hold: Cell<Option<Instant>>,
    gate: Arc<RepaintGate>,
    /// Kitty graphics images (fed by the output thread, painted by the widget).
    pub(crate) graphics: Arc<Graphics>,
    /// Per-terminal widget state (selection drag, blink timer, render buffers).
    pub(crate) view: ViewState,
}

impl Terminal {
    /// Spawns the PTY child (alacritty tty + EventLoop thread). Initial size 80x24 until the
    /// first `TerminalView::show`.
    #[cfg(unix)]
    pub fn spawn(
        spec: SpawnSpec,
        opts: TermOptions,
        ctx: egui::Context,
    ) -> std::io::Result<Terminal> {
        Self::spawn_with_wake(spec, opts, ctx.clone(), Wake::Gui(ctx))
    }

    /// Spawns without a GUI event loop. The driver polls on output wakeups and the deadlines
    /// returned by its command controller.
    #[cfg(unix)]
    pub fn spawn_headless(
        spec: SpawnSpec,
        opts: TermOptions,
        wake: WakeCallback,
    ) -> std::io::Result<Terminal> {
        Self::spawn_with_wake(spec, opts, egui::Context::default(), Wake::Headless(wake))
    }

    #[cfg(unix)]
    fn spawn_with_wake(
        spec: SpawnSpec,
        opts: TermOptions,
        ctx: egui::Context,
        wake: Wake,
    ) -> std::io::Result<Terminal> {
        use alacritty_terminal::event_loop::EventLoop;
        use alacritty_terminal::tty::{self, Options, Shell};

        let parts = Parts::new(&opts, &ctx, true, wake);
        let tty_opts = Options {
            shell: Some(Shell::new(spec.program, spec.args)),
            working_directory: Some(spec.cwd),
            drain_on_exit: true,
            env: spec.env.into_iter().collect(),
        };
        let pty = tty::new(&tty_opts, parts.window_size, 0)?;
        let pty = crate::pty::TrackedPty::new(pty, parts.scan(), parts.progress.clone())?;
        let event_loop = EventLoop::new(
            parts.term.clone(),
            parts.listener.clone(),
            pty,
            tty_opts.drain_on_exit,
            false,
        )?;
        // Set before the loop starts parsing, so no PtyWrite can be dropped.
        parts.out.set_sink(Sink::Local(event_loop.channel()));
        // Detached: the thread owns the Pty and drops it (SIGHUP + wait) after Msg::Shutdown.
        drop(event_loop.spawn());
        Ok(parts.into_terminal(opts, spec.tmux_client, false, None))
    }

    /// Attaches to a session on a remote host. The host's `Term` answers every terminal query;
    /// this side only renders, except that OSC 52 clipboard reads are answered here (the
    /// clipboard lives here). A [`crate::RemoteEvent::Snapshot`] resets the local `Term` and
    /// replays; the palette of `opts.theme` is sent at once (and again on
    /// [`Self::set_theme`]), the grid size on the first `TerminalView::show`. `Displaced`
    /// shows `[attached elsewhere]` and stops input. Dropping the terminal detaches (queued
    /// input is sent first), [`Self::kill`] kills the session.
    pub fn connect(remote: Box<dyn Remote>, opts: TermOptions, ctx: egui::Context) -> Terminal {
        Self::connect_with_wake(remote, opts, ctx.clone(), Wake::Gui(ctx), false)
    }

    /// Connects a controller without exposing an egui context to the headless caller.
    pub fn connect_headless(
        remote: Box<dyn Remote>,
        opts: TermOptions,
        wake: WakeCallback,
    ) -> Terminal {
        Self::connect_with_wake(
            remote,
            opts,
            egui::Context::default(),
            Wake::Headless(wake),
            false,
        )
    }

    /// Follows output without acquiring input, focus, palette, resize or process-kill rights.
    /// Host geometry arrives through `RemoteEvent::Resize`; widget layout cannot change it.
    pub fn observe(
        observer: Box<dyn remote::Observer>,
        opts: TermOptions,
        ctx: egui::Context,
    ) -> Terminal {
        Self::connect_with_wake(
            Box::new(remote::ObserverRemote(observer)),
            opts,
            ctx.clone(),
            Wake::Gui(ctx),
            true,
        )
    }

    /// Read-only output subscription for a headless caller.
    pub fn observe_headless(
        observer: Box<dyn remote::Observer>,
        opts: TermOptions,
        wake: WakeCallback,
    ) -> Terminal {
        Self::connect_with_wake(
            Box::new(remote::ObserverRemote(observer)),
            opts,
            egui::Context::default(),
            Wake::Headless(wake),
            true,
        )
    }

    fn connect_with_wake(
        remote: Box<dyn Remote>,
        opts: TermOptions,
        ctx: egui::Context,
        wake: Wake,
        read_only: bool,
    ) -> Terminal {
        let parts = Parts::new(&opts, &ctx, false, wake.clone());
        let feeder = Feeder {
            term: parts.term.clone(),
            config: parts.config.clone(),
            listener: parts.listener.clone(),
            scan: parts.scan(),
            out: parts.out.clone(),
            keep: parts.scroll_keep.clone(),
            notices: parts.tx.clone(),
            wake,
        };
        let (cmds, writer) = remote::start(remote, feeder, parts.progress.clone());
        parts.out.set_sink(Sink::Remote(cmds));
        if !read_only {
            parts
                .out
                .palette(Palette::for_theme(opts.theme).host_colors());
        }
        let mut terminal = parts.into_terminal(opts, false, true, writer);
        terminal.read_only = read_only;
        terminal
    }

    /// Drains pending terminal events: answers ColorRequest / TextAreaSizeRequest /
    /// ClipboardLoad, stores ClipboardStore, records title / bell / exit / session state.
    pub fn poll(&mut self) -> TerminalEvents {
        let mut out = TerminalEvents::default();
        while let Ok(notice) = self.rx.try_recv() {
            match notice {
                Notice::Term(event) => self.on_event(event, &mut out),
                Notice::Exited(code) => self.set_exit(code.unwrap_or(-1)),
                Notice::Displaced => {
                    if self.exit_code.is_none() && !self.displaced {
                        self.displaced = true;
                        self.shut_down = true;
                        self.banner = Some("[attached elsewhere]".to_owned());
                    }
                }
                Notice::Closed => {
                    if self.exit_code.is_none()
                        && !self.displaced
                        && !(self.read_only && self.shut_down)
                    {
                        self.exit_code = Some(-1);
                        self.banner = Some("[connection to the session host lost]".to_owned());
                    }
                }
                Notice::Reset => {
                    if self.title.take().is_some() {
                        out.title_changed = Some(String::new());
                    }
                }
                Notice::Resized(size) => {
                    self.window_size = size;
                    self.size_sent = true;
                }
            }
        }
        self.out.flush_due();
        if !self.exit_reported && self.exit_code.is_some() {
            self.exit_reported = true;
            out.exited = self.exit_code;
        }
        out
    }

    fn on_event(&mut self, event: Event, out: &mut TerminalEvents) {
        match event {
            Event::Title(t) => {
                out.title_changed = Some(t.clone());
                self.title = Some(t);
            }
            Event::ResetTitle => {
                self.title = None;
                out.title_changed = Some(String::new());
            }
            Event::Bell => out.bell = true,
            Event::ClipboardStore(..) | Event::ClipboardLoad(..) if self.read_only => {}
            Event::ClipboardStore(ty, text) => match ty {
                ClipboardType::Clipboard => self.clipboard.set_text(&text),
                ClipboardType::Selection => self.clipboard.set_primary(&text),
            },
            Event::ClipboardLoad(ty, fmt) => {
                // Answered from the clipboard worker once the read completes.
                if !self.shut_down {
                    let out = self.out.clone();
                    self.clipboard
                        .load(ty == ClipboardType::Selection, move |text| {
                            out.reply(fmt(text.as_deref().unwrap_or("")).into_bytes());
                        });
                }
            }
            Event::ColorRequest(index, fmt) => {
                let rgb = {
                    let term = self.term.lock();
                    theme::indexed(term.colors(), &self.palette, index)
                };
                self.out.reply(fmt(rgb).into_bytes());
            }
            Event::TextAreaSizeRequest(fmt) => {
                self.out.reply(fmt(self.window_size).into_bytes());
            }
            Event::PtyWrite(s) => self.out.reply(s.into_bytes()),
            Event::ChildExit(status) => self.set_exit(status.code().unwrap_or(-1)),
            Event::Exit => self.set_exit(-1),
            Event::Wakeup | Event::MouseCursorDirty | Event::CursorBlinkingChange => {}
        }
    }

    /// Records the exit code (the first one wins).
    fn set_exit(&mut self, code: i32) {
        if self.exit_code.is_none() && !self.displaced {
            self.exit_code = Some(code);
            self.banner = Some(format!("[process exited: {code}]"));
        }
    }

    /// Last OSC 0/2 title (None after reset).
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Some once the child exited (signal → -1; a lost remote connection → -1).
    pub fn exit_code(&self) -> Option<i32> {
        self.exit_code
    }

    pub fn is_exited(&self) -> bool {
        self.exit_code.is_some()
    }

    /// Remote: another client attached to the session (input has stopped; the session lives
    /// on, so this is not an exit).
    pub fn is_displaced(&self) -> bool {
        self.displaced
    }

    /// Whether the terminal is on screen (default true). While visible, output-driven
    /// repaints are coalesced to one per 16 ms (immediate after a quiet slot, else one delayed
    /// repaint per slot); while hidden, to one delayed repaint per 250 ms (enough for
    /// `poll` to surface title/bell/exit). Query events always repaint at once.
    pub fn set_visible(&self, visible: bool) {
        if self.gate.visible.swap(visible, Ordering::Relaxed) != visible && visible {
            self.gate.reset();
        }
    }

    /// Switches the palette; a remote host gets the new query colors.
    pub fn set_theme(&mut self, theme: TermTheme) {
        let palette = Palette::for_theme(theme);
        if palette != self.palette {
            if !self.shut_down && !self.read_only {
                self.out.palette(palette.host_colors());
            }
            self.palette = palette;
        }
    }

    /// Records keyboard focus. On a change it updates the Term and, when the child enabled
    /// focus reporting (DECSET 1004), writes `ESC[I` / `ESC[O`. [`crate::TerminalView`] uses
    /// the same state, so calling `set_focused(false)` on a tab being hidden and showing it
    /// again later yields exactly one out/in pair.
    ///
    /// Like every terminal-generated byte (query replies too), a report due less than 100 ms
    /// after a [`Self::write`] that ended with ESC is queued until that quiet time passed (sent
    /// by a later [`Self::poll`], which the widget runs every frame; a repaint is requested)
    /// or until the next write that does not end with ESC, right behind its bytes. An out/in
    /// pair falling into the same quiet time cancels out.
    pub fn set_focused(&mut self, focused: bool) {
        if focused == self.view.focused {
            return;
        }
        self.view.focused = focused;
        let mode = {
            let mut term = self.term.lock();
            term.is_focused = focused;
            *term.mode()
        };
        if !self.read_only
            && let Some(bytes) = crate::input::encode_focus(focused, mode)
        {
            self.out.focus_report(bytes.to_vec());
        }
        self.view.on_focus_change(focused);
    }

    /// Ends the child at once. Local: Msg::Shutdown → the PTY thread drops the Pty, which
    /// SIGHUPs the child. Remote: the host kills the session. Input not yet written is
    /// discarded (unlike drop, see [`Terminal`]'s `Drop`).
    pub fn kill(&mut self) {
        if self.read_only {
            self.shut_down = true;
            self.out.displaced();
            self.banner = Some("[observer detached]".to_owned());
            return;
        }
        if !self.shut_down {
            self.shut_down = true;
            self.out.kill();
            if let Some(writer) = self.writer.take() {
                register_detaching(writer);
            }
        }
        self.set_exit(-1);
    }

    /// Raw input to the child (keys, paste, injected text): sent at once, in order, never held
    /// by [`Self::hold_input_until`] or by the lone-ESC quiet time (the caller's
    /// responsibility).
    pub fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() || self.shut_down || self.read_only {
            return;
        }
        if bytes.contains(&0x03) {
            self.note_ctrl_c();
        }
        self.out.write(bytes);
    }

    /// Whether this connection still owns its write path. Observer and displaced/lost
    /// connections must be refused by command controllers, never reported as delivered.
    pub fn accepts_input(&self) -> bool {
        !self.read_only && !self.shut_down && !self.out.is_closed()
    }

    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// A checkpoint for asynchronous command delivery, without waiting on the I/O thread.
    pub fn input_progress(&self) -> crate::InputProgress {
        self.out.input_progress()
    }

    /// While `Instant::now() < until`, all user keyboard/paste/IME input to this terminal is
    /// dropped (mouse reports, wheel, selection and focus reports are unaffected; [`Self::write`]
    /// is never held). Each call replaces the previous deadline.
    pub fn hold_input_until(&self, until: Instant) {
        self.input_hold.set(Some(until));
    }

    /// User keystrokes/pastes must not pass while an injected sequence owns input.
    /// Raw [`Self::write`] and [`Self::send_key`] deliberately bypass this gate for the injector.
    pub fn input_held(&self) -> bool {
        self.read_only || self.input_hold.get().is_some_and(|u| Instant::now() < u)
    }

    /// Instant the last Ctrl+C was written to the child: any 0x03 byte (key input or
    /// [`Self::write`]) or a Ctrl+C key press encoded without one (kitty CSI-u,
    /// win32-input-mode). omp exits on two Ctrl+C within 500 ms; injectors must keep ≥ 650 ms
    /// distance.
    pub fn last_ctrl_c(&self) -> Option<Instant> {
        self.last_ctrl_c.get()
    }

    pub(crate) fn note_ctrl_c(&self) {
        self.last_ctrl_c.set(Some(Instant::now()));
    }

    /// Visible rows of the active screen (viewport at the bottom, i.e. ignoring the local
    /// scrollback position): wide-char spacers skipped, combining marks kept, trailing blanks
    /// trimmed.
    pub fn screen_text(&self) -> ScreenText {
        let term = self.term.lock();
        let grid = term.grid();
        let rows = term.screen_lines();
        let mut lines = Vec::with_capacity(rows);
        for line in 0..rows {
            let mut out = String::with_capacity(term.columns());
            for cell in &grid[Line(line as i32)][..] {
                if cell
                    .flags
                    .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                {
                    continue;
                }
                out.push(cell.c);
                if let Some(marks) = cell.zerowidth() {
                    out.extend(marks.iter());
                }
            }
            out.truncate(out.trim_end().len());
            lines.push(out);
        }
        let cursor = grid.cursor.point;
        ScreenText {
            lines,
            cursor_row: usize::try_from(cursor.line.0).unwrap_or(0),
            cursor_col: cursor.column.0,
        }
    }

    pub(crate) fn palette(&self) -> &Palette {
        &self.palette
    }

    pub(crate) fn clipboard(&self) -> &Clipboard {
        &self.clipboard
    }

    /// Non-kitty key-encoding rules: win32-input-mode while the child requested it (ConPTY);
    /// else tmux always speaks modifyOtherKeys 2 to us; a direct child gets rule 2 while it
    /// has requested level ≥ 2.
    pub(crate) fn key_rules(&self) -> KeyRules {
        if self.key_modes.win32_input.load(Ordering::Relaxed) {
            KeyRules::Win32
        } else if self.tmux_client {
            KeyRules::Tmux
        } else if self.key_modes.modify_other_keys.load(Ordering::Relaxed) >= 2 {
            KeyRules::ModifyOtherKeys
        } else {
            KeyRules::Legacy
        }
    }

    pub(crate) fn mode(&self) -> TermMode {
        let mode = *self.term.lock().mode();
        if self.read_only {
            mode.difference(TermMode::MOUSE_MODE | TermMode::ALTERNATE_SCROLL)
        } else {
            mode
        }
    }

    /// Resizes the Term and tells the child (local SIGWINCH, remote host) when the grid or the
    /// cell pixel size changed. Grids are clamped to at least 2×1 cells. Read-only observers
    /// ignore this call and follow their controller's geometry.
    pub fn resize(&mut self, cols: u16, rows: u16, cell_width: u16, cell_height: u16) {
        if self.read_only {
            return;
        }
        let cols = cols.max(2);
        let rows = rows.max(1);
        let ws = WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width,
            cell_height,
        };
        let old = self.window_size;
        let same = (old.num_lines, old.num_cols, old.cell_width, old.cell_height)
            == (ws.num_lines, ws.num_cols, ws.cell_width, ws.cell_height);
        if same && self.size_sent {
            return;
        }
        self.window_size = ws;
        self.size_sent = true;
        if (old.num_lines, old.num_cols) != (rows, cols) {
            let mut term = self.term.lock();
            self.scroll_keep.arm(term.grid().display_offset());
            term.resize(TermSize::new(usize::from(cols), usize::from(rows)));
        }
        if !self.shut_down {
            self.out.resize(ws);
        }
    }
}

impl Drop for Terminal {
    /// Detaches without blocking: every byte already passed to [`Terminal::write`] (and
    /// every queued reply, after the lone-ESC quiet time) still reaches the child before the
    /// connection ends (local: the PTY thread hangs up, which SIGHUPs only the tmux client;
    /// remote: the host is told to detach). When bytes are in flight a short-lived thread
    /// waits for them to be written (bounded by 2 s) plus 50 ms after the last write for the
    /// child to read them, then ends the connection. That thread and a remote writer are
    /// registered for [`crate::wait_detached`].
    fn drop(&mut self) {
        if let Some(writer) = self.writer.take() {
            register_detaching(writer);
        }
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        if !self.out.close() {
            self.out.shutdown();
            return;
        }
        let out = self.out.clone();
        let spawned = std::thread::Builder::new()
            .name("deck-term-detach".into())
            .spawn(move || out.finish_detach());
        match spawned {
            Ok(handle) => register_detaching(handle),
            Err(_) => self.out.shutdown(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every repaint request the gate makes, as its delay.
    fn observed(ctx: &egui::Context) -> Arc<std::sync::Mutex<Vec<Duration>>> {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        ctx.set_repaint_observer(move |info| sink.lock().expect("lock").push(info.delay));
        seen
    }

    #[test]
    fn repaint_gate_coalesces_output_wakeups() {
        let ctx = egui::Context::default();
        let seen = observed(&ctx);
        let wake = Wake::Gui(ctx);
        let gate = RepaintGate::new();

        // Simulated clock (µs since the gate's epoch), so slot boundaries are exact.
        let v = VISIBLE_REPAINT.as_micros() as u64;
        let h = HIDDEN_REPAINT.as_micros() as u64;
        let take = || std::mem::take(&mut *seen.lock().expect("lock"));
        let burst = |from: u64| {
            for k in 0..50 {
                gate.request_at(from + k * 100, &wake);
            }
        };

        // Burst inside one slot: one immediate repaint, then one at the slot end.
        burst(1_000);
        assert_eq!(take(), [Duration::ZERO, Duration::from_micros(v - 100)]);
        // Sustained output in the following slot: delayed again (≤ 1 repaint per slot).
        burst(1_000 + v + 100);
        assert_eq!(take(), [Duration::from_micros(v - 100)]);
        // After a quiet slot: immediate again (typing echo).
        gate.request_at(1_000 + 4 * v, &wake);
        assert_eq!(take(), [Duration::ZERO]);

        // Hidden: never immediate; one repaint per 250 ms.
        gate.visible.store(false, Ordering::Relaxed);
        let t0 = 1_000 + 6 * v;
        burst(t0);
        assert_eq!(take(), [HIDDEN_REPAINT]);
        gate.request_at(t0 + h - 1, &wake);
        assert!(take().is_empty());
        gate.request_at(t0 + h + 10, &wake);
        assert_eq!(take(), [HIDDEN_REPAINT]);
    }

    /// Local PTY children (`sh`, `cat`, `sleep`).
    #[cfg(unix)]
    mod local {
        use super::*;
        use alacritty_terminal::index::{Column, Point};

        fn spawn(script: &str) -> Terminal {
            let spec = SpawnSpec {
                program: "sh".into(),
                args: vec!["-c".into(), script.into()],
                cwd: std::env::temp_dir(),
                env: vec![("TERM".into(), "xterm-256color".into())],
                tmux_client: false,
            };
            Terminal::spawn(spec, TermOptions::default(), egui::Context::default()).expect("spawn")
        }

        fn row_text(t: &Terminal, line: i32) -> String {
            let term = t.term.lock();
            let cols = term.columns();
            (0..cols)
                .map(|c| term.grid()[Point::new(Line(line), Column(c))].c)
                .collect()
        }

        fn wait_exit(t: &mut Terminal) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !t.is_exited() {
                assert!(Instant::now() < deadline, "child did not exit");
                t.poll();
                std::thread::sleep(Duration::from_millis(10));
            }
            // ChildExit can precede the drained output; wait for Exit (grid final).
            std::thread::sleep(Duration::from_millis(100));
            t.poll();
        }

        #[test]
        fn grid_contents_colors_and_wide_chars() {
            let mut t = spawn("printf 'hello \\033[31mred\\033[0m 漢x'; sleep 0.2");
            wait_exit(&mut t);
            let row = row_text(&t, 0);
            assert!(row.starts_with("hello red 漢"), "row: {row:?}");
            let term = t.term.lock();
            let grid = term.grid();
            let red = &grid[Point::new(Line(0), Column(6))];
            assert_eq!(red.c, 'r');
            assert_eq!(
                red.fg,
                alacritty_terminal::vte::ansi::Color::Named(
                    alacritty_terminal::vte::ansi::NamedColor::Red
                )
            );
            let wide = &grid[Point::new(Line(0), Column(10))];
            assert!(
                wide.flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR)
            );
            assert!(
                grid[Point::new(Line(0), Column(11))]
                    .flags
                    .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
            );
            assert_eq!(grid[Point::new(Line(0), Column(12))].c, 'x');
        }

        #[test]
        fn combining_mark_kept_as_zerowidth() {
            let mut t = spawn(r"printf 'e\314\201x'; sleep 0.2");
            wait_exit(&mut t);
            let term = t.term.lock();
            let grid = term.grid();
            let e = &grid[Point::new(Line(0), Column(0))];
            assert_eq!(e.c, 'e');
            assert_eq!(e.zerowidth(), Some(&['\u{301}'][..]));
            assert_eq!(grid[Point::new(Line(0), Column(1))].c, 'x');
        }

        #[test]
        fn resize_updates_term_grid() {
            let mut t = spawn("sleep 2");
            t.resize(100, 30, 9, 18);
            {
                let term = t.term.lock();
                assert_eq!((term.columns(), term.screen_lines()), (100, 30));
            }
            t.kill();
            assert_eq!(t.poll().exited, Some(-1));
            assert_eq!(t.poll().exited, None);
        }

        #[test]
        fn screen_text_wide_chars_cursor_and_scrollback() {
            let t = spawn(
                "i=1; while [ $i -le 30 ]; do printf 'l%d\\r\\n' $i; i=$((i+1)); done; \
                 printf '> a漢be\\314\\201  \\033[2D'; sleep 3",
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut text = t.screen_text();
            while text.lines.get(23).map(String::as_str) != Some("> a漢be\u{301}") {
                assert!(Instant::now() < deadline, "screen: {text:?}");
                std::thread::sleep(Duration::from_millis(10));
                text = t.screen_text();
            }
            assert_eq!(text.lines.len(), 24);
            assert_eq!(text.lines[0], "l8");
            assert_eq!(text.lines[22], "l30");
            // Two cells for 漢, none for the combining mark; then back two columns.
            assert_eq!((text.cursor_row, text.cursor_col), (23, 7));
            // A local scrollback position does not change what is reported.
            t.term
                .lock()
                .scroll_display(alacritty_terminal::grid::Scroll::Delta(5));
            assert_eq!(t.screen_text(), text);
        }

        #[test]
        fn screen_text_empty_screen() {
            let t = spawn("sleep 2");
            let text = t.screen_text();
            assert_eq!(text.lines, vec![String::new(); 24]);
            assert_eq!((text.cursor_row, text.cursor_col), (0, 0));
        }

        #[test]
        fn set_visible_resets_hidden_window() {
            let ctx = egui::Context::default();
            let seen = observed(&ctx);
            let t = Terminal::spawn(
                SpawnSpec {
                    program: "sleep".into(),
                    args: vec!["2".into()],
                    cwd: std::env::temp_dir(),
                    env: Vec::new(),
                    tmux_client: false,
                },
                TermOptions::default(),
                ctx.clone(),
            )
            .expect("spawn");
            t.set_visible(false);
            t.gate.request(&Wake::Gui(ctx.clone()));
            t.set_visible(true);
            seen.lock().expect("lock").clear();
            t.gate.request(&Wake::Gui(ctx));
            assert_eq!(*seen.lock().expect("lock"), [Duration::ZERO]);
        }

        #[test]
        fn last_ctrl_c_tracks_etx_bytes() {
            let t = spawn("cat -v");
            assert_eq!(t.last_ctrl_c(), None);
            t.write(b"abc\x1b[A");
            assert_eq!(t.last_ctrl_c(), None);
            let before = Instant::now();
            t.write(b"x\x03");
            let first = t.last_ctrl_c().expect("0x03 recorded");
            assert!(first >= before && first <= Instant::now());
            t.write(b"plain");
            assert_eq!(t.last_ctrl_c(), Some(first));
            std::thread::sleep(Duration::from_millis(5));
            t.write(&[0x03]);
            assert!(t.last_ctrl_c().expect("second 0x03") > first);
        }
    }
}
