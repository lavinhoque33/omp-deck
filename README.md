# OmpDeck

A native Linux desktop wrapper for [`omp`](https://github.com/can1357/oh-my-pi) (oh-my-pi), the coding-agent TUI.
A sidebar lists every persisted omp session; clicking one opens a tab running that session in an
embedded, fully interactive terminal backed by tmux, so closing a tab or the whole app leaves omp running.

## Features

- **Session sidebar**: every omp session grouped by project (only the active tab's project is expanded;
  the rest stay collapsed until you open them), newest first, with titles, relative age,
  a running indicator, a filter box, background auto-refresh, and context menus (open, copy/move to another
  project, copy path/id/resume command, "Restart session" for running sessions, "Stop session process",
  and "Delete session…"). Deleting is permanent: it removes the session file and its subagent transcripts
  and artifacts (stopping a running session first) and cannot be undone.
- **Tabs**: open, focus, middle-click close, reopen the last closed tab. Closing a tab only detaches;
  the tab context menu's "Kill session process" stops omp, as does "Stop session process" in the sidebar.
  Tabs re-attach to live tmux sessions on the next launch.
- **Terminal fidelity**: truecolor, wide/emoji cells, mouse reporting, bracketed paste, OSC 52 clipboard,
  clickable hyperlinks, kitty CSI-u / modifyOtherKeys key encoding.
- **Session status bar**: model, thinking effort, path, git state, context meter, tokens and cost, plus a
  model picker and actions (handoff, compact, fork, rename, plan, retry, stop, export). Hovering the
  context meter shows omp's `/context` breakdown (system prompt, tools, skills, messages, free space,
  auto-compact buffer, per-tool schema sizes) for tabs running with live control.
- **Usage**: omp's `/usage` as a native view (Ctrl+Shift+U or the status-bar usage chip): remaining quota
  per provider and window with reset times, a year of activity as a heatmap, and top models by cost.
- **Agent hub and session tree**: browse subagent transcripts and the session tree, fork or navigate from a node.
- **omp updates and settings**: update check badge, update actions, and a searchable editor for every `omp config` key.
- **Per-folder mode**: scope the sidebar to a single project folder.
- **Appearance**: a flat, neutral default look (Geist font, lucide icons, compact spacing) or the
  "Midnight neon" style, each in dark and light, plus a Comfortable density option (Settings → OmpDeck).
  Integrated title bar (or native decorations), persisted window geometry and open tabs.

OmpDeck never writes to the omp session store directly; changes go through omp's own CLI.

## Requirements

- Linux x86_64, Wayland or X11, with fontconfig.
- [`omp`](https://github.com/can1357/oh-my-pi) on `PATH` (required).
- `tmux` (optional but recommended): without it, omp runs directly in the terminal tab, a banner is shown,
  and sessions do not survive closing the tab. OmpDeck uses its own tmux server (`tmux -L omp-deck`) and
  never touches your default one.

## Install

**One-liner** (installs the latest release into `~/.local`, no root):

```sh
curl -fsSL https://raw.githubusercontent.com/lavinhoque33/omp-deck/main/packaging/install.sh | sh
```

Options (pass after `sh -s --`): `--version <x.y.z>`, `--prefix <dir>`, `--uninstall`.
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
| Ctrl+click | Open a hyperlink |

Ctrl+C, Ctrl+X and Ctrl+V go straight to omp (omp uses Ctrl+V for image paste).

## Configuration and data

| Path | Contents |
| --- | --- |
| `$XDG_CONFIG_HOME/omp-deck/` (default `~/.config/omp-deck/`) | `state.json` (window, tabs, settings), generated `tmux.conf` |
| `$XDG_DATA_HOME/omp-deck/` (default `~/.local/share/omp-deck/`) | live-control extension loaded into omp tabs |

Environment: `OMP_DECK_TMUX_SOCKET` selects a different tmux socket (name must match `omp-deck-test-[A-Za-z0-9_-]+`);
`RUST_LOG` enables logging.

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
