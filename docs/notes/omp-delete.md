**Recommendation:** for a closed session, run a headless `omp` with a small `-e` extension that takes omp's session lease and then calls `FileSessionStorage.deleteSessionWithArtifacts`. For a running tab, inject `/delete` into the pane.

Installed binary is `omp/18.12.6`. The `oh-my-pi` checkout is 18.4.10, so wherever the two differ I used the bundle (`strings ~/.local/bin/omp`). Experiments ran in a `/tmp` sandbox with temp `PI_CODING_AGENT_DIR` and `PI_CONFIG_DIR`, a local fake model, and tmux socket `-L omp-deck-test-del`. Everything is cleaned up: socket, fake server, `/tmp/qa_del`.

## 1. How omp deletes a session

- **CLI:** there is no `omp session delete|rm`. `omp session` only has `pack|unpack|moved` (`omp --help`). `omp gc` has no delete either: it handles blobs, archive, WAL and stale markers.
- **RPC:** there is no `delete_session`. The RPC command list has only `new_session`, `switch_session`, `branch`, `fork` and `open_session` for session changes, and `new_session` takes no `drop` option. ACP has `session/new|load|list|fork|resume|close`, nothing for delete.
- **Slash commands:**
  - `/delete` is TUI-only, described as "Delete the current session and start a new one". It calls `handleDeleteCommand`, which calls the same code as `/new` with `{drop:true}`, then `AgentSession.newSession({drop:true})`, which runs `sessionManager.dropSession(oldFile)` and then `sessionManager.newSession()`.
  - Source for the in-process path: `session-manager.ts:2240` (`dropSession` = drain writer, then `storage.deleteSessionWithArtifacts`, ENOENT tolerated).
  - `/session delete` has a headless handler (`slash-commands/builtin-session.ts:~272`, bundle line 791336) that calls `sessionManager.dropSession(file)`. It refuses only while streaming or for an in-memory session.
- **Resume picker (Ctrl+D, `app.session.delete`, plus `app.session.deleteNoninvasive` on Ctrl+Backspace):**
  - Interactive picker: `selector-controller.ts:1612-1626` (same storage call).
  - `omp --resume` with no argument: `main.ts:203`.
  - Only this picker path shows a "Delete session?" dialog (bundle string). There is no `/delete` confirm in 18.12.6; the old `showHookConfirm` one at `selector-controller.ts:1780` is gone.
- **What gets removed**, from `FileSessionStorage.deleteSessionWithArtifacts` (`session-storage.ts:1057-1100`, bundle line 502102):
  - the `.jsonl`;
  - the sibling `<stem>/` directory recursively, which holds artifacts and subagent sessions;
  - stale `<name>.jsonl.<n>.bak` EPERM-rewrite leftovers.
- **Not touched:**
  - pins, in `<agentDir>/session-pins.json`, keyed by session id (`session-pins.ts:9-20`);
  - blobs;
  - terminal breadcrumbs, which I saw still pointing at deleted files after delete (`omp gc --stale` prunes them);
  - `history.db` and stats.
- **Trash:** none, deletion is permanent.
- **Live-process check:**
  - **The delete functions themselves never check for a live owner.** The picker path, the `/delete` path and `deleteSessionWithArtifacts` have no lease check. I deleted a session file out from under a live TUI with no refusal. I did not test what that TUI does on its next write.
  - **omp's lease is keyed by session id, not file path.** That changed in 18.5.0: `claimSession(sessionId, path)` and `tryAcquireSessionLease(sessionId)` replaced `claimSessionFile` and `sessionOwnerLeasePath` (changelog in the bundle). The old `.<name>.owner` approach in the checkout is obsolete.
  - **How the lease works.** It lives under `$config/run/session-owners/<id>`. On Linux it is an abstract unix socket (`@omp-file-lock-<hash>`), so there is no file on disk. The OS releases it on exit.
  - **How to probe it.** `tryAcquireSessionLease` is exported from `@oh-my-pi/pi-coding-agent/session/session-storage` (bundle lines 765726 and 907536). It returns a handle, or null while a live omp holds it. I proved that below.

## 2. Best non-interactive mechanism

### (a) Closed session

**RPC `/session delete` is not safe.** I ran `omp --mode rpc --resume X` and sent `{"type":"prompt","message":"/session delete"}`. The reply was "Session deleted…". On exit the disposer re-created a 239-byte stub containing only `session_exit {"reason":"dispose"}`, with no header.

```
before: X.jsonl(2098) X/x/a.txt  Y.jsonl  Z.jsonl
after : X.jsonl(239, stub)       Y.jsonl  Z.jsonl
```

**Working mechanism:** a headless rpc process with `--no-session` and a small delete extension.

Extension (`del.ts`) logic:
1. Read the session id from the header line (first 3 lines of the `.jsonl`).
2. `lease = tryAcquireSessionLease(id)`; if null, report `live` and do nothing.
3. Otherwise `await new FileSessionStorage().deleteSessionWithArtifacts(file)`, then `lease.release()`.

