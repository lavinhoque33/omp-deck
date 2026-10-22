# OmpDeck Agent Hub — data sources and actions

Source readings are against `~/PersProject/oh-my-pi` (18.4.10). The 18.12.5 bundle embeds the same files, e.g. `packages/coding-agent/src/registry/persisted-agents.ts` and `packages/tui/src/overlays/agent-hub*.ts`. Observations come from real sessions under `~/.omp/agent/sessions` (read-only) and a sandbox run of omp 18.12.5 against a local fake model server. The sandbox used `PI_CODING_AGENT_DIR=/tmp/hubx/agent` on tmux socket `omp-deck-test-hubx`. It is cleaned up: socket killed, fake server killed, `/tmp/hubx` removed. The fake server used port 18853 because 18791 was already taken by another process. No omp process was left running.

## 0. Answers

1. omp's Hub is an in-memory registry viewer. It is built from the live `AgentRegistry` plus progress events, and merged on open with a scan of the session's artifact tree. Fields and actions are in section 1.
2. The data an external reader needs is on disk and sufficient. It comes from:
   - `<stem>/<id>.jsonl` per subagent (`session` header and `session_init` give identity, task, agent, model, tools);
   - `<id>.md` (final output) and `<id>.jsonl.tombstone` (killed);
   - the parent's `task` toolCall and its `async-result` custom_messages.
   
   Status cannot be read from one field. Use the inference rules in section 3.
3. No per-agent registry file exists. `AgentRegistry` is process-global and in memory (`agent-registry.ts:1-12`, `static global()`). The only on-disk per-process records are `~/.omp/run/collab-hosts/*.json` and `~/.omp/agent/terminal-sessions/<tty>`. Subagent transcripts are append-only except for a rewritten title slot (section 4).
4. Kill, chat and revive are only reachable through the collab guest or through hotkey injection into the Hub. No CLI subcommand does it (`omp ps` is for daemon processes, not agents). Injection was verified for kill; the exact sequence and its risks are in section 5.
5. Render shapes are in section 6.

## 1. What omp's own Agent Hub shows

**Entry points.** `Alt+A` (`app.agents.hub`), `Ctrl+S` (`app.session.observe`), `/hub`, double-`←` on an empty composer.
- `/hub` is `builtin-session.ts:582-590`, with `initialSection: "activity"`.
- `/agents` is a different thing: agent-definition settings (`docs/agent-hub.md`, "Related surfaces").
- The binary's `Alt+A` matches the checkout and was observed working.

**Bundle module paths (18.12.5).** Chunks `agent-hub-runtime-*.js`, `agents-hub-deps-*.js`, `persisted-agents-*.js`, `agent-transcript-viewer-*.js`. Embedded source comments:
- `packages/tui/src/overlays/agent-hub.ts`, `agent-hub-types.ts`, `agent-hub-projection.ts`, `agent-hub-renderer.ts`, `agent-transcript-viewer.ts`
- `packages/coding-agent/src/modes/agent-hub-runtime.ts`
- `packages/coding-agent/src/registry/agent-registry.ts`, `persisted-agents.ts`

**Record model** (`agent-hub-types.ts` `AgentRecordLike`, `AgentStatus = "running"|"idle"|"parked"|"aborted"`):
```ts
{ id, displayName, kind: "main"|"sub"|"advisor", parentId?, status,
  sessionFile, createdAt, lastActivity, activity?,          // activity = task gist
  history?: { agent, modelRole, resolvedModel, resolvedModelIsFallback,
              metrics{tokens,requests,tools,cost,durationMs,durationKind,contextTokens,contextWindow},
              readOnly, outputPath, patchPath, branchName, nestedPatchPaths },
  lifecycle?: { responseAt, acceptedAt, terminalAt } }
```
- Status order is running, idle, parked, aborted (`STATUS_ORDER`, projection.ts).
- "Main" is never listed.
- Advisor transcripts `__advisor[.slug].jsonl` are rows with kind `advisor` and id `<owner>/advisor[:slug]`. They are read-only.

