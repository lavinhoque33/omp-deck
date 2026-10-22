## Summary

An extension loaded with `omp -e <file.ts>` on omp 18.12.5 can reach the live `AgentRegistry`, the `AgentLifecycleManager` and the `IrcBus`. With them it can kill, chat to and revive subagents in-process. All of it was run for real in a sandbox against live subagents. Recommendation: use a CLI-loaded `-e` extension that serves a unix socket, and drop the collab guest for agent control.

Sandbox: `PI_CODING_AGENT_DIR` and `PI_CONFIG_DIR` under `/tmp`, tmux socket `-L omp-deck-test-ctl`, a fake OpenAI-compatible server on port 18931 that answers with a `task` tool call to spawn 3–4 subagents (two SLOW, one QUICK, one running `bash sleep 25`), and a hand-written local collab relay. All of it is torn down, including the `/tmp/ompctl` dir, the tmux socket and all processes. `/tmp/ompx` was already there with 38 older files that aren't mine, so I left it alone. I wrote nothing under `~/.omp`.

## Q1. What the extension gets, and what is reachable (all observed)

**Context and events.** The factory receives `api: ExtensionAPI` (`extensibility/extensions/types.ts:1354`).
- `api.on(event, (ev, ctx))` is typed at `types.ts:1378-1446`. `ctx: ExtensionContext` is at `types.ts:483-593`.
- `ctx.agent` is `{kind:"main"|"sub", id, name, depth, parentId?}` (`types.ts:464-480`). Observed: `{"kind":"main","id":"Main","name":"main","depth":0}`.
- `ctx.sessionManager.getSessionFile()`, `ctx.isIdle()` and `ctx.abort()` exist.
- `ctx.setInterval/setTimeout` are the contained timers, cleared on `session_shutdown` (`types.ts:547-555`).
- `api.events` is an EventBus. The extension can subscribe to `"task:subagent:progress"` and `"task:subagent:lifecycle"`.
  - The channel names are at `packages/tui/src/overlays/session-observer-registry.ts:4,7`.
  - Observed progress payload keys: `index, agent, agentSource, task, parentToolCallId, detached, assignment, progress{currentTool,…,status}, sessionFile`.
  - Lifecycle payload: `{id, agent, parentToolCallId, detached, agentSource, status:"started", sessionFile}`.

**Handles reachable without any hack:**

| Handle | Route | Evidence |
|---|---|---|
| `AgentRegistry` | `api.pi.AgentRegistry.global()` (`api.pi` is the whole package index, 699 keys, because `loader.ts:29` imports `../../index` and `index.ts:40` does `export * from "./sdk"`). Also a static import. | `static import ok; same as api.pi? true` |
| `AgentLifecycleManager` | `import { AgentLifecycleManager } from "@oh-my-pi/pi-coding-agent/registry/agent-lifecycle"` | `OK AgentLifecycleManager` |
| other bundled modules | `registry/agent-registry` → `AgentRegistry, MAIN_AGENT_ID, isResumable`; `registry/persisted-agents` → `ensurePersistedRoster, isCurrentSessionRosterRef, registerPersistedSubagents, sessionFileBelongsToRoot` | observed |
| `IrcBus` | not in the specifier map (`@oh-my-pi/pi-coding-agent/irc/bus` → `Cannot find package`). Reachable via `(await import("@oh-my-pi/pi-coding-agent/modes/agent-hub-runtime")).createAgentHubRuntime().irc`: `irc: object send=function unread=function`. I did not exercise `send`. | — |
| `globalThis` | holds no omp handles (only `__ompLegacyPiRequireGraphModule`) | observed |

**Module-resolution rules, because this is where it gets fragile:**
- The binary has a bundled specifier map of about 1208 `@oh-my-pi/pi-coding-agent/<path>` modules (the strings dump has the line `"@oh-my-pi/pi-coding-agent/registry/agent-lifecycle": $b`).
- Static imports and literal dynamic imports (`await import("@oh-my-pi/...")`) work, even from `/tmp` with no `node_modules`.
- Dynamic imports with a variable specifier fail: `Cannot find package '@oh-my-pi/pi-coding-agent'`.
- A failed literal dynamic import is catchable. A failed static import makes omp print `Failed to load extension: …`.

