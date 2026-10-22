# omp status line → external readers (OmpDeck bottom status bar)

Everything below was read from the embedded JS of the installed binary and proven by experiment. Citations: `bundle:<module path>#<symbol>` = `strings -n 6 ~/.local/bin/omp | grep -n '^// packages/<module path>'` (minified names are chunk-local; module path + behaviour is the stable handle), or exact command + observed output.

**Version caveat.** `~/.local/bin/omp` was replaced at 07:50 today: `omp --version` → `omp/18.12.5`. The bundle read and every sandbox run here is **18.12.5**; real sessions on disk were written by 18.12.3 (`"runtime":{"version":"18.12.3"}`) and 18.12.5 lines look the same. Differences already seen between the two: 18.12.3 accepted only 11 segment names (`docs/notes/omp-cli.md` A1.2: `pi,status,model,mode,path,git,pr,subagents,token_in,token_out,token_total`), 18.12.5 accepts 27 (below). **Never hard-fail on an unknown segment/entry type.** Running tabs may still be 18.12.3 [UNVERIFIED: no old bundle left to diff].

Sandbox used: `PI_CODING_AGENT_DIR=/tmp/stw/agent`, a fake OpenAI-compatible provider in its `models.yml` pointing at a local python SSE server (no real provider contacted), tmux socket `omp-deck-test-stw…`. Everything deleted at the end (see "Cleanup").

---------------------------------------------------------------------------------------------------
## 1. The omp status line

### 1.1 Where it is drawn (this decides whether scraping is viable – see §2d)
The status line is a component (`bundle:tui/src/status-line/component.ts`, class `StatusLineComponent`) attached to the **composer**; its placement is set by `composer.shape` (default `band`; **this user has `composer.shape: rail`** in `~/.omp/agent/config.yml` (read-only look), everything else default). Shape registry (`bundle:tui/src/components/composer/*.ts`):

| `composer.shape` | `statusAttachment` | `bottomBar` | status line is… |
|---|---|---|---|
| `band` (default) | `top-band` | none | top border row of the editor (above the input box, moves when the editor grows) |
| `box` | `top-border` | none | top border row of the editor |
| `rule`, `claude` | `top-rule-chip` | left | bottom row (left segments only) + rule chip on top |
| `borderless`, `field`, `pi`, `rail` | `none` | full | **last row below the editor** (+ optional hook-status rows under it) |

Observed in the sandbox (200×40): with `rail` the status line is the last non-empty row (below an empty `▎` composer row):
```
▎
 pi · [med] Opus 5.5 · [T] stw/proj · @ feat/status *1 +1 ?1 · ctx: 0.9%/1M [A]       (ascii symbols, preset default)
```
With `band`/`box` and `contextLine=embedded` the context % is drawn **inside the rule** (`──5%────╎┃────1M─`) and the row sits above the editor. I did not capture the `box`/`rule`/`claude` shapes individually; their placement above is from the bundle (UNVERIFIED live).
Real unicode/default/rail, colours via `capture-pane -e` (24-bit SGR from theme `dark-terminal`):
```
 π · ◑ Opus 5.5 · 🗑 stw/proj · ⑂ feat/status *1 +1 ?2 · ◫ 0.9%/1M ⟲
 ESC[38;2;107;114;128m π  ESC[38;2;42;48;56m·  ESC[38;2;0;180;255m◑ Opus 5.5  ESC[38;2;232;236;244m🗑 stw/proj  ESC[38;2;255;179;71m⑂ feat/status *1  ESC[38;2;0;255;136m+1  ESC[38;2;156;163;176m?2 · ◫ 0.9%/1M ⟲
```
(`model`/`pi`/`path` fg is the **session-accent colour** hashed from the session name when `statusLine.sessionAccent=true`, otherwise theme keys `statusLineModel`/`statusLinePath`; git dirty=`statusLineGitDirty` warning-orange, staged=green, untracked=muted, `$`=`statusLineCost`.) Do not copy these RGBs – map semantics to OmpDeck's palette.

### 1.2 Settings that control it (all verified with `omp config list` on 18.12.5, sandbox dir)
```
symbolPreset = unicode (unicode|nerd|ascii)          # glyph set for ALL icons (table in 1.4)
composer.shape = band (string)                        # §1.1 placement
statusLine.preset = default (default|minimal|compact|full|nerd|ascii|custom)
statusLine.separator = powerline-thin (powerline|powerline-thin|slash|pipe|block|none|ascii)   # per-preset default; setting overrides
statusLine.contextLine = embedded (off|percentage|annotated|embedded)   # top-border/band only: ctx % drawn INSIDE the rule
statusLine.sessionAccent = true  statusLine.transparent = false
statusLine.compactThinkingLevel = true               # model segment shows only the thinking glyph (first word of "◑ med")
statusLine.showHookStatus = true                     # extension `setStatus` rows (+ `status` segment)
statusLine.leftSegments  = ["vim","model","mode","path","git","pr"]            # [internal tab]
statusLine.rightSegments = ["session_name","token_total","cost","context_pct"] # [internal tab]
statusLine.segmentOptions = {}                       # record; e.g. {"path":{"maxLength":50},"git":{"showStaged":false},"model":{"showThinkingLevel":false},"time":{"format":"12h","showSeconds":true}}
tui.titleState = true   tui.titleSpinner = braille (braille|pulse|dots|line)   # terminal title activity glyph, §2c
```
**`leftSegments`/`rightSegments` are honoured ONLY when `statusLine.preset=custom`** (`bundle:tui/src/status-line/component.ts`, `#Pt()`: `a = (preset==="custom") ? settings.leftSegments ?? preset.left : preset.left`). Proven: with preset `default` setting `leftSegments ["model","token_in","collab"]` changed nothing; after `statusLine.preset custom` only `◑ Opus 5.5` remained. `segmentOptions` merge over the preset's options in every preset. Live reload ≈200 ms after `omp config set` (docs/notes/omp-cli.md A3) – verified: preset flips re-rendered the running TUI within 1.5 s. Segment-name vocabulary (closed, `omp config set statusLine.leftSegments '["__probe__"]'` → rc 1):
`pi, status, model, mode, path, git, pr, subagents, token_in, token_out, token_total, token_rate, cost, context_pct, context_total, time_spent, time, session, hostname, cache_read, cache_write, cache_hit, session_name, usage, collab, stream, vim` (bundle `tui/src/status-line/schema.ts`).