**Roster row** (`#pickerAgentItem`, agent-hub.ts:~915-960), confirmed in a real capture:
```
❯ ⟳ SubOne-2                                                              fake-opus
    Complete assignment thoroughly: SUBWORK one: sleep then yield
    $0.0000   259ms active    1 req   1 tools 1.1K tok just now
```
- Glyphs: `⟳` running, `●` idle, `⏹` aborted; parked also exists.
- Fields: id, status dot, model chip, task text, cost, time, req, tools, tok, age, role badge, `↳ parent` badge, `N unread` badge.
- Header aggregate: `⟳ 2 running · ● 2 idle · ⏹ 1 aborted`, then cost, active time, req, tools, tok.

**Inspector** (`#detailFacts`, ~1353-1410):
- Task, Current (tool and args, or last intent), Usage, Lineage ("Spawned by X · N children"), Children, Registered (local datetime), Changes, Output (the `.md` path), Patch, Nested patch, Worktree branch.
- A context meter and "Recent activity" ("✔ bash …", "✘ yield …", "○ completed …", "⟳ Response …").
- Live-only (in-process `progress`, not on disk): currentTool, retryState, activeTime.

**Activity section (`2`).** Filters: `f` cycles All/Errors/Responses/Tools; `s` cycles scope; `/` searches.

**Actions** (agent-hub.ts:880-885, 2462-2557):

| Key | Action | Notes |
|---|---|---|
| Enter | Focus the agent's session in the main TUI | Aborted, advisor and collab rows open the in-hub transcript viewer instead (`#activateAgent`) |
| `r` | `ensureLive` revive | Only `parked` |
| `x` | `abort` if running, then `release(tombstone:true)` | Immediate, no confirm |
| `t` | Flat/tree toggle | |
| `/` | Filter | Substring on id or display name |
| `1` / `2` | Switch section | |
| Tab | Toggle inspector on narrow terminals | |

Chat goes through the transcript viewer's input line or by focusing the agent. It revives a parked agent, steers a running one, and prompts an idle one.

## 2. Data model

Layout, verified in real sessions and the sandbox:
```
<agentDir>/sessions/<cwd-slug>/<ts>_<uuid>.jsonl            parent session ("stem" = filename without .jsonl)
<stem>/<Id>.jsonl                  subagent transcript
<stem>/<Id>.md                     final output, written at completion; 0 bytes if failed/killed
<stem>/<Id>.jsonl.tombstone        0-byte (mode 0600) marker, written only by the Hub `x` (or collab) kill
<stem>/<Id>.nested-<n>-*.patch, <Id>.patch   isolation outputs
<stem>/<Parent>/<Parent>.<Child>.jsonl       nested agents (id = "Parent.Child", dir = parent id)
<stem>/__advisor[.slug].jsonl      advisor transcript
<stem>/<n>.bash.log, *.read.log, *.eval.log  tool output artifacts (not agents)
<stem>/<Id>.json                   structured output, when an outputSchema was used
```

- **Scan rules.** The scan is `registerPersistedSubagentsFromDir` (persisted-agents.ts:~690-835). `id` is the filename stem; `parentId` is the directory name, or `Main` at the top level. Files containing `.bak` are skipped. A transcript with only title and session lines (no `session_init` and no message) is skipped as an incomplete stub.
- **Cross-session identity.** Forking or moving a session copies the artifact dir, so the same `<Id>.jsonl` exists under several parent stems (seen in `Samsung Playground`). Always scope by the owning stem.
- **Id allocation.** `AgentOutputManager.allocate` (`task/output-manager.ts`) returns the requested name first, then `name-2`, `name-3`. `__advisor` is reserved. A nested child id is `<Parent>.<name>`.
- **Sandbox confirmation.** Spawning `SubOne` again created `SubOne-2`, then `SubOne-3`.

## 3. Mapping a parent `task` call to child files, and status inference

### Mapping

A real parent toolCall (OmpDeck session):
```json
{"type":"toolCall","id":"toolu_012n…","name":"task","arguments":{"i":"Running P0 recon scouts","context":"# Goal…",
 "tasks":[{"agent":"deck-api-scout","name":"EguiInput","solutionSpace":"…","task":"# Target\nWrite …"}, …]}}
```
- Schema (`task/types.ts:48-100`): `tasks[]` items hold `name?`, `agent` (default `'task'`), `task`, `solutionSpace`, `outputSchema?`, `schemaMode?`, `tools?`, `isolated?`. The single-task form has no `tasks[]` or `context`.
- `name` is optional. When omitted the name is auto-generated. The requested name is not the file id when there is a collision.

