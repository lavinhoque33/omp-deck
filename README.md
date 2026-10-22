# OmpDeck

A native Linux desktop wrapper for [`omp`](https://github.com/can1357/oh-my-pi) (oh-my-pi), the coding-agent TUI.
A sidebar lists every persisted omp session; clicking one opens a tab running that session in an
embedded, fully interactive terminal backed by tmux, so closing a tab or the whole app leaves omp running.

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
  Tabs re-attach to live tmux sessions on the next launch.
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
- **Agent hub and session tree**: browse subagent transcripts and the session tree, fork or navigate from a node.
- **omp updates and settings**: update check badge, update actions, and a searchable editor for every `omp config` key.
- **Google Drive backup**: sign in with Google and back up your omp config (settings, models,
  custom agents, skills, …) and prompt history to a private Drive app folder, automatically when it
  changes or on demand, and restore it on any machine (Settings → OmpDeck). See
  [below](#google-drive-backup).
- **Per-folder mode**: scope the sidebar to a single project folder.
- **Appearance**: a flat, neutral default look (Geist font, lucide icons, compact spacing) or the
  "Midnight neon" style, each in dark and light, plus a Comfortable density option (Settings → OmpDeck).
  Integrated title bar (or native decorations), persisted window geometry and open tabs.

OmpDeck never writes to the omp session store directly; changes go through omp's own CLI.

## Requirements

- Linux x86_64, Wayland or X11, with fontconfig.
- [`omp`](https://github.com/can1357/oh-my-pi) on `PATH` or in `~/.local/bin` (required). If it is missing,
  `install.sh` installs it, and OmpDeck shows an "Install omp" button (on the start screen, in Settings and in
  Usage) that runs omp's official installer (`https://omp.sh/install`, prebuilt binary into `~/.local/bin`,
  no root) in a tab and picks omp up when it finishes, without a restart.
- `tmux` (optional but recommended): without it, omp runs directly in the terminal tab, a banner is shown,
  and sessions do not survive closing the tab. OmpDeck uses its own tmux server (`tmux -L omp-deck`) and
  never touches your default one.

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

Release assets come with `.sha256` files and a combined `SHA256SUMS`. Prebuilt binaries are built on
Ubuntu 22.04 (glibc 2.35 or newer).

**From source** (Rust stable, 2024 edition; on Debian/Ubuntu you need
`pkg-config libfontconfig1-dev libxkbcommon-dev libwayland-dev`):

```sh
cargo install --git https://github.com/lavinhoque33/omp-deck omp-deck --locked
```

This installs only the binary. For the menu entry and icon, use one of the packages above.

## Usage

```
omp-deck [OPTIONS] [FOLDER]

  [FOLDER]                Per-folder mode: show only the sessions of FOLDER (and below)
  --folder <dir>          Same as the FOLDER argument
  --sessions-root <dir>   Read omp sessions from <dir> instead of the default store
  -h, --help              Print this help
```

The default session store is `$PI_CODING_AGENT_DIR/sessions` if set, otherwise `~/.omp/agent/sessions`
(`$PI_CONFIG_DIR` replaces `.omp`).

### Shortcuts

| Shortcut | Action |
| --- | --- |
| Ctrl+Shift+T | Reopen last closed tab |
| Ctrl+PgUp / Ctrl+PgDn | Previous / next tab |
| Ctrl+Shift+A | Agent hub |
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
| `$XDG_CONFIG_HOME/omp-deck/` (default `~/.config/omp-deck/`) | `state.json` (window, tabs, settings), generated `tmux.conf`, `google-drive.json` (Google sign-in, mode 0600) |
| `$XDG_DATA_HOME/omp-deck/` (default `~/.local/share/omp-deck/`) | live-control extension loaded into omp tabs, `restore-safety/` (copies kept before a Drive restore) |

Environment: `OMP_DECK_TMUX_SOCKET` selects a different tmux socket (name must match `omp-deck-test-[A-Za-z0-9_-]+`);
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

**Backups**: each backup is one `omp-backup-<UTC time>-<host>.tar.gz` with a manifest. With
**Back up automatically** on (the default), OmpDeck checks every 10 minutes and uploads only when
the backed-up content changed. **Back up now** uploads at once. Drive keeps the newest 30 backups.

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

## Uninstall

- Installed with `install.sh`: `curl -fsSL https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.sh | sh -s -- --uninstall`
  (or `./install.sh --uninstall` from the extracted tarball).
- `.deb`: `sudo apt remove omp-deck`; `.rpm`: `sudo dnf remove omp-deck`; AppImage: delete the file.
- Settings and data stay in the directories above; delete them for a full clean-up. Running omp sessions
  live in the `omp-deck` tmux server: `tmux -L omp-deck kill-server` stops them.

## Development

```sh
cargo build --workspace
cargo test --workspace            # needs tmux; tests use throwaway tmux sockets
cargo clippy --workspace --all-targets -- -D warnings
```

Packaging scripts live in `packaging/`: `build-tarball.sh`, `appimage/build-appimage.sh`, `install.sh`.
Releases are built by GitHub Actions when a `v*` tag is pushed (the tag must match the version in `Cargo.toml`).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

Bundled third-party assets: the [Geist](https://github.com/vercel/geist-font) font (SIL Open Font License
1.1, `crates/omp-deck/assets/fonts/Geist-OFL.txt`) and [lucide](https://lucide.dev) icons (ISC,
`crates/omp-deck/assets/fonts/lucide-LICENSE.txt`).