**Hub action semantics (source):**
- Kill (`packages/tui/src/overlays/agent-hub.ts:2546-2548`): `ref.session.abort({reason})` if running, then `lifecycle.release(id, ref, {tombstone:true})`.
- Chat (`agent-transcript-viewer.ts:572-574`): `(await lifecycle.ensureLive(id)).prompt(text, {streamingBehavior:"steer"})`.
- Revive: `ensureLive(id)`.
- `AgentRef` fields are at `registry/agent-registry.ts:70-87`.

**Experiments, all through a unix-socket client driving the extension below:**

| Op | Result |
|---|---|
| (a) list | `Main running; QuickOne idle streaming=False; SlowTwo running True; SlowOne running True; ToolOne running activity="running bash"`. Status values seen: `running, idle, parked, aborted`. |
| (a) kill SlowOne (running) | reply `{ok:true, released:true}`. The agent became `aborted` with `live=false`. A `SlowOne.jsonl.tombstone` file was created. The fake server saw `ConnectionResetError`, so the stream really was aborted. The main transcript showed `Background job completed [task] SlowOne` (same as the Hub). |
| (b) chat to an idle agent | the fake server received the user message `PING hello idle`. |
| (b) chat to a running agent | steered. The fake server received `<system-notice> User interjection during work: priority; supersedes…`. |
| (c) park, then revive a parked agent | `park` gave `parked`, `ensureLive` gave `idle`. `chat` to a parked agent woke it (`statusBefore:"parked"` → `idle`, message delivered). |
| revive after a fresh process | after `--resume`, persisted subagents appeared as `SlowTwo parked`, `QuickOne parked`, `SlowOne aborted` with no Hub opened. `revive QuickOne` → `idle`. `chat` to parked SlowTwo woke it, and a following `kill` returned `released:true`. |

Safety:
- Kill, chat and revive on `Main` or an unknown id returned `no such subagent`.
- Because the registry is process-global it keeps the agents of earlier sessions. After `/new` the global list still held old agents. Scoping with `sessionFileBelongsToRoot(ref.sessionFile, rootSessionFile)` (what the Hub does) gave `agents: []` for the new session.

## Q2. Local control endpoint

**Endpoint: works.**
- `Bun.listen({unix: <0700 dir>/ctl.sock})` ran inside omp.
- The socket was created `srwxr-xr-x` inside a `drwx------` dir, so the directory permission is the gate.
- It pushed snapshots on registry change plus a 1 s tick. 3 pushes arrived in 1.2 s while agents were streaming.
- A snapshot carries status including `parked`, `activity`, and `streaming`. `activity` was `running bash` for the tool-using agent. `api.events` `task:subagent:progress` gives richer data (`progress.currentTool`).
- On `/quit`, the `session_shutdown` handler stopped the server and removed the socket (`ls run` empty afterwards). After SIGKILL a stale socket would remain, so OmpDeck must treat a connect failure as "no endpoint".

**User-visible side effects: none found.**
- The startup banner, status line, footer and transcript were identical to a run without the extension.
- The only transcript lines were omp's own `Background job completed` notices when a kill or a finish happens, exactly as the Hub produces.
- Startup cost: `omp -p hi`, 3 runs each, `noext 0.78 / 0.79 / 0.83 s` vs `ext 0.81 / 0.82 / 0.85 s`, about +30 ms.
- The factory also runs once per subagent session. The log showed `factory re-entered` for each subagent and for each revived one, so a process-wide guard is required.

**`-e` with `--resume <path>`: works.** `omp -e ctl.ts --resume <jsonl>` loaded the extension and started with `sessionFile` equal to the resumed file.

**Session switches inside the process:** the extension survives them and receives `session_switch` events (`ev: {type:"session_switch", reason, previousSessionFile}`; `ctx.sessionManager.getSessionFile()` is the new file).
- `/new` → `reason:"new"`
- `/fork` → `reason:"fork"`
- `/resume <id>` → `reason:"resume"`
- The factory is not re-run, `session_start` is not re-fired, and the socket stayed up.

**`/restart`:**
- It re-execs: `session_shutdown`, then a fresh `factory` and `session_start`.
- The pid is unchanged (exec) and `-e` and `--resume` are retained, so the socket is rebuilt automatically.
- A client must reconnect.

