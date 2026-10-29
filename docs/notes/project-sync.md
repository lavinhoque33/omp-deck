# Project sync facts (Google Drive, per-project `.omp` + sessions)

Facts behind wave 10 (`deck_core::backup::project`, `crates/omp-deck/src/app/project_sync.rs`, `sync_view.rs`).
Verified 2026-10-22 against omp 18.12.6 (bundle extracted with `strings -n 6 ~/.local/bin/omp`, citations
`bundle:<line>`) in a sandbox (`PI_CODING_AGENT_DIR`/`HOME` temp dirs), Google Drive v3 docs, and ureq 3.5.0
sources. The existing config backup (`docs/notes/google-drive-backup.md`) is a separate workflow and is unchanged.

## omp session archives (`omp session pack/unpack`, `session/session-transfer.ts`, bundle:845182+)

- Pack tar members, nothing else allowed (`Bad session archive … unexpected member`): `manifest.json`,
  `session.jsonl`, `artifacts/<relpath>` (everything under `<jsonl without .jsonl>/`, incl. subagent jsonls),
  `blobs/<64 lowercase hex>`.
- `manifest.json` = `{"version":1,"id":"<id>","cwd":"<abs>","omp":"<ver>"}`; `version` must be the number 1,
  the other three strings. Manifest `id` must equal the session header `id` (first entry with
  `type:"session"`); id must match `^[A-Za-z0-9_-]{1,128}$`. Artifact paths: no empty/`.`/`..` segment, no `\`.
  Each blob's sha256 must equal its name. Per-member limit 2 GiB.
- Blob refs: regex `/\bblob:sha256:([a-f0-9]{64})\b/g` over `session.jsonl` and every artifact `*.jsonl`.
  Store: flat `<agent>/blobs/<hex>` (no extension, no sharding; optional `<hex>.<ext>` hardlinks beside it).
  Missing blobs: pack warns and continues.
- The session id appears only in the header line (plus the filename), so a fresh id = rewrite header `id` +
  manifest `id`. Header `timestamp` drives the new filename (`<ts with [^\w-]→->_<id>.jsonl`).
- **A hand-built tar is accepted** (Python `tarfile` proof: manifest + jsonl cut at the last complete line +
  artifact + synthetic blob → rc 0). A jsonl cut mid-line fails to parse; cut at the last `\n`.
- `omp session unpack <tar> --cwd <dir>`: `<dir>` must exist (`error: Directory not found`), rc 1 on error with
  stderr `error: <msg>`. stdout = new `.jsonl` path; stderr one of `Unpacked session (<id>).` (created),
  `Replaced the local copy of this session (<id>).`, `The local copy of this session diverged; unpacked as a fork (<new id>).`.
- Conflict rule: local copies = every `*_<id>.jsonl` under the sessions root whose header id is `<id>`. None →
  created. All local copies are entry-prefixes of the archive → **replaced: the local copies (jsonl + artifacts
  dir) are deleted, even in another project dir** (verified). Any diverged → forked (new id). Only the replace
  path takes the live-writer lease. No CLI/function option forces a copy (`unpackSession(archive,{cwd,sessionDir?})`).
- Every unpack rewrites header `cwd` to the target, adds `previousSessionFiles` (staging path, ignore), and
  appends a `custom_message` `customType:"session-transfer"`, `details.kind:"arrived"` telling the model the
  session moved. Blobs land in the store, artifacts beside the new file.
- Side effects: `<agent>/custom-session-files/<hash>` (harmless); `<agent>/terminal-sessions/<terminal-id>`
  breadcrumb unless stdin is null and these are removed from the env: `TMUX`, `TMUX_PANE`, `ZELLIJ_PANE_ID`,
  `CMUX_SURFACE_ID`, `KITTY_WINDOW_ID`, `WEZTERM_PANE`, `TERM_SESSION_ID`, `WT_SESSION` (bundle:565593+).
- ⇒ **Restore "alongside"** (the Keep both choice): keep the original id when no local `*_<id>.jsonl` exists (mode
  `created`); otherwise rewrite to a fresh id first, so omp never takes the replace path. **Overwrite** deletes the
  local copies through omp (headless delete after an `omp session pack` safety tar) and then unpacks with the
  original id (`created`); relying on omp's replace path would fork whenever the local copy diverged.
- Session age: omp's picker and OmpDeck's sidebar both use the `.jsonl` mtime (omp `session-listing.ts`
  `modified: mtime`, newest-mtime-first listing), and unpack writes a fresh file (mtime = now) plus a `session-transfer`
  entry; the backed-up `modified_unix` (jsonl mtime, s) is set back on the new file after unpack.

## Project `.omp` (bundle:620242-621022, 382469)

- omp reads `<project>/.omp/`: `AGENTS.md`, `RULES.md`, `SYSTEM.md`, `SYSTEM_TEMPLATE.md`, `rules/`, `skills/`,
  `commands/`, `prompts/`, `instructions/`, `agents/`, `hooks/pre|post/`, `tools/`, `extensions/`,
  `config.yml`, `settings.json`, `mcp.json`, `.mcp.json`, `lsp.json`, `secrets.yml`, `ratchet/`.
  omp writes `.omp/plans/` (plan autosave), `.omp/ratchet/`, `omp agents unpack --project` → `.omp/agents`.
- Discovery walks ancestors up to the repo root or `$HOME` and **skips `$HOME/.omp`** (that is omp's global
  dir, holding the agent dir and its sessions). A project whose folder is `$HOME` has no project `.omp`.
- Sensitive: `config.yml` credential settings, `mcp.json` headers/env tokens, `secrets.yml`.
- Real dirs here are 24–252 KiB, no `node_modules`/`.git`/sockets. Skip `node_modules/`, `.git/`, non-regular
  files and symlinks.

## Per-project state in the agent dir

- Only `sessions/<encoded-cwd>/` is cwd-keyed (`<stem>.jsonl`, `<stem>/` artifacts, stale `*.jsonl.<n>.bak`).
  `blobs/` is shared/content-addressed: back up only referenced blobs. `history.db`, `session-pins.json`,
  `terminal-sessions/`, `custom-session-files/`, `agent.db`, `models.db`, `stats.db`: global, not per project.

## Google Drive v3 large files

- Multipart is documented for ≤ 5 MB; resumable for larger (works for small too). Initiate:
  `POST {upload}/drive/v3/files?uploadType=resumable` with `Content-Type: application/json; charset=UTF-8`,
  optional `X-Upload-Content-Type`/`X-Upload-Content-Length`, JSON metadata (`name`, `parents:["appDataFolder"]`,
  `appProperties`) → 200 + `Location:` session URI (valid 1 week).
- Chunks: `PUT <session-uri>` with `Content-Length` + `Content-Range: bytes a-b/total`, multiples of 256 KiB
  except the last. `308 Resume Incomplete` + `Range: bytes=0-N` → continue at N+1 (no `Range` → 0). Final
  200/201 with file metadata. Interrupted (network/5xx): `PUT` empty with `Content-Range: bytes */total` to
  query. 404/410 → session expired, start over; other 4xx → new session.
- appProperties: ≤ 124 bytes key+value UTF-8 per property, ≤ 30 private per app. Query
  `appProperties has { key='kind' and value='project' } and trashed = false` (escape `'` as `\'`).
- Download `GET /drive/v3/files/{id}?alt=media`, stream the body; `Range: bytes=a-b` supported.
- appDataFolder files count against the user's Drive storage [community sources]; no app-folder size limit.
- ureq 3.5.0 traps: any 3xx except 304 is treated as a redirect, so a 308 without `Location` errors
  (`NoLocationHeader`) unless the request/agent sets `max_redirects(0)`. `timeout_global` also covers body
  reads (current agent: 600 s). `read_to_vec` defaults to a 10 MB cap; stream with `body_mut().as_reader()`.
  Fixed-length chunk body: `.header("Content-Length", n)` + `SendBody::from_reader(&mut file.take(n))` or a slice.
