Native `/usage` and `/context` views are both doable from facts below. The hard questions are settled: there is an `omp usage --json` CLI, and an in-process extension can compute the `/context` breakdown directly. Everything was observed on the installed `omp/18.12.6` bundle (`strings ~/.local/bin/omp`; bundle chunks carry `// packages/…` headers, cited below). Sandbox cleanup is complete.

## Method and caveats

- **Sandbox:** all runs used `env -i` with `HOME=/tmp/…/home`, temp `PI_CODING_AGENT_DIR` and `PI_CONFIG_DIR`, a private `XDG_RUNTIME_DIR`, tmux socket `-L omp-deck-test-usage1`, and a fake model server on `127.0.0.1:18955`.
- **No real providers:** the fake server also answered `GET /api/oauth/usage`. I pointed an `anthropic` entry in the sandbox `models.yml` at it and inserted one fake OAuth credential into the sandbox `agent.db`. Nothing touched your credentials or the real `~/.omp`.
- **Sandbox quirk:** the native addon failed to extract into the sandbox home with `EDQUOT`. I symlinked `~/.omp/natives` into the sandbox; omp only reads it.
- **`PI_CONFIG_DIR`:** it is treated as relative to `$HOME`. `stats.db` landed at `$HOME/<PI_CONFIG_DIR>/stats.db`. Real runs use `~/.omp/stats.db` (`ih()` → `rootSubdir("stats.db","data")`).
- **Line numbers:** I use symbol names and file headers, not `strings` line numbers, because those aren't stable.

---

## /usage

### Q1. Implementation

**Slash command** (`packages/coding-agent/src/slash-commands/builtin-session.ts`, `name: "usage"`):
- `/usage` and `/usage show` open the TUI dashboard through `ctx.handleUsageCommand()`. That calls `session.fetchUsageReports()`, then `ctx.showUsageDashboard(reports)`. If the session has no `fetchUsageReports`, it shows a warning; on a fetch error, an error.
- `/usage reset [provider/credentialId|provider/active]` is the function `FI` (`handleUsageResetCommand`). With no argument the TUI opens a reset selector. With an argument it calls `session.redeemResetCredit(target)`. This spends a saved rate-limit reset, which is mutating and hits the network. It is only for Codex (`openai-codex`) and Claude (`anthropic`) (`reset-usage.ts`).
- The non-TUI `handle` (ACP) is `buildUsageReportText`. It prints a text report, or, when no reports exist, a local fallback built from `sessionManager.getUsageStatistics()`.

**Network data** (`session.fetchUsageReports(signal?)` → `authStorage.usage.reports({baseUrlResolver, signal})`, `packages/ai/src/auth/usage.ts`):
- It fetches every stored credential of every provider that has a usage fetcher (`packages/ai/src/usage/registry.ts`). Candidates are all providers with stored credentials, plus env-var keys for api-key providers.
- Providers in the registry include anthropic, openai-codex, github-copilot, gemini, google-antigravity, kimi, zai, cursor, opencode-go, synthetic, xai-oauth, minimax, alibaba-token-plan, factory-droid, commandcode, ollama, umans, devin, cline-pass, muse-code and charm-hyper.
- Confirmed endpoints:

| Provider | Endpoint |
|---|---|
| Anthropic | `GET {base}/api/oauth/usage`, plus `?cedar_ember=1&skip_spend=1` and `?at_wall=1&skip_spend=1` for saved resets. Sandbox-observed (3 requests). Default base `https://api.anthropic.com`. |
| Codex | `wham/usage` under `https://chatgpt.com/backend-api` (constant in the bundle, not exercised), plus reset-credit calls |
| Copilot | `api.github.com` (`copilot_internal/user`, premium-request usage) |
| Cursor | `https://cursor.com/api/usage-summary` |
| Gemini CLI | `cloudcode-pa.googleapis.com` (`v1internal:retrieveUserQuota`) |
| Antigravity | `daily-cloudcode-pa.googleapis.com` |
| Kimi | `api.kimi.com/coding/v1` |
| Synthetic | `api.synthetic.new/v2/quotas` |
| Others | `api.z.ai`, `opencode.ai/zen/go`, `api.minimax.io`, `api.code.umans.ai`, `api.commandcode.ai`, Alibaba/Qwen consoles |