**Loading for OmpDeck-launched tabs without touching user config:** yes.
- Use `omp -e <path>` (`docs/cli-reference.md:152`; `--hook` is an alias, `:153`; `docs/hooks.md` says `--hook` is merged into `additionalExtensionPaths`, and `--hook <file>` also loaded my probe).
- Pass the socket dir through an env var set per tab (`OMPDECK_CTL_DIR`). OmpDeck can use `<XDG_RUNTIME_DIR>/ompdeck/<tabid>`, so no pid discovery is needed. In my run it was derived under `XDG_RUNTIME_DIR` too.
- `-e` is additive: the user's own extensions still load unless `--no-extensions` is passed. `--trusted-extension` cannot be combined with `-e`.
- OmpDeck must materialize the `.ts` onto disk, for example in its data dir.

## Q3. Other in-process routes (not needed if Q1 is adopted)

- **Bun inspector: works in the compiled binary.**
  - `BUN_INSPECT=127.0.0.1:19339/ompx omp` made `ss -ltnp` show `LISTEN 127.0.0.1:19339 users:(("omp",…))`.
  - A WebSocket client connecting to `ws://127.0.0.1:19339/ompx` and sending `Runtime.evaluate` got a result (`typeof Bun` = `object`, the pid).
  - `AgentRegistry` is not a global (`typeof AgentRegistry` = `undefined`), so using it would mean reaching the modules through `__ompLegacyPiRequireGraphModule` or module internals. I did not explore that.
  - Security: it is unauthenticated arbitrary code execution for anything that can connect to that TCP port, guarded only by the path string. Don't use it. `--inspect` as a CLI flag was not tested [UNVERIFIED].
- **`--hook`:** an alias of `--extension` (above), so it gains nothing.
- **`--plugin-dir` / plugins:** [UNVERIFIED]. Plugin extension paths go through the same loader (`loader.ts:38`, `getAllPluginExtensionPaths`). I did not run a plugin.
- **Registry endpoint**: omp's own collab host registry socket (`omp collab list`) only answers `{"op":"snapshot"}`-style discovery (per `docs/collab.md:89`). It is not a control channel.

## Q4. Collab join notice

- **Source:** `packages/coding-agent/src/collab/host.ts:786` does `emitNotice("info", "<name> joined the collab session", "collab")` and `:986` does `"… left the collab session"`. `emitNotice` (`session/agent-session.ts:2941`) is documented as UI-only: "Notices are NOT added to agent state and never reach the LLM". It also drives `#updateStatusSegment()` (the footer shows `⇄ collab:N`).
- **Experiment:**
  - A host with `collab.autoStart=control` was run against the local relay.
  - A guest joined through `omp join <link>`.
  - The host screen showed ` collab: axeleratorx joined the collab session` and the footer went `⇄ collab:2`.
  - After the guest left it showed `… left the collab session` and `⇄ collab:1`.
  - The session `.jsonl` contained **0** occurrences of `joined the collab` and 0 of `left the collab`. Its entry types were only `title, session, model_change, thinking_level_change, message, custom (system-prompt-digest), message, title_change`.
  - **The notice is display-only, not persisted.**
- **Silent join:** no. The guest `hello` frame is `{t:"hello", proto, name, writeToken?}` (`packages/wire/src/index.ts:330-339`). There is no silent or observer flag. The host always emits the notice and updates the participant count. Read-only guests are also announced, as `(read-only)`.

## Recommendation

- **Channel:** a CLI-loaded `-e` extension with a per-tab unix socket.
  - It is in-process, with no relay and no transcript download.
  - It has no join or leave notices and no keystroke injection.
  - It sees exactly what the Hub sees, including parked agents.
  - It survives `/new`, `/fork`, `/resume` and `/restart`.
- **Degradation:** if the extension fails to load or an internal import fails (specifier names can change between omp versions), it returns without opening a socket. OmpDeck then falls back to the existing jsonl and injection paths.
- **Pin the specifiers:** `registry/agent-lifecycle`, `registry/agent-registry` and `registry/persisted-agents` are internal API, so re-check them on each omp upgrade.

