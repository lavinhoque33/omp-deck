# Session tree, branch switch and fork-from-node in omp (source 18.4.10, bundle 18.12.6)

**Version:** `omp --version` prints `omp/18.12.6`, not 18.12.5. I checked the source (18.4.10, cited as `path:line`) against the bundle (`strings ~/.local/bin/omp`, cited as "bundle"). Both were taken from the same files at the time.

**Recommendation:** Do not use the CLI for fork-from-node. `--fork` always copies the whole file. Add three ctl-socket ops to the existing extension, which I proved in a sandbox:
- `tree`
- `fork_at` (opens a second `SessionManager` on the file and calls `createBranchedSession`; the running tab is untouched)
- `navigate` (calls `extensionRunner.createCommandContext().navigateTree`; the TUI re-renders)

## 1. Tree model and `/tree` labeling

### Leaf computation

- **18.4.10 source:** every inserted entry becomes the leaf, so the leaf is the last physical entry (`session/session-manager.ts:520-540`, `insert()` sets `#leaf = entry.id`).
- **18.12.6 bundle (new):** entries can carry `activeLeafId`.
  - The rule is `A$(e) = e.activeLeafId !== undefined ? e.activeLeafId : e.id`.
  - `insert` sets `leaf = A$(entry)`.
  - Load rule: `ET(entries)` takes the last entry `o`. Let `a = A$(o)`.
    - If `a === null`, `a === o.id`, or `a` exists in the file, the leaf is `a`.
    - Otherwise it is `vee(entries)`, the last entry with `A$(e) === e.id`.
  - `rebuild` applies the same fallback.
  - Real files use it on off-branch `model_usage` entries. Example from a real subagent file: `{"type":"model_usage","parentId":"46319469","activeLeafId":"11e90257"}`.
  - `appendMessageToBranch` also writes it unless `preserveActiveLeaf:false`.
- **Only the last line matters.** `label`, `branch_summary` and `reset_boundary` entries are not special: if one is last, it is the leaf. A `label` entry's `parentId` is the previous leaf.
- **Non-entry lines:** the first line is `{"type":"title","v":1,...,"pad":...}` and the second is the `session` header. Neither has an `id`, so neither is a tree node.
- **A plain `/tree` jump is not persisted.** `navigateTree` calls `SessionManager.branch(id)` or `resetLeaf()` (`session-manager.ts:3556-3563`). Both only call `setLeaf` in memory.
  - `session-manager.ts:3573-3580` says: "the loader reconstructs the active branch from the last physical journal entry, so changing the in-memory leaf alone is lost on reload".
  - It becomes durable on the next append, since `parentId = leaf`.
  - It also becomes durable on a signal exit. I observed omp append `{"type":"custom","customType":"session_exit","data":{"reason":"sighup"...},"parentId":"<in-memory leaf>"}` on kill.
  - A SIGKILL or crash loses it.
  - **A file-only reader therefore cannot know the live leaf. Take the live leaf from the ctl `tree` op (`ctx.sessionManager.getLeafId()`).**
- **Only `branch_summary` is a durable jump.** `branchWithSummary(branchFromId, summary, details, fromExtension)` (`:3598-3613`) sets the leaf and appends `{type:"branch_summary", parentId:<target>, fromId, summary, details, fromExtension}`.
- **Child order:** `tree()` sorts children by ascending timestamp (`:460`, `:607-640`). It handles `parentId` null, a missing parent, and self-parent by turning the node into a root, so there can be several roots.

### `/tree` selector

Source: `packages/tui/src/overlays/tree-selector.ts`. The compiled strings match (`[compaction: `, `[branch summary]: `, `[model usage: `, `[label: `, filter tabs).

**Active path:**
- Marker: `tree-selector.ts:831-837`. Rows on the active path get the bullet `theme.md.bullet` before the text.
- Active path itself: `:263-284`, walked from `currentLeafId` up through the parents.
- Within each sibling list the child containing the leaf comes first (`#orderedChildren`, `:350-364`). Roots containing the leaf also come first.
- Initial selection is the leaf (`:268-274`).
- `[label] ` (warning colour) is the `label` entry's text for that target (`:834`).