- Expired OAuth tokens are refreshed first and persisted back to the credential store.
- `baseUrlResolver` uses `models.yml` provider `baseUrl` overrides. That is what let me redirect Anthropic to the fake server.
- Anthropic retries 3 times with 500 ms·2ⁿ backoff.

**Cache and timing:**
- Per-credential reports are cached for 5 min (±25% jitter) in the SQLite `cache` table of `agent.db` (key `usage_cache:report:…`). The cache is shared across omp processes, so a second `omp usage` run made zero requests (request log stayed at 3).
- Stale entries are kept 24 h as `lastGood`; a failed fetch is retried after about 10 s (`failureBackoff`).
- Per-request timeout defaults to 10 s (`usageRequestTimeoutMs ?? 1e4`).
- Interactive omp already calls `refreshUsageInBackground()` from its status-line state builder, at most once per 5 min. So `session.cachedUsageReports` is usually populated without any extra network.
- Observed: `omp usage --json` took 0.34 s wall against the local fake. An in-process `fetchUsageReports()` took 67 ms cold and 0.5 ms cached. Real latency is the provider round-trip, up to 10 s per provider.

**Side effects of fetching:**
- `usage_history` rows are written to `agent.db` (hourly snapshots; 4 rows after the run).
- Refreshed tokens are persisted.
- `/usage` also loads activity data: a worker runs `jF()`, which syncs all session files into `stats.db`. That means writes to `~/.omp/stats.db` and a heavy first sync.

**Local data in the dashboard:**
- The activity strip and the "Past year / Last 30 days / Busiest day" stats come from `stats.db`, table `messages`:

```sql
SELECT date(timestamp/1000,'unixepoch','localtime') day, SUM(cost_total) cost, COUNT(*) requests, SUM(total_tokens) total_tokens
FROM messages WHERE timestamp >= ? GROUP BY day ORDER BY day
```
- The window is the last 371 days. `messages` columns include `session_file, entry_id, folder, model, provider, timestamp, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, total_tokens, cost_total, cost_unpriced, agent_type, stop_reason`.
- Per-session totals (the no-credentials fallback) come from `sessionManager.getUsageStatistics()`.

### Q2. Structured access

**Option A: CLI (verified).** Run `omp usage --json`.
- Flags: `--provider/-p <id>`, `--redact/-r`, `--history --days N --json`, `-e <ext>`, `--no-extensions`.
- Subcommands: `usage accounts --json` (OAuth identity keys), `usage clients --json` (per-client token burn), `usage invalidate [-p provider]`, and `usage reset` (list saved resets with `--json`; `reset <provider>/<id>` spends one, so never call that).
- Safe to run read-only. It does hit the network, and side effects are listed in Q1.

No-credentials output (verified):
```json
{ "generatedAt": 1792647445164, "reports": [], "accountsWithoutUsage": [], "disabledCredentials": [], "capacity": {} }
```