Imports used: `import { FileSessionStorage, tryAcquireSessionLease } from "@oh-my-pi/pi-coding-agent/session/session-storage"`.

Argv (the extension reads the target path and result path from env):

```
env DEL_TARGET=<abs .jsonl> DEL_OUT=<result file> \
  omp --mode rpc --no-session --no-extensions -e <del.ts> </dev/null
```

The extension's factory runs at startup and appends one JSON line to `DEL_OUT`. Closing stdin makes omp exit. Observed: rc=0, 0.74 s.

Before and after listing (the `.jsonl.99.bak` and the `subagents/` dir were seeded to prove cleanup):

```
before: X.jsonl  X.jsonl.99.bak  X/artifacts  X/subagents/s.jsonl
after : (empty dir)
result: {"ok":true,"id":"01a1612a-…"}
```

No stub came back, no new session file was created, and the breadcrumbs were untouched.

**Lease guard:** with the session open in a live tmux omp, the same extension returned `{"ok":false,"error":"live","id":"01a16127-…"}` and the file was untouched.

**Failure modes:**
- Result `live`: the session is open somewhere, so refuse the delete.
- Result `no session header`: the file is unreadable or not a session.
- Result with an `error` string, e.g. `Session file deleted but failed to remove artifacts directory …`: the `.jsonl` is already gone.
- `-e` extension load failure: omp prints `Failed to load extension …` and no result line appears. This happened to me when I imported `sessionOwnerLeasePath`, which 18.12.6 no longer exports. Treat a missing result line as failure.
- Internal specifiers can change between omp versions, so re-check on upgrade. The existing `ompdeck-ctl.ts` has the same exposure.
- A headless omp also loads global config. If the user's config has `collab.autoStart`, it may host collab briefly [UNVERIFIED]; `--config <overlay>` could turn that off.

### (b) Running session in a tmux tab

**Inject `/delete` into the pane.** I typed `/delete` and pressed Enter:
- The pane showed "✔ Session deleted" and a cleared transcript.
- The `.jsonl` and `<stem>/` were gone, and a fresh session file appeared (header plus a settings-only entry).
- The breadcrumb `terminal-sessions/pts-N` now points at the new file.
- The process kept running (the TUI is not exited) and there was no confirmation prompt.
- The empty replacement session is not cleaned up: it was still on disk after `/exit`, with no `session_exit`. OmpDeck must handle this leftover, because `omp -c` and the picker may see it.

Use the injector from `omp-actions.md` §3.7 (idle check, one Ctrl-C if the composer may be non-empty, paste `/delete`, Enter).

**In-process alternative, not recommended.** From the extension, `AgentRegistry.global().get("Main").session.newSession({drop:true})` returned `true` and deleted the files. It also fired `session_switch` with `reason:"new"` and updated the breadcrumb. But the TUI did not clear the transcript, so the old conversation stayed on screen against a new empty session. Only the TUI handler resets the UI.

**What does not work:**
- `api.sendUserMessage("/delete")` is sent to the LLM as plain user text, not run as a command.
- The extension command context's `newSession` forwards only `parentSession`, so there is no `drop` option.
- The headless delete (a) must never be pointed at a live session; the lease guard exists to stop that.

## 3. Restart semantics

- **Kill then relaunch:** `tmux kill-server` (SIGHUP) appended `{"customType":"session_exit","data":{"reason":"sighup","kind":"signal",…}}` and the process exited. `omp --resume <abs path>` then resumed with the full transcript and no extra entry.
- **Mid-stream kill:** a SIGHUP while a slow fake stream was running persisted the partial assistant message with `stopReason:"aborted"` ("Request was aborted") after the `session_exit`. The resumed TUI shows "Operation aborted".
- **`/restart`:**
  - It does a graceful dispose, writes `session_exit reason:"dispose" kind:"normal"`, then re-execs with the same pid.
  - The new argv was `omp --model fake/fake-1 --resume <abs session path>`.
  - Source: `interactive-mode restart()` (bundle line 818049) builds the args with `Zie(process.argv.slice(2), file, …)`, which drops one-shot flags and appends `--resume <file>`.
  - A mid-stream `/restart` gave the same persisted result as the kill: a partial aborted assistant message, with no automatic continuation.
- **Differences:**
  - `/restart` keeps the same pid and tmux pane, and records `dispose` instead of `sighup`.
  - It keeps the other launch flags, including `-e` and `--model`, so the ctl extension reloads.
  - Kill plus relaunch needs OmpDeck to rebuild argv itself.
- **`--restore`:** I ran it after the clean aborts above and it reported "Restored: nothing was left unfinished." Its help says it continues the interrupted turn and subagents, so it likely matters only after a crash or SIGKILL [UNVERIFIED].
- **Subagents:** not exercised here. `omp-control-ext.md` already records that after `--resume`, persisted subagents come back `parked` or `aborted`, and I did not re-verify that.

## Not tested

- Resume-picker Ctrl+D (read from source only).
- The live TUI's behaviour after another process deletes its file.
- `omp gc --stale`.