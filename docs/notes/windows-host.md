# Windows port + OmpDeck host: verified facts (2026-10-23)

Sources: recon agents AlacrittyWin, CrossCheck, OmpWinTerm, WinMux (registry sources, omp v18.12.6 raw
files, Microsoft docs). `[UNVERIFIED]` = needs a run on real Windows (CI `windows-latest`).

## Compile-checking Windows on this Linux box

```
rustup target add x86_64-pc-windows-msvc        # done
cargo install cargo-xwin --locked               # v0.23.1, done; SDK/CRT cached in ~/.cache/cargo-xwin
export CARGO_TARGET_DIR=$HOME/.cache/omp-deck-xwin   # never the repo's target/
cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc --keep-going -- -D warnings
```
- All C build scripts (ring 0.17, libsqlite3-sys bundled, cc) build under cargo-xwin. `-gnu` needs mingw (absent).
- No Wine: nothing Windows runs locally. Runtime proof = GitHub Actions `windows-latest`.
- Before the port: deck-term 6 errors (arboard Linux ext traits, `tty::Options.escape_args`, `Pty::file()`),
  deck-core 58 (all `std::os::unix::*`). egui/eframe/arboard/rusqlite/ureq/alacritty/polling compile for msvc.

## Runtime Unix assumptions to replace (compile fine, break on Windows)
`sh -c` installer (`omp.rs:315`), `xdg-open` (`app.rs:2659`), kdialog/zenity picker (`transfer.rs`),
`fc-match` (`deck-term/fonts.rs`, `omp-deck/ui_fonts.rs`), `/proc` (`tmux.rs`, `ctl.rs`, `ompdeck-ctl.ts`,
hostname in `backup/mod.rs`, `tabs.rs:278`), exec-bit checks (`omp.rs`, `status.rs on_path`),
`PathBuf::from("/")` fallbacks, `~/.config`/`~/.local/share`/`~/.cache` fallbacks, process groups +
`kill(-pgid)` (`usage.rs`, `omp.rs`), 0600/0700 modes, dev/ino file identity (`tail.rs`, `status.rs`, `tree.rs`),
`OsStrExt::as_bytes` (`path_id`, `sqlite_uri`), FIFO O_NONBLOCK (`agents/tail.rs`).
Rust std on Windows: `FileType::is_symlink()` is true for symlinks and junctions (name-surrogate reparse
points); `OsStr::as_encoded_bytes()` is the portable byte view.

## ConPTY
- Closing a pseudoconsole kills its attached processes; persistence therefore needs a process that owns the
  ConPTY and outlives the GUI.
- Sideload: `LoadLibraryW("conpty.dll")` (default search order: exe dir first) with kernel32 fallback; the
  sideloaded `conpty.dll` starts `OpenConsole.exe` from its own dir (or `x64`/`arm64` subdir), else inbox conhost
  (`microsoft/terminal src/winconpty/winconpty.cpp:45-95`). Source: NuGet `Microsoft.Windows.Console.ConPTY` (MIT).