Credentialed output (fake Anthropic account; this is the exact shape):
```json
{
  "generatedAt": 1792647505321,
  "reports": [{
    "provider": "anthropic",
    "fetchedAt": 1792647505316,
    "limits": [
      { "id": "anthropic:5h", "label": "Claude 5 Hour",
        "scope": { "provider": "anthropic", "windowId": "5h", "shared": true },
        "window": { "id": "5h", "label": "5 Hour", "durationMs": 18000000, "resetsAt": 1792655545000 },
        "amount": { "used": 37, "limit": 100, "remaining": 63, "usedFraction": 0.37, "remainingFraction": 0.63, "unit": "percent" },
        "status": "ok" },
      { "id": "anthropic:7d", "label": "Claude 7 Day", "scope": {"provider":"anthropic","windowId":"7d","shared":true},
        "window": {"id":"7d","label":"7 Day","durationMs":604800000,"resetsAt":1792924705000},
        "amount": {"used":81.5,"limit":100,"remaining":18.5,"usedFraction":0.815,"remainingFraction":0.185,"unit":"percent"}, "status":"ok" },
      { "id": "anthropic:7d:sonnet", "label": "Claude 7 Day (Sonnet)", "scope": {"provider":"anthropic","windowId":"7d","tier":"sonnet"},
        "window": {…same as 7d…}, "amount": {…"used":12…}, "status":"ok" },
      { "id": "anthropic:extra", "label": "Claude Extra Usage", "scope": {"provider":"anthropic","windowId":"extra"},
        "amount": {"used":12.34,"unit":"usd","limit":50,"remaining":37.66,"usedFraction":0.2468,"remainingFraction":0.7532}, "status":"ok" }
    ],
    "metadata": { "endpoint": "http://127.0.0.1:18955/api/oauth/usage", "accountId": "acc-uuid-1", "email": "sandbox@example.test" },
    "resetCredits": { "availableCount": 0, "redeemableCount": 0, "eligible": false, "credits": [] }
  }],
  "accountsWithoutUsage": [],
  "disabledCredentials": [],
  "capacity": { "anthropic": [
    { "window": "5h", "durationMs": 18000000, "accounts": 1, "usedAccounts": 0.37, "remainingAccounts": 0.63 },
    { "window": "7d", "durationMs": 604800000, "accounts": 1, "usedAccounts": 0.815, "remainingAccounts": 0.185 },
    { "window": "7d", "durationMs": 604800000, "meter": "sonnet", "accounts": 1, "usedAccounts": 0.12, "remainingAccounts": 0.88 },
    { "window": "extra", "accounts": 1, "usedAccounts": 0.2468, "remainingAccounts": 0.7532 } ] }
}
```

**`UsageReport` type** (`packages/ai/src/usage.ts` arktype schemas):
- `UsageReport` = `{provider: string, fetchedAt: number, limits: UsageLimit[], resetCredits?: ResetCredits, notes?: string[], metadata?: Record<string,unknown>, raw?: unknown}`.
- `UsageLimit` = `{id, label, scope: UsageScope, window?: UsageWindow, amount: UsageAmount, status?: 'ok'|'warning'|'exhausted'|'unknown', notes?: string[]}`.
- `UsageScope` = `{provider, accountId?, projectId?, orgId?, modelId?, tier?, windowId?, shared?: boolean, sharedGroup?: string}`.
- `UsageWindow` = `{id, label, durationMs?, resetsAt? (epoch ms), resetLabel?}`.
- `UsageAmount` = `{used?, limit?, remaining?, usedFraction?, remainingFraction?, unit: 'percent'|'tokens'|'requests'|'credits'|'usd'|'minutes'|'bytes'|'unknown'}`.
- `ResetCredits` = `{availableCount, redeemableCount?, nextCreditId?, eligible?, reason?, cooldownUntil?: ISO, credits?: [{id?, title?, program?, remainingCount?, usable?, requiresLimit?, clears?: string[], blocking?: string[], usedFractions?: Record<string,number>, grantedAt?, expiresAt?: ISO, status?}]}`.
- Metadata keys seen across providers: `email, accountId, projectId, orgId, orgName, planType, tier, daybreak, endpoint`.
- `accountsWithoutUsage` entries are `{provider, label?, …}` for credentials with no report. `disabledCredentials` are credentials disabled with a cause (expired logins).
- The CLI strips `raw`; do the same in the extension.

