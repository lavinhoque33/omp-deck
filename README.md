# OmpDeck

A native desktop wrapper for Linux and Windows around [`omp`](https://github.com/can1357/oh-my-pi) (oh-my-pi),
the coding-agent TUI. A sidebar lists every persisted omp session; clicking one opens a tab running that session
in an embedded, fully interactive terminal. The session runs outside the app (in tmux, or in OmpDeck's own session
host), so closing a tab or the whole app leaves omp running.

## Features

- **Session sidebar**: an "Open tabs" section pinned at the top lists every open tab with its status
  (click to switch, ✕ or middle-click to close), above every omp session grouped by project (only the active
  tab's project is expanded; the rest stay collapsed until you open them), newest first, with titles, relative age,
  a running indicator (sessions open in a tab show the tab's working spinner / needs-input badge instead),
  a filter box, background auto-refresh, and context menus (open, copy/move to another
  project, copy path/id/resume command, "Restart session" for running sessions, "Stop session process",
  and "Delete session…"). Deleting is permanent: it removes the session file and its subagent transcripts
  and artifacts (stopping a running session first) and cannot be undone.
- **Tabs**: open, focus, middle-click close, reopen the last closed tab. Each tab shows omp's state: a
  spinner while the agent works and an amber "!" badge with a tinted tab when it waits for your input,
  also on background tabs. Right-clicking a tab (or its "Open tabs" row) shows "Close tab" plus the
  session's full sidebar menu. Closing a tab only detaches; "Stop session process" stops omp.
  Tabs re-attach to their live sessions on the next launch.
- **Terminal fidelity**: truecolor, wide/emoji cells, mouse reporting, bracketed paste, OSC 52 clipboard,
  clickable hyperlinks and plain URLs (Ctrl+click), a right-click copy / paste / link menu,
  kitty CSI-u / modifyOtherKeys key encoding, inline image previews (omp's kitty
  graphics images, shown through Unicode placeholders).
- **Session status bar**: model, thinking effort, path, git state, context meter, tokens and cost, plus a
  model picker and actions (handoff, compact, fork, rename, plan, retry, stop, export). Hovering the
  context meter shows omp's `/context` breakdown (system prompt, tools, skills, messages, free space,
  auto-compact buffer, per-tool schema sizes) for tabs running with live control.
- **Usage**: omp's `/usage` as a native view (Ctrl+Shift+U or the status-bar usage chip): remaining quota
  per provider and window with reset times, a year of activity as a heatmap, and top models by cost.
- **Agents dashboard** (Ctrl+Shift+A or the status-bar Agents chip): one card per open tab and per session still
  running in the background, with its state (working, needs input, idle, ended), model, context use, cost,
  tokens, changed files and last reply; expand a card to see its subagents and open their transcripts. Search and
  filter by state; click a card to jump to that session.
- **Split view**: show two to four tabs side by side, stacked or in a grid (tab or sidebar menu "Open in split
  right / down", or the command palette); each pane is a full terminal, Ctrl+Alt+Arrow moves between panes, and
  the status bar follows the focused pane. The layout is restored on the next start.
- **Session tree**: browse the session tree, fork or navigate from a node.
- **omp updates and settings**: update check badge, update actions, and a searchable editor for every `omp config` key.
- **Google Drive backup**: sign in with Google and back up your omp config (settings, models,
  custom agents, skills, …) and prompt history to a private Drive app folder, automatically when it
  changes or on demand, and restore it on any machine (Settings → OmpDeck). See
  [below](#google-drive-backup).
- **Project sync**: back up chosen project folders' `.omp` folder and omp sessions to the same private
  Drive app folder (Settings → Sync, or right-click a project in the sidebar), and restore them into any
  folder on any machine; restored sessions are added alongside the existing ones. See
  [below](#project-sync).
- **Worktree sessions**: run several agents on one repository in parallel, each in its own git worktree
  and branch: start a session in a new worktree, move a running session into one, finish it (the agent
  merges following the project's own rules) and let OmpDeck remove it once it is merged. See
  [below](#worktree-sessions).
- **Desktop notifications**: when a background tab finishes a turn, needs your attention or exits, OmpDeck
  shows a desktop notification (KDE/GNOME notifications on Linux, toasts on Windows); clicking it brings
  OmpDeck forward on that tab. By default only while the OmpDeck window is not focused; each event can be
  turned off in Settings → OmpDeck → Notifications.
- **Changes panel** (Ctrl+Shift+G, or click the git chip in the status bar): the active tab's git changes,
  grouped into conflicted, staged, changed and untracked files, with a diff view, stage / unstage, discard,
  delete untracked files (both confirmed first) and copy the whole diff. Click a diff line (Shift+click for a
  range) to leave a review comment; **Insert into omp** puts all comments with file and line references into
  omp's input for you to edit, **Send** submits them when omp is idle.
- **Command palette** (Ctrl+Shift+K) runs any OmpDeck action by typing part of its name: menus, session
  and project actions, model and thinking switches, settings pages. The **quick switcher** (Ctrl+Shift+E)
  jumps to an open tab, a session or a project.
- **OmpDeck updates**: OmpDeck checks for new releases (at startup and every 6 hours, can be turned off) and
  shows a title-bar badge; **Update and restart** downloads, verifies and installs the new version, then
  restarts OmpDeck with your tabs re-attached. See [below](#updating-ompdeck).
- **Per-folder mode**: scope the sidebar to a single project folder.
- **Appearance**: a flat, neutral default look (Geist font, lucide icons, compact spacing) or the
  "Midnight neon" style, each in dark and light, plus a Comfortable density option (Settings → OmpDeck).
  Integrated title bar (or native decorations), persisted window geometry and open tabs.

OmpDeck never writes to the omp session store directly; changes go through omp's own CLI.

## Requirements

- **Linux** x86_64, Wayland or X11, with fontconfig; or **Windows** 10 (1809 or newer) / 11, x64
  (arm64 builds are published when they build).
- [`omp`](https://github.com/can1357/oh-my-pi) (required): on `PATH`, in `~/.local/bin` (Linux), or in
  `%LOCALAPPDATA%\omp` / `%USERPROFILE%\.bun\bin` (Windows). If it is missing, the installers install it, and
  OmpDeck shows an "Install omp" button (on the start screen, in Settings and in Usage) that runs omp's official
  installer (prebuilt binary, no admin rights) in a tab and picks omp up when it finishes, without a restart.
- Linux: `tmux` (optional). OmpDeck uses its own tmux server (`tmux -L omp-deck`) and never touches your
  default one. Without tmux it uses its own session host instead (see [Session backends](#session-backends)).

### Session backends

Each omp tab runs in a session that lives outside the app, so it survives closing the tab or OmpDeck:

- **tmux** (Linux default when installed): a dedicated, invisible tmux server `-L omp-deck`.
- **OmpDeck host** (always on Windows; on Linux when tmux is missing or when chosen): `omp-deck-host`, a small
  per-session daemon shipped next to `omp-deck`. It owns the session's pseudo-terminal (a ConPTY on Windows),
  keeps the screen and 50 000 lines of scrollback, answers terminal queries while no tab is attached, and
  restores the screen when a tab re-attaches. Sessions are reachable only by your user (a private socket
  directory on Linux; named pipes restricted to your account on Windows).

On Linux, Settings → OmpDeck → **Session backend** chooses Auto (tmux if installed, else the host), tmux or
OmpDeck host for new tabs; running sessions keep their backend. If neither is available, omp runs directly in
the tab, a banner says so, and sessions end with the tab.

`omp-deck-host ls|info <name>|dump <name>|kill <name>` inspect and stop host sessions from a shell.

## Install

**One-liner** (installs the latest release into `~/.local`, no root):

```sh
curl -fsSL https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.sh | sh
```

It also installs omp (omp's official installer, prebuilt binary into `~/.local/bin`) when omp is not found.
Options (pass after `sh -s --`): `--version <x.y.z>`, `--prefix <dir>`, `--no-omp` (skip installing omp),
`--uninstall` (removes OmpDeck only; omp stays).
Make sure `~/.local/bin` is on your `PATH`.

**Release tarball**: download `omp-deck-<version>-x86_64-linux.tar.gz` from the
[Releases page](https://github.com/lavinhoque33/omp-deck/releases), extract it, and run `./install.sh`
(or `sh install.sh`).

**AppImage**: download `omp-deck-<version>-x86_64.AppImage`, then `chmod +x` it and run it.

**Debian / Ubuntu**: `sudo apt install ./omp-deck_<version>_amd64.deb`

**Fedora / openSUSE**: `sudo dnf install ./omp-deck-<version>-1.x86_64.rpm`

**Windows** (PowerShell, no admin rights; installs into `%LOCALAPPDATA%\Programs\OmpDeck` with a Start menu
entry, and installs omp when it is not found):

```powershell
irm https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.ps1 | iex
```

Options: `& ([scriptblock]::Create((irm https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.ps1))) -Version x.y.z -NoOmp`
(or `-Uninstall`). Alternatively download `omp-deck-<version>-x86_64-windows.zip`, extract it anywhere and run
`omp-deck.exe`; keep `omp-deck-host.exe`, `conpty.dll` and `OpenConsole.exe` next to it.

Release assets come with `.sha256` files and a combined `SHA256SUMS`. Linux binaries are built on
Ubuntu 22.04 (glibc 2.35 or newer).

**From source** (Rust stable, 2024 edition; on Debian/Ubuntu you need
`pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev`):

```sh
cargo install --git https://github.com/lavinhoque33/omp-deck omp-deck deck-host --locked
```

This installs only the binaries (`omp-deck` and `omp-deck-host`). For the menu entry and icon, use one of the
packages above.

### Updating OmpDeck

When a newer release exists, a title-bar badge "OmpDeck x.y.z" offers **Update and restart** (also in
Settings → OmpDeck and the command palette). The download is checked against the release's `.sha256`
file before anything is replaced; running sessions keep running and the tabs re-attach after the restart.

- `install.sh`, tarball and Windows installs (or any folder you can write to) are updated in place.
- The AppImage file is replaced with the new one.
- `.deb` / `.rpm` installs are updated through your package manager after your system asks for your
  password (`pkexec`); without `pkexec`, OmpDeck shows the command to run.
- Builds run from a source checkout (`cargo run`) never update themselves.

**Skip this version** hides the badge until a newer release appears; Settings → OmpDeck turns the
automatic check off.

## Usage

```
omp-deck [OPTIONS] [FOLDER]

  [FOLDER]                Per-folder mode: show only the sessions of FOLDER (and below)
  --folder <dir>          Same as the FOLDER argument
  --sessions-root <dir>   Read omp sessions from <dir> instead of the default store
  -h, --help              Print this help
```

The default session store is `$PI_CODING_AGENT_DIR/sessions` if set, otherwise `~/.omp/agent/sessions`
(Windows: `%USERPROFILE%\.omp\agent\sessions`; `$PI_CONFIG_DIR` replaces `.omp`).

### Shortcuts

| Shortcut | Action |
| --- | --- |
| Ctrl+Shift+T | Reopen last closed tab |
| Ctrl+Shift+K | Command palette |
| Ctrl+Shift+E | Quick switcher: open tabs, sessions, projects |
| Ctrl+Shift+G | Changes panel |
| Ctrl+PgUp / Ctrl+PgDn | Previous / next tab |
| Ctrl+Shift+A | Agents dashboard |
| Ctrl+Alt+Arrow | Move focus between split panes |
| Ctrl+Shift+B | Session tree |
| Ctrl+Shift+= / - / 0 | Font larger / smaller / reset |
| Ctrl+Shift+C | Copy terminal selection |
| Ctrl+Shift+V, Shift+Insert | Paste |
| Shift+drag | Always select locally, even when the app uses mouse reporting |
| Ctrl+click | Open a hyperlink or a plain `http(s)://` / `mailto:` URL |
| Right click | Terminal menu: copy, paste, open / copy the link under the pointer |

Ctrl+C, Ctrl+X and Ctrl+V go straight to omp (omp uses Ctrl+V for image paste).

## Configuration and data

| Path | Contents |
| --- | --- |
| `$XDG_CONFIG_HOME/omp-deck/` (default `~/.config/omp-deck/`; Windows `%APPDATA%\omp-deck\`) | `state.json` (window, tabs, settings, Sync folders), generated `tmux.conf`, `google-drive.json` (Google sign-in, mode 0600), `project-sync.json` (last project backups, mode 0600), `ctl.key` (Windows: secret for the live-control pipes) |
| `$XDG_DATA_HOME/omp-deck/` (default `~/.local/share/omp-deck/`; Windows `%APPDATA%\omp-deck\`) | live-control extension loaded into omp tabs, `restore-safety/` (copies kept before a Drive restore; `project-*` for project restores) |
| `$XDG_CACHE_HOME/omp-deck/` (default `~/.cache/omp-deck/`; Windows `%LOCALAPPDATA%\omp-deck\`) | `project-sync/` temporary archives while a project backup or restore runs (removed afterwards), `host/` session-host logs |

Environment: `OMP_DECK_TMUX_SOCKET` / `OMP_DECK_HOST_NS` select a different tmux socket / host namespace
(name must match `omp-deck-test-[A-Za-z0-9_-]+`); `OMP_DECK_HOST_EXE` points at another `omp-deck-host`;
`OMP_DECK_CONPTY_FLAGS` overrides the ConPTY creation flags (Windows, troubleshooting);
`OMP_DECK_GOOGLE_CLIENT_ID` / `OMP_DECK_GOOGLE_CLIENT_SECRET` supply or override the Google OAuth
client for Drive backup; `RUST_LOG` enables logging.

## Google Drive backup

Settings → OmpDeck → **Google Drive backup**. **Sign in with Google** opens Google's consent page
in your browser, and OmpDeck receives the answer on a local `127.0.0.1` port. OmpDeck only asks
for access to its own hidden app folder on your Drive (`drive.appdata`) and your email address. It
cannot see your other Drive files. See the [privacy policy](PRIVACY.md).

**What is backed up** (from the omp agent directory, `$PI_CODING_AGENT_DIR` or `~/.omp/agent`):
`config.yml`/`config.yaml`, `models.yml`/`models.yaml`, `keybindings.yml`, `mcp.json`, `lsp.json`,
`AGENTS.md`, `SYSTEM.md`, `SYSTEM_TEMPLATE.md`, `APPEND_SYSTEM.md`, `TITLE_SYSTEM.md`,
`PERSONALITY.md`, `RULES.md`, the directories `agents/`, `commands/`, `extensions/`, `hooks/`,
`instructions/`, `managed-skills/`, `prompts/`, `rules/`, `skills/`, `themes/`, `tools/`
(`node_modules/` and `.git/` inside them are skipped), and the prompt history `history.db`, as a
consistent SQLite snapshot.

**What is not backed up:** logins and API credentials (`agent.db`), `secrets.yml`, sessions, logs,
caches, the model catalog (`models.db`) and other runtime state.

**Backups**: each backup is one `omp-backup-<UTC time>-<host>.tar.gz` with a manifest. **Back up
automatically** sets how often OmpDeck checks (Off, every 10 minutes (default), 30 minutes, hour, 6 hours,
day); it uploads only when the backed-up content changed. **Back up now** uploads at once. Drive keeps the
newest 30 backups.

**Restore…** lists the backups, newest first, with date, machine and size. After you pick one,
OmpDeck downloads and verifies it, then shows exactly which files will be replaced, added or
removed before anything changes. Before it overwrites anything, OmpDeck copies the current versions
to `~/.local/share/omp-deck/restore-safety/<time>/` (the last 5 copies are kept). Files are written
atomically. The synced directories are made identical to the backup, so local files that the
backup lacks are removed after being copied to the safety folder. The prompt history is restored
into the live database, so running omp processes keep working. Restart running omp sessions to pick
up the restored config.

**Sign out** revokes OmpDeck's access at Google and deletes the local sign-in. Builds made without
Google OAuth credentials show the card as "not available in this build". Maintainers: see
[docs/google-drive-setup.md](docs/google-drive-setup.md).

## Project sync

Settings → **Sync** is a separate workflow from the config backup above and uses the same Google
sign-in. Each row of the **Projects** table is a folder (**Add folder…**, **Change…**) with two
switches, each showing how much it would upload. A newly added folder starts with both switches off and
nothing is uploaded until you pick what to keep and click **Back up now**:

- **.omp folder**: the project's `<folder>/.omp` (project `AGENTS.md`, rules, skills, commands, agents,
  `config.yml`, `mcp.json`, plans, …). `node_modules/`, `.git/`, symlinks and special files are skipped
  (and listed). A project `.omp` can hold secrets; it is stored only in your private Drive app folder.
  Not offered for your home folder, whose `.omp` is omp's global directory.
- **Sessions**: every omp session whose project is this folder, with its attachments, subagent
  transcripts and referenced images. Sessions that are being written are saved up to their last complete
  line. **Choose…** opens the session list: untick the sessions to leave out. Sessions created later are
  backed up automatically.

**Back up now** uploads one row at once. **Back up listed projects automatically** (Off, every 10 or 30
minutes, every hour, every 6 hours (default), every day) uploads only rows whose files or session choices
changed since their last backup. Each backup is one
`omp-project-<UTC time>-<host>-<folder>.tar.gz`; Drive keeps the newest 3 per project and machine.
Backups count toward your Drive storage. Right-click a project (or a session, under **Google Drive**) in the
sidebar → **Back up to Google Drive…** for a one-off backup, optionally adding the folder to the table.

**Restore…** (on the Sync page, or **Restore from Google Drive…** in the sidebar menu, which restores
into that project's folder) lists project backups by project and machine, newest first, with older
versions selectable. Pick the folder to restore into (it is created if missing) and **.omp folder** and/or
**Sessions**. OmpDeck downloads and verifies the backup, then shows what will change before writing:

- **.omp**: files from the backup are written into `<folder>/.omp`. If the folder already has a `.omp`,
  you are warned which files will be overwritten; on Restore they are replaced (the old versions are
  copied to `~/.local/share/omp-deck/restore-safety/project-<time>/`, last 5 kept). Files the backup lacks
  are kept. OmpDeck never writes through symlinks; such files are skipped and listed.
- **Sessions**: each session is added through `omp session unpack --cwd <folder>`, so omp moves it to the
  new folder and tells the agent where it now lives. Existing sessions are never replaced or removed; a
  session that already exists on this machine is added as a copy with a new id. Restoring sessions needs
  omp installed.

**Delete backups**: **Manage cloud backups…** lists every project backup by project, machine and version;
tick the ones to remove and **Delete permanently…**. The restore dialog also has a delete button per
version. Deleted backups are removed from Drive at once (they do not go to the Drive trash) and cannot be
recovered.

## Worktree sessions

Right-click a project (or use the button next to its **+**) → **New session in worktree…**: name a branch
(prefilled `wt/<date>-<time>`), pick the branch or commit to start from, and a folder (default: omp's
worktree folder, `~/.omp/wt/<branch>-<repo>`; `OMP_WORKTREE_DIR` or omp's `worktree.base` setting move it).
OmpDeck creates it through `omp worktree add` and opens a tab running omp there. New worktrees are clean
checkouts: gitignored files such as `.env`, `target/` or `node_modules/` are not copied, so the first build
in a worktree starts from scratch.

**Move to new worktree…** on a running session types omp's `/wt <branch>` once omp is idle: the session,
with its uncommitted changes, moves into a new worktree and the tab follows it.

Worktree sessions are listed under their repository with the branch name (also those created by omp's own
`/wt` and PR checkouts), and the status bar's git chip shows which repository a worktree belongs to.

**Finish worktree** (on a worktree in the sidebar) asks that worktree's omp session to commit its work and
merge the branch into the branch it started from. The agent follows the project's own rules (`AGENTS.md`
and the like): if they don't allow committing or merging on its own, it asks you in the chat first. Once
the branch is merged and nothing uncommitted is left, OmpDeck stops only that worktree's tabs, removes the
worktree and deletes the merged branch. **Remove worktree…** does the same on demand. Both refuse when
work would be lost (uncommitted changes, commits not in the base branch); OmpDeck never force-removes a
worktree or force-deletes a branch. Sessions of removed worktrees stay in the sidebar under their
repository, marked "(removed)", and resume in the repository's folder.

## Uninstall

- Installed with `install.sh`: `curl -fsSL https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.sh | sh -s -- --uninstall`
  (or `./install.sh --uninstall` from the extracted tarball).
- `.deb`: `sudo apt remove omp-deck`; `.rpm`: `sudo dnf remove omp-deck`; AppImage: delete the file.
- Windows: `& ([scriptblock]::Create((irm https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.ps1))) -Uninstall`
  (removes the program and its Start menu entry; omp stays).
- Settings and data stay in the directories above; delete them for a full clean-up. Running omp sessions keep
  running: stop them from the sidebar, with `tmux -L omp-deck kill-server` (tmux backend) or
  `omp-deck-host kill <name>` (host backend; `omp-deck-host ls` lists them).

## Development

```sh
cargo build --workspace
cargo test --workspace            # Linux tests use throwaway tmux sockets and host namespaces
cargo clippy --workspace --all-targets -- -D warnings
# Windows compile check from Linux (cargo install cargo-xwin; rustup target add x86_64-pc-windows-msvc):
cargo xwin clippy --workspace --all-targets --target x86_64-pc-windows-msvc -- -D warnings
```

Packaging scripts live in `packaging/`: `build-tarball.sh`, `appimage/build-appimage.sh`, `install.sh`,
`install.ps1`, `windows/fetch-conpty.ps1`. CI runs on Linux and Windows; releases are built by GitHub Actions
when a `v*` tag is pushed (the tag must match the version in `Cargo.toml`).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Bundled third-party assets: the [Geist](https://github.com/vercel/geist-font) font (SIL Open Font License
1.1, `crates/omp-deck/assets/fonts/Geist-OFL.txt`) and [lucide](https://lucide.dev) icons (ISC,
`crates/omp-deck/assets/fonts/lucide-LICENSE.txt`). The Windows build ships Microsoft's ConPTY
(`conpty.dll`, `OpenConsole.exe` from the `Microsoft.Windows.Console.ConPTY` package, MIT).