Presets (`bundle:tui/src/status-line/presets.ts`, order = display order):
| preset | left | right | separator |
|---|---|---|---|
| default | pi vim model mode collab stream path git pr context_pct cost | session_name | powerline-thin |
| minimal | vim path git | session_name mode context_pct | slash |
| compact | vim model mode git pr | session_name cost context_pct | (default sep) |
| full | pi vim hostname model mode path git pr subagents | session_name cache_hit token_in token_out token_rate cache_read cost context_pct time_spent time | powerline |
| nerd | as `full` plus `session` | as `full` plus `cache_write`, `context_total`; `time` with seconds | powerline |
| ascii | vim model mode path git pr | session_name token_total cost context_pct | ascii |
| custom | `statusLine.leftSegments` (default `vim model mode path git pr`) | `statusLine.rightSegments` (default `session_name token_total cost context_pct`) | setting |
`subagents` is hidden when the live count is 0 and, when >0, is hoisted to the right side (as is the `⚙ N` background-job badge). `status`, `collab`, `stream`, `vim` are empty when inactive. Narrow-width dropping order: not verified.

### 1.3 Every segment: exact output (`bundle:tui/src/status-line/segments.ts`; formats `v()` = token formatter below)
Token formatter `v(n)`: `<1000` → `n`; `<1e4` → `x.yK` (trailing `.0` dropped); `<1e6` → `NNNK`; `<1e7` → `x.yM`; `<1e9` → `NNNM`; then `B` (bundle fn `v`, ~l.385458; observed `23K`, `1M`).

| id | text (unicode glyphs; ascii in `[]`) | source in omp / hidden when |
|---|---|---|
| `pi` | idle: `π` (`pi`); with profile: `π-<profile>`. **While a turn runs: `<braille spinner> <elapsed>` e.g. `⠸ 2s`** (`Ns`/`Nm`/`Nh`) in accent colour. Focused sub-agent view: `👻 <agentId>` | live (`turnElapsedMs`). Observed `⠸ 2s` while streaming, `π` after. |
| `status` | hook/extension status strings joined with ` · ` | `setHookStatus` (live). Hidden if none. |
| `model` | `<icon.model ⬢> <name>` where name = `model.name ‖ id ‖ "no-model"` with leading `Claude ` stripped (`Opus 5.5`); compact thinking (default): the icon is replaced by the thinking glyph (`◑ Opus 5.5`; ascii `[med] Opus 5.5`); non-compact: `⬢ Opus 5.5 · ◑ med`. Extras: ` ⚡` fast mode (service tier), ` 👁` advisor (colour = advisor status), `· <slow-mode label>` | `session.state.model`, `thinkingLevel`; thinking part omitted if `model.thinking` is null (non-reasoning model); `auto` thinking shows resolved level or `⌛ auto`; level `off` → `⦸ off` (`[ ] off`). Always visible. |
| `mode` | `🗺 Plan` / `🗺 Plan ⏸` (paused, warning colour) / `Prewalk` / `Goal` (+tokens) / `Vibe` / `Loop …`. ascii: `Plan`, `Goal`, `Vibe`, `loop …`. Priority: plan > prewalk > goal > vibe > loop. | plan/goal/vibe persisted as `mode_change` entries (§2a); **prewalk and loop are live-only**. Hidden when none. |
| `collab` | host: `⇄ collab:<participants>` (**participants includes the host** → `⇄ collab:1`); guest: `⇄ collab guest:<n>` | live (`setCollabStatus`); only when a collab room is running (`collab.autoStart` ≠ off or `/collab`). |
| `stream` | `● LIVE <viewers>` and/or `● REC` (red) | live; only with `omp stream`/`/record`. |
| `path` | `📁 ~/PersProject` (`[D] ~/PersProject`); scratch dir (under `/tmp`, `/var/tmp`, `~/tmp`) `🗑 stw/proj` (`[T]`); worktrees `project/worktree`; options `abbreviate`, `stripWorkPrefix`, left-truncation to `maxLength` (default 40) | `process cwd` of that omp (= session header `cwd`). |
| `git` | `⑂ <branch> *<unstaged> +<staged> ?<untracked>` (ascii `@ branch *1 +1 ?1`; detached → `detached`; `⎇ *1 …` if showBranch=false). Colours: branch clean=`statusLineGitClean`, dirty=`statusLineGitDirty`; `*` warn, `+` green, `?` muted | see §2b. Hidden outside a repo. **Verified counts**: repo with 1 modified-unstaged, 1 staged-new, 1 untracked file + 1 untracked dir with 3 files → `*1 +1 ?2` (untracked dir collapsed = `git status --porcelain` default `-unormal`). |
| `pr` | `⤴ #<number>` (OSC-8 hyperlink to the PR url when `tui.hyperlinks`; ascii `PR #12`) | §2b (`gh pr view`). Hidden if no PR. |
| `subagents` | `👥 <n>` (`AG <n>`) running sub-agents; when n>0 it is hoisted to the right side as a separate badge | live (`setRunningSubagents`). Hidden at 0. A second badge `⚙ <n>` (`bg <n>`) shows running background async jobs not already counted. |
| `token_in` | `⤵ <v(usage.input)>` (`in: 50K`) — **cumulative session input tokens (excluding cache)** | §2a |
| `token_out` | `⤴ <v(usage.output)>` (`out: 200`) | §2a |
| `token_total` | `🪙 <v(input+output+cacheWrite+orchestrationInput+orchestrationOutput)>` (**cacheRead NOT included**) | §2a. Hidden at 0. |
| `token_rate` | `⚡ 14.3 tok/s` (live generation rate of the last assistant message) | live; not persisted exactly (`duration` + `usage.output` in the assistant entry allow recomputing). |
| `cost` | `$0.26` = `$` + `(sumCost − subagentCost).toFixed(2)`; then ` (+1.23)` sub-agent tree cost (live, from child sessions), ` ↑`/` ↓` time-based pricing peak/off-peak, ` ★ N` premium requests, ` + 👁 0.12` advisor. **OAuth/subscription providers: prefix `S` instead of `$` (`S0.26`) and `(sub)` when cost is 0** (from the cost segment code; sandbox is API-key so not observed live). Hidden when everything is 0. | §2a |
| `context_pct` | `◫ 17.3%/1M ⟲` (ascii `ctx: 17.3%/1M [A]`): `<pct.toFixed(1)>%/<v(window)>`; `?%/…` when unknown; window unknown → `23K/?`; `⟲` (`[A]`) = auto-compact enabled (accent when a speculative compaction summary is armed/running). Colour by thresholds: pct ≥ min(50 %, 150 K tok) warning, ≥ min(70 %, 270 K) purple, ≥ min(90 %, 500 K) error (`chrome/context-thresholds.ts`: 1 M window → 15 % / 27 % / 50 %). **With `contextLine=embedded` (band/box shapes) the segment is removed from the segment list and drawn inside the rule: `──5%────╎───┃─────1M─`** (`╎` = speculative-compaction mark, `┃` = auto-compact threshold; ascii `:|`). `percentage|annotated|off` keep the text segment or the plain bar. Fractions <1 % print one decimal (`0.9%`), ≥1 % rounded in the embedded form. | §2a |
| `context_total` | `◫ 1M` window size | model catalog |
| `time_spent` | `⏱ 14.1s` active (streaming) time since this omp process started (**in-memory, resets per process**) | live; hidden <1 s |
| `time` | `⏱ 8:07` wall clock (`24h|12h`, `showSeconds`) | local clock |
| `session` | `🆔 01a15e91` first 8 chars of session id (`new` before one exists) | header `id` |
| `hostname` | `🖥 axeleratorPC` | `os.hostname()` |
| `cache_read` / `cache_write` | `💾 <v>` | cumulative usage.cacheRead / cacheWrite |
| `cache_hit` | `💾 97.12%` = `cacheRead / (cacheRead+cacheWrite+input)` (cumulative) | hidden if cacheRead=0 |
| `session_name` | the session title, accent colour, right aligned; truncated first when narrow | session title (§2a) |
| `usage` | `⏱ 5h 42% (3h 10m) · 7d 12% (4d 2h)` provider quota windows | `omp usage` data, fetched in background every ≥5 min; not in any default preset |
| `vim` | `NORMAL|INSERT|VISUAL|V-LINE|REPLACE` | live (vim mode only) |

