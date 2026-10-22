# omp CLI facts for OmpDeck (settings / updates / session pack-unpack / misc)

Verified against the installed binary `omp/18.12.3` (`~/.local/bin/omp`, a 256 MB bun-compiled ELF, regular file).
**IMPORTANT – source drift:** `~/PersProject/oh-my-pi` is **v18.4.10** (`packages/coding-agent/package.json`), 8 minors older than the binary.
It lacks `omp session …` and uses a different `omp update` backend. The compiled JS is embedded in the binary as plain text:
`strings -n 6 ~/.local/bin/omp > /tmp/x.txt` then grep `^// packages/coding-agent/src/<file>.ts` (e.g. `cli/update-cli.ts`, `cli/build-service.ts`,
`session/session-transfer.ts`, `commands/session.ts`). Citations below are `repo path:line` (18.4.10 source) or `binary:<symbol/file>` (18.12.3 bundle) or exact commands+output.
All experiments used `PI_CODING_AGENT_DIR=/tmp/omp-sb/agent` (verified: it redirects config.yml, agent.db, models.db, sessions/, blobs/, custom-session-files/ — see "Side effects" for the one exception). Sandbox deleted at the end.

---------------------------------------------------------------------------------------------------
## A. Settings (`omp config`)

### A1. Where the schema lives / what is obtainable

* Registry: `packages/coding-agent/src/config/registry.ts` (`register({id,type,default,values?,env?,credential?,validate?,normalize?,ui?})` ; `Setting.enumValues` getter in class `Setting`, `.default`, `.ui`, `.isCredential`).
  Each domain registers next to its feature (`src/*/settings.ts`); `config/all-settings.ts:1-125` fixes panel order (`orderedSettings()`).
* UI metadata type `UiBase = {tab, group?, label, description, warning?, condition?}` + `options?` (submenu choices `{value,label,description}`), `secret?`, `ordered?` — `registry.ts:36-62` & `packages/tui/src/overlays/settings-defs.ts` (`UiBase`).
* **No min/max/step in the schema.** `UiNumber` only has optional `options` (discrete choices); ranges appear only as prose (e.g. `autocompleteMaxVisible` description "…(3-20)"). Real validation is a per-setting `validate`/`normalize` function. Sandbox proof: `omp config set autocompleteMaxVisible 12.5` → rc 0 accepted, `… 0` accepted.
* **omp's own `/settings` UI groups into 10 tabs** (`settings-defs.ts` `SETTING_TABS`, identical in 18.12.3 bundle `packages/tui/src/overlays/settings-defs.ts`), each with ordered section groups (`TAB_GROUPS`), label (`TAB_METADATA`) and one-line lead (`TAB_LEADS`):
  ```
  appearance (Appearance): Theme, Composer, Status Line, Display, Images
  model (Model):           Thinking, Sampling, Prompt, Retry & Fallback, Advisor, Prewalk, Vision
  interaction (Interaction): Input, Approvals, Notifications, Speech, Collab, Stream, Magic Keywords, Startup & Updates, Power, Agent, Git, Skills
  context (Context):       General, Compaction, Rules (TTSR), Experimental
  memory (Memory):         General, Auto-Learn, Mnemopi, Hindsight, Sharpshooter
  files (Files):           Editing, Reading, Read Summaries, LSP
  shell (Shell):           Bash, Eval & Runtimes
  tools (Tools):           Available Tools, Todos, Grep & Browser, Computer, IDA Pro, GitHub, Output Limits, Execution, Discovery & MCP, Extensions, Developer
  tasks (Tasks):           Modes, Subagents, Isolation, Commands & Skills
  providers (Providers):   Services, Fireworks, Tiny Model, Protocol, Timeouts, Privacy
  ```
  Keys without `ui` have tab `internal` (hidden config-file-only keys). Counts on 18.12.3 (`config list --json`): 559 keys = 221 boolean, 139 number, 98 enum, 55 string, 28 array, 18 record. Per tab (from human list): tools 73, model 62, interaction 55, appearance 44, providers 39, tasks 39, context 31, memory 30, files 28, shell 18, **internal 140** (= exactly the 140 keys whose JSON `description` is `""`).
  Some `ui.condition`s hide rows in omp's panel (`macOS`, `hasImageProtocol`, `advisorEnabled`, `vimModeEnabled`, `planModeEnabled`, … `config/settings-ui.ts:19-35`) – CLI does not expose them.
