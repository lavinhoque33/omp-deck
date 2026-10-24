# Wave 14 facts (recon 2026-10-24)

User decisions: palette **Ctrl+Shift+K**, quick switcher **Ctrl+Shift+E**; deb/rpm self-update via **pkexec**
(fallback: copyable command + release page); Changes panel is **git diff only** (no per-turn annotation).

## Keys (omp v18.12.6 `packages/tui/src/keybindings.ts`, `app-keybindings.ts`)
- Taken by omp: `ctrl+p` (model cycle fwd), `shift+ctrl+p` (model cycle back), `ctrl+k` (delete to EOL),
  `ctrl+shift+o` (tool visibility), `ctrl+g` (external editor), `ctrl+x`/`ctrl+shift+x` (autoresearch ext).
- Free: `ctrl+shift+k`, `ctrl+shift+e`, `ctrl+shift+j`, `ctrl+tab` (winit delivery unverified).
- OmpDeck shortcuts are consumed in `App::handle_shortcuts` (`app.rs` ~1186) before panels draw; Ctrl+Shift+B/U
  have helpers in `app/tree.rs`/`app/usage.rs`. Overlays need a `modal_open()` entry + the terminal focus-grab list.
- No fuzzy matcher in Cargo.lock; existing filters are substring (`sidebar.rs matches_filter`,
  `model_picker.rs matches_words`, `omp_settings.rs Row::matches`). Decision: in-house subsequence scorer.

## Notifications
- KDE Plasma 6.7.5, spec 1.2, caps incl. `actions`, `persistence`. Plasma emits `ActivationToken(id, token)` then
  `ActionInvoked(id, "default")` on click (plasma-workspace `server.cpp` `Server::invokeAction`); `replaces_id`
  honoured. Hints: `desktop-entry` = `omp-deck` (matches `packaging/omp-deck.desktop`, window app_id `omp-deck`).
- notify-rust drops `ActivationToken` and opens one D-Bus connection + blocked thread per notification → use
  `zbus` 5 (`async-io`, no libdbus) directly with one connection.
- winit 0.30.13 Wayland: `focus_window()` is a no-op; `request_user_attention` = attention only; no API applies an
  external activation token to an existing window. egui `ViewportCommand::Focus` → `focus_window`. Raise on
  Wayland = own `xdg_activation_v1.activate(token, wl_surface)` on winit's `wl_display` (foreign backend
  `wayland_backend::client::Backend::from_foreign_display`, surface via raw window handle). Fallback:
  `Focus` + `RequestUserAttention(Informational)`.
- Windows: `tauri-winrt-notification` 0.8 (`Toast::new(aumid)`, `.on_activated`, `.on_dismissed`; callbacks on
  WinRT threads). Toasts need a Start-menu shortcut carrying `System.AppUserModel.ID` (install.ps1 creates
  `OmpDeck.lnk` without it today) + `SetCurrentProcessExplicitAppUserModelID`; fallback `Toast::POWERSHELL_APP_ID`.
  Raise: `Minimized(false)` → `Focus` (winit forces foreground with the Alt trick) → attention fallback.

## Self-update
- Assets: `omp-deck-{v}-x86_64-linux.tar.gz` (flat dir `omp-deck-{v}-x86_64-linux/` with `omp-deck`,
  `omp-deck-host`, …), `omp-deck-{v}-x86_64.AppImage`, `omp-deck_{v}_amd64.deb`, `omp-deck-{v}-1.x86_64.rpm`,
  `omp-deck-{v}-{x86_64|aarch64}-windows.zip` (arm64 best effort, may be missing); `<asset>.sha256`
  (`<64hex>  <name>`) + `SHA256SUMS`.
- Layouts: install.sh → `~/.local/bin/{omp-deck,omp-deck-host}`; AppImage → `$APPIMAGE` (exe runs from the
  read-only mount); deb/rpm → `/usr/bin`; Windows install.ps1 → `%LOCALAPPDATA%\Programs\OmpDeck\` with
  `omp-deck.exe`, `omp-deck-host.exe`, `conpty.dll`, `OpenConsole.exe` (renames running files to
  `<name>.old-<yyyyMMddHHmmss>` then copies; sweeps leftovers).
- Host lookup: `deck_host::client::daemon_exe()` = `$OMP_DECK_HOST_EXE` or sibling of `current_exe()`. Running
  daemons survive replacement (Linux new inode; Windows rename-aside). Never bump `deck_host::PROTOCOL` casually:
  live sessions of an older protocol become kill-only after an update.
- Latest version: `https://github.com/lavinhoque33/omp-deck/releases/latest` 302 → `/releases/tag/vX.Y.Z`
  (no API rate limit); API `releases/latest` = 60 req/h/IP unauthenticated.
- Restart: tabs persist in `state.json` (flushed in `on_exit` via `writer.finish`); daemons/tmux keep running;
  no single-instance guard → spawn the successor only after the state flush.
- Existing omp update check (`update.rs` `UpdateTracker`, title-bar pill, `AppConfig.check_updates`) is gated on
  omp being installed — the OmpDeck check needs its own tracker/event.

## Changes panel (git)
- Reuse `worktree.rs` git runner (`git_command`/`git_raw` via `usage::run_bounded`, tree kill, 16 MiB cap) — not
  `status.rs::run_capture` (drops non-zero exits/stderr).
- `status --porcelain=v2 -z --branch -unormal`: `1 XY …  <path>`, `2 XY … <path>\0<orig>`, `u XY …`, `? <path>`;
  paths unquoted with `-z`; untracked dirs end in `/`. Diffs: `-c core.quotepath=off -c color.ui=never
  -c diff.noprefix=false -c diff.mnemonicPrefix=false`, `--no-ext-diff --no-textconv`; `GIT_LITERAL_PATHSPECS=1`
  + `--` for every path operand. Untracked diff synthesized in Rust (NUL in first 8000 bytes ⇒ binary).
- Live cwd of the active tab = status watcher `SessionStatus.cwd` (breadcrumb), fallback `tab.cwd`.
- Right panel pattern: agent hub `egui::Panel::right("agent-hub")` (`app/agents.rs`), shown before
  `CentralPanel`; `copy_text`, `PendingConfirm{danger}` for destructive actions.