**Option B: in-process (verified, including no-credentials).** In the extension:
```ts
const { AgentRegistry } = await import("@oh-my-pi/pi-coding-agent/registry/agent-registry"); // already used by ompdeck-ctl.ts
const session = AgentRegistry.global().list().find((r: any) => r.kind === "main")?.session;   // AgentSession
const reports = await session.fetchUsageReports(signal?);   // UsageReport[] | null (null if authStorage.usage.reports is missing)
const cached  = session.cachedUsageReports;                  // last result, no network
const sel     = session.getUsageReportingModelSelectors(reports); // string[] "provider/modelId"
```
- `ctx` does not expose the session or `AuthStorage` as a key (`Object.keys(ctx)` showed `["ui"]` only). Use the registry's main record, like ompdeck-ctl does.
- No credentials: `fetchUsageReports()` returned `[]`. `buildUsageReportText({session, output})` returned the local fallback text:
```
Usage
Input tokens: 0
Output tokens: 0
Cache read tokens: 0
Cache write tokens: 0
Total tokens: 0
Premium requests: 0
Cost: $0.000000
```
- `sessionManager.getUsageStatistics()` returns `{input, output, cacheRead, cacheWrite, totalTokens, orchestrationInput, orchestrationOutput, orchestrationCacheRead, premiumRequests, cost, subagentCost}`.

**Import specifiers** (all resolved in `omp --mode rpc … -e x.ts` and in interactive; they must be literal, since non-literal `import(var)` fails with "Cannot find package '@oh-my-pi/pi-coding-agent'"):

| Specifier | Exports |
|---|---|
| `@oh-my-pi/pi-coding-agent/slash-commands/helpers/usage-report` | `buildUsageReportText({session, output?}): Promise<string>` |
| `@oh-my-pi/pi-coding-agent/cli/usage-cli` | `buildRedactionMap, collectHistoryIdentityStrings, computeProviderWindowStats, formatClientUsage, formatUsageBreakdown, formatUsageHistory, runUsageCommand` |
| `@oh-my-pi/pi-tui/overlays/usage-dashboard` | `buildProviderCards, buildHeatmapLayout, HEATMAP_DAY_LABELS, UsageDashboardComponent, formatActivityErrorDetail` |
| `@oh-my-pi/pi-tui/overlays/usage-display` | `collapseSharedUsageReports, formatLimitTitle, formatUsageResetWindow, summarizeUsageResetCredits, bankedResetCreditExpiryMs` |

- `@oh-my-pi/pi-coding-agent/stats/activity-client` does not resolve; there is no specifier for the activity loader. Read `stats.db` yourself with SQLite (read-only).
- `buildProviderCards(reports, now, unavailableAccounts[])` is the pure model behind the Overview tab. Output on the sample:
```json
[{"provider":"anthropic","name":"Anthropic","accounts":1,"unavailableAccounts":[],
  "windows":[{"label":"Claude 5 Hour","fraction":0.37,"status":"ok","resetMs":8039515},
             {"label":"Claude 7 Day","fraction":0.815,"status":"ok","resetMs":277199515},
             {"label":"Claude 7 Day (Sonnet)","fraction":0.12,"status":"ok","resetMs":277199515},
             {"label":"Claude Extra Usage","fraction":0.2468,"status":"ok"}],
  "unlimited":false,"notes":[],"idle":false}]
```
  Cards may also carry `windowTag`, `usedText`, `resetCredits` and `daybreakAccounts`.
- `buildHeatmapLayout(points, 53)` returns `{monthLabels, cells[7][53] (0–4 or null), totalCost, totalRequests, start}`.

**Recommendation for usage:**
- **Preferred:** for tabs with our extension, add an op that returns `{reports: session.cachedUsageReports ?? await session.fetchUsageReports(), …}` with `raw` stripped. It reuses the cache and the shared SQLite cache.
- **Fallback:** for tabs without the extension, or an app-level "Usage" view, spawn `omp usage --json` (about 0.3–10 s). Parse `reports` and `capacity`.
- **Activity heatmap:** read `~/.omp/stats.db` read-only with the SQL above. I did not open the real db and did not verify read-only access on the WAL file, so confirm that before relying on it.
- **Failure modes:**
  - `reports` is `[]` with no credentials or when every fetch fails.
  - `null` means no usage backend.
  - A provider can be missing silently after a 401/403, or after a 10 s failure with the stale value retained.
  - `accountsWithoutUsage` explains what is missing.
  - Anthropic OAuth grants expire about 30 days after login (`disabledCredentials`).
  - Do not call `usage reset <target>` or `/usage reset <target>`; they spend a saved reset.