The immediate toolResult (always the "Spawned…" ack; 760/760 task results across all local sessions were async-style):
```json
{"role":"toolResult","toolName":"task","content":[{"text":"Spawned 4 background agents using deck-api-scout.\n- `EguiInput` (job `EguiInput`)…"}],
 "details":{"projectAgentsDir":null,"results":[],"totalDurationMs":2,
   "progress":[{"index":0,"id":"EguiInput","agent":"deck-api-scout","agentSource":"project","status":"pending","task":"Complete assignment thoroughly:…","assignment":"# Target…","recentTools":[],"recentOutput":[],"toolCount":0,"requests":0,"tokens":0,"cost":0,"durationMs":0}, …],
   "async":{"state":"running","jobId":"EguiInput","type":"task"}}}
```
- `details.progress[i].id` is the authoritative child id (nested ids are `M2cCore.M2cSaveService`). The child file is `<stem>/<id>.jsonl`, or `<stem>/<ParentId>/<id>.jsonl` when the calling agent is itself a subagent.
- The child transcript carries no `toolCallId` back-reference. Join only by id.
- Sandbox: the child file appeared in the same millisecond as the task toolResult (18:03:51.658 vs 18:03:51.680).
- `progress[].status` stays `pending` forever in the stored result (all 1690 items). `details.async.jobId` is only the first task's id.
- `SingleResult` (`details.results`) and `AgentProgress` types are in `packages/tui/src/tools/task.ts:2256-2470`. A real sync-mode `results[]` example was not found locally, and the fields are not populated in async mode.

Completion arrives later in the parent as a `custom_message` with `customType:"async-result"`:
```json
{"type":"custom_message","customType":"async-result","display":true,
 "content":"<system-notice>\nBackground job SubTwo has completed. …\n<task-result id=\"SubTwo\" agent=\"task\" status=\"completed\" duration=\"15.9s\">\n\n<output>\nchild failing no yield\n</output>\n</task-result>\n\nSubTwo is now idle — message it via `write agent://SubTwo`…\n</system-notice>",
 "details":{"meta":{"source":{"type":"report","value":"background job delivery"}},
   "jobs":[{"jobId":"SubTwo","type":"task","status":"completed","agentId":"SubTwo","label":"SubTwo","durationMs":15858}]}}
```
- Real `task-result status=` values (503 completed, 38 `failed (exit N)`, 9 `cancelled`) are not the same as the `jobs[].status`.
- A killed agent has XML `status="cancelled"` plus `<abort-reason>Cancelled by caller…`, but `jobs[].status:"failed"`.
- Older files omit `status` or `agentId` in `jobs[]`.
- A follow-up chat run on an idle agent produces another async-result with `jobId:"SubTwo-2-2"` (`<id>-<n>`) and the same `agentId`. Always key on `agentId`, falling back to `jobId`.
- Background `bash` jobs (`bg_N`) use the same message with `type:"bash"`. Filter on `type:"task"`.

### Child header lines (all verified)

Line 1, title slot (fixed width, rewritten in place; see section 4):
```json
{"type":"title","v":1,"title":"","updatedAt":"2026-10-21T14:19:13.489Z","pad":"   …"}
```
Line 2, session header. `parentSession` is the absolute path of the parent's jsonl. For nested agents it is the parent *subagent's* jsonl (e.g. `…/<stem>/M2cCore.jsonl`):
```json
{"type":"session","version":3,"id":"01a15ed4-…","timestamp":"2026-10-21T14:19:13.489Z","cwd":"/home/…/OmpDeck","parentSession":"/home/…/sessions/-PersProject-OmpDeck/2026-10-21T05-19-…jsonl"}
```
Then `model_change` (`model`, `resolvedModelIsFallback`, optional `role`), `thinking_level_change`, and:
```json
{"type":"session_init","id":"62bd8db5","parentId":"fff549c6","timestamp":"…","runtime":{"version":"18.12.3"},
 "systemPrompt":["…","…","…"],"task":"Complete assignment thoroughly:\n\n# Questions …",
 "tools":["read","grep","bash","eval","web_search","wait"],"agent":"deck-api-scout",
 "resolvedModel":"anthropic/claude-sonnet-5-5:medium","readOnly":false,"spawns":""}