**Rows are a DFS pre-order list** filtered by a predicate. Hidden nodes simply drop out, so their visible descendants lift to the nearest visible ancestor (`tui/src/components/tree-view.ts:501-511`). Depth rule (`tree-selector.ts:251-256`): depth + 1 only when a node has more than 1 child (counting hidden children) or sits under a virtual multi-root.

**Visibility** (`tree-selector.ts:373-440`):
1. An `assistant` message without non-whitespace text is hidden, unless it is the current leaf or its `stopReason` is neither `stop` nor `toolUse`. "Has text" means a `text` block whose trimmed content is not only `.`, `…` or whitespace (`chat/thinking-display.ts:155`).
2. Settings types are hidden in the default view: `label`, `custom`, `model_change`, `model_usage`, `thinking_level_change`, `service_tier_change`, `title_change`, `credential_pin`, `session_init`, `ttsr_injection`, `mode_change`, `reset_boundary`.
3. Filter modes:

   | Mode | Shows |
   |---|---|
   | `default` | everything except the settings types |
   | `no-tools` | `default` minus `toolResult` messages |
   | `user-only` | `isUserRequestEntry`: message role `user`, or role `custom` with `isUserTurnInitiator` (user-invoked skill prompt or collab prompt with user attribution), or a `custom_message` that qualifies the same way (`transcript-entry.ts:59`, `messages.ts:282`) |
   | `labeled-only` | nodes with a label |
   | `all` | everything |

   Mode cycling is Ctrl+O; the Alt+D, Alt+T, Alt+U, Alt+L, Alt+A shortcuts pick a mode directly.
4. Search tokens are fuzzy-matched against the label, role, the first 200 characters of content, and the bash command.

**Row text** (`#entryParts`, `:933-1036`). Whitespace is normalised by replacing `\n` and `\t` with a space and trimming:

| Entry | Text |
|---|---|
| user message | `user: <first 200 chars of text blocks>` |
| developer message | `developer: …` |
| assistant message | `assistant: <text>` |
| assistant with `retryRecovery.status=="recovered"` | `assistant: <note>` |
| assistant without text, error or abort | the error text (first 80 chars), or `(aborted)`, or `(no content)` |
| toolResult | the matching toolCall rendered as `[read: path:off-end]`, `[write: p]`, `[edit: p]`, `[bash: <cmd ≤50>...]`, `[grep: /pat/ in paths]`, `[glob: p]`, `[ls: p]`, or `[name: <args json ≤40>...]`; falls back to `[toolName]` |
| bashExecution | `[bash]: <command>` |
| other roles | `[role]` |
| `custom_message` | `[customType]: <content with one <system-*> wrapper stripped>` |
| `custom_message` advisor | `advisor (<names, severities>): <notes>` |
| `compaction` | `[compaction: <round(tokensBefore/1000)>k tokens]` |
| `branch_summary` | `[branch summary]: <summary>` |
| `model_change` | `[model: <model>]` |
| `model_usage` | `[model usage: <purpose> <role> <provider>/<model>]` |
| `thinking_level_change` | `[thinking: <level or off>]` |
| `custom` | `[custom: <customType>]` |
| `label` | `[label: <label or (cleared)>]` |
| `service_tier_change` | `[service tier: fam:tier ...]`, or `(default)` |
| `title_change` | `[title: <title>]` |
| `mode_change` | `[mode: <mode>]` |
| `credential_pin` | `[credential pin: <provider>]` |
| anything else (`reset_boundary`, `session_init`, `ttsr_injection`) | `[<type with _ replaced by space>]` |

Every field is sanitised first.

## 2. Navigation, `/branch` and `/fork`

### `/tree` select (in place)

`selector-controller.ts:1285-1420` calls `AgentSession.navigateTree(id, {summarize, customInstructions, allowAskReopen:true})` (`agent-session.ts:11289-11640`).

- **Branch summary prompt:** shown when the setting `branchSummary` is enabled. Choices are "No summary", "Summarize" and "Summarize with custom prompt"; Shift+Enter pre-answers "Summarize".
- **Target rules:**
  - If the target is a user request (`isUserRequestEntry`), the new leaf is its **parent**. The message text and images go back into the editor draft. A leaf user message still counts as a real move.
  - If the target is any other `custom_message` that is not a skill prompt, the new leaf is also the parent, with its text going to the editor.
  - Otherwise the leaf lands on the target itself.
  - With `allowAskReopen` a toolResult for an `ask` tool triggers the re-answer protocol. Other callers get a plain leaf move.
  - Navigating to the current leaf is a no-op, unless the target is a user message.