### Q3. TUI rendering (to mirror)

The dashboard is `UsageDashboardComponent` in `packages/tui/src/overlays/usage-dashboard.ts`. It has two tabs, Overview and Details, a header line "Updated Xs ago", and `r` to refresh. What I captured in tmux is the text-mode rendering; the "native" variant uses the same data with meters.

**Provider cards:**
- Each card is a status glyph and provider name, plus "N accounts" when more than one (card sort order: below).
- Under it come up to 4 rows per provider. Each row is the limit label (plus a window tag like `5h`, `7d`, `mo` only when two rows share a label), a bar, `NN%` and the time until reset.
- Extra rows: `+N more` (text) or "N more in Details" (native), `✦ N resets banked · M usable · expires in …`, `daybreak · email`, and "email — usage unavailable".
- Observed text rendering for the sample (bars are `█`/`░`, 30 cells):
```
✔ Anthropic
  Claude 5 Hour
  ███████████░░░░░░░░░░░░░░░░░░░  63% 2h11m
  Claude 7 Day
  ████████████████████████░░░░░░  19%  3d4h
  Claude 7 Day (Sonnet)
  ████░░░░░░░░░░░░░░░░░░░░░░░░░░  88%  3d4h
  Claude Extra Usage
  ███████░░░░░░░░░░░░░░░░░░░░░░░  75%
```
  **The percent and bar show REMAINING, not used.** The bar's filled length is `round((1−fraction)·width)`, and the number is `max(0, round((1−fraction)·100))` labelled "left" in native mode. The CLI `omp usage` shows used instead: `37.0% used · resets in 2h13m`.
- Multiple accounts of a provider: the card shows the mean fraction across accounts, and `resetMs` comes from the fullest window. Windows are grouped by `label|window.id`.
- Colors: status `ok` → success/accent; `warning` → warning; `exhausted` → error; unknown → dim.
- Provider-set status is `Rr(usedFraction)`: ≥1 exhausted, ≥0.9 warning, else ok. The CLI's fallback is ≥0.8 warning when status is unknown.
- A provider with every window under 0.5% used (and no resets or errors) is "idle"; idle ones are collapsed to a dim footnote "untouched: A · B". Active cards are sorted by max window fraction descending, then name.
- Native mode only: a stats strip with "Tightest quota" (highest fraction across all windows, shown as `NN% left`), "Past year", "Last 30 days" (with a sparkline) and "Busiest day".

**Activity heatmap:**
- 53 weeks × 7 rows (M T W T F S S), month labels on top, cell intensity 0–4 using `ceil(sqrt(v/max)·4)`, colored from dim to the accent color. Value is cost if any cost is present, otherwise request count.
- Footer: "N active days · longest streak M days".

**Details tab:**
- One block per provider, with a table `Limit | Account | Left | Resets` (rows grouped by limit; resets read "resets in 2h13m").
- Above it: "Models with usage data" (the full `provider/model` list from `getUsageReportingModelSelectors`) and a "Resets · account" fact line.

---

## /context

### Q4. Implementation

- **Command:** `name: "context"` in `builtin-session.ts`. The TUI calls `ctx.handleContextCommand()`:
```js
const t = jY(session, { snapcompactSavings: true });   // computeSessionContextBreakdown
if (t.contextWindow <= 0) showWarning("Context usage is unavailable: no model is selected for this session.");
showCommandReport({ title: "Context Usage", head: kYe(t), body: new ARe(t, theme) })
```
- **Non-TUI report:** `buildContextReportText({session})` in `slash-commands/helpers/context-report.ts` prints the same breakdown as text. I called it in a sandbox session.
- **What it reports:** only 5 categories, plus autocompact buffer, free space and snapcompact savings (`RYe` / `computeContextBreakdown`, `session/context-usage-runtime.ts` and `packages/tui/src/status-line/context-usage.ts`).