```
- `modelRole` is optional, e.g. `"slow"` (5 of 47).
- `spawns` is `""`, a list name, or `"*"`.
- `systemPrompt` can be large; skip it.

The first `user` message is the assignment, with `attribution:"agent"`. Hub row text is the task with the preamble `Complete assignment thoroughly:` stripped (regex `^Complete the assignment below,\s*thoroughly:\s*` in persisted-agents.ts; real prompts use the other wording, so strip both). If a `# Target` section exists it takes only that section, capped at 1000 chars.

### What the Hub derives from the file

From `readPersistedAgentHistory`:
- Walk entries by `id`/`parentId` from the leaf.
- Sum `message.usage` (tokens = input+output+cacheWrite, or `totalTokens` if 0; cost = `usage.cost.total`) over the active-branch assistant messages.
- `tools` is the number of toolCall blocks, `requests` is the number of assistant messages.
- `durationMs` is leaf timestamp minus session timestamp, kind `"span"`.
- `contextTokens` is the latest assistant `usage.totalTokens`.
- `model_usage` entries are not counted by the Hub, though omp's session total does include them.

### Status inference (files only)

| Observed | State |
|---|---|
| `<id>.jsonl.tombstone` exists | **aborted (killed)**. Hub marks it `aborted` after a restart. Sandbox kill: tombstone 0 B, `.md` 0 B, assistant `stopReason:"aborted"` with `errorMessage:"Interrupted by user"`, then `session_exit`. |
| Last `custom` is `session_exit` with `kind:"signal"|"fatal"|"process_exit"` | **interrupted**: the omp process died or was killed. Contains `pendingToolCalls[]`. Sandbox SIGHUP: reason `sighup`, then synthetic `toolResult isError:true` and `assistant stopReason:"aborted"` appended after the exit marker. Treat as aborted. |
| `session_exit` `{reason:"dispose",kind:"normal"}` | session disposed, so it is **parked or done**. Then use the final message below. |
| No `session_exit`, last `message` is `user`, `toolResult`, or assistant `toolUse`, or a `tool_execution_start` has no matching `toolResult` by `toolCallId` | **running** |
| No `session_exit`, last message is assistant `stopReason:"stop"` (or the `yield` toolResult), and `<id>.md` exists with size > 0 | **idle** (finished, still live; accepts follow-ups). Sandbox: `.md` written at completion, no `session_exit` until process exit. |
| Final assistant `stopReason:"error"` (`errorMessage`, `errorStatus`) | **failed**. Real example: 429, `.md` 0 B, parent `status="failed (exit 1)"`. |
| Final assistant `stopReason:"aborted"` without a tombstone | **aborted** (user interrupt or process death) |
| `stopReason:"stop"` and `.md` size > 0 | **completed**, output in `.md` |
| Last entries of a finished `yield` agent | `toolResult yield` with `details:{"data":…,"status":"success"}`; yield error forms are `isError:true` and retry. |
| No `session_exit`, no activity, owning omp gone | **stale**: crashed with SIGKILL |

Corpus cross-check, last message of 1,683 subagent transcripts: yield toolResult 1,219 (success), assistant `stop` 406, `error` 38, `aborted` 22. 1,817 `session_exit dispose/normal`; 7 have none.

Caveats:
- File mtime is not a liveness signal. A running `bash` of 14 s left the file untouched from 13:03:51 to 13:04:07. Use pending tool calls instead.
- Staleness check: `omp collab list --json` gives `{pid, sessionId, busy, cwd}` per live omp process, and `kill -0 <pid>` works too.
- An error followed by an auto-retry may look like `error` while still running [UNVERIFIED]. Verify with a forced 429 against the fake server.
- `.md` content is the agent's final text/yield. Do not use its mtime for ordering; a follow-up run rewrites it.

## 4. Live-update rules

