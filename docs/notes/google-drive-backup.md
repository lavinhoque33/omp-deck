These are the facts behind the Google Drive backup (`deck_core::gdrive`, `crates/omp-deck/src/app/drive.rs`,
`drive_view.rs`). The synced file set comes from oh-my-pi's own docs (`docs/*.md` on `main`, read
2026-10-22). The Google behaviour comes from Google's OAuth and Drive v3 documentation. Maintainer
setup is covered in [`../google-drive-setup.md`](../google-drive-setup.md).

## What omp reads from its agent directory

The agent dir is `SessionsRoot::agent_dir()`: `$PI_CODING_AGENT_DIR`, default `~/.omp/agent`. Only
names omp documents as user config are synced (`SYNCED_FILES` / `SYNCED_DIRS` in `gdrive/archive.rs`):

| Name | Source doc |
|---|---|
| `config.yml`, `config.yaml` (first present wins) | `config-usage.md`, `settings.md` |
| `models.yml`, `models.yaml` | `models.md` |
| `keybindings.yml` (`keybindings.yaml`, legacy `keybindings.json` migrated) | `keybindings.md` |
| `mcp.json`, `.mcp.json` | `mcp-config.md` |
| `lsp.json` | `lsp-config.md` (follows `PI_CONFIG_DIR`, same default path) |
| `AGENTS.md`, `SYSTEM.md`, `SYSTEM_TEMPLATE.md`, `APPEND_SYSTEM.md`, `TITLE_SYSTEM.md`, `PERSONALITY.md`, `RULES.md` | `config-usage.md` §6, `system-prompt-customization.md` |
| `agents/*.md` | `task-agent-discovery.md` |
| `commands/*.md`, `rules/*.{md,mdc}`, `prompts/*.md`, `instructions/*.md`, `hooks/{pre,post}/*`, `tools/…`, `extensions/…` | `config-usage.md` §6 (native provider), `hooks.md`, `custom-tools.md`, `extension-loading.md` |
| `skills/*/SKILL.md`, `managed-skills/*/SKILL.md` | `skills.md` |
| `themes/*.json` | `theme.md` |
| `history.db` (prompt history; tables `history`, `history_fts*` FTS5, `session_titles`, `session_recaps`; WAL) | observed on this machine |

These are deliberately excluded: `agent.db` (logins), `secrets.yml` and `secret-placeholder.key`
(secrets and the per-install HMAC key), `settings.json` (legacy, migrated into `config.yml`),
`sessions/`, `blobs/`, `cache/`, `models.db`, `stats.db`, `terminal-sessions/`, `run/`,
`natives/`, `skill-descriptions.db`, and `*.broken-*` (omp's copies of unparsable config). Inside
synced dirs, `node_modules/` and `.git/` are skipped.

## Google

- **OAuth**: installed-app flow, `https://accounts.google.com/o/oauth2/v2/auth` with PKCE S256,
  random `state`, `access_type=offline` and `prompt=consent`. The consent prompt makes Google issue
  a refresh token again on a re-sign-in. The redirect goes to `http://127.0.0.1:<ephemeral port>`
  (desktop clients accept any loopback port, and no URI is registered). The loopback answers
  non-`/` paths with 404 and keeps waiting. A redirect with a wrong `state` or an `error` ends the
  sign-in.
- **Token endpoint**: `https://oauth2.googleapis.com/token` for the code exchange and refresh.
  `invalid_grant` on refresh means the token was revoked, or expired after 7 days while the consent
  screen is in *Testing*. OmpDeck then deletes `google-drive.json`, and the card shows signed out.
  Revocation goes to `https://oauth2.googleapis.com/revoke`; a 400 `invalid_token` counts as already
  revoked.
- **Email**: read from the `email` claim of the `id_token` payload (scopes `openid email`). The
  token comes straight from the token endpoint over TLS, so the signature is not checked.
- **Drive v3, `spaces=appDataFolder`**: list `GET /drive/v3/files`
  (`q=name contains 'omp-backup-' and trashed = false`, paginated); upload
  `POST /upload/drive/v3/files?uploadType=multipart` (`multipart/related`, metadata
  `parents: ["appDataFolder"]` and `appProperties: {host, fingerprint}`); download
  `GET /drive/v3/files/{id}?alt=media`; delete `DELETE /drive/v3/files/{id}`. `size` arrives as a
  string. A 401 triggers one token refresh and a retry. 429/5xx map to "busy or unavailable"; other
  errors surface Google's `error.message`.

## Archive

The archive is `omp-backup-<YYYYMMDDTHHMMSSZ>-<host>.tar.gz`: `manifest.json` (format 1, created,
host, app version, fingerprint, `dirs`, `files[]` with path, size, sha256 and mode), then
`agent/<path>` regular files.

- `history.db` is copied with the SQLite online backup API in a single `step(-1)` from a read-only
  connection, retrying while it is busy, and the copy is switched to `journal_mode=DELETE`. The
  raw db/-wal/-shm files are never read.
- The **fingerprint** hashes paths, exec bits and contents, but `history.db` by its rows. Its page
  bytes differ between snapshots of the same data. Auto backup uploads when the fingerprint
  differs from `synced_fingerprint` in `google-drive.json`. A restore sets that field to the
  backup's fingerprint, so restored content is not uploaded again.
- **Validation on restore**: the archive must hold only regular files or directories. Paths must be
  relative, free of `.`/`..`, UTF-8 and inside the synced set. The file set must match the
  manifest, each sha256 and size must match, and the format must be 1. Unpacked size is capped at
  1 GiB.
- **Apply order**: first the safety copy (every top-level file the backup overwrites, plus every
  current file in each synced dir the backup contains; `history.db` via the backup API) into
  `$XDG_DATA_HOME/omp-deck/restore-safety/<stamp>/` (0700, newest 5 kept). Then atomic writes
  (temp file in the same dir, then rename; a symlinked target is written through). Then removal
  of files the synced dirs should not have, along with directories left empty. Last, `history.db`
  is written into the live db with the backup API (15 s busy timeout). The live db stays in WAL
  mode, and open connections see the restored rows.

## Auto backup

The first check runs 45 s after startup, sign-in, or turning the toggle on; later checks run every
10 minutes. A check is skipped while another Drive job runs or a restore is being picked or
confirmed (one job at a time). An automatic failure shows one toast, and later automatic failures
only log until a backup succeeds. Retention runs after every successful upload: the newest 30
backups by `createdTime` are kept, ties broken by name.