| Category (`id`) | Meaning |
|---|---|
| `systemPrompt` | `systemPrompt[0]` tokens minus the skills catalog. AGENTS.md/context files, rules, memory, environment and tool-list text are rendered inside this part. |
| `systemTools` | Tool schemas: for each tool name, description, JSON parameters and examples, plus the model's tool text. Zero if the model has `toolSchemasInContext === false`. |
| `systemContext` | `systemPrompt[1..]`: tool `guidance` strings and the active-repo-context block. |
| `skills` | The skills catalog (visible skills rendered as name+description). Zero unless the `read` tool is active. |
| `messages` | `usedTokens − (skills + tools + systemContext + systemPrompt)`, clamped ≥ 0. |

- **Totals and thresholds:**
  - `usedTokens` is not the sum of the categories. It is provider-anchored: the last assistant message's reported usage, adjusted for system-prompt and tool changes since that message (`session.getContextBreakdown()`, with `anchored: true`), plus the tokenized messages after it. With no anchor it is `nonMessage + countMessages(all)`.
  - `autoCompactBufferTokens = contextWindow − thresholdTokens`, where the threshold comes from the compaction strategy. If compaction is off it is 0.
  - `freeTokens = contextWindow − used − buffer`, floored at 0.
  - `thresholdTokens` is the auto-compact trigger, as an absolute token count. In the sandbox: window 50000, threshold 33616, buffer 16384.
- **TUI grid:** a 20×10 glyph grid (200 cells; `⛁`/`⛃` categories, `⛶` free, `⛝` autocompact) beside a legend:
  - Header: `Fake One (50k context)`, `fake-1[50k]`, `1.2K/50k tokens (2.5%)`.
  - Then "Estimated usage by category" with a glyph, label, tokens and share per category, then "Free space" and "Autocompact buffer".
  - Native mode instead shows one stacked meter with a mark at the auto-compact threshold. Its summary reads "N left before auto-compaction at 67%" or "Past auto-compaction…".
- **Not reported:** per-item breakdowns, MCP, memories, rules, compaction state or cache. If the tooltip should show these, we have to compute them ourselves (see Q5).

**Tokenizer** (`packages/agent/src/tokenizer.ts`, class `Ss`; model map in `packages/catalog/src/model-tokenizer.ts`):
- It is exact BPE via the native addon when the model's `tokenizer` field is known: `claude-v3`/`v47`/`v5`/`v5-sonnet`, `qwen3`, `deepseek-v3`, `kimi-k2`, `glm5`, `o200k`. For example `claude-sonnet-5` maps to `ClaudeV5Sonnet`.
- Otherwise it estimates `ceil((utf8 bytes + 3) / 4)`, as for the sandbox model.
- Images are priced by per-model rules, and encrypted reasoning is counted when it is replayed.
- `PI_TOKENIZER_ACCURATE=1` forces exact counting even for unknown tokenizers.

### Q5. Extension access (proven in tmux interactive with fake model, a `/new` switch, and RPC)

```ts
const cu = await import("@oh-my-pi/pi-coding-agent/session/context-usage-runtime");
// exports: computeSessionContextBreakdown, getSessionCompactionBoundaries
const { AgentRegistry } = await import("@oh-my-pi/pi-coding-agent/registry/agent-registry");
const session = AgentRegistry.global().list().find((r: any) => r.kind === "main").session;
const b = cu.computeSessionContextBreakdown(session, { snapcompactSavings: true });
const raw = session.getContextBreakdown();   // cheap raw numbers
const usage = ctx.getContextUsage();         // also available on ctx: {tokens, contextWindow, percent}
```