- **Append-only with `O_APPEND`.** `FileSessionStorageWriter` (`session-storage.ts:207+`) keeps an fd and writes each entry synchronously. Sandbox entries appeared at their own timestamps (`tool_execution_start` before the tool ran).
- **Whole entries only.** Assistant messages are written at message end, with no streaming deltas. Expect gaps during long generations.
- **Title slot.** Line 1 is a fixed 256-byte `title` record (`SESSION_TITLE_SLOT_BYTES = 256`, session-title-slot.ts) overwritten in place when the title changes. Skip it when tailing; it never changes the file's length.
- **Atomic rewrite fallback (inode change).** On migration or repair, branch/move/fork and disk-failure recovery, `session-manager.ts` (`#rewriteRequired`, `#rewriteAtomically`) writes a new file and renames it. Tailers must detect it.
- **omp's own tail algorithm** (`agent-transcript-viewer.ts:86-381`, reusable):
  1. `POLL_MS = 250`, `stat` per poll.
  2. If dev and ino are unchanged, size is not smaller, and the 4 KiB sentinels (at start, middle and end of the previously read range, `SENTINEL_BYTES = 4096`) still match, read only the new bytes from `offset`.
  3. Keep a `pending` partial last line (no trailing `\n`) and join it on the next read.
  4. On any mismatch, full reload.
- Between polls the file can lag by one write; there are no partial JSON lines other than the unterminated tail.
- **New agents.** Appear when `<stem>/` gains a `.jsonl`. Also rescan nested dirs (`<stem>/<Id>/`).
- **Lifecycle events** show up as entries:
  - `tool_execution_start` (`data: {toolCallId, toolName, args:{command|path}, intent, startedAt}`)
  - `session_exit` at dispose
  - the `.md` appearing
  - the tombstone appearing
- **Parent notifications** (`async-result` custom_message) arrive within milliseconds of the child's final message.
- **No per-agent registry file under `~/.omp`:**
  - `run/collab-hosts/<id>.json` plus a `.sock` per running omp: `{version, instanceId, pid, endpoint, createdAt, token}`. The `token` is a secret; never log it or index it.
  - `run/daemons/*` are broker/service pids.
  - `agent/terminal-sessions/<pts-N>` is a 2-line breadcrumb: cwd, then current session file.
  - None of them list agents.
- **Safe process-level metadata:** `omp collab list --json` returns `{instanceId, generation, pid, sessionId, sessionName, cwd, model, startedAt, participants, relayConnected, inputRequired, busy, access}`. It has no URLs, tokens or agents. Use it to find the live process for a session id.
- **Only live source of `running|idle|parked|aborted`** is the collab `agents` frame: `AgentSnapshot{id, displayName, kind:main|sub, parentId, status, hasSessionFile, createdAt, lastActivity}`, debounced (`collab/host.ts:1026-1053`).

## 5. Actions without a collab guest

| Action | Feasible externally? |
|---|---|
| Read transcript, usage, output, lineage | Yes, from files (sections 2–3). |
| Kill | Only by injecting Hub keys (below), or collab `agent-cmd{cmd:"kill"}` (`guest.ts:205-217`, `host.ts:688`, needs a control link from `omp collab link`). |
| Chat or steer | Same: collab `agent-cmd chat`, or typing into the Hub viewer or focused agent. |
| Revive | Same (`ensureLive`). Only parked agents. |
| CLI | None. `omp ps` lists `daemon-supervised background processes (logs, stop, kill, restart)`, not agents. `omp agents` manages bundled agent definitions. `--restore` resumes a crashed session with its subagents but is a startup flag. |
| Writing files | Not an action channel. Creating a tombstone file would only change what a future scan shows; the live process does not watch it. [INFERENCE] |

**Injection sequence that worked (omp 18.12.5, tmux, default keybindings).** Preconditions are an idle composer with no overlay open and the default `Alt+A`.

1. `ESC a` (Alt+A) opens the Hub.
2. `/` starts the roster filter.
3. Type the exact agent id.
4. `Enter` ends filter editing. The first match is selected (rows are ordered by recency, and the filter is a substring match).
5. `x` kills the selected agent.