Real captures (sandbox; 50 000 prompt tokens + 200 out, window 1 M, high thinking, plan mode on, preset `full`, ascii symbols, contextLine off):
```
pi > host axeleratorPC > [high] Opus 5.5 > Plan > [T] stw/proj > @ feat/status *1 +1 ?1 >---… in: 50K < out: 200 < tok/s: 14.3 tok/s < $0.26 < ctx: 5.0%/1M [A] < t: 14.1s < t: 8:07
unicode default, plan:  π > ◒ Opus 5.5 > 🗺 Plan > 🗑 stw/proj > ⑂ feat/status *1 +1 ?1 …
while working:          " ⠸ 2s > ◑ Opus 5.5 > 🗑 stw/proj > ⑂ feat/status *1 +1 ?1 ▶─0.9%────…"  plus a "⎋ Working…" line above the editor
```

### 1.4 Glyph tables (`bundle:tui/src/theme/symbols.ts`, unicode / ascii; selected by `symbolPreset` unicode|nerd|ascii)
`icon.model ⬢/[M]`, `icon.plan 🗺/(none)`, `icon.folder 📁/[D]`, `icon.scratchFolder 🗑/[T]`, `icon.branch ⑂/@`, `icon.pr ⤴/PR`, `icon.context ◫/ctx`, `icon.auto ⟲/[A]`, `icon.agents 👥/AG`, `icon.job ⚙/bg`, `icon.input ⤵/in`, `icon.output ⤴/out`, `icon.tokens 🪙/tok`, `icon.time ⏱/t`, `icon.omp π/pi`, `icon.host 🖥/host`, `icon.session 🆔/id`. Thinking: `○ min ◔ low ◑ med ◒ high ◕ xhigh ◉ max` / `[min] [low] [med] [high] [xhi] [max]`; off `⦸ off`. Nerd glyphs not tabulated.
Thinking levels: `off, minimal, low, medium, high, xhigh, max` (+ `auto`). The set a model supports is `omp models --json` → `models[].thinking` (e.g. `["low","medium","high","xhigh","max"]`; `null` = no thinking → no glyph).

---------------------------------------------------------------------------------------------------
## 2. Cheapest robust external sources

### 2a. Session `.jsonl` (`bundle:coding-agent/src/session/session-manager.ts`, `session-title-slot.ts`, `session-stats.ts`, `context-usage-runtime.ts`)
File = one JSON object per line, **append-only** (O_APPEND writer, `bundle: class hr appendSync`), **except**
* line 0 = fixed-width **title slot**: exactly **256 bytes incl. `\n`** (`n5 = 256`), rewritten in place (pwrite) whenever the title changes (`session-title-slot.ts` `r5()`); `pad` is spaces so the length never changes. A fresh session has `"title":""`.
* the writer may **replace** the file (rewrite to temp `.<name>.<hex>.tmp` + rename under a publish lock; `SessionWriteConflictError`/`rewriteEntries`, compaction/repair paths) → **inode can change, size can shrink**. Reader must key its state on `(dev, ino)` and reset when `ino` changes or `size < parsed_offset`.
* lines can be big (assistant lines with thinking signatures, tool results): max seen 233 KB in a 58 MB file, p99 28 KB. Never assume a line fits a fixed window; the last line may be half-written (no trailing `\n`) → consume only through the last `\n`.
Sizes seen: 58.5 MB (21 620 lines, 6 294 usage entries) read+parse of all usage entries in 0.24 s (python; `read()` alone 35 ms).

Lines (key order as written; `id` = 8-hex, `parentId` = tree parent, `runtime.version` added by 18.12.x, absent on old files). Real lines, trimmed (`…`):