* **No CLI/JSON flag exposes enum values, defaults, labels, groups, number options.** `config list --json` entries are only `{value?, redacted?, type, description}` (`cli/config-cli.ts` handleList; 18.12.3 same – checked key set `{redacted,value,type,description}`). BUT two verified side channels together give almost everything:

  1. **Human `omp config list` (non-JSON) is grouped by tab and prints enum choices inline.** Output is plain (no ANSI when piped; verified `cmp` identical with NO_COLOR). Format:
     ```
     Settings:
     <blank>
     [appearance]
       theme.dark = titanium (string)
       symbolPreset = unicode (unicode|nerd|ascii)
       statusLine.preset = default (default|minimal|compact|full|nerd|ascii|custom)
     ...
     [internal]
     ```
     Regexes: tab header `^\[(\w+)\]$`; row `^  (\S+) = (.*) \((.*)\)$`; trailing group is `string|number|boolean|array|record` or `a|b|c` (enum). Keys appear in omp's registration order inside a tab (= panel order). Verified: parsed 559 rows, key set == JSON key set; 98 enum rows == 98 JSON enums; credentials printed `********`; unset shows `(not set)`.
  2. **Invalid-value probe lists enum members** (`registry.ts:#parseText`): `omp config set defaultThinkingLevel bogus` → rc 1, stderr `Error: Invalid value: bogus. Valid values: minimal, low, medium, high, xhigh, max, auto`. Verified for all 98 enum keys: `Valid values:` set == human-list set (0 mismatches). Never writes (parse throws before set). Closed-vocabulary arrays also reveal members this way: `omp config set statusLine.leftSegments '["__probe__"]'` → rc 1 `Error: Unknown status line segment: "__probe__". Valid status line segments: pi, status, model, mode, path, git, pr, subagents, token_in, token_out, token_total` (only 2 of 28 arrays have a closed vocab: statusLine.left/rightSegments; the others — **careful** — accept any JSON array and WRITE it, e.g. `enabledModels '["__probe__"]'` returned rc 0 and wrote it. Don't probe arrays other than those two.)
  3. **Defaults:** run `PI_CODING_AGENT_DIR=<empty tmpdir> omp config list --json` with cwd = an empty dir (no `.omp/config.yml`) → values are the defaults (omp creates only `agent.db` in that dir). Env vars with `env:` bindings (e.g. `PI_*`) can still override; strip them (`env -i HOME=$HOME PATH=$PATH`). Alternative for one key: `omp config reset KEY --json` prints the effective value after removal (but it mutates the real config → don't use against the real dir). Unset optional strings have no `value` field (default `undefined`).
  4. **Labels, groups, number options, `warning`, `secret` flags are NOT available at runtime.** Options: (a) derive label from key (`statusLine.compactThinkingLevel` → "Compact Thinking Level"), section from tab + key prefix (`compaction.*`); description from JSON; (b) **recommended for polish:** a dev-time generator (`xtask`/script, checked-in generated file stamped with omp version) that regex-extracts `register({ id, type, default, ui:{tab,group,label,description,options,condition}})` blocks from `strings -n 6 $(which omp)` — prototype got tab/group/label for 415/419 UI keys (4 missed: dynamically registered, e.g. `magicKeywords.ultrathink`, `retry.enabled`, `thinkingBudgets.*`, `modelRoles`…; fallback to (a)); tab values matched the human list 100 %. At runtime treat the generated table as optional enrichment; the CLI (JSON + human list) is the authoritative key/type/enum/default source so omp upgrades never break the editor (unknown keys → generic widget by `type`, grouped by tab).
  5. Credential keys in 18.12.3 (`credential: true` / `secret: true`): `auth.broker.token, dev.autoqaPush.token, mnemopi.embeddingApiKey, mnemopi.llmApiKey, hindsight.apiToken, searxng.token, searxng.basicPassword, images.urls.credentials` (list extracted from bundle; CLI only reveals redaction once a value is set).

### A2. Command syntax (all verified, sandbox)

`omp config {list|get|set|reset|path|init-xdg} [KEY] [VALUE...] [--json]`; parser `cli/config-cli.ts parseConfigArgs`: positional args not starting with `-` are collected, `value = positional[1..].join(" ")`, then `Setting.parse(text)` (`registry.ts:#parseText`): trims.
| type | syntax | verified |
|---|---|---|
| boolean | `true/false/yes/no/on/off/1/0` (case-insens.) | `set compaction.enabled ON` → `✔ Set compaction.enabled = true`; `maybe` → rc 1 `Error: Invalid boolean value: maybe. Use true/false, yes/no, on/off, or 1/0` |
| number | any finite JS number text; **negative needs `--`**: `config set k -- -5` (else oclif: `error: Unknown option '-5'…` rc 1) | `12.5` ok; `abc` → rc1 `Error: Invalid number: abc` |
| enum | exact member (or JSON-quoted); bad → `Invalid value: X. Valid values: a, b, c` rc 1 | |
| string | bare text or JSON-quoted `"…"`; several VALUE args are space-joined (`set theme.dark my fancy theme` → `my fancy theme`); surrounding whitespace trimmed; `""` sets empty string (rc 0, YAML `interpreter: ""`) | |
| array | **JSON only**: `set enabledModels '["a/b","c"]'`; `a b` / `a,b` → rc 1 `Error: Invalid array JSON: a b` | |
| record | **JSON object only**: `set modelRoles '{"default":"anthropic/claude-sonnet-4"}'`; `[1]` → `Invalid record JSON: [1]`. Whole-record replace; no per-entry CLI | |
Always pass VALUE as ONE argv element (GUI: `Command::new("omp").args(["config","set",key,&json])`); `--json` anywhere switches output.

* `get KEY [--json]`: plain prints the value (`false`; objects as JSON); `--json` → `{"key","value","type","description"}` (**not redacted even for credentials**). Unknown key: rc 1, stderr `Unknown setting: nosuch\n\nRun 'omp config list' to see available keys`.
* `set KEY VALUE [--json]` stdout: `✔ Set KEY = VALUE` or JSON `{"key":"…","value":<global-saved value>[,"overriddenBy":"project"|"env"|"overlay"|"runtime"|"fallbackEnv":"NAME"]}`. If another layer shadows it also prints `⚠ Project settings override this value here; …` (verified with `<cwd>/.omp/config.yml` containing `ask.timeout: 99`: `get`→99, `set … 5 --json` → `{"key":"ask.timeout","value":5,"overriddenBy":"project"}`). **`set` echoes credential values in clear** (`✔ Set searxng.token = s3cr3t`) — never log/display set's stdout for credential keys.
* `reset KEY [--json]` removes the key from config.yml (so default tracks future omp changes): `{"key":"compaction.enabled","value":true}`; for credentials prints `Reset … to ********` (also for an empty `{}` record) / JSON `{"key","redacted":true}` when a value remains from another layer. Use `reset` (not `set ""`) to clear optional strings/secrets: `set searxng.token ""` stores `""` (list shows `value:""`).
* `list --json`: `{key:{value?,type,description}}`; credential with non-empty value → `{redacted:true,type,description}` (no `value`). Quirk: `images.urls.credentials` (record) always `redacted:true` because `{}` is truthy. Order = panel order.
* `path` → prints agent dir (`/tmp/omp-sb/agent` under the override; `~/.omp/agent` normally). `init-xdg` migrates dirs – never call.
* **File written:** global `<agentDir>/config.yml` only (YAML, nested by dotted path; e.g. `compaction:\n  enabled: false`), rc 0. `reset` leaves a file with the other keys. Never writes project `.omp/config.yml`, though **reads layer in**: precedence env > runtime > `--config` overlay > project (`<cwd>/.omp/config.yml`, `.omp/settings.json`, `.claude/settings.json`) > global > default (`registry.ts` Setting docs; `docs/config-usage.md:150-186`). ⇒ **`config get/list` results depend on the process cwd**: run the GUI's config commands with `current_dir` = a neutral dir (e.g. `$HOME`) so project overrides don't leak into the global editor (verified: same key reads 99 in `proj/`, 6 in another cwd).
* Concurrency: 6 parallel `config set` of different keys all persisted (file lock + merge). Writes are awaited (`settings.flush()`) before exit.
* Exit codes/stderr: unknown key / bad value / missing args → rc 1 (stderr as above; usage: `Usage: omp config set <key> <value>`); success rc 0; unknown action → rc 1 `Unknown config command: X\nValid commands: list, get, set, reset, path, init-xdg`. Each invocation ≈0.15–0.5 s.
* **Don't hand-edit config.yml with bad YAML:** next omp start fails and *moves* it: `Settings config is invalid: …/config.yml (moved to …/config.yml.broken-<ts>-<pid>-<uuid>)` rc 1 (verified). Use `omp config set/reset` only (it merges with concurrent external edits).

### A3. Live vs restart
* Interactive TUI and rpc/rpc-ui hosts call `settingsInstance.startWatching()` (`src/main.ts:2462-2466`; present in 18.12.3 bundle: `if (R==="rpc"||R==="rpc-ui"||te){ me.startWatching(); … }`). It `fs.watch`es the dirs of `config.yml`/`config.yaml`, project `.omp/config.yml`, `settings.json`, `.claude/settings.json`, `--config` overlays; **200 ms debounce** (`settings.ts:150 CONFIG_WATCH_DEBOUNCE_MS`), then keep-last-good reload (`settings.ts:1093-1210`); an invalid file keeps its layer's last good values + logged warning. `-p`/print mode does NOT watch.
* So a `omp config set` from OmpDeck reaches already-running tabs within ~0.2 s *as a store update*. Whether a given setting takes effect immediately depends on the consumer: ~15 settings are process-wide `effect(...)` (theme.dark/light, symbolPreset, colorBlindMode, composer.shape, display.shimmer, tui.hyperlinks, tui.maxInlineImage*, emoji autocomplete, secrets.enabled, providers.maxInFlightRequests, task.showResolvedModelBadge, worktree.base, mcp.renderMarkdownResults) and ~60 have `.listen(...)` (browser.*, lsp*, mcp.*, followUpMode, interruptMode, modelRoles, power.sleepPrevention, prewalk.enabled, extension sources, disabledProviders…) (grep of `effect(cfg` / `cfg*.listen(` in 18.4.10 src); everything else is read via `cfgX.get(settings)` at point of use (next turn/tool call) or captured at session start (tool set, system prompt, extension/MCP/skill discovery, model registry). **Per-key classification is [UNVERIFIED]**; verify by RPC (`omp --mode rpc`) + config edit if needed. UX recommendation: toast "Saved. Running sessions pick this up live where supported; restart a tab to be sure" and offer "restart tab".
* Credential/auth keys (`auth.broker.*`) and startup-only things (`startup.*`, `update.channel`) apply on next launch.

---------------------------------------------------------------------------------------------------
## B. Updates (`omp update`)

**18.12.3 backend ≠ 18.4.10 source.** For a standalone-binary install (ours: `~/.local/bin/omp` regular file → method `binary`, `resolveUpdateMethod`) the bundle's `cli/build-service.ts` asks `https://build.stencil.so` (override env `PI_BUILD_URL`). Proof: through a CONNECT-logging proxy, `HTTPS_PROXY=http://127.0.0.1:18940 omp update --check` → proxy saw `CONNECT build.stencil.so:443`; `npm_config_registry`/`.npmrc` overrides had no effect.

### B1. `omp update --check` output (stdout, exit 0 unless the lookup throws)
Real run today: 
```
$ omp update --check            ; rc=0
Current version: 18.12.3
New version available: 18.12.5
```
Variants (bundle `Cze/Re` + verified where noted):
| situation | stdout | stderr | rc |
|---|---|---|---|
| newer | `Current version: A` / `New version available: B` | | 0 (verified) |
| up to date | `Current version: A` / `✔ Already up to date` | | 0 (verified via `PI_BUILD_URL=http://127.0.0.1:…` fake returning 18.12.3) |
| canary persisted or `--canary` | adds `Current channel: canary`; with `--canary` while persisted is stable: `Switching to canary B` (or `… (downgrade from A)`) — `--check` does **not** persist the channel (verified: no `update:` in config.yml) | | 0 (verified) |
| `--force --check` | `Forcing reinstall of B` if same version | | 0 |
| offline/proxy refused | `Current version: 18.12.3` | `Failed to check for updates: TypeError: ECONNREFUSED: Unable to connect. Is the computer able to access the url?` | **1** (verified) |
| HTTP error (fake server via `PI_BUILD_URL`) | | `Failed to check for updates: Error: <base> answered 500 Internal Server Error for the omp stable build for linux-x86_64` | **1** (verified) |
| malformed answer | | `Failed to check for updates: Error: Invalid answer from <base> for the omp stable build for linux-x86_64: build has no valid version` | 1 (verified) |
| no canary published (404 `{"error":"no_build"}`) | `Current version…` / `Current channel: canary` | `Failed to check for updates: Error: No omp canary build for linux-x86_64 is published on <base> yet. Try \`omp update --stable\`.` | **1** (verified) |
| nix/brew/mise-managed | `<path> is installed and kept up to date by <mgr>; update it from <mgr>.` | | 0 |
Colors: chalk; disabled when piped (set `NO_COLOR=1` anyway). `--check --canary` and `--check --stable` are allowed; `--canary --stable` → usage error.
**No JSON mode** in `omp update`. Machine-readable check = the same endpoint, verified:
```
GET https://build.stencil.so/api/products/omp/latest/<target>?channel=stable|canary[&from_version=<cur>]   Accept: application/json
 <target> = linux-x86_64 | linux-arm64 | linux-musl-x86_64 | linux-musl-arm64 | macos-arm64 | macos-x86_64 | windows-… (xOt in build-service.ts)
 200 → {"build":{"id","product":"omp","version":"18.12.5","channel":"stable","commit","created_at","published_at","notes":"### Added\n…markdown changelog…","files":[…]},
        "file":{"name":"omp-linux-x64","platform","arch","kind":"archive","size":256575456,"sha256":"…","url":"/d/omp/…"},
        "download":"https://…r2.cloudflarestorage.com/…?X-Amz-…(600 s presigned)",
        "patch":{"format":"file","from":"<build id>","from_sha256","size":62636561,"sha256","url":"https://…patch?…"}}   (patch present when from_version matches a known build)
 400 {"error":"invalid channel"}; 404 {"error":"no_build"} (client message "No … is published … yet")
```
Recommendation: OmpDeck "check for updates" = parse `omp --version` (`omp/18.12.3`) + one GET to that endpoint (`build.version`, `build.notes` for a changelog popover), compare semver; or simply run `omp update --check` and match `^New version available: (\S+)$` / `Already up to date`. Prefer the CLI (keeps working if omp changes backend); use the HTTP API only as a bonus for release notes. Rate limits: none observed. The startup check inside omp is governed by `startup.checkUpdate`, channel by `update.channel` (enum stable|canary).

### B2. `omp update` (install) — NOT executed; source read (bundle `Cze`, `oe`, `Eze`, `hOt`; 18.4.10 `replaceBinaryForUpdate` :1411)
* Flags: `-c/--check`, `-f/--force` (reinstall same version), `-l/--plugins` (runs `plugin upgrade` instead: `Upgrade all marketplace plugins` → prints `All marketplace plugins are up to date.` or `  <id> (<scope>): <from> -> <to>` lines; failure `Failed to upgrade: …` rc 1; no JSON for the all-plugins path), `--canary` (switch to canary + update; persists `update.channel=canary` **after a successful install**), `--stable` (switch back). `--canary` with `--stable` → `--canary and --stable are mutually exclusive`.
* Method chosen from the first `omp` in PATH: nix → refuses ("managed by Nix"), brew → `brew update && brew upgrade/reinstall`, mise → `mise upgrade`, bun/npm global symlink → `bun/npm install -g` with fallback to binary repair, **else "binary"** (our case).
* Binary path: print `Downloading omp-linux-x64 (NNN MB)…` (or tries the **binary delta patch** first if the current file's sha256 == `patch.from_sha256`: `Downloading patch from X (60 MB)…`/`Patched and verified sha256:…`, falling back to full download), verify size+sha256, then under a file lock **rename** `omp` → `omp.<ts>.<pid>.<n>.bak`, rename temp (`omp.<attempt>.new`, chmod 0755) → `omp`, run `<omp> --version` to verify (`<omp> --version`), delete backup (restore on failure), sweep stale backups. Prints dim `Installing update...`, green `✔ Updated to B at <path>` (bundle `j()`), and finally dim `Restart omp to use the new version`. No interactive prompts. Failure → stderr `Update failed: …` rc 1. Needs write permission on the directory of the binary; GITHUB_TOKEN not used on this backend.
* Effect on running sessions: the file at the path is replaced via rename (old inode stays alive) → running tabs keep running the old version until restarted; new tabs use the new binary. Natives dir `~/.omp/natives/<ver>` is per version. OmpDeck should: run `omp update` in a **background child (no pty needed)** capturing stdout line-by-line for progress, then show "Restart tabs to use B".
* Long-running: ~256 MB download (patch ~60 MB). Timeouts: metadata 30 s, download 15 min (18.4.10 constants `update-cli.ts:40-41`).

---------------------------------------------------------------------------------------------------
## C. Sessions (`omp session pack | unpack | moved`) — all verified in sandbox unless marked

Sources: bundle `packages/coding-agent/src/commands/session.ts`, `session/session-transfer.ts` (exports `packSession`=Sze, `unpackSession`=Aze, `markSessionMoved`=vze, resume-time fork `hM`, entry classifier `oOt`), `session/session-manager.ts` (`moveTo`, `openRelocated`, `forkFrom`, `continueRecent`). Help:
```
omp session pack <session> <out.tar>      # <session> = .jsonl path OR id/prefix (resolved against process cwd then globally)
omp session unpack <archive.tar> --cwd <dir>   # prints new session file path on STDOUT; mode message on STDERR
omp session moved <session> --to <label>
```
Errors: stderr `error: <msg>` rc 1 (`Session "deadbeef" not found.`, `Session file not found: /x.jsonl`, `Missing --to <label>…`, `Directory not found: <cwd>`, `Bad session archive …`). Another live omp holding the session: `Cannot pack|replace|mark moved <id>: another live omp process writes this session. Exit it first. Nothing was changed.` (source `session-transfer.ts:D`, not reproduced: needs a live omp).

### C1. pack
`omp session pack /abs/…/<ts>_<id>.jsonl /tmp/s.tar` → rc 0, stdout = archive path. Tar members (verified `tar tvf`): `manifest.json` (`{"version":1,"id":"<uuid>","cwd":"/home/…/Downloads","omp":"18.12.3"}`), `session.jsonl` (verbatim bytes), `artifacts/**` (everything under `<session-file-without-.jsonl>/`, incl. subagent jsonls), `blobs/<sha256>` for every `blob:sha256:<hex>` referenced from the journal/artifact jsonls (read from the agent blob store; missing ones → stderr warning, still rc 0). Max member size 2 GiB.

### C2. unpack (the important one)
`omp session unpack /tmp/s.tar --cwd /tmp/omp-sb/proj2` (target must exist, else rc 1 `Directory not found`):
* New file: `<sessionsRoot>/<encoded-cwd>/<header.timestamp with [^\w-]→->_<SAME id>.jsonl`, i.e. **same session id and same filename as the original**, in the *destination project's dir* (`-tmp-omp-sb-proj2`). Dir encoding (`src/session/session-paths.ts:62-95`, lossy — don't invert it, read header `cwd`): under `$HOME` → `-` + relpath with `/ \ :`→`-` (`/home/u/Downloads`→`-Downloads`); under tmpdir → `-tmp-…`; else `--<abs path with />-…-->`. Use the **stdout path** omp prints instead of recomputing.
* Header line rewritten: `"cwd":"<new cwd>"`, plus `title`, `titleSource`, and `previousSessionFiles:["/tmp/omp-session-unpack-XXXX/…"]` (staging path, ignore). Line 1 (`{"type":"title",…,"pad":"   …"}` fixed-width title record) preserved. All other lines byte-identical (diff: only header line changed; 849 bytes appended).
* Appended at end: `{"type":"custom_message","customType":"session-transfer","display":true,"details":{"kind":"arrived","fromCwd":"…","cwd":"…"},"content":"This session was moved here from another machine. It ran in `…`; its working directory is now `…`…"}`.
* Artifacts dir (`<file>/0-test.txt`, `sub/n.txt`) recreated next to the new file; blobs written into the agent blob store (content-addressed, idempotent).
* **Conflict semantics** (`Aze`; local copies = every `*_<id>*.jsonl` anywhere under the sessions root + target dir):
  | local copies of that id | result | stderr | stdout |
  |---|---|---|---|
  | none | created | `Unpacked session (<id>).` | new path |
  | exist and each one's entries are a prefix of the archive's (a trailing `departed` entry is ignored, so a `moved` original counts as non-diverged) | **local copies are set aside and deleted** (jsonl + artifacts dir), new file created → effectively a MOVE; rollback on failure | `Replaced the local copy of this session (<id>).` | new path |
  | exist but diverged (has entries the archive lacks, e.g. the `arrived` note) | **forked**: new id, header `parentSession:<old id>`, original untouched | `The local copy of this session diverged; unpacked as a fork (<new id>).` | fork path |
  Verified all three (original in `-Downloads` vanished after unpack to `--cwd proj2`; second unpack to proj3 forked because proj2's copy had the arrived entry).
  ⇒ `pack`+`unpack` **is the omp-native “Move session to project…”**, NOT a copy.

### C3. moved
`omp session moved <path|id> --to cloud-box` → rc 0, **appends exactly one JSONL line** (342 B) and nothing else changes (verified `b.startswith(a)`; mtime bumps):
```
{"type":"custom_message","customType":"session-transfer","content":"This session was moved to cloud-box and continues there. This copy stops here.","display":true,"details":{"kind":"departed","to":"cloud-box"},"attribution":"agent","id":"3c6952fc","parentId":"777761b3","timestamp":"…Z","runtime":{"version":"18.12.3"}}
```
Repeat calls append again. Resume behavior (bundle fn `hM`): when the **last** `session-transfer` entry is `departed`, `omp --resume <id|path>` (and `--continue`) does not continue it: it creates a **fork** (new id, `parentSession`=old id, same project dir derived from header cwd, artifacts copied, fork gets a `{"kind":"forked","source":…,"to":…}` custom_message); original file unchanged (verified: `--resume <moved.jsonl> -p hi` created a new `…_<newid>.jsonl`; original size unchanged). omp's picker does **not** hide departed sessions (no filter on `departed` anywhere in the bundle).
**Detecting `moved` in a scan:** it is an appended line at the *end* of the file, not in the header/title line. Bounded read: read the last ~16 KiB (more if the file is smaller), split lines, from the end find the first line containing `"customType":"session-transfer"`; parse JSON; `details.kind` ∈ `departed` (moved away; `details.to` = label) | `arrived` | `forked`. Latest wins (that is omp's rule, `oOt` scans from the end). Cheap pre-filter: `memmem(tail, b"\"kind\":\"departed\"")`. A moved original can still receive later lines only if something resumes it by raw path in a way that bypasses `hM` (not observed). Suggested UI: dim row + badge “moved → <to>”; clicking Resume still works (omp forks) – show a tooltip “resuming will fork”; do not hide it (omp doesn't). If OmpDeck itself moves sessions via pack/unpack the original is deleted, so no stale row appears.

### C4. Is there a direct copy / fork / re-root?
* **Copy (new id, original untouched): `omp --fork <id|path> [--cwd <dest>]`** — hidden flag (not in `--help`, but in `src/cli/args.ts:64 fork?: string`, bundle `V9e`: `Ce.forkFrom(path, cwd, sessionDir, …{repairInterruptedTail:true})`; `--fork requires session persistence`). Verified: `cd proj && omp --fork <src.jsonl> --cwd /tmp/omp-sb/proj3 -p "say hi"` immediately wrote `sessions/-tmp-omp-sb-proj3/<now>_<NEWID>.jsonl` (+ `<NEWID>/` artifacts copy `0-test.txt`, `sub/n.txt`), header `cwd`=dest, `parentSession`=source id, `providerPromptCacheKey`=source id, title kept, all 77 non-header entries identical, no extra entry; source untouched. (The sandbox process then died with `Could not restore model …` because the sandbox has no providers – a real env with the same models continues into the TUI.) The fork file is created *before* model restore, so it exists even if the launch later fails. Blobs are content-addressed in one store → nothing to copy.
* **Re-root / move in place:** TUI slash command `/move <path>` (`handleMoveCommand` → `SessionManager.moveTo(newCwd)`: renames the jsonl into the target project dir and renames its artifacts dir, rewrites cwd; refuses while streaming or when another live process holds it) — interactive only; can be driven by sending keys `"/move /abs/path\r"` into a running tab's tmux pane. The “project dir missing” prompt (`V_`/`openRelocated`) is also interactive-only (`--resume <id>` non-interactive → `Session "…" belongs to a directory that no longer exists (…); run interactively to move it into the current project.`). No non-interactive CLI for in-place re-root other than pack/unpack.

### C5. Recommended command sequences for OmpDeck (omp performs every write)
**Copy session to project…**: (new id; original and its artifacts untouched)
```
omp --fork <abs src.jsonl> --cwd <abs dest dir>        # run as a new tmux tab (cwd=dest); this tab IS the copy
```
After launch, locate the copy by scanning `<sessionsRoot>/<encode(dest)>/` for the newest `*.jsonl` whose header has `parentSession == <src id>` (or just wait for the normal session scan). Pre-checks in the GUI: dest exists; src not needed to be idle (forking a live session is allowed: unresolved tool calls get unknown-outcome results, changelog 13620). If the user wants a copy without opening a UI, `omp --fork … --mode rpc < /dev/null` should create and exit [UNVERIFIED – needs a provider-enabled agent dir; a sandbox dir without credentials crashes on model restore after writing the file].
**Move session to project…**: (same id/filename, original removed, arrival note appended)
```
omp session pack  <abs src.jsonl> /tmp/ompdeck-<id>.tar        # rc 0 → stdout path
omp session unpack /tmp/ompdeck-<id>.tar --cwd <abs dest dir>    # rc 0 → stdout = new .jsonl path; stderr "Replaced the local copy…"
rm /tmp/ompdeck-<id>.tar
```
Requirements/handling: close the session's tab first (live writer ⇒ rc 1 `…another live omp process writes this session. Exit it first. Nothing was changed.`); dest dir must exist; treat stderr `…diverged; unpacked as a fork` as “moved as a new session (id changed)”; on any rc≠0 nothing changed (unpack has rollback). For a live tab one could instead send `/move <dest>` into its pane (interactive, UNVERIFIED end-to-end). Subsequent resume: `omp --resume <stdout path>` with `current_dir(dest)`.

---------------------------------------------------------------------------------------------------
## D. Misc for a status widget
* `omp --version` / `-v` → `omp/18.12.3` (single line, `cat -A`: `omp/18.12.3$`), rc 0. Parse `^omp/(\d+\.\d+\.\d+(?:-[\w.]+)?)$`.
* `omp usage --json [--provider P] [--redact]` (works, real run ≈1–3 s, network): 
  ```
  {"generatedAt":<ms>,"reports":[{"provider":"anthropic","fetchedAt":<ms>,"limits":[{"id":"anthropic:5h","label":"5-hour limit","scope":{"provider","accountId","windowId":"5h","shared":true},"window":{"id","label","durationMs","resetsAt":<ms>},"amount":{"used","limit"?,"remaining"?,"usedFraction","remainingFraction"?,"unit":"percent"|"credits"},"status":"ok"|…}],"metadata":{"accountId","email"}}],
   "accountsWithoutUsage":[],"disabledCredentials":[],"capacity":{"<provider>":[{"window":"5h","durationMs","accounts","usedAccounts","remainingAccounts"}]}}
  ```
  (observed on this machine: providers commandcode, opencode-go, openai-codex ×2, anthropic; `status` may be missing → treat as ok). Sub-actions `usage invalidate|clients|accounts|reset`; `--history --days N`. Use `--redact` for any screenshot/log.
* `omp stats --json` → `{"overall":{totalRequests,successfulRequests,failedRequests,errorRate,totalInputTokens,totalOutputTokens,totalCacheReadTokens,totalCacheWriteTokens,cacheRate,cacheSavings,totalCost,unpricedRequests,totalPremiumRequests,avgDuration,avgTtft,avgTokensPerSecond,firstTimestamp,lastTimestamp},"byModel":[],"byFolder":[],"byAgentType":[],"timeSeries":[],"modelSeries":[],"modelPerformanceSeries":[],"costSeries":[]}`; first stderr lines `Syncing session files…` / `Synced N new entries from M files (T total)`. **It syncs session jsonls into a DB and writes `~/.omp/stats.db` as a side effect – and that path is NOT redirected by `PI_CODING_AGENT_DIR`** (see below). `stats --summary` prints text; `stats -p PORT` starts a dashboard server. Prefer `usage --json` for a live widget; use `stats --json` sparingly (it also scans every session).
* `omp models --json` → `{"models":[{"provider","kind":"chat","id","selector":"anthropic/claude-3-5-sonnet-20240620","name","contextWindow","maxTokens","reasoning","thinking":null|…,"input":["text","image"],"cost":{"input","output","cacheRead","cacheWrite"},"pricingStatus":"fixed"}]}` – 168 models on this machine, rc 0, ~1 s, from cached catalog (`omp models refresh` forces network; `--kind all`; `omp models find <pat>`; `omp models <provider>`). Useful for model pickers for settings like `modelRoles`.
* Launch flags useful for tabs: `--resume <id|path>`, `--continue`, `--fork <id|path>`, `--cwd <dir>`, `--session-dir <dir>`, `--no-session`, `--approval-mode always-ask|always-ask-cached|write|yolo`, `--profile <name>`, `--config <overlay.yml>` (extra overlay per run, repeatable – can pass per-tab settings without touching config.yml).

---------------------------------------------------------------------------------------------------
## Side effects of my experiments / cleanup (disclosure)
* Everything ran under `PI_CODING_AGENT_DIR=/tmp/omp-sb/agent`, real `~/.omp/agent/config.yml` unchanged (mtime Oct 20 22:16, 827 B), nothing written under `~/.omp/agent/sessions|blobs` (only read-only copies of one `-Downloads` session + one blob were copied out).
* **One unintended write:** `omp stats --json` with the sandbox agent dir still synced the sandbox's session copy into the real `~/.omp/stats.db` (16 messages, 1 user_message, 25 tool_calls, 1 file_offsets, 1 session_rollup — all `session_file LIKE '/tmp/omp-sb/%'`). I deleted exactly those rows with sqlite3 (messages 30 130→30 114, user_messages 597→596, tool_calls 29 663→29 638, file_offsets 1→0, session_rollup 218→217); the DB triggers re-marked the affected rollup buckets dirty (`rollup_dirty`=2, `session_dirty`=1), which omp recomputes on its next stats read. **`omp stats` is not isolated by `PI_CODING_AGENT_DIR`.** No other real files touched (other mtime changes under `~/.omp` come from the live omp processes running now: agent.db WAL, logs, daemons).
* Sandbox dirs/servers/tars removed (`/tmp/omp-sb`, `/tmp/omp-session-unpack-*`).