Observed result:
- Tombstone written.
- `.md` empty.
- Parent `async-result` with `status="cancelled"`.
- Header changed to `⏹ 1 aborted`.
- The other running sibling (`SubOne-3`) was untouched.

`Esc` closes the Hub: once if no filter is active, twice with one (the first clears the filter, then the second closes). Check the header counts before sending `x`.

Risks, observed:
- **`x` is immediate and irreversible, and there is no confirm.** It kills whichever row is selected. The filter is substring-based: `SubOne` matches `SubOne`, `SubOne-2`, `SubOne-3`, and the first match wins. Verify the selected row on the grid before pressing `x`.
- **Enter on a row is not harmless.** In one run, Enter after typing the filter opened the in-hub transcript viewer instead of just committing the filter. Cause not isolated [UNVERIFIED]. It did happen in the first attempt, right after a previous kill.
- **Stray keystrokes become chat.** The viewer has an input line. My `x` was typed into it. A later `/SubTwo` + Enter was sent to the agent as a real prompt, which restarted a finished agent and changed its roster description to `/SubTwo`. `Esc` in the viewer clears the input first, then closes it.
- **Overlay already open or composer non-empty.** Keys go to the wrong widget; follow the guard guidance in `docs/notes/omp-actions.md` §2 item 8.
- Users can remap `app.agents.hub`; injection must read `keybindings.yml`.
- Kitty/CSI-u encoding for Alt+A is [UNVERIFIED].

**Revive** is `r` on a parked row. Parked rows only exist after a restart or resume (persisted-agents registers them as `parked`). The sandbox never reached that state, so `r` was not exercised [UNVERIFIED]. The source gate is `agent-hub.ts:2493-2500`.

## 6. Render shapes for a native transcript viewer

Every non-header line is `{type, id, parentId, timestamp, runtime:{version}, …}`. All timestamps are ISO strings, except `message.timestamp`, which is epoch ms. Order follows file order; follow `parentId` only if you need branch handling. The Hub shows the leaf branch.

**Entry type counts** (46 OmpDeck subagents): `message` 6046, `custom` 3385 (`tool_execution_start` 3294, `system-prompt-digest` 47, `session_exit` 44), `model_usage` 70, `custom_message` 18, `compaction` 6, `ttsr_injection` 2, `credential_pin` 46, plus the headers.

1. **User** (assignment or steer)
   ```json
   {"type":"message","message":{"role":"user","content":[{"type":"text","text":"Complete assignment thoroughly:\n\n# Target…"}],"attribution":"agent","timestamp":1792592353608}}
   ```
   - `attribution` is `"agent"` for the assignment and `"user"` for chat typed in the Hub (`"/SubTwo"` in the sandbox).
   - A steer from another agent carries `"steering":true` and text like `[Wait interrupted by message]\n<irc from="parent" agent="Main">\n…markdown…\n</irc>`. OmpDeck splits every `<irc>` block out into its own IRC row (`TranscriptItem::Irc`: `from`, `agent`, body, the bracketed line as a note); text around the blocks stays a user row.
2. **Assistant**, content blocks `thinking`, `text`, `toolCall`:
   ```json
   {"message":{"role":"assistant","content":[
     {"type":"thinking","thinking":"I should be efficient but thorough…","thinkingSignature":"CAQSgQYK…"},
     {"type":"text","text":"…"},
     {"type":"toolCall","id":"toolu_01Q9n…","name":"read","arguments":{"path":"crates/deck-core/src/status.rs:50-130"},"intent":"Reading status"}],
    "api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-5-5",
    "usage":{"input":4,"output":157,"cacheRead":9579,"cacheWrite":2789,"totalTokens":12529,"cost":{"input":…,"output":…,"cacheRead":…,"cacheWrite":…,"total":0.0091167}},
    "stopReason":"toolUse","timestamp":…,"responseId":"…","duration":…,"ttft":…,"completedAt":…}}
   ```
   - `stopReason` values: `toolUse`, `stop`, `error`, `aborted`.
   - Error and aborted messages also carry `errorMessage`, and sometimes `errorStatus` and `stopDetails`.
   - `intent` on a toolCall is optional (seen on `bash`, `edit`, `wait`).
   - `thinkingSignature` can be a long opaque string; skip it.
   - OpenAI-style thinking has `"thinkingSignature":"reasoning_content"`.
   - Aborted messages can have `content:[]`.