- Inbox ConPTY drops APC (kitty graphics); ConPTY ≥ 1.22/1.23 passes unrecognized sequences through
  (terminal#19926, cursor-desync caveat). `PSEUDOCONSOLE_PASSTHROUGH_MODE` (0x8) only stable on Win11 22H2+.
- Flags (portable-pty values): INHERIT_CURSOR 0x1 (conhost then blocks on a `CSI 6n` reply), RESIZE_QUIRK 0x2,
  WIN32_INPUT_MODE 0x4, PASSTHROUGH 0x8. windows-sys 0.61 defines only INHERIT_CURSOR. `[UNVERIFIED]` semantics.
- Input: ConPTY input is INPUT_RECORD based. ConPTY asks the hosting terminal for win32-input-mode by emitting
  `CSI ? 9001 h`; keys must then be sent as `CSI Vk;Sc;Uc;Kd;Cs;Rc _`. Plain VT `\r` for Shift+Enter loses the
  modifier. ConPTY does not implement kitty keyboard input.
- alacritty_terminal 0.26 `tty::windows`: flags always 0, `assert_eq!` panics on create/resize errors, program
  path unquoted, process handle leaked, reader type private -> unusable for the host. portable-pty 0.9.0:
  flags 0x1|0x2|0x4 hardcoded, `WinChildKiller::kill` inverted result bug, no tree kill -> not used either.
- Job objects: windows-sys 0.61.2 (already in Cargo.lock) with `Win32_System_JobObjects` +
  `Win32_System_Threading`; `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`; create the child `CREATE_SUSPENDED`, assign,
  then resume (no grandchild escapes the job).
- Resize (observed with omp 18.12.6 in a ConPTY at 140→90→140 columns): ConPTY sends `CSI 8;rows;cols t` and
  repaints its viewport from `CSI H`; omp, which draws into the main screen, then clears screen and
  scrollback (`CSI 2 J`, `CSI 3 J`) and prints its whole transcript again. The local `Term` drops to the
  bottom of the view on the clear, so any resize (changes panel, sidebar drag, window resize) used to throw
  a scrolled-back view to the end. `deck-term` `ScrollKeep` re-pins the distance from the bottom for 1.5 s
  after a resize that started scrolled back (cancelled by wheel, selection autoscroll, typing).
  `[INFERENCE]` why Linux/tmux never showed it: tmux repaints only the visible pane to its client.

## alacritty_terminal state for snapshots (public in 0.26)
Grid cells (`c`, `fg`, `bg`, `flags`, `zerowidth()`, `underline_color()`, `hyperlink()`), history via
`topmost_line()..=bottommost_line()`, `grid().cursor`/`saved_cursor` (point, template SGR, charsets),
`cursor_style()`, `*term.mode()` (all DEC/kitty top-of-stack bits), `term.colors()` (OSC 4/10/11/12 overrides).
Primary grid while in alt screen: `let alt = term.grid().clone(); term.swap_alt(); /* read primary */
term.swap_alt(); *term.grid_mut() = alt;` (verified). Not public: title (record `Event::Title`), scroll region,
tab stops, active GL charset, kitty stack below top. Not tracked at all: `CSI > 4;n m`, `?9001`, `?2031`,
`?2026`, APC images. `vte::ansi::Processor` is re-exported; `Term::new(Config, &dims, listener)` works with any
`EventListener`; `Term` answers DSR/DA/kitty `CSI ? u`/color requests via `Event::PtyWrite`/`ColorRequest`.

## omp 18.12.6 on Windows
- Native `omp-windows-{x64,arm64}.exe`; `install.ps1 -Binary` -> `%LOCALAPPDATA%\omp\omp.exe` (+ user PATH);
  bun installs `%USERPROFILE%\.bun\bin\omp.exe`; npm `omp.cmd` (spawn via `cmd /c`). Data `%USERPROFILE%\.omp`
  (`PI_CONFIG_DIR`/`PI_CODING_AGENT_DIR` override; no XDG). `omp update` renames the running exe aside.
- Keyboard: omp probes `CSI ? u` + DA1 (150 ms). On win32 without a kitty reply it enables `?9001` and decodes
  win32-input records; with a reply it pushes flags 1/3 only. => On Windows the host must NOT answer `CSI ? u`
  (Term `kitty_keyboard = false`); keys travel as win32-input-mode records. `ENABLE_VIRTUAL_TERMINAL_INPUT` is set.
- Env: `PI_FORCE_IMAGE_PROTOCOL=kitty` + `PI_KITTY_PLACEHOLDERS=1`, `PI_FORCE_HYPERLINKS=1` work on win32.
  `WT_SESSION` changes behavior (sync output, Ctrl+Backspace mapping) -> scrub. SSH vars disable the 9001 path.
- Title: win32 uses `SetConsoleTitleW` (ConPTY re-emits OSC 0); bug #16842: helper children set the title to
  bare `omp` -> treat a bare `omp` title as noise on Windows.
- Breadcrumb `<agentDir>/terminal-sessions/<id>`: id = tty path minus `/dev/` (`pts-N`) when stdin is a tty
  (never on win32), else env fallbacks in order ZELLIJ_PANE_ID, TMUX_PANE, CMUX_SURFACE_ID, KITTY_WINDOW_ID,
  WEZTERM_PANE, TERM_SESSION_ID (`apple-<v>`), WT_SESSION (`wt-<v>`). Windows plan: host sets
  `TERM_SESSION_ID=<hex>` -> file `apple-<hex>`.
- Extensions: `-e C:\path\ext.ts` works. Bun `node:net` `createServer().listen("\\\\.\\pipe\\name")` works
  (Bun ≥ 1.1.29); `Bun.listen({unix})` on Windows unsupported. `ompdeck-ctl.ts` must use `node:net` on win32,
  skip the `mode & 0o077` check (Windows modes look world-writable), skip lock file + `/proc` + socket
  lstat/unlink; treat any listen error as "owned elsewhere". `ompdeck-del.ts` is portable.
- Session lease on win32 = named mutex `Global\omp-file-lock-<hash of lock path>`: helpers must run with the
  same `USERPROFILE`/`PI_*` env as the live omp.

## Env for omp children (host backend)
Set: `TERM=xterm-256color`, `COLORTERM=truecolor`, `TERM_PROGRAM=omp-deck`, `PI_FORCE_IMAGE_PROTOCOL=kitty`,
`PI_KITTY_PLACEHOLDERS=1`, `PI_FORCE_HYPERLINKS=1`, Windows: `TERM_SESSION_ID=<16 hex>`.
Remove: `WT_SESSION`, `TMUX`, `TMUX_PANE`, `STY`, `ZELLIJ`, `ZELLIJ_PANE_ID`, `ZELLIJ_SESSION_NAME`,
`CMUX_SURFACE_ID`, `CMUX_WORKSPACE_ID`, `WMUX`, `WMUX_SURFACE_ID`, `KITTY_WINDOW_ID`, `WEZTERM_PANE`,
`GHOSTTY_RESOURCES_DIR`, `ITERM_SESSION_ID`, `LC_TERMINAL`, `VSCODE_PID`, `ALACRITTY_WINDOW_ID`,
`WINDOWID`, `TERM_FEATURES`, `PASEO_TERMINAL_ID`, `SSH_CONNECTION`, `SSH_TTY`, `SSH_CLIENT`,
`TERM_SESSION_ID` (inherited; then set ours on Windows).

## Windows multiplexers surveyed (rejected)
psmux (own vt100 re-render + console attach client = double ConPTY), rmux (too new), Zellij (own UI, IPC bugs),
tmux ports / MSYS2 tmux (extra layer, Cygwin env mangling), WezTerm mux (GUI-only client), Windows Terminal
(no live persistence). Decision: our own per-session host daemon (contracts.md "Wave 12").