```jsonc
// line 0 – title slot (256 B). 18.12.5 sandbox fresh session; real ones: {"type":"title","v":1,"title":"🖥️ DECK: Open an instance for testing","source":"auto","updatedAt":"2026-10-21T12:59:09.501Z","pad":"      …"}
{"type":"title","v":1,"title":"","updatedAt":"2026-10-21T13:08:15.761Z","pad":"                      …"}
// line 1 – header (version 3). Optional: title,titleSource,parentSession,providerPromptCacheKey,previousSessionFiles
{"type":"session","version":3,"id":"01a15e93-a111-7435-b761-6dc2b6e15acd","timestamp":"2026-10-21T13:08:15.761Z","cwd":"/tmp/stw/proj"}
{"type":"session","version":3,"id":"01a15ce6-b149-762a-9a91-637baf991154","title":"🖥️ DECK: Set up Rust terminal emulator workspace","titleSource":"auto","timestamp":"2026-10-21T05:19:44.969Z","cwd":"/home/axeleratorx/PersProject/OmpDeck","parentSession":"01a15c89-f4ac-770d-b9c2-6309681752c6","providerPromptCacheKey":"01a15c89-f4ac-770d-b9c2-6309681752c6"}
// model: "provider/id". role absent|"default" = the session's model; "temporary" (Ctrl+P cycling / one-shot), "fallback" (retry fallback; resolvedModelIsFallback may be true), other role names possible (smol/slow/plan…)
{"type":"model_change","id":"e95d1b07","parentId":null,"timestamp":"2026-10-21T03:38:27.434Z","runtime":{"version":"18.12.3"},"model":"anthropic/claude-opus-5-5","resolvedModelIsFallback":false}
{"type":"model_change","id":"bef5e483","parentId":"fc828256","timestamp":"2026-09-30T04:16:41.033Z","model":"anthropic/claude-fable-5-1","role":"default","resolvedModelIsFallback":false}
{"type":"model_change","id":"1225c6a0","parentId":"25633d09","timestamp":"2026-09-23T01:15:38.650Z","model":"openai-codex/gpt-5.6-luna","role":"temporary","resolvedModelIsFallback":false}
// thinking: thinkingLevel = effective level (null → off); configured = what the user picked (null=default, "auto" = auto-thinking, effective level then resolved)
{"type":"thinking_level_change","id":"ffbf3ce9","parentId":"e95d1b07","timestamp":"2026-10-21T03:38:27.434Z","runtime":{"version":"18.12.3"},"thinkingLevel":"high","configured":null}
{"type":"thinking_level_change","id":"05782eb4","parentId":"9c6b84eb","timestamp":"2026-10-21T13:06:39.391Z","runtime":{"version":"18.12.5"},"thinkingLevel":"high","configured":"high"}
{"type":"service_tier_change","id":"96f2f194","parentId":"b1175b86","timestamp":"2026-09-28T04:47:09.885Z","serviceTier":null}
// mode: mode ∈ none|plan|plan_paused|goal|goal_paused|vibe ; data = {planFilePath} | {goal:…} | {previousTools:[…]}
{"type":"mode_change","id":"6f9f1fe4","parentId":"05782eb4","timestamp":"2026-10-21T13:06:40.925Z","runtime":{"version":"18.12.5"},"mode":"plan","data":{"planFilePath":"local://PLAN.md"}}
// assistant message – THE usage source. usage.cost.* in USD; contextSnapshot only on 18.12.5 lines
{"type":"message","id":"b82541e0","parentId":"e4b1afab","timestamp":"2026-10-21T13:08:35.545Z","runtime":{"version":"18.12.5"},"message":{"role":"assistant","content":[{"type":"text","text":"word0 …"}],"api":"openai-completions","provider":"deckfake","model":"fake-opus","usage":{"input":50000,"output":200,"cacheRead":0,"cacheWrite":0,"totalTokens":50200,"cost":{"input":0.25,"output":0.005,"cacheRead":0,"cacheWrite":0,"total":0.255}},"stopReason":"stop","timestamp":1792588101512,"contextSnapshot":{"promptTokens":50000,"nonMessageTokens":8552,"toolSchemaTokens":5534,"compactionEpoch":0}}}
{"type":"message","id":"7257b360","parentId":"a24b505a","timestamp":"2026-10-04T03:46:37.525Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"toolu_014d…","name":"eval","arguments":{…},"intent":"running python"}],"api":"anthropic-messages","provider":"anthropic","model":"claude-sonnet-5-5","usage":{"input":4,"output":78,"cacheRead":0,"cacheWrite":12952,"totalTokens":13034,"cost":{"input":8e-06,"output":0.00078,"cacheRead":0,"cacheWrite":0.051808,"total":0.052596},"cttl":{"ephemeral1h":12952}},"stopReason":"toolUse","timestamp":1791085595139,…}}
// aborted turn: zero usage, never counts for context
{"type":"message","id":"4305e77a",…,"message":{"role":"assistant","content":[],"api":"openai-completions","provider":"opencode-go","model":"glm-5.3-flash","usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0,"cost":{…,"total":0}},"stopReason":"aborted","errorMessage":"Operation aborted",…}}
// side-model calls (job widget, judge, title generation…) – COUNT toward session cost/tokens but not toward context
{"type":"model_usage","id":"78a56362","parentId":"528f9e70","timestamp":"2026-10-21T03:41:10.851Z","runtime":{"version":"18.12.3"},"purpose":"job-widget","api":"openai-codex-responses","provider":"openai-codex","model":"gpt-5.6-luna","usage":{"input":1486,"output":109,"cacheRead":1664,"cacheWrite":0,"totalTokens":3259,"reasoningTokens":59,"cost":{"input":0.0002972,"output":0.00013079999999999998,"cacheRead":3.328e-05,"cacheWrite":0,"total":0.00046128},"premiumRequests":0},"stopReason":"toolUse"}
// compaction (method handoff|snapcompact|remote|shake|soft…); tokensAfter = estimated context after it (absent on old files)
{"type":"compaction","id":"92106dc1","parentId":"26c8e036","timestamp":"2026-10-21T05:44:38.485Z","runtime":{"version":"18.12.3"},"summary":"## Goal\nOmpDeck (…) is a native…","firstKeptEntryId":"06d7bc3c","tokensBefore":272456,"tokensAfter":80290,"method":"handoff","details":{"readFiles":[…],"modifiedFiles":[…]},"fromExtension":false}
// title changes (also rewrites line 0)
{"type":"title_change","id":"bb577f12","parentId":"b1f99aec","timestamp":"2026-09-26T14:52:31.868Z","title":"Define the M7 Gates","source":"auto"}      // source: "auto" | "user"
// in-flight tool marker: written when a tool STARTS; its completion is the matching message.role=="toolResult" with the same toolCallId
{"type":"custom","customType":"tool_execution_start","data":{"toolCallId":"toolu_01AYsfF1BTAmEBPTU7PnMbok","toolName":"wait","startedAt":"2026-10-21T13:00:16.842Z","intent":"Waiting for recon scouts"},"id":"…","parentId":"…","timestamp":"2026-10-21T13:00:16.842Z"}
{"type":"custom","customType":"session_exit","data":{"reason":"sighup","kind":"signal","recordedAt":"2026-09-30T01:30:17.469Z"},"id":"7af89a27","parentId":"f29f2570","timestamp":"2026-09-30T01:30:17.469Z"}     // omp died/exited (pendingToolCalls may be present)
// background sub-agents: spawn result then per-job completion
{"type":"message",…,"message":{"role":"toolResult","toolCallId":"toolu_01Ku…","toolName":"task","content":[{"type":"text","text":"Spawned 3 background agents using …\n- `StatusData` (job `StatusData`)\n…"}],"details":{"projectAgentsDir":null,"results":[],"progress":[{"index":0,"id":"StatusData","agent":"deck-api-scout","status":"pending",…}],"async":{…}}}}
{"type":"custom_message","customType":"async-result","content":"<system-notice>\nBackground job CoreSockets has completed. …","display":true,"details":{"jobs":[{"jobId":"CoreSockets","type":"task","status":"completed","agentId":"CoreSockets","label":"CoreSockets","durationMs":24072}]},"attribution":"agent",…}
```
Other types seen (ignore for status): `credential_pin`, `custom:system-prompt-digest`, `custom:todo_hud_state`, `custom_message:{launch-completion,mid-run-todo-nudge,irc:incoming,session-transfer,image-attachment,…}`, `ttsr_injection`, `branch_summary`, `label`, `reset_boundary`, `session_init`, `message.role ∈ {user,toolResult,developer,custom,bashExecution…}`. Census over all 106 local sessions: assistant 97 603, toolResult 120 019, `model_usage` 2 365, `model_change` 593, `thinking_level_change` 522, `compaction` 597, `title_change` 464, `service_tier_change` 12, **`mode_change` 0 (none of the existing sessions ever used a mode; verified live: `/plan` in the sandbox produced exactly the line above)**.