3. **ToolResult**
   ```json
   {"message":{"role":"toolResult","toolCallId":"toolu_01W…","toolName":"bash","content":[{"type":"text","text":"…"}],
    "details":{"wallTimeMs":10078.98,"timeoutSeconds":900},"isError":false,"timestamp":…}}
   ```
   - `details` varies per tool:
     - `read`: `displayContent{text,startLine}`, `summary`, `fileSize`
     - `edit`: `diff`, `op`, `path`, `oldText`, `newText`, `firstChangedLine`
     - `write`: `resolvedPath`
     - `grep`: `matchCount`, `files`, `searchPath`
     - `eval`: `cells[{code,output,status}]`
     - `web_search`: `response{answer,sources}`
     - `wait`: `jobs[]`
     - `yield`: `{data, status:"success"}`
     - `task`: see section 3
   - Images appear as `{"type":"image","data":"blob:sha256:2465…","mimeType":"image/png"}`. The bytes live in `~/.omp/agent/blobs/<sha256>`; `details.mediaPath` may point to the original file.
   - Large outputs may be truncated with the full copy in `<n>.<tool>.log` artifacts [INFERENCE, from artifact files].
   - Synthetic results (`details:{"__synthetic":true,"source":"assistant_stop_error","executed":false}`) mark calls that never ran.
4. **`custom` marker entries** (not shown as chat):
   - `tool_execution_start`: `data:{toolCallId,toolName,args?:{command|path},intent?,startedAt}`. Use it for the live "Current tool" row.
   - `system-prompt-digest`: `data:"469bcc78…"` (hide).
   - `session_exit`: `data:{reason,kind,recordedAt,pendingToolCalls?}`.
5. **`custom_message`** with `display:true`, e.g. `irc:incoming`:
   ```json
   {"type":"custom_message","customType":"irc:incoming","content":"<irc>\nIncoming IRC message from agent `CoreOmp`:…","display":true,"details":{"id":"…","from":"CoreOmp","message":"…"},"attribution":"agent"}
   ```
   - `async-result` (section 3) renders as a notice. Its `<system-notice>` / `<task-result …>` / `<output>` wrappers render as captioned subsections (`deck_core::agents::markup`: a wrapper is a lowercase tag alone on its line, closed by `</name>` alone on a later line, outside fenced code).
   - `irc:*` notices go through the same `<irc>` split as steers; the "Incoming IRC message from agent `X`:" line becomes the agent.
   - The Hub's own renderer handles `display:false` by hiding.
6. **Message roles `developer` and `custom`:**
   - `{"role":"developer","content":[{"type":"text","text":"<system-reminder reason=\"rule_violation\" rule=\"rs-lazylock\"…"}]}` (rule injections).
   - `{"role":"custom","customType":"eval-state-context","content":"<system-reminder>…","display":false}`.
7. **Bookkeeping, mostly hidden:**
   - `model_change` (switches the displayed model)
   - `thinking_level_change`
   - `credential_pin{provider,hash}`
   - `ttsr_injection{injectedRules}`
   - `model_usage{purpose,provider,model,usage,…}` (side-model call: counts toward cost, not context)
   - `compaction{summary,firstKeptEntryId,tokensBefore,tokensAfter,method,details}`
8. **Tolerate unknown `type` values and malformed lines.** omp skips them (`visitEntriesFromFileStream`).

**Reference collapse points.** Tool result bodies, thinking, and `systemPrompt` are big, so collapse them by default, as the Hub does (`Ctrl+O` expand).

**Model chip.** Latest `model_change.model` (may carry an `@upstream` route), or `session_init.resolvedModel` (with `:thinking` suffix); fall back to the first assistant `provider/model`.

## 7. Open items

- Parked/revive path: needs a restart of the sandbox omp with `--resume` (or `--restore`) to see parked rows and `r`.
- Retry state: a forced 429 on a live child, to see what the file shows while auto-retrying.
- Why Enter after the filter opened the viewer once (observed once).
- The kitty/CSI-u form of Alt+A.