**Protocol (NDJSON, one request per line):**
- Server → client, on connect: `{"t":"hello","v":1,"pid":…}` then a snapshot.
- Snapshot: `{"t":"agents","pid","sessionFile","mainIdle","agents":[{id,name,kind,parent,status,activity,live,streaming,createdAt,lastActivity,sessionFile,model,metrics}]}`.
- Pushes use the same shape on registry change (50 ms coalesced), on `task:subagent:progress`, and every 1 s while a client is connected.
- Client → server: `{"rid":N,"t":"list"|"kill"|"revive"|"chat","id":"<AgentId>","text":"…"}`.
- Replies: `{"t":"reply","rid":N,"ok":true,…}` or `{"ok":false,"error":"…"}`. `list` replies with `snapshot` inside the reply.

## Extension source (final version, throwaway, deleted)

The sandbox regression ran this exact file. Output: agents listed, `kill SlowOne` → `released:true`, `chat` to idle and to running → `ok:true`, `kill Main` / `kill nope` → `no such subagent`, and after `/new` the list was `[]`.

```ts
// ompdeck-ctl.ts — OmpDeck control extension. Load with: omp -e <path> (env OMPDECK_CTL_DIR selects the socket dir)
import * as fs from "node:fs";
import * as path from "node:path";

const DIR = process.env.OMPDECK_CTL_DIR!;               // OmpDeck creates it 0700 under $XDG_RUNTIME_DIR and passes it per tab
const LOG = process.env.OMPDECK_CTL_LOG;                 // debug only
const log = (s: string) => { if (LOG) fs.appendFileSync(LOG, `${new Date().toISOString()} ${s}\n`); };
const G = globalThis as any;                             // one listener per process (factory runs for main + every subagent session)

export default async function ompdeckCtl(api: any) {
  if (!DIR) return;                                      // not launched by OmpDeck -> inert
  // Internal modules are bundled in the omp binary; literal specifiers only (non-literal dynamic import fails).
  let AgentLifecycleManager: any, AgentRegistry: any, sessionFileBelongsToRoot: any;
  try {
    ({ AgentLifecycleManager } = await import("@oh-my-pi/pi-coding-agent/registry/agent-lifecycle"));
    ({ AgentRegistry } = await import("@oh-my-pi/pi-coding-agent/registry/agent-registry"));
    ({ sessionFileBelongsToRoot } = await import("@oh-my-pi/pi-coding-agent/registry/persisted-agents"));
  } catch (e) { log("internal import failed: " + e); return; }   // degrade: OmpDeck falls back to jsonl + injection

  api.on("session_start", (_ev: any, ctx: any) => { if (ctx.agent.kind === "main") start(ctx); });
  api.on("session_switch", (ev: any, ctx: any) => { if (ctx.agent.kind === "main") { log(`session_switch ${ev.reason}`); G.__ompdeckCtl?.setCtx(ctx); } });
  api.on("session_shutdown", (_ev: any, ctx: any) => { if (ctx.agent.kind === "main") stop(); });

  function start(ctx: any) {
    if (G.__ompdeckCtl) { G.__ompdeckCtl.setCtx(ctx); return; }
    const registry = AgentRegistry.global();
    const clients = new Set<any>();
    let mainCtx = ctx;
    let pending: Timer | undefined;
    const rootFile = () => mainCtx.sessionManager.getSessionFile?.() ?? null;

    const snapshot = () => {
      const root = rootFile();
      return {
        t: "agents", pid: process.pid, sessionFile: root, mainIdle: mainCtx.isIdle(),
        // The registry is process-global and keeps agents of earlier sessions (/new, /resume): scope to the current root exactly like the Hub.
        agents: registry.list().filter((r: any) => r.kind !== "main" && r.sessionFile && root && sessionFileBelongsToRoot(r.sessionFile, root)).map((r: any) => ({
          id: r.id, name: r.displayName, kind: r.kind, parent: r.parentId ?? null, status: r.status,   // running|idle|parked|aborted
          activity: r.activity ?? null, live: !!r.session, streaming: r.session?.isStreaming ?? null,
          createdAt: r.createdAt, lastActivity: r.lastActivity, sessionFile: r.sessionFile,
          model: r.history?.resolvedModel ?? null, metrics: r.history?.metrics ?? null,
        })),
      };
    };
    const push = (m: unknown) => { if (!clients.size) return; const l = JSON.stringify(m) + "\n"; for (const c of clients) try { c.write(l); } catch {} };
    const schedule = () => { if (pending) return; pending = setTimeout(() => { pending = undefined; push(snapshot()); }, 50); };

    async function handle(cmd: any): Promise<any> {
      if (cmd.t === "list") return { ok: true, snapshot: snapshot() };
      const lifecycle = AgentLifecycleManager.global();
      const ref = registry.get(String(cmd.id));
      if (!ref || ref.kind !== "sub") return { ok: false, error: "no such subagent" };
      switch (cmd.t) {
        case "kill":   // == Hub `x`
          if (ref.status === "running" && ref.session) await ref.session.abort({ reason: "OmpDeck kill" });
          return { ok: true, released: await lifecycle.release(ref.id, ref, { tombstone: true }) };
        case "revive": // == Hub `r`
          await lifecycle.ensureLive(ref.id); return { ok: true, status: registry.get(ref.id)?.status };
        case "chat": { // == transcript-viewer input line: revives parked, steers running, prompts idle
          const s = await lifecycle.ensureLive(ref.id);
          await s.prompt(String(cmd.text), { streamingBehavior: "steer" }); return { ok: true };
        }
        default: return { ok: false, error: "unknown cmd" };
      }
    }

    fs.mkdirSync(DIR, { recursive: true, mode: 0o700 }); fs.chmodSync(DIR, 0o700);
    const sockPath = path.join(DIR, "ctl.sock"); try { fs.unlinkSync(sockPath); } catch {}
    const server = Bun.listen<{ buf: string }>({
      unix: sockPath,
      socket: {
        open(s) { s.data = { buf: "" }; clients.add(s); s.write(JSON.stringify({ t: "hello", v: 1, omp: process.versions.bun ? "bun" : "", pid: process.pid }) + "\n" + JSON.stringify(snapshot()) + "\n"); },
        close(s) { clients.delete(s); },
        data(s, chunk) {
          s.data.buf += chunk.toString();
          for (let i; (i = s.data.buf.indexOf("\n")) >= 0;) {
            const line = s.data.buf.slice(0, i); s.data.buf = s.data.buf.slice(i + 1);
            let cmd: any; try { cmd = JSON.parse(line); } catch { continue; }
            handle(cmd).then(r => s.write(JSON.stringify({ t: "reply", rid: cmd.rid, ...r }) + "\n"), e => s.write(JSON.stringify({ t: "reply", rid: cmd.rid, ok: false, error: String(e?.message ?? e) }) + "\n"));
          }
        },
      },
    });
    const off = registry.onChange(schedule);                      // registered/status_changed/metadata_changed(activity)/removed
    const tick = setInterval(() => { if (clients.size) push(snapshot()); }, 1000);  // safety net for isStreaming/mainIdle
    api.events.on("task:subagent:progress", schedule);            // payload.progress.currentTool/recentOutput/tokens (richer than registry.activity)
    G.__ompdeckCtl = {
      setCtx(c: any) { mainCtx = c; schedule(); },
      close() { off(); clearInterval(tick); server.stop(true); try { fs.unlinkSync(sockPath); } catch {} },
    };
    log("listening " + sockPath);
  }
  function stop() { G.__ompdeckCtl?.close(); G.__ompdeckCtl = undefined; }
}
```

## Caveats and unverified items

- The `ctx.models`, `lifecycle.park()` and `registry.get()` calls used in the experiment are not guarded against omp version drift. The `park` command was only in the earlier experimental version, not the final file, and only used to produce a parked agent for testing.
- `IrcBus.send` via `createAgentHubRuntime().irc` is reachable but not exercised. The Hub itself does not use it for chat (it uses `session.prompt`).
- The final extension's `api.events.on("task:subagent:progress", …)` registration is not removed on shutdown. Harmless in my runs, but worth an `off` on `stop()`.
- `registry.get("Main").status` showed `running` while the main session was idle, so don't use the registry for the main agent's busy state. Use `ctx.isIdle()` (`mainIdle` in the snapshot), which I did not cross-check against a real streaming turn.
- A running agent's `streaming` was `true` and `activity` stayed `null` unless the agent used a tool or set an intent. `activity` was `running bash` only for the tool-using agent.
- Not tested: plugins/`--plugin-dir`, `--inspect` as a flag, `/move` and `/wt` session switches, and extension behaviour if the user's real config also loads extensions that register the same events.