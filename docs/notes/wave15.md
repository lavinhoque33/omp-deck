# Wave 15 recon facts (2026-10-24)

Full reports: ReconSplit / ReconHub / ReconReview transcripts. Key facts:

- One visible terminal is assumed via `App.active` (`app.rs:216`), `focus_pending`/`terminal_focused`
  (`app.rs:247-248`), visibility loop `set_visible(i == active)` (`app.rs:2621`), hidden-tab poll for `i != active`
  (`app.rs:2627`), focus reclaim (`app.rs:2641`), `show_terminal` (`app.rs:1608-1650`). `TerminalView::show`
  allocates `ui.available_size()` with an auto id (`deck-term/src/widget.rs:282`), resizes the PTY from its rect,
  takes focus on click, owns IME/blink only when focused.
- Active-tab singletons: `status` watcher, `agents` watcher, single `CtlClient` (`app/ctl.rs:33`), `bar_view`,
  `picker`, `changes`, hub, tree. Keeping `active` = focused pane's tab makes them follow pane focus.
- Bell/notification "in view" use `i == active` (`tabs.rs:146`, `app/notifications.rs:65`, `tracker.rs:45,143`);
  modal input hold only on the active tab (`app/bar.rs:150-164`).
- Persistence: `open_tabs` + `active_tab` (+ `folder_tabs`), persisted index = position among session tabs
  (`app.rs:1728-1773`); uids are not stable across runs.
- The Agent Hub is per **active session** only (`app/agents.rs:27`, `AgentsWatcher` single target), not cross-session.
- Per-session data sources usable off the UI thread for any session file: `SessionStatsReader` (model, thinking,
  context tokens, cost, tokens, last assistant text ≤ 256 KiB, last_turn_failed; incremental), `AgentsScanner`
  (subagents), `git_info(cwd)` (GitInfo counts). Running sessions without a tab: `RunningMonitor` (3 s poll).
  Activity of a tab-less session is not tracked today; tmux sets `#T` titles (`set-titles on`), `#{pane_title}`
  is queryable; `deck-host` `HostInfo` has no title (snapshot holds it, `deck-host/src/snapshot.rs:20`).
- Injection: `Payload::{Slash, Key, Stop}` only; `Slash` is forced to one line by `sanitize_line`
  (`inject.rs:85-104`); bracketed paste hand-built (`inject.rs:~397`), not via `encode_paste_win32`.
  A newline inside a bracketed paste does not submit in omp (omp-actions.md §3.7 item 5). Enter on an empty
  composer while busy aborts the turn. ctl `chat` is subagent-only; there is no main-agent prompt command.
- Changes panel diff rows (`changes_view.rs:183-190`) carry `old`/`new` line numbers; rows are hover-only
  (`changes_view.rs:1090`); `show_diff_pane` ignores its action vec.