- **Returned shape of `computeSessionContextBreakdown`:** it also includes a full `model` object, so pick fields rather than serializing it (it carries a large `compat` blob). I serialized it as follows.
```json
{ "model": {"id":"fake-1","provider":"fake","name":"Fake One","contextWindow":50000},
  "contextWindow": 50000,
  "categories": [
    {"id":"systemPrompt","label":"System prompt","tokens":2901,"color":"accent","glyph":"⛁"},
    {"id":"systemTools","label":"System tools","tokens":4380,"color":"warning","glyph":"⛁"},
    {"id":"systemContext","label":"System context","tokens":116,"color":"customMessageLabel","glyph":"⛁"},
    {"id":"skills","label":"Skills","tokens":0,"color":"success","glyph":"⛁"},
    {"id":"messages","label":"Messages","tokens":0,"color":"userMessageText","glyph":"⛃"} ],
  "usedTokens": 1234, "autoCompactBufferTokens": 16384, "freeTokens": 32382, "thresholdTokens": 33616 }
```
  `thresholdTokens` is `undefined` when auto-compaction is off. `snapcompact` is present only when the model/settings enable it, as `{visionCapable, systemPrompt?: {scope, applied, textTokens, frames, imageTokens, savedTokens, reason?}, toolResults?: {swapped, total, textTokens, frames, imageTokens, savedTokens}, savedTokens}`. It was absent in the sandbox, so that shape comes from the bundle only.
- **`session.getContextBreakdown()` returns:** `{contextWindow, anchored, usedTokens, systemPromptTokens, systemToolsTokens, systemContextTokens, skillsTokens, messagesTokens}`.
- **Fresh session at start:** `anchored:false`, `usedTokens:7397` (system prompt 2901 + tools 4380 + context 116). The status line showed 15%.
- **After a turn:** `usedTokens:1234`, `anchored:true`, and the `/context` TUI showed `1.2K/50k (2.5%)` matching the JSON.
- **Messages artifact:** `messagesTokens` is 0 because the fake provider reported `prompt_tokens=1234`, which is below the static system total. Categories don't sum to total here. Real providers report prompt tokens above the static sum. The UI should show `usedTokens` as the headline and categories as-is.
- **After `/new`:** the main record stayed and its `session` reflected the new session file (`s3_switch_new`: `used:7397`, `msgs:0`, new jsonl). The registry lookup is safe across `/new`.
- **Cost:**
  - First call 5.9 ms (cold tokenizer caches); later calls 0.1–0.24 ms; a 200-call average of 0.009 ms. Results are cached by WeakMap keyed on the system prompt, tools and skills refs, and per message.
  - Exact-BPE worst case: with the model on `ClaudeV5Sonnet` and a synthetic 2000-message history, `countMessages` took 225 ms cold and 125 ms warm. These were uncached fake messages; real sessions use the per-message cache. After the first assistant response only the tail after the anchor is tokenized, so refreshing after every `agent_end` or `session_switch` is cheap.
- **Extras for a richer tooltip.** `@oh-my-pi/pi-tui/status-line/context-usage` exports `computeNonMessageBreakdown, computeNonMessageTokens, estimateToolSchemaTokens, estimateSkillsTokens, computeContextBreakdown, …`, and `session.agent.tokenizer.countTokens([...])` is available. With these the extension can add what `/context` does not show:
  - Per-tool schema tokens. Sandbox sample: `task` 881, `eval` 653, `read` 553, `edit` 519, `todo` 436, `bash` 361, `grep` 156, `web_search` 127 (11 tools).
  - Per system-prompt-part tokens. Sample: part 0 was 10169 chars / 2559 tokens, part 1 (`<project-context>` block) 461 chars / 116 tokens.
  - Skill count: `session.skills.length`.
- **Wire design:** push `{usedTokens, contextWindow, categories[{id,tokens}], autoCompactBufferTokens, freeTokens, thresholdTokens, anchored}` on `agent_end`, `session_switch` and `session_start`, plus an on-demand op. That is a trivial payload; the 100 ms debounce used in `snapshot()` is plenty.