- **What it writes:**
  - With a summary, `branchWithSummary(newLeaf, text, {readFiles, modifiedFiles}, fromExtension)` appends one `branch_summary` entry.
  - Without a summary it appends nothing (in-memory only, see above).
- **Events:** `session_before_tree` (can cancel or supply a summary) and `session_tree {newLeafId, oldLeafId, summaryEntry}`.
- **After a jump:** the agent messages are replaced from `buildSessionContext()`. The TUI then runs `renderInitialMessages({clearTerminalHistory:true})`, `reloadTodos()`, sets the editor draft, and shows "Navigated to selected point".
- **Idle only:** there is no streaming guard (see experiment).

### `/branch` (alias `/rewind`)

`AgentSession.branch(entryId)` (`agent-session.ts:11037-11140`):
- The entry **must be a user message**; otherwise it throws "Invalid entry ID for branching".
- If the user message has a parent, `createBranchedSession(parentId)` moves the running tab to a **new file**. If it has no parent, `newSession({parentSession})` runs instead.
- The old file is kept. The user text goes back to the editor.
- Hooks: `session_before_branch` (can cancel or `skipConversationRestore`), then `session_branch`.

### `/fork`

- `handleForkCommand` (`command-controller.ts:1183`) calls `AgentSession.fork()` (`agent-session.ts:9410`) with no argument.
- It forks the **whole file** via `SessionManager.fork()` (`session-manager.ts:2252`). All entries are kept and the new header has `parentSession = <old session id>`. Artifacts are copied.
- The running process switches to the fork. It refuses while streaming.
- **Bundle (18.12.6, new):** `fork(entryId?, {requireIdle})` at bundle line 803281 forks at a message entry. `Fork(entryId)`:
  - throws `Invalid entry ID for forking` unless the entry is a `message`;
  - extends the cut over the toolResults answering a tool-call batch (`#zr`);
  - calls `#tr("fork", …, {copyArtifacts:true})`, which uses `createBranchedSession(id, {copyArtifacts})`.
  - The TUI `/fork` does not use the `entryId` form.
  - The RPC `fork` command does (changelog #14077).

## 3. Fork from an arbitrary node

### Reachability

- **CLI:** `--fork <path|id>` is hidden from `--help`. It calls `SessionManager.forkFrom(path, cwd, sessionDir, storage, {repairInterruptedTail:true})` (`main.ts:1146-1175`, bundle 819975). It copies **all** entries, and the leaf is whatever the last line says. There is no `--fork-at`, `#entryId` or resume-at (no match for `fork-at` in the bundle). `forkFrom`'s options are `copyArtifacts, suppressBreadcrumb, sessionFile, resetInheritedCost, repairInterruptedTail`, plus `seance` in the bundle. None takes a leaf.
- **Extension API:**
  - The typed `ExtensionContext` has `sessionManager: ReadonlySessionManager` (a `Pick<>` at `session-manager.ts:639`; `createBranchedSession` is not in it).
  - At runtime it is the real instance. The probe showed `main().session.sessionManager === ctx.sessionManager` is true.
  - `ExtensionCommandContext` (`types.ts:603-630`) has `newSession`, `branch(entryId)`, `navigateTree(id,{summarize})`, `switchSession`, `reload`, `compact`. There is no `fork`.
  - Both `createBranchedSession` and `forkFrom` can be reached by literal import, e.g. `await import("@oh-my-pi/pi-coding-agent/session/session-manager")`. The bundle has the specifier at line 879030.
  - `ctx.agent`/event ctx has no command methods.
  - `AgentRegistry.global().get("Main").session` is the live `AgentSession` and has `navigateTree`, `fork`, `branch`, and `extensionRunner`. The bundle specifier for the registry is at line 878950.

### Experiment (sandbox)

Setup:
- `PI_CODING_AGENT_DIR`, `PI_CONFIG_DIR` and a private-mode `XDG_RUNTIME_DIR` under `/tmp`.
- A python fake OpenAI-compatible SSE server on 127.0.0.1:18941 (`models.yml` `api: openai-completions`).
- `startup.setupWizard: false`.
- A no-op `xdg-open` first on `PATH`.
- tmux socket `-L omp-deck-test-treefork` with OmpDeck's `tmux.conf`.
- `omp --model fake/fake-1 -e treeext.ts [--resume f]`, with a unix socket in `TX_DIR` driven by a small python client.

**(a) Fork at a node without disturbing the tab: works.**
- Session: user(A) → `custom` → assistant `da31aca2` → `title_change` → user(B) → assistant `08bfd862` (leaf).
- `{"t":"fork_at","entryId":"da31aca2"}` returned a new file path.
- The tab's `sessionFile` and `leaf` were identical before and after. The new session id and file are different, and no screen change.
- New file contents:
  - Line 1 is `title`. Line 2 is the `session` header: new id, `title`, `parentSession` = the **source file path** (not the id; `forkFrom` uses the id).
  - Then only the path root → `da31aca2`: `model_change`, `thinking_level_change`, user, `custom`, assistant. No siblings, no `title_change`. `label` entries on the path are re-created at the end.
- `omp --resume <newfile> -p "continue from fork"` appended `user` with `parentId:da31aca2`, so the fork resumes correctly.
- Same call **while a turn was streaming** (7 s fake delay): the turn completed normally afterwards.
- I did not test a node in the middle of a tool batch. `createBranchedSession` just slices the path, so the cut batch may be unpaired. `AgentSession.fork(entryId)`'s `#zr` helper exists to avoid that. [UNVERIFIED]
- Extension code for `fork_at`:
  ```ts
  const { SessionManager } = await import("@oh-my-pi/pi-coding-agent/session/session-manager");
  // ... in the op handler, sm = ctx.sessionManager:
  await sm.flush?.();
  const m2 = await SessionManager.open(sm.getSessionFile(), undefined, undefined, { suppressBreadcrumb: true });
  const newFile = m2.createBranchedSession(entryId, { copyArtifacts: true }); // 18.12.6 signature: (leafId, {copyArtifacts}?)
  await m2.flush?.(); await m2.close?.();
  return { newFile };
  ```
  The `open` signature is `(path, sessionDir?, storage?, {throwIfMissing?, initialCwd?, suppressBreadcrumb?})`. `createBranchedSession` throws `Entry … not found` for an unknown id.

**(b) Navigate in place:**
- `AgentRegistry.global().get("Main").session.navigateTree(id)` moves the leaf and fires `session_tree`, but **the screen did not re-render**. It still showed the old branch, and the next prompt hung off the new leaf. Do not use this.
- `main.session.extensionRunner.createCommandContext().navigateTree(id, {summarize})` does the TUI-correct thing, because it is the wiring from `extension-ui-controller.ts:293-308`.
  - It re-rendered the transcript to the new branch and printed "Navigated to selected point".
  - For a user target, the leaf moved to the parent and the editor draft held the user text.
  - It needs no slash-command invocation, and the result is only `{cancelled}`.
  - `leaf` changed in the JSON before/after.
- `createCommandContext().branch(userEntryId)` moved the tab to a new file (leaf = the user entry's parent), printed "Branched to new session", and put the text in the editor.
- `session.fork(entryId)` direct: moved the tab to a new file (leaf = the entry), **but the screen stayed stale**. Not usable from the extension.
- **Persistence:** after a plain navigate then kill-server, the resumed session's leaf was `84b9e3dc`, a `session_exit` whose parent was the navigated leaf. The jump survived only because omp appended `session_exit` on shutdown.
- **Navigating mid-stream was not rejected.** `navigateTree` ran during a streaming turn, returned `cancelled:false`, and the in-flight reply later appeared. Gate navigation in the extension on `ctx.isIdle()`.
- **Headless fork** of a session with no tab: `omp --mode rpc --resume <file>`, then `{"id":"2","type":"fork","entryId":"da31aca2"}` returned `{"cancelled":false}`. A new file appeared with `parentSession` = source path, history up to and including `da31aca2`, and a `session_exit` appended at shutdown. The path is available from `get_state` (not captured). On the source it needs the session lease, so it fails if a live omp holds the file.

**Can OmpDeck write the forked jsonl itself?** Not allowed. Note what omp does so we call it: strip `label` entries from the root path, write header + path + re-created labels, bump to a new session id (`mintSessionId`), file name `<fileSafeTimestamp>_<id>.jsonl` in the session dir, copy the artifacts dir, and write the breadcrumb.

## 4. Performance (read-only)

- `~/.omp/agent/sessions` has 18 project dirs and **2014 `.jsonl` files** (2.6 GB total, including subagent files).
- 110 top-level session files: p50 2.4 MB, p90 24.9 MB, p99 56.9 MB, max 60.9 MB (`…Samsung Playground/…01a14b07….jsonl`: 22,691 lines, max line 233 KB, 5 branch points).
- The largest file is a subagent file at 205 MB (`…StatusForge/…/Drill19A11y.jsonl`, 269 lines).
- Timings are page-cache warm, 12 cores:

  | Scan | 60 MB file (22,691 lines) | 205 MB file (269 lines) |
  |---|---|---|
  | Rust, raw line read | 18 ms | 97 ms |
  | Rust, serde typed `{type,id,parentId,message.role}` (skips content) | 67 ms | 235 ms |
  | Python, regex on the line prefix for `type`/`id`/`parentId` | 60 ms | 410 ms |
  | Python, full `json.loads` | 350 ms | not run |

- Entry types in the 60 MB file: message 14542, custom 7809, custom_message 224, compaction 39, title_change 26, model_change 24, thinking_level_change 20, credential_pin 2, branch_summary 2, model_usage 1.
- **Practical rule:** scan the id/parentId/type/timestamp prefix (the key order is `type,id,parentId,timestamp,…`), and only parse `message`/`custom_message`/`branch_summary`/`compaction` bodies for visible rows. A cold cache will be slower; I did not measure it [UNVERIFIED].
- The live `tree` op avoids this altogether, because omp already holds the index in memory.

## Recommended design

**Sessions with a tab, via the ctl socket** (add to `ompdeck-ctl.ts`; each op carries `root`/session-file the same way as the existing ops and replies `{ok:false,error:"session changed"}` on mismatch):
- `{t:'tree'}` → `{ok, sessionFile, leaf, entries:[{id, parentId, type, ts, role?, activeLeafId?, label?}]}`.
  - Build it from `ctx.sessionManager.getEntries()`, `getLeafId()` and `getLabel(id)`.
  - Add short text previews, built to mirror the table above, so the GUI needs no file read for the running tab.
- `{t:'fork_at', entryId}` → `{ok, newFile}`. Implementation as above, with `flush()` first.
  - Then OmpDeck spawns a new tab with `omp -e … --resume <newFile>`.
  - Guard: the entry exists and is a `message`; otherwise reply `{ok:false,error}`.
- `{t:'navigate', entryId, summarize?:false}` → `{ok, leaf}`.
  - Calls `AgentRegistry.global().get("Main")?.session.extensionRunner.createCommandContext().navigateTree(...)`.
  - Reply `{ok:false,error:"busy"}` unless `ctx.isIdle()`.
  - Do not pass `summarize:true` from the GUI without the model-key check. omp throws "No model available for summarization" or "No API key" otherwise.
- `{t:'branch', entryId}` → the same context's `branch(entryId)` (user messages only, the /branch semantics). Optional.
- A tab that moved root (`/new`, `/resume`) must be detected as the existing ops do.

**Sessions without a tab:** use headless RPC (`--mode rpc --resume <file>` plus `fork`), or open the session in a tab first. For a plain full copy keep `omp --fork <path>` in a new tab.

## Node labels to mirror in the GUI

Use the `/tree` default filter and the table in section 1. Show `[label]` before the text, a bullet on the active path, and put the leaf's branch first. Treat hidden nodes as lifted. For "fork from here" enable only `message`-type nodes (the extension's `entryId` is checked against `getEntry`). For "switch to this branch": a user node moves to its parent, with the text going to the editor.

All throwaway files are gone: `/tmp/ompfork`, `/tmp/ompfork_scan`, `/tmp/ompstr.txt`, the test tmux socket and config; the fake server was killed; nothing was written under `~/.omp` or the project tree.