**Derivations (what omp itself does):**
* **Model** shown = `session.state.model`. On disk: replay `model_change` in order (any role except you may want to skip `fallback`); the model that actually answered = last `message.role=assistant` `provider/model`. Resume (`buildSessionContext`, bundle `session-manager.ts` `Cg`) restores `models.default` = last `model_change` whose role is absent/`default`, falling back to the first assistant message's `provider/model` if no default was recorded. Display name/window: `omp models --json` → `models[]` `{provider,id,selector:"provider/id",name,contextWindow,maxTokens,reasoning,thinking:[levels]|null,cost:{input,output,cacheRead,cacheWrite},pricingStatus}`. Look up by `selector`; strip leading `Claude `. Verified for custom `models.yml` providers (sandbox shows `Claude Opus 5.5`, 1 000 000). Catalog call ≈1.2 s (cached, local) – cache it per omp version.
* **Thinking** = last `thinking_level_change`: show `thinkingLevel ?? "off"`; if `configured==="auto"` show `auto` (+ resolved level). Hide when catalog `thinking` is null.
* **Mode** = last `mode_change.mode` (`none`=cleared; `plan_paused`/`goal_paused` = paused). Loop/prewalk are not persisted.
* **Cumulative tokens / cost** (`session-manager.ts` class `Ts` / `getUsageStatistics`): omp sums, over **every entry in the file (all branches; compaction/handoff do NOT reset it)**: `message.usage` of every assistant message **and** `model_usage.usage` of every `model_usage` entry (**and** `toolResult` of tool `task` when `details.usage` exists – none in practice: all 717 local task results are async-style without `details.usage`). Fields: `input, output, cacheRead, cacheWrite, totalTokens, cost.total, premiumRequests`(+`orchestration.*`). Displayed `$` = Σ`cost.total` − subagentCost; ` (+x)` sub-agent tree cost is *live from the child sessions*, **not in the parent file**. `token_in`=Σinput, `token_out`=Σoutput, `token_total`=Σ(input+output+cacheWrite). Verified: sandbox cost `0.00742 + 0.255 = 0.26242` → `$0.26`.
* **Context %** (`context-usage-runtime.ts` `getContextBreakdown`/`getContextUsage`, `session-stats.ts` `R()`): `percent = usedTokens / model.contextWindow × 100` where `usedTokens` = prompt tokens of the **last non-aborted/non-error assistant message after the last `compaction` entry** on the active branch (= `message.contextSnapshot.promptTokens` if present, else `usage.contextTokens`, else `usage.input + usage.cacheRead + usage.cacheWrite`; **output tokens excluded**) **plus omp's tokenizer estimate of everything appended after that message** (tool results, user text) – not reproducible exactly. Verified: assistant usage `{input:4,output:717,cacheRead:22192,cacheWrite:1085}` → `/context` printed `23K/1m tokens (2.3%)` (4+22192+1085 = 23 281); `{input:50000,output:200}` → status `5%`/`5.0%`. After a compaction and before the next assistant reply omp uses its own re-based estimate; externally use `compaction.tokensAfter` (present on 18.12.x lines) as the estimate. No assistant message yet → omp shows ≈0.9 % (system prompt+tools estimate, ~8.5 K tokens in `contextSnapshot.nonMessageTokens`); show `—`/`0%`. Window = catalog `contextWindow`; omp then also applies `baseWindowTokens` for some models [UNVERIFIED].
* **Auto-compact mark** (`jo`/`R2`/`y9` in `session/compaction`): threshold = `compaction.thresholdTokens` (if >0, capped below the window) else `floor(window × thresholdPercent/100)` if `compaction.thresholdPercent` ∈ (0,100) else `window − max(floor(window×0.15), compaction.reserveTokens)` ⇒ 1 M window → 850 K (`/context` shows "Autocompact buffer 150K (15.0%)"). read `compaction.*` from config.yml. Also per-model `compaction.modelThresholds`.
* **Title** = line 0 `title` (fresher than the header's `title` field and cheaper than finding the last `title_change`); `source` auto|user; empty string = none yet. If empty omp shows nothing (the pane title falls back to cwd basename).
* **Fast mode / service tier**: last `service_tier_change.serviceTier` (`null` or provider tier string); omp shows `⚡` when active [exact value→`isFastModeActive` mapping UNVERIFIED].

### 2b. Git branch / dirty counts / PR (`status-line/component.ts`, `status-line/git-utils.ts`, `coding-agent/src/utils/github.ts`)
* Working dir = the omp process cwd (`O()`): the session header `cwd` (`/proc/<omp pid>/cwd` is the live truth, it can change via `/cd`-like moves). Repo handle via omp's native `vcs` module (git **and jj**).
* **Branch**: omp reads `HEAD` natively (no subprocess; `headSync()`), caches it, and **watches the HEAD file with an fs watcher** (`watchBranch`/`ete`) → invalidates on change. Label: `refs/heads/x` → `x`; detached → `detached`. External equivalent: read `<gitdir>/HEAD` (resolve `.git` *file* → `gitdir:` for worktrees; `git rev-parse --absolute-git-dir`), or `git -C <cwd> symbolic-ref --short -q HEAD || echo detached`.
* **Dirty counts**: async `statusSummary()` in the background, **cached 10 s** (`Me=1e4`), 5 s timeout, only refreshed when the TUI re-renders. `*N` unstaged (worktree change entries), `+N` staged (index change entries), `?N` untracked entries with directories collapsed. Verified equal to `git status --porcelain=v1` (default `-unormal`): `XY` with Y∈{M,D,…}→`*`, X∈{A,M,D,R,C}→`+`, `??`→`?` (a file staged *and* modified counts in both). Use `git -C <cwd> status --porcelain=v1 --branch -unormal --no-optional-locks` (cheap; `--no-optional-locks` avoids touching the index lock while omp's own git tools run).
* **PR**: `gh pr view --json number,url` run in the repo (`lookupPullRequest: ur.run(cwd, ["pr","view","--json","number,url"], timeout 5 min)`), **only when** the branch is not detached and not the repo's default branch, **once per branch/repo change** (cache key = branch + git-dir, invalidated by the HEAD watcher), result `{number,url}` → `⤴ #N`; non-zero exit (no PR, not logged in, no `gh`) → segment hidden. `gh` is installed and logged in on this machine (`gh auth status` ✓).

### 2c. Live-only state
| value | persisted/exposed? | best external source |
|---|---|---|
| **working / idle / attention** | not in jsonl as a state. **Terminal title (OSC 0) is updated by omp**: format `π <g> <label>` where `<g>` = `>` idle, **braille spinner frame (U+2800–28FF, 80 ms) while a turn runs**, `!` = attention/needs input; `<label>` = session title or cwd basename (`bundle: terminal-title`, `v_t(label,state,frame)`; `tui.titleState` (default true) / `tui.titleSpinner` (`braille|pulse|dots|line`)). **Verified**: idle `π > proj`; during the 14 s stream `π ⠦ proj`; tmux `#{pane_title}` reflects it. | **OSC title from the terminal grid / `tmux display -p -t <pane> '#{pane_title}'`** – zero cost, no polling of files. Spinner style `pulse`=○◔◑◕●, `dots`, `line`(`-\|/`) are other glyph sets; treat "second token is not `>`/`!`" as working. [`!` UNVERIFIED – not triggered; `:` replaces the spinner on some terminals (`K()` = WSL/legacy)]. Secondary: jsonl `tool_execution_start` without matching `toolResult` (tool running) and file mtime < ~2 s old. |
| subagent count (`👥 n`) | live only in-process (`setRunningSubagents`). Persisted traces: parent `task` toolResult `details.progress[].id` (spawned) − `custom_message async-result details.jobs[].jobId` with `status` terminal (finished); child transcripts are files **`<session path minus .jsonl>/<AgentId>.jsonl`** (a running agent keeps appending → mtime <≈30 s) | derive: spawned − completed, cross-check mtime of `<dir>/*.jsonl`; **approximate** (omp counts live hub agents incl. nested). The OmpDeck orchestrator's directory `…/sessions/-PersProject-OmpDeck/<id>/` holds dozens of such `<AgentId>.jsonl` files. |
| collab (`⇄ collab:N`) | **Yes if the instance hosts a room**: `omp collab list --json` (`version:1`) → `hosts[]` `{instanceId,generation,pid,sessionId,sessionName,cwd,model:{provider,id},startedAt(ms),participants,relayConnected,inputRequired,busy,access:"view"|"control"}`; backed by `~/.omp/run/collab-hosts/<id>.json` (`{version,instanceId,pid,endpoint,createdAt,token}`, mode 0600) + a unix socket answering `{"v":1,"token":…,"op":"snapshot"}` (`collab/registry.ts`). Schema observed from real output on this machine. **Only exists when `collab.autoStart` ≠ `off` (default `off`; this user has `control`, relay `ws://127.0.0.1:7466`)**. | Use `omp collab list --json` (CLI contract; tokens not printed) matched on `pid` (= tmux `#{pane_pid}`'s `omp` child) or `sessionId`; it additionally gives authoritative `busy`, `inputRequired`, `model`, `sessionName`. **Do not enable collab from OmpDeck**: `--config` overlay with `collab.autoStart` would open a relay connection (default `wss://my.omp.sh`) = privacy decision for the user. Never read the registry JSON yourself (contains the control token). |
| queued/steering messages, loop state, prewalk, vim mode, hook statuses, goal budget, `time_spent`, token rate | not persisted, no socket | none (omit). Goal objective/token budget is in `mode_change.data.goal` only at set time. |
| background daemons | `omp ps --json` → `[{kind:"project",projectDir,runtimeDir,brokerPid,daemons:[{name,id,state,pid,createdAt,startedAt,readyAt,restartCount,outputBytes,owner:"<sessionId>",readyMatch,…}]}]` (daemons the agent started via the `ps`/daemon tool, owner = session id) | optional badge `⚙ n` = daemons with `owner == sessionId`; distinct from omp's `⚙ n` async-job badge which is live-only. |
| which session file a tab is on | **terminal breadcrumb**: `~/.omp/agent/terminal-sessions/<tty-name>` (`pts-6` for `/dev/pts/6`; else `tmux-%N`, `kitty-…`, `wezterm-…`; `bundle: Qee()/REt()`), 3 lines: `<cwd>\n<abs session .jsonl>\n[fresh\n]cwdstat <dev> <ino>`. Written at startup (**before the session file exists: line 3 contains `fresh`**), rewritten on `/new`, `/resume`, handoff/fork switches. Verified (fresh start: file points at the future path + `fresh`; after `/new`: path changed, `fresh` gone, both files listed in sessions dir; `ls sessions/<dir>` empty until the first assistant reply completes). | Authoritative `tab → current session path` mapping: `tmux display -p -t <pane> '#{pane_tty}'` → strip `/dev/`, `/`→`-` → read that file. Check `exists` via stat. Survives `/new` where the tab's recorded `--resume` path would go stale. Stale after omp exits (ignore if no omp process holds that tty). |

### 2d. Scraping the terminal grid – feasible but NOT recommended as the primary source
* Position depends on `composer.shape` (§1.1): last row for `borderless/field/pi/rail` (user's default), top border of the editor for `band/box` (moves with the editor height and sits above the `╰─` row and an optional hint row), top rule chip + left-only bottom bar for `rule/claude`. Hook-status rows (`statusLine.showHookStatus`) and the "Warning:/Working…" rows can appear below it.
* Content depends on `statusLine.preset`, `symbolPreset`, `separator`, `contextLine` (ctx % is *inside the rule* with the default `band` shape: `──5%──╎┃──1M─`), terminal width (segments are dropped from the right, `path` is ellipsised), and `sessionAccent` colours; it also changes with 18.12.x segment additions. Three glyph sets × seven presets × 8 shapes ⇒ no stable regex.
* Cheap but valid uses: (1) `pane_title`/OSC title for working/idle (§2c) – robust; (2) as a *fallback only*, last non-empty row of the grid matching ` ctx: 5.0%/1M` / `◫ 5.0%/1M` for the `rail` shape.
Everything on the line except live-only items (subagents, loop, prewalk, rate, time) can be computed from jsonl + git + catalog, so scraping buys nothing.

---------------------------------------------------------------------------------------------------
## 3. Recommended design for OmpDeck

### 3.1 Data flow
```
per tab  SessionStatusTracker { path: PathBuf, file_id:(dev,ino), parsed_to:u64, title_slot:[u8;256], agg:Totals, latest:Latest, open_calls:HashMap<String,String>, children... }
tick (UI thread: only stat; worker thread: read/parse)  →  Arc<StatusSnapshot> published via channel  →  egui bar paints from the snapshot
```
1. **Resolve the session path** per tab: tab record (`--resume <path>`) → override with breadcrumb `terminal-sessions/<tty>` when it differs and `omp` still owns the tty (§2c). Fresh tab (`omp` without `--resume`): breadcrumb path may not exist yet.
2. **jsonl tail-parser** (worker, active tab every 500 ms–1 s via `fstat` – skip the read if `(ino,size)` unchanged; inactive tabs every 5 s, or only for tab badges; optionally `notify`/inotify on the file to wake the worker – polling is the robust baseline):
   * `fstat`; if `ino` changed or `size < parsed_to` → reset state, rescan from 0.
   * read line 0 (256 B) every tick the mtime changed → parse `title` (`serde_json`, tolerate invalid → keep previous).
   * **First open** (cost is cumulative over the whole file, so a bounded tail is *not* enough for `$`/tokens): worker does one forward pass from byte 256 (`memchr` `\n`), with a **prefilter on the line prefix** so that only needed lines are JSON-parsed: `{"type":"model_change"`, `{"type":"thinking_level_change"`, `{"type":"mode_change"`, `{"type":"service_tier_change"`, `{"type":"compaction"`, `{"type":"model_usage"`, `{"type":"title_change"`, `{"type":"custom","customType":"tool_execution_start"`/`"session_exit"`, `{"type":"message"` **and** the substring `"role":"assistant"` right after `"message":{` (toolResult/user lines — the huge ones — are skipped after a `memmem(b"\"toolCallId\"")` only when you track open tool calls; otherwise skipped unparsed). 58 MB ⇒ ~35 ms read + a few ms/1 000 parsed lines. Show the state from a **reverse bounded read first** (last 256 KiB, grow ×2 up to 8 MiB until model/thinking/last-usage were all found) so the bar paints instantly, then fill `$`/tokens when the pass finishes (`…` placeholder until then).
   * **Incremental**: afterwards read only `[parsed_to, size)`; consume up to the last `\n`; `parsed_to` = byte after it. Dispatch table below.
   * Cache aggregates by `(path, ino, size, parsed_to)` in memory only (no on-disk cache needed).
3. **Catalog**: `omp models --json` once at startup per omp version, on a worker thread (~1.2 s); map `selector → {name, contextWindow, thinking[]}`; refresh on `omp --version` change. Unknown model → name = id, window unknown → show `23K/?` like omp.
4. **Pane title** (alacritty `Event::Title`/tmux `#{pane_title}` on the existing control path): parse `^π (\S+) ` → `>` idle, `!` attention, anything else = working (spinner glyph can also drive a tiny activity indicator). Free, event-driven, no timers.
5. **Git** (worker, per distinct cwd, shared by tabs): branch = read `<gitdir>/HEAD` (stat every 1 s, only re-read when mtime changes) — instant branch changes without spawning git; dirty counts = `git status --porcelain=v1 -unormal --no-optional-locks` for the **active tab only**, at most every 10 s and additionally 1.5 s after the session jsonl grows (a tool probably edited files) with a 5 s timeout; PR = `gh pr view --json number,url` **only on branch change / tab open** (skip if branch == default or detached, 20 s timeout in OmpDeck rather than 5 min, cache `(gitdir, branch)` for the run; manual "refresh" button). Absence of `gh`/non-zero exit = no PR segment, no error UI.
6. **Collab/busy** (optional, only if the user already has `collab.autoStart`≠off): `omp collab list --json` every 3 s *for the active tab only*; adds `busy`, `inputRequired`. Not needed when the OSC title works.
7. **Subagent count** (approximate): from the incremental parse: `spawned` ids (from `toolResult` `task` `details.progress[].id`) minus finished `async-result.details.jobs[].jobId`; show only if > 0, tooltip "approx.". (Requires parsing `task` toolResult lines – they are not huge.)

### 3.2 Parsing pseudo-spec (implement + unit-test with the fixture lines above)
```text
struct Totals { input,output,cache_read,cache_write: u64, cost: f64, n_usage: u32 }
struct Latest { model: Option<String> /*provider/id*/, model_role, thinking:Option<String>, configured:Option<String>, tier:Option<String>,
                mode:String="none", mode_data, ctx_prompt:Option<u64>, ctx_anchor_entry:Option<String>, last_compaction:Option<{id,tokens_after,tokens_before,ts,method}>,
                title:Option<String>, title_source, last_ts, last_assistant:{provider,model,stop_reason} }

for each complete line L (bytes, up to '\n'):
  v = serde_json::from_slice(L)  (only after the prefix filter; parse failure ⇒ skip line, keep going)
  match v["type"]:
   "model_usage"            => totals.add(v.usage)                                   // cost, input, output, cacheRead, cacheWrite
   "message"                => match v.message.role:
        "assistant"         => u = v.message.usage            // may be absent
                               totals.add(u)                  // aborted/error entries have all-zero usage – harmless
                               if stopReason ∉ {"aborted","error"} && u:
                                   latest.ctx_prompt = message.contextSnapshot.promptTokens
                                                        ?? u.contextTokens
                                                        ?? (u.input+u.cacheRead+u.cacheWrite)  (if 0 → totalTokens−output)
                                   latest.ctx_after_compaction = true   // anchored again
                               latest.last_assistant = {provider, model, stopReason}
        "toolResult"        => open_calls.remove(toolCallId); if toolName=="task": spawned += details.progress[*].id
        _                   => ignore
   "model_change"           => latest.model = v.model; latest.model_role = v.role.unwrap_or("default"); (role=="fallback" ⇒ flag)
   "thinking_level_change"  => latest.thinking = v.thinkingLevel.unwrap_or("off"); latest.configured = v.configured
   "service_tier_change"    => latest.tier = v.serviceTier
   "mode_change"            => latest.mode = v.mode; latest.mode_data = v.data
   "compaction"             => latest.last_compaction = {tokensAfter, tokensBefore,…}; latest.ctx_prompt = v.tokensAfter (estimate) ; anchored=false
   "title_change"           => latest.title = v.title; latest.title_source = v.source
   "custom"                 => customType=="tool_execution_start" ⇒ open_calls.insert(data.toolCallId, data.toolName);  "session_exit" ⇒ latest.exited = data
   "custom_message"         => customType=="async-result" ⇒ for j in details.jobs: finished.insert(j.jobId) if j.status ∈ {completed,failed,aborted,cancelled}
   _                        => ignore (unknown type must not fail)
  latest.last_ts = v.timestamp

render:
  model_name   = catalog[latest.model ?? last_assistant provider/model]?.name.strip_prefix("Claude ") ?? raw id ; prefer last_assistant when its entry is newer than the last model_change
  window       = catalog.contextWindow
  ctx_pct      = ctx_prompt / window * 100                      // one decimal <1%, shown as "17.3%/1M" ; None ⇒ "?%/1M" or "—"
  ctx_color    = pct >= min(50,150000/window*100) ? warn : pct >= min(70,270000/window*100) ? purple : pct >= min(90,500000/window*100) ? err : muted
  cost         = "$" + format(totals.cost,"%.2f")  ; subscription providers (anthropic OAuth, openai-codex…) omp shows "S0.17" – OmpDeck may prefer "$0.17 (sub)" 
  tokens       = fmt_tokens() per §1.3 (in, out, in+out+cache_write)
  thinking     = hidden if !catalog.thinking ; "auto" if configured=="auto"
  mode         = Plan / Plan ⏸ / Goal / Goal ⏸ / Vibe / hidden(none)
  working      = pane_title glyph != ">" 
```
Edge cases to unit-test: truncated last line; two appended chunks splitting a multibyte char/line; `ino` swap; file shrink; title line rewritten (same length) with emoji; `thinkingLevel:null`; `role` missing in `model_change`; assistant line with `usage` before `content`? (order is fixed content→usage, don't rely on it); aborted message zero usage; missing `runtime`/`contextSnapshot` (old files); unknown future `type`; 233 KB line; session with only title+header (no assistant yet).

### 3.3 What to show when there is no session file yet (fresh `omp` tab)
Breadcrumb exists with `fresh` and the planned path + cwd. Show: **cwd path chip and git from breadcrumb cwd**, model/thinking from config (`~/.omp/agent/config.yml` `modelRoles.default` [UNVERIFIED parsing path] and `defaultThinkingLevel`, dim/italic "default"), `ctx —`, cost `$0.00` hidden, title empty, working-state from the pane title. Poll for the file at 1 s (`stat` the breadcrumb path); the file appears after the **first assistant reply completes** (verified: user prompt sent, `ls` empty for the whole 14 s stream, file written when the assistant message finished) or immediately after `/new`. Then switch to jsonl-driven state without flicker (keep cwd/git).

### 3.4 Cadence summary
| source | active tab | background tabs | trigger |
|---|---|---|---|
| jsonl `fstat` + incremental read | 500 ms–1 s | 5 s (or on hover/badge) | size/ino change; first open = full pass off-thread |
| pane title/working | event-driven | event-driven | OSC 0 / `#{pane_title}` |
| git HEAD stat | 1 s | 10 s | mtime change |
| git dirty counts | ≤10 s + 1.5 s after jsonl growth | — | background, 5 s timeout |
| PR (`gh`) | on branch change / tab open | — | cached per (gitdir, branch) |
| catalog `omp models --json` | startup, `omp --version` change | | |
| `omp collab list --json` (optional) | 3 s | — | only if collab hosts exist |
| breadcrumb | 2 s | 10 s | detects `/new`, `/resume`, handoff |

---------------------------------------------------------------------------------------------------
## Evidence index / cleanup
* Sandbox facts: status line captures §1.1/1.3 (tmux `capture-pane -p/-e`, 200×40); `omp config set` live reload; fresh-vs-`/new` breadcrumb; first-reply file creation; title `π ⠦ proj` vs `π > proj`; `*1 +1 ?2` git counts; ctx `23K/1m (2.3%)`; costs `$0.26`.
* Not exercised: `gh pr view` result (no PR to look up), attention title `π !`, OAuth `S`-prefixed cost, jj repos, `usage` segment, goal/vibe `mode_change`, `/handoff` breadcrumb update, tmux socket of the real app.
* A foreign python process was already listening on 127.0.0.1:18777; it was not touched. My own server used 127.0.0.1:18911 and was killed.
* Cleanup performed: tmux `omp-deck-test-…` server killed + socket removed, python server killed, `/tmp/stw` deleted. Nothing written under `~/.omp/agent/**` or the repo except this note. `omp collab list`, `omp ps --json`, `omp models --json` were read-only calls on the real agent dir; `omp config set` calls only touched `/tmp/stw/agent/config.yml`.