### Q6. Headless resume

- **Resuming the real file writes to it.** `omp --mode rpc --resume <file>` appended a `session_exit` custom entry on dispose: `{"type":"custom","customType":"session_exit","data":{"reason":"dispose","kind":"normal",…}}`. Size went from 1890 to 2129 bytes and the md5 changed.
- **Resuming a copy is side-effect free for the original.** I copied the jsonl, resumed the copy (`--resume /tmp/…/copy/s.jsonl --no-extensions -e y.ts`, `get_state`), and the original's md5 stayed identical. No new files appeared in `agent/sessions`.
- **RPC does expose context usage.** `get_state` returns `contextUsage:{tokens,contextWindow,percent}`. For the full breakdown, load an `-e` extension that calls `computeSessionContextBreakdown` as above. About 5 ms for the compute, plus process startup.
- **The numbers differ from the interactive session.** RPC mode has a different system prompt and tool set. Resumed `usedTokens` was 881 versus 1234 live, and the difference matches the tool-token delta (4380 − 4027 = 353). Treat headless numbers as an estimate. For closed sessions the only exact value is the last assistant message's `usage` in the jsonl.
- **Recommendation:** for closed sessions show the last assistant `usage.input` (plus cache read/write) from the jsonl as the context estimate, and do not compute a category breakdown. Use headless-copy resume only when the user opens a closed-session tooltip. The usual cost is process spawn plus loading omp.

---

## Summary of recommended mechanisms

| Feature | Live tab | No extension / closed session |
|---|---|---|
| `/usage` quotas | extension op returning `session.cachedUsageReports ?? await fetchUsageReports()`, `raw` stripped; render with `buildProviderCards` logic | `omp usage --json` subprocess (0.3–10 s; reads `agent.db`, writes `usage_history`) |
| `/usage` activity | read `~/.omp/stats.db` `messages` | same |
| `/context` tooltip | extension pushes `computeSessionContextBreakdown` fields after each turn | jsonl last-assistant `usage`, or resume a copy of the session with an `-e` extension |

**[UNVERIFIED]:**
- Real-provider latency and responses for any provider other than the fake Anthropic one.
- Codex's report shape.
- The `snapcompact` branch of the breakdown (absent for the sandbox model).
- Reading the real `~/.omp/stats.db` read-only while omp has it open in WAL mode. Verify with `sqlite3 'file:~/.omp/stats.db?mode=ro'` on a copy.

**Cleanup:** the tmux server and socket, the fake server (port 18955), `/tmp/ompusage-sb` and `/tmp/ompstr.txt` are all removed. Nothing was written to the repo or the real `~/.omp`.
## Sandbox addenda (wave 9 implementation)

- omp looks for natives at `$HOME/.omp/natives` regardless of `PI_CONFIG_DIR`. In a sandbox with a temp
  HOME, symlink `<tmpHOME>/.omp/natives` → the real `~/.omp/natives` (read-only use). A wrong path makes
  omp re-extract (touches the real dir's mtime); never point it anywhere else.
- Fake OAuth credential: `agent.db` table `auth_credentials(provider, credential_type='oauth', data,
  identity_key)`, `data` JSON with stored names `access`, `refresh`, `expires`, `accountId`, `email`
  (camelCase `accessToken`/`expiresAt` → account lands in `accountsWithoutUsage`).
- Anthropic usage payload: `five_hour`, `seven_day`, `seven_day_sonnet` as `{utilization, resets_at}`,
  plus `extra_usage {is_enabled, monthly_limit, used_credits, currency}` (minor units).
- A sandbox omp creates no `stats.db` without the `/usage` dashboard sync; build a WAL-mode db with the
  `messages` columns for tests.
- The ctl extension's breakdown recomputes on session_start/switch, agent_end, turn_end, model_select,
  session_compact, plus a model-key check on the 1 s tick; `/switch` reflected within ~0.6 s.
