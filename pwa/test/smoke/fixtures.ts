// Deterministic fixture data for the smoke gateway (test/smoke/gateway.ts): every v1 and slice-4
// feed and op answer, shaped like a busy real desktop. Times are relative to `now` so relative
// ages ("2 min ago") read naturally; everything else is fixed.

import type { api } from '../../src/remote/index.ts';

const MIN = 60_000;
const HOUR = 60 * MIN;
const DAY = 24 * HOUR;
const MIB = 1024 * 1024;

export const PROJECTS = {
  ompdeck: { id: 'p-ompdeck', display: '~/PersProject/OmpDeck' },
  website: { id: 'p-website', display: '~/code/website' },
  infra: { id: 'p-infra', display: '~/infra/homelab' },
  wt: { id: 'p-ompdeck-wt', display: '~/PersProject/OmpDeck-wt/feat-push' },
} as const;

/** A live session the fixtures run, keyed by its omp session id. */
export interface LiveSpec {
  session: string;
  title: string;
  project: keyof typeof PROJECTS;
  state: 'idle' | 'busy' | 'attention' | 'exited';
  model: string;
  ui: api.UiRequestView | null;
  tmux: string;
}

function ui(request: string, kind: api.UiKind, title: string, patch: Partial<api.UiRequestView>): api.UiRequestView {
  return {
    request,
    kind,
    title,
    message: null,
    timeout_ms: null,
    options: [],
    multi: false,
    initial: null,
    prefill: null,
    questions: [],
    approve: null,
    ...patch,
  };
}

const option = (label: string, description: string | null = null): api.UiOption => ({ label, description });

/** One pending UI request of each kind (the `api-s4.json` shapes, with realistic text). */
export const UI_REQUESTS: Readonly<Record<'select' | 'multi' | 'confirm' | 'input' | 'editor' | 'ask' | 'approve', api.UiRequestView>> = {
  select: ui('u-11', 'select', 'Which branch should the release build from?', {
    message: 'The release workflow needs a source branch.',
    timeout_ms: 30_000,
    options: [option('main', 'Default branch, last CI green 12 min ago'), option('mobile-relay', '14 commits ahead of main'), option('release/0.10')],
    initial: 0,
  }),
  multi: ui('u-12', 'select', 'Pick the crates to bump', {
    options: [option('deck-core'), option('deck-remote', 'Wire changes since 0.9'), option('deck-term'), option('omp-deck-relay')],
    multi: true,
  }),
  confirm: ui('u-13', 'confirm', 'Delete 3 stale worktrees?', {
    message: 'feat-old-sidebar, spike-wgpu and tmp-bench have been merged and are 21+ days old.',
  }),
  input: ui('u-14', 'input', 'Name for the new tag', { message: 'e.g. v0.10.0', prefill: 'v0.10.0' }),
  editor: ui('u-15', 'editor', 'Edit the commit message', {
    prefill: 'feat(remote): slice-4 transfers\n\n- pull-based downloads\n- chunked uploads with quotas\n',
  }),
  ask: ui('u-16', 'ask', 'Ask', {
    questions: [
      {
        id: 'scope',
        question: 'Which scope should the migration cover?',
        header: 'Scope',
        options: [option('Gateway only', 'Leaves the desktop UI on the old path'), option('Gateway + desktop', 'Clean cutover, one implementation'), option('Everything incl. relay')],
        multi: false,
        recommended: 1,
      },
      {
        id: 'extras',
        question: 'Also do any of these?',
        header: 'Extras',
        options: [option('Update docs'), option('Add e2e coverage', 'Playwright against the smoke harness'), option('Bump versions')],
        multi: true,
        recommended: null,
      },
    ],
  }),
  approve: ui('u-17', 'approve', 'Approval', {
    message: 'omp wants to run a command outside the workspace.',
    approve: {
      full: 'bash: rm -rf ~/.cache/ompdeck/build-artifacts && cargo clean -p deck-remote',
      compact: 'rm -rf ~/.cache/ompdeck/build-artifacts …',
      session_label: 'Allow bash for this session',
      grants: [
        { scope: 'bash:rm', label: 'Allow rm in this session', always_label: 'Always allow rm' },
        { scope: 'bash:cargo', label: 'Allow cargo in this session', always_label: 'Always allow cargo' },
      ],
    },
  }),
};

export const LIVE: readonly LiveSpec[] = [
  { session: 's-convo', title: 'Ship mobile relay', project: 'ompdeck', state: 'busy', model: 'anthropic/claude-opus-4-5', ui: null, tmux: 'omp-1a2b' },
  { session: 's-idle', title: 'Refactor tmux launcher', project: 'ompdeck', state: 'idle', model: 'anthropic/claude-sonnet-4-5', ui: null, tmux: 'omp-3c4d' },
  { session: 's-ui-select', title: 'Cut the 0.10 release', project: 'ompdeck', state: 'attention', model: 'openai/gpt-5.1', ui: UI_REQUESTS.select, tmux: 'omp-5e6f' },
  { session: 's-ui-multi', title: 'Version bumps', project: 'ompdeck', state: 'attention', model: 'anthropic/claude-sonnet-4-5', ui: UI_REQUESTS.multi, tmux: 'omp-7081' },
  { session: 's-ui-confirm', title: 'Clean old worktrees', project: 'ompdeck', state: 'attention', model: 'anthropic/claude-haiku-4-5', ui: UI_REQUESTS.confirm, tmux: 'omp-9a0b' },
  { session: 's-ui-input', title: 'Tag the release', project: 'website', state: 'attention', model: 'anthropic/claude-sonnet-4-5', ui: UI_REQUESTS.input, tmux: 'omp-c1d2' },
  { session: 's-ui-editor', title: 'Write the changelog', project: 'website', state: 'attention', model: 'google/gemini-3-pro', ui: UI_REQUESTS.editor, tmux: 'omp-e3f4' },
  { session: 's-ui-ask', title: 'Plan the ctl migration', project: 'infra', state: 'attention', model: 'anthropic/claude-opus-4-5', ui: UI_REQUESTS.ask, tmux: 'omp-0516' },
  { session: 's-ui-approve', title: 'Free disk space', project: 'infra', state: 'attention', model: 'anthropic/claude-sonnet-4-5', ui: UI_REQUESTS.approve, tmux: 'omp-2738' },
  { session: 's-exited', title: 'Flaky test hunt', project: 'wt', state: 'exited', model: 'openai/gpt-5.1', ui: null, tmux: 'omp-495a' },
];

export function liveRef(spec: LiveSpec): api.LiveRef {
  return { key: `tmux:8:omp-deck:${spec.tmux}`, generation: 1 };
}

export function liveSession(spec: LiveSpec, requests: api.OwnRequest[] = []): api.LiveSession {
  const project = PROJECTS[spec.project];
  const status: api.Status = spec.state === 'exited' ? { state: 'exited', clean: false } : { state: spec.state };
  const out: api.LiveSession = {
    live: liveRef(spec),
    kind: 'session',
    label: spec.title,
    title: spec.title,
    session: spec.session,
    project: project.id,
    display: project.display,
    activity: spec.state === 'exited' ? 'unknown' : spec.state,
    status,
    exit_code: spec.state === 'exited' ? 1 : null,
    busy: spec.state === 'busy',
    model: spec.model,
    pending_switch: null,
    live_control: spec.state !== 'exited',
    desktop_controlled: spec.session === 's-convo',
    views: spec.session === 's-convo' ? 1 : 0,
    requests,
  };
  if (spec.ui !== null) out.pending_ui = [spec.ui];
  return out;
}

const HISTORY: Readonly<Record<string, readonly [string, string | null, string | null, number, boolean?][]>> = {
  'p-ompdeck': [
    ['s-convo', 'Ship mobile relay', 'Build slice 4 of the phone companion', 2 * MIN],
    ['s-idle', 'Refactor tmux launcher', 'Split launch command building out of deck-core', 9 * MIN],
    ['s-ui-select', 'Cut the 0.10 release', null, 14 * MIN],
    ['s-ui-multi', 'Version bumps', null, 31 * MIN],
    ['s-ui-confirm', 'Clean old worktrees', null, 48 * MIN],
    ['s-old-1', 'Theme mockups round 3', 'Make five more dark theme mockups, cutting-edge look', 5 * HOUR],
    ['s-old-2', null, 'why does the sidebar flicker when sessions refresh?', 26 * HOUR],
    ['s-old-3', 'Notification sounds', 'Add a sound when a turn finishes', 3 * DAY],
    ['s-stub-1', null, null, 4 * DAY, true],
    ['s-old-4', 'Backup restore conflicts', 'Restoring onto a folder with the same session ids duplicates them', 9 * DAY],
  ],
  'p-website': [
    ['s-ui-input', 'Tag the release', 'Prepare the release notes page', 22 * MIN],
    ['s-ui-editor', 'Write the changelog', null, 40 * MIN],
    ['s-web-1', 'Blog: mobile companion', 'Draft a post about the relay design', 2 * DAY],
  ],
  'p-infra': [
    ['s-ui-ask', 'Plan the ctl migration', null, 18 * MIN],
    ['s-ui-approve', 'Free disk space', 'The CI runner disk is at 94%', 35 * MIN],
    ['s-infra-1', 'Caddy config for the relay', 'Terminate TLS for omp-deck-relay', 6 * DAY],
  ],
  'p-ompdeck-wt': [
    ['s-exited', 'Flaky test hunt', 'tmux_alive test fails one run in twenty', 3 * HOUR],
    ['s-wt-1', 'Push subscription storage', null, 7 * HOUR],
  ],
};

export function projects(now: number): api.Project[] {
  return Object.values(PROJECTS).map((p) => ({
    id: p.id,
    display: p.display,
    worktree: p.id === PROJECTS.wt.id,
    session_count: HISTORY[p.id]?.length ?? 0,
    modified_unix_ms: now - Math.min(...(HISTORY[p.id] ?? [[0, 0, 0, 0]]).map((row) => row[3])),
  }));
}

export function sessions(now: number, project: string, limit: number): Extract<api.FeedValue, { feed: 'sessions' }> {
  const rows = HISTORY[project] ?? [];
  const summaries: api.SessionSummary[] = rows.slice(0, limit).map(([id, title, first, age, stub]) => {
    const summary: api.SessionSummary = { id, title, first_prompt: first, modified_unix_ms: now - age };
    if (stub === true) summary.stub = true;
    return summary;
  });
  return { feed: 'sessions', project, total: rows.length, sessions: summaries, truncated: false };
}

/** Every session id the fixtures know, with its project and title. */
export function allSessions(): { id: string; project: string; title: string | null }[] {
  return Object.entries(HISTORY).flatMap(([project, rows]) => rows.map(([id, title]) => ({ id, project, title })));
}

export function inbox(now: number): api.Inbox {
  const entry = (spec: LiveSpec, kind: api.InboxKind, body: string, age: number, armed = true): api.InboxItem => ({
    live: liveRef(spec),
    kind,
    title: spec.title,
    project: PROJECTS[spec.project].display,
    body,
    at_unix_ms: now - age,
    armed,
  });
  const by = (id: string): LiveSpec => LIVE.find((spec) => spec.session === id) as LiveSpec;
  return {
    entries: [
      entry(by('s-ui-approve'), { type: 'attention' }, 'Needs approval: rm -rf ~/.cache/ompdeck/build-artifacts', 1 * MIN),
      entry(by('s-idle'), { type: 'finished' }, 'Split launch.rs into command.rs and config.rs; 14 tests pass.', 9 * MIN),
      entry(by('s-exited'), { type: 'exited', code: 1 }, 'omp exited with code 1', 3 * HOUR, false),
      entry(by('s-ui-input'), { type: 'error', message: 'rate limited by the provider (429)' }, 'The last turn failed.', 4 * HOUR, false),
    ],
    warns: true,
    badge: '2',
  };
}

export function card(session: string): api.Card {
  const spec = LIVE.find((s) => s.session === session);
  const seed = [...session].reduce((sum, c) => sum + c.charCodeAt(0), 0);
  return {
    session,
    model: spec?.model ?? 'anthropic/claude-sonnet-4-5',
    thinking: seed % 2 === 0 ? 'high' : 'medium',
    context_tokens: 40_000 + (seed % 97) * 1_100,
    input_tokens: 180_000 + seed * 310,
    output_tokens: 22_000 + seed * 41,
    cost: Math.round((1.2 + (seed % 23) * 0.37) * 100) / 100,
    last_reply:
      session === 's-convo'
        ? 'Uploads now stream in 512 KiB chunks; the gateway writes them owner-only and quotas are enforced per device. Next: wiring compose attachments.'
        : 'Done. All 14 tests pass and clippy is clean.',
    last_turn_failed: session === 's-ui-input',
    git: { branch: session === 's-exited' ? 'feat-push' : 'mobile-relay', ahead: 3, behind: 1, staged: 2, unstaged: 4, untracked: 1 },
    agents: session === 's-convo' ? { running: 2, done: 3, failed: 1, total: 6 } : { running: 0, done: seed % 3, failed: 0, total: seed % 3 },
  };
}

// ---- transcript ---------------------------------------------------------------------------------

export const IMAGE_BLOB = 'b-screenshot-1';
export const TOOL_IMAGE_BLOB = 'b-screenshot-2';

export function transcript(now: number, session: string, agent: string | null): api.Item[] {
  if (agent !== null) {
    return [
      { type: 'user', text: `Investigate why ${agent} reports flaky reconnects.`, from_agent: true, at_unix_ms: now - 20 * MIN },
      { type: 'thinking', text: 'Start with the relay logs, then the retry policy in client.ts.' },
      {
        type: 'tool_call',
        call_id: 'a1',
        name: 'grep',
        intent: 'Find the retry policy',
        summary: 'grep decideRetry pwa/src',
        args: '{"pattern":"decideRetry","path":"pwa/src"}',
        args_value: { pattern: 'decideRetry', path: 'pwa/src' },
      },
      { type: 'tool_result', call_id: 'a1', name: 'grep', text: 'pwa/src/app/retry.ts:31: export function decideRetry(', is_error: false, truncated: false },
      { type: 'assistant', text: 'The backoff resets only after a session opened; a relay that refuses admission keeps the attempt counter climbing. That is intended.', model: 'anthropic/claude-sonnet-4-5', at_unix_ms: now - 18 * MIN },
    ];
  }
  const title = LIVE.find((s) => s.session === session)?.title ?? 'this session';
  const items: api.Item[] = [
    {
      type: 'user',
      text: `Let's continue: ${title}. Here is what the phone shows right now.`,
      from_agent: false,
      at_unix_ms: now - 42 * MIN,
      images: [{ blob: IMAGE_BLOB, mime: 'image/png', bytes: 17_524 }],
    },
    { type: 'thinking', text: 'The screenshot shows the sessions list without running indicators. Check the live feed wiring first, then the uploads.' },
    {
      type: 'assistant',
      text: 'I will look at the **live feed** wiring and then finish the upload path. Plan:\n\n1. Read `client.ts` feed handling\n2. Patch `transfer.ts`\n3. Run `bun test`',
      model: 'anthropic/claude-opus-4-5',
      at_unix_ms: now - 41 * MIN,
    },
    {
      type: 'tool_call',
      call_id: 'c1',
      name: 'read',
      intent: 'Reading the client feed handling',
      summary: 'read pwa/src/app/client.ts:780-850',
      args: '{"path":"pwa/src/app/client.ts:780-850"}',
      args_value: { path: 'pwa/src/app/client.ts:780-850' },
    },
    {
      type: 'tool_result',
      call_id: 'c1',
      name: 'read',
      text: "  #onFeed(feed: api.Feed, value: api.FeedValue): void {\n    const key = feedKey(feed);\n    if (!this.#subs.has(key)) return;\n    …",
      is_error: false,
      truncated: true,
    },
    {
      type: 'tool_call',
      call_id: 'c2',
      name: 'edit',
      intent: 'Patching the upload chunking',
      summary: 'edit pwa/src/app/transfer.ts',
      args: '{"path":"pwa/src/app/transfer.ts","old_text":"const CHUNK = 256 * 1024;","new_text":"const CHUNK = api.MAX_TRANSFER_CHUNK;"}',
      args_value: { path: 'pwa/src/app/transfer.ts', old_text: 'const CHUNK = 256 * 1024;', new_text: 'const CHUNK = api.MAX_TRANSFER_CHUNK;' },
    },
    {
      type: 'tool_result',
      call_id: 'c2',
      name: 'edit',
      text: 'Edited pwa/src/app/transfer.ts (+1 −1)',
      is_error: false,
      truncated: false,
      details: {
        diff: '@@ -12,7 +12,7 @@\n import { api } from \'../remote/index.ts\';\n \n-const CHUNK = 256 * 1024;\n+const CHUNK = api.MAX_TRANSFER_CHUNK;\n \n export interface Progress {\n',
        firstChangedLine: 14,
        path: 'pwa/src/app/transfer.ts',
      },
    },
    {
      type: 'tool_call',
      call_id: 'c3',
      name: 'bash',
      intent: 'Running the unit tests',
      summary: 'bun test test/transfer.test.ts',
      args: '{"command":"bun test test/transfer.test.ts","cwd":"pwa","timeout":120}',
      args_value: { command: 'bun test test/transfer.test.ts', cwd: 'pwa', timeout: 120 },
    },
    {
      type: 'tool_result',
      call_id: 'c3',
      name: 'bash',
      text: 'bun test v1.3.4\n\ntest/transfer.test.ts:\n✓ download reassembles chunks [2.1ms]\n✓ upload sends ≤512 KiB chunks [1.4ms]\n✗ upload cancels on abort [0.9ms]\n\n 2 pass\n 1 fail',
      is_error: true,
      truncated: false,
      details: { exitCode: 1, durationMs: 1840 },
    },
    {
      type: 'tool_call',
      call_id: 'c4',
      name: 'browser',
      intent: 'Capturing the phone screen',
      summary: 'screenshot 390×844',
      args: '{"action":"screenshot","width":390,"height":844}',
      args_value: { action: 'screenshot', width: 390, height: 844 },
    },
    {
      type: 'tool_result',
      call_id: 'c4',
      name: 'browser',
      text: 'Captured 390×844.',
      is_error: false,
      truncated: false,
      images: [{ blob: TOOL_IMAGE_BLOB, mime: 'image/png', bytes: 71_603 }],
    },
    { type: 'irc', from: 'PwaFleet', agent: 'PwaFleet', text: 'The running feed now carries memory per session.', note: null },
    { type: 'compaction', tokens_before: 182_000, tokens_after: 41_000, summary: 'Earlier: wire types for slice 4, vectors, and the gateway feed plumbing.' },
    { type: 'model_change', model: 'anthropic/claude-opus-4-5' },
    {
      type: 'assistant',
      text: 'The abort test failed because `upload_cancel` was sent after the promise settled. Fixed: the cancel goes out first, then the promise rejects with an `AbortError`.\n\n```ts\nsignal.addEventListener("abort", () => void client.request({ op: "upload_cancel", upload }));\n```',
      model: 'anthropic/claude-opus-4-5',
      at_unix_ms: now - 3 * MIN,
    },
    { type: 'notice', kind: 'info', text: 'Context 62% full' },
  ];
  if (session === 's-exited') items.push({ type: 'error', message: 'provider returned 500' }, { type: 'exit', reason: 'omp exited with code 1' });
  return items;
}

export function agents(now: number, session: string): api.AgentView[] {
  const agent = (id: string, parent: string | null, depth: number, label: string, status: api.AgentStatus, patch: Partial<api.AgentView> = {}): api.AgentView => ({
    id,
    parent,
    depth,
    label,
    agent: 'task',
    status,
    model: 'anthropic/claude-sonnet-4-5',
    thinking: 'medium',
    error: null,
    current: status === 'running' ? 'Reading src/app/client.ts' : null,
    created_unix_ms: now - 40 * MIN,
    last_activity_unix_ms: now - (status === 'running' ? 20_000 : 12 * MIN),
    duration_ms: status === 'running' ? null : 14 * MIN,
    requests: 18,
    tool_calls: 41,
    tokens: 312_000,
    cost: 0.84,
    context_tokens: 58_000,
    has_output: status !== 'running',
    live: status === 'running',
    ...patch,
  });
  if (session !== 's-convo') return [agent('PwaWorker', null, 0, 'Worker', 'done')];
  return [
    agent('PwaCore', null, 0, 'PWA foundation', 'done', { agent: 'implement-heavy', cost: 6.12, tokens: 2_400_000 }),
    agent('PwaHarness', null, 0, 'Smoke harness + plumbing', 'running', { agent: 'implement-heavy' }),
    agent('PwaHarness.Scout', 'PwaHarness', 1, 'Find feed wiring', 'done', { agent: 'scout', model: 'anthropic/claude-haiku-4-5', cost: 0.07 }),
    agent('PwaConvo', null, 0, 'Conversation screens', 'running', { agent: 'implement-heavy' }),
    agent('GwPush', null, 0, 'Gateway push', 'failed', { error: 'cargo test: 2 failures in push::tests', cost: 2.31 }),
    agent('GwDevice', null, 0, 'Device ops', 'parked', { current: null }),
  ];
}

export function context(spec: LiveSpec, now: number): api.ContextView {
  return {
    model: spec.model,
    context_window: 200_000,
    used_tokens: 124_300,
    anchored: true,
    categories: [
      { id: 'system', label: 'System prompt', tokens: 9_800 },
      { id: 'tools', label: 'Tools', tokens: 14_200 },
      { id: 'skills', label: 'Skills', tokens: 3_100 },
      { id: 'messages', label: 'Messages', tokens: 97_200 },
    ],
    auto_compact_buffer: 13_000,
    free_tokens: 62_700,
    threshold_tokens: 160_000,
    snapcompact_saved: 21_400,
    tools: [
      { name: 'bash', tokens: 3_400 },
      { name: 'edit', tokens: 2_900 },
      { name: 'read', tokens: 2_100 },
      { name: 'task', tokens: 4_100 },
      { name: 'grep', tokens: 1_700 },
    ],
    skills: 6,
    computed_unix_ms: now - 40_000,
  };
}

// ---- changes ------------------------------------------------------------------------------------

export function changes(project: string): Extract<api.FeedValue, { feed: 'changes' }> {
  if (project === PROJECTS.infra.id) return { feed: 'changes', changes: null, error: 'not a git repository', truncated: false };
  const file = (path: string, patch: Partial<api.ChangedFile>): api.ChangedFile => ({
    path,
    orig_path: null,
    staged: null,
    unstaged: null,
    untracked: false,
    conflicted: false,
    is_dir: false,
    ...patch,
  });
  return {
    feed: 'changes',
    changes: {
      branch: 'mobile-relay',
      files: [
        file('pwa/src/app/client.ts', { staged: 'modified', unstaged: 'modified' }),
        file('pwa/src/app/transfer.ts', { staged: 'added' }),
        file('pwa/src/app/nav.ts', { unstaged: 'modified' }),
        file('crates/deck-remote/src/api.rs', { unstaged: 'modified' }),
        file('docs/notes/mobile-pwa.md', { unstaged: 'modified' }),
        file('pwa/src/app/push-client.ts', { orig_path: 'pwa/src/app/push.ts', staged: 'renamed' }),
        file('pwa/legacy/old-pairing.tsx', { unstaged: 'deleted' }),
        file('pwa/test/smoke/', { untracked: true, is_dir: true }),
        file('Cargo.lock', { conflicted: true, unstaged: 'modified' }),
      ],
    },
    error: null,
    truncated: false,
  };
}

export const DIFF_TEXT = `diff --git a/pwa/src/app/client.ts b/pwa/src/app/client.ts
index 3f2a9c1..8b7d4e0 100644
--- a/pwa/src/app/client.ts
+++ b/pwa/src/app/client.ts
@@ -24,11 +24,14 @@ import type { DialTarget, Dialer, Link, LinkClose, PairingResult, PairingStage } f
 import type { StoredPairing } from './keystore.ts';
 import { decideRetry, type Stop } from './retry.ts';
 
-/**
- * Optional features this app announces in \`DeviceHello.features\`. Slice 4 announces "s4" once
- * the gateway side of it exists; until then none.
- */
-export const DEVICE_FEATURES: readonly string[] = [];
+/** Optional features this app announces in \`DeviceHello.features\`. */
+export const DEVICE_FEATURES: readonly string[] = [api.FEATURE_S4];
+
+/** Whether the gateway behind \`welcome\` speaks slice 4. */
+export function hasS4(welcome: api.Welcome | null): boolean {
+  return welcome !== null && welcome.features.includes(api.FEATURE_S4);
+}
 
 /** Recently finished requests kept for display. */
 const RECENT_REQUESTS = 20;
@@ -760,8 +763,9 @@ export class Client {
     this.#setConn({ phase: 'online', attempt: 0, retryAt: null, stop: null, everOnline: true });
     this.#rateLimitedSince = null;
     for (const { feed } of this.#subs.values()) this.#sendSubscribe(feed);
+    this.#sendPresence();
     this.#recoverOutcomes();
   }
 
@@ -1111,6 +1115,12 @@ export class Client {
   #sendSubscribe(feed: api.Feed): void {
+    // An older desktop does not know slice-4 feeds.
+    if (!this.s4 && isS4Feed(feed)) return;
     this.request({ op: 'subscribe', feed }).catch((error: unknown) => {
       if (error instanceof ClientError && error.kind === 'api') this.notify(error.message, 'warn');
     });
`;

export function diff(): api.Diff {
  return { text: DIFF_TEXT, binary: false, truncated: false, added: 11, removed: 5 };
}

// ---- fleet --------------------------------------------------------------------------------------

export function running(now: number): Extract<api.FeedValue, { feed: 'running' }> {
  const rows: api.RunningView[] = LIVE.filter((spec) => spec.state !== 'exited').map((spec, index) => ({
    id: `r-1a2b3c4d5e6f70${index.toString(16).padStart(2, '0')}`,
    sessions: [spec.session],
    project: PROJECTS[spec.project].id,
    display: PROJECTS[spec.project].display,
    title: spec.title,
    backend: 'tmux',
    started_unix_ms: now - (index + 1) * 17 * MIN,
    pid: 41_200 + index * 37,
    memory_bytes: Math.round((180 + ((index * 73) % 410)) * MIB),
    processes: 3 + (index % 4),
    live: liveRef(spec),
  }));
  rows.push({
    id: 'r-00ff00ff00ff00ff',
    sessions: [],
    project: null,
    display: null,
    title: null,
    backend: 'tmux',
    started_unix_ms: now - 2 * DAY,
    pid: 9_001,
    memory_bytes: 38 * MIB,
    processes: 1,
    live: null,
  });
  return { feed: 'running', sessions: rows, server_bytes: 12 * MIB, truncated: false };
}

export function inventory(now: number, project: string | null, limit: number): Extract<api.FeedValue, { feed: 'inventory' }> {
  const live = new Set(LIVE.filter((s) => s.state !== 'exited').map((s) => s.session));
  const all: api.InventoryView[] = Object.entries(HISTORY).flatMap(([projectId, rows]) =>
    rows.map(([id, title, first, age, stub], index): api.InventoryView => {
      const p = Object.values(PROJECTS).find((x) => x.id === projectId) as { id: string; display: string };
      const isStub = stub === true;
      return {
        id,
        project: projectId,
        display: p.display,
        title,
        first_prompt: first,
        stub: isStub,
        created_unix_ms: now - age - 2 * HOUR,
        modified_unix_ms: now - age,
        jsonl_bytes: isStub ? 812 : 240_000 + index * 913_000,
        artifact_bytes: isStub ? 0 : 1_200_000 + index * 3_400_000,
        artifact_files: isStub ? 0 : 4 + index * 3,
        subagents: id === 's-convo' ? 6 : index % 3,
        parent: id === 's-wt-1' ? 's-convo' : null,
        cwd_exists: projectId !== PROJECTS.wt.id || id !== 's-wt-1',
        running: live.has(id),
        stats: isStub
          ? null
          : {
              user_messages: 6 + index * 4,
              assistant_turns: 11 + index * 9,
              input_tokens: 820_000 + index * 120_000,
              output_tokens: 64_000 + index * 9_000,
              cache_read_tokens: 5_400_000 + index * 800_000,
              cache_write_tokens: 410_000,
              cost: Math.round((3.1 + index * 1.7) * 100) / 100,
              subagent_cost: id === 's-convo' ? 12.4 : null,
              model: 'anthropic/claude-opus-4-5',
              compactions: index % 3,
              last_activity_unix_ms: now - age,
            },
      };
    }),
  );
  const filtered = project === null ? all : all.filter((entry) => entry.project === project);
  filtered.sort((a, b) => b.modified_unix_ms - a.modified_unix_ms);
  return { feed: 'inventory', project, total: filtered.length, entries: filtered.slice(0, limit), truncated: false };
}

export function worktrees(now: number, project: string): Extract<api.FeedValue, { feed: 'worktrees' }> {
  const linked = project === PROJECTS.wt.id;
  const parked: api.ParkedView[] =
    project === PROJECTS.ompdeck.id || linked
      ? [
          { id: 'k-1', branch: 'feat-push', base: 'main', sessions: 2, parked_unix_ms: now - 3 * HOUR, exists: true, merged: false, checked_out: linked },
          { id: 'k-2', branch: 'spike-wgpu', base: 'main', sessions: 1, parked_unix_ms: now - 6 * DAY, exists: true, merged: true, checked_out: false },
          { id: 'k-3', branch: 'feat-old-sidebar', base: 'main', sessions: 4, parked_unix_ms: now - 21 * DAY, exists: false, merged: true, checked_out: false },
        ]
      : [];
  return {
    feed: 'worktrees',
    project,
    worktree: {
      linked,
      branch: linked ? 'feat-push' : 'mobile-relay',
      base: linked ? 'main' : null,
      primary: linked ? PROJECTS.ompdeck.id : null,
      primary_display: PROJECTS.ompdeck.display,
      branches: ['main', 'mobile-relay', 'feat-push', 'release/0.10', 'spike-wgpu'],
      has_commits: true,
    },
    parked,
    truncated: false,
  };
}

const TREE_TEXTS: readonly [api.TreeNodeKind, string][] = [
  ['user', 'Build slice 4 of the phone companion'],
  ['assistant', 'Reading the contract in docs/notes/mobile-pwa.md first.'],
  ['tool_result', 'read docs/notes/mobile-pwa.md (412 lines)'],
  ['assistant', 'Plan: wire types, vectors, gateway ops, PWA screens.'],
  ['model_change', 'anthropic/claude-opus-4-5'],
  ['user', 'Start with the wire types'],
  ['assistant', 'Added FEATURE_S4 and every s4 op to api.rs.'],
  ['bash_execution', 'cargo test -p deck-remote'],
  ['compaction', 'Compacted 182k → 41k tokens'],
  ['label', 'checkpoint: wire done'],
  ['user', 'Now the transfers'],
  ['assistant', 'Downloads are pull-based JSON chunks of 512 KiB.'],
  ['thinking_change', 'high'],
  ['user', 'Add uploads too'],
  ['assistant', 'upload_begin / upload_chunk / upload_cancel are in.'],
];

export function tree(now: number, session: string, filter: api.TreeFilter, cursor: string | null, limit: number): Extract<api.Resp, { type: 'tree' }> {
  // Two branches: the main line, and an alternative continuation forked at entry 5.
  const all: api.TreeNode[] = TREE_TEXTS.map(([kind, text], index) => ({
    id: `${session}-e${index}`,
    parent: index === 0 ? null : `${session}-e${index - 1}`,
    kind,
    subtype: kind === 'bash_execution' ? 'bash' : null,
    at_unix_ms: now - (TREE_TEXTS.length - index) * 4 * MIN,
    text,
    has_text: true,
    stop_reason: kind === 'assistant' ? 'stop' : null,
    label: kind === 'label' ? 'checkpoint' : null,
    forkable: kind === 'user' || kind === 'assistant',
    cost_before: Math.round(index * 0.41 * 100) / 100,
    cost_incomplete: false,
  }));
  all.push(
    { id: `${session}-b1`, parent: `${session}-e5`, kind: 'user', subtype: null, at_unix_ms: now - 30 * MIN, text: 'Actually, start with the PWA', has_text: true, stop_reason: null, label: null, forkable: true, cost_before: 2.05, cost_incomplete: false },
    { id: `${session}-b2`, parent: `${session}-b1`, kind: 'assistant', subtype: null, at_unix_ms: now - 29 * MIN, text: 'Scaffolding the app shell and themes.', has_text: true, stop_reason: 'stop', label: null, forkable: true, cost_before: 2.31, cost_incomplete: false },
  );
  const filtered = filter === 'conversation' ? all.filter((n) => n.kind === 'user' || n.kind === 'assistant') : filter === 'labeled' ? all.filter((n) => n.label !== null) : all;
  const start = cursor === null ? 0 : Number(cursor.slice(1));
  const page = filtered.slice(start, start + limit);
  const next = start + limit < filtered.length ? `c${start + limit}` : null;
  return { type: 'tree', nodes: page, active_leaf: `${session}-e${TREE_TEXTS.length - 1}`, next_cursor: next, truncated: false };
}

/** The desktop folders `browse` lists; tokens are `dir:` + path. */
const FOLDERS: Readonly<Record<string, readonly [string, string | null, boolean][]>> = {
  '~': [
    ['PersProject', null, false],
    ['code', null, false],
    ['infra', null, false],
    ['Documents', null, false],
  ],
  '~/PersProject': [
    ['OmpDeck', PROJECTS.ompdeck.id, true],
    ['OmpDeck-wt', null, false],
    ['raytracer', null, true],
    ['notes', null, false],
  ],
  '~/PersProject/OmpDeck-wt': [['feat-push', PROJECTS.wt.id, true]],
  '~/code': [
    ['website', PROJECTS.website.id, true],
    ['dotfiles', null, true],
  ],
  '~/infra': [['homelab', PROJECTS.infra.id, true]],
};

export function browse(dir: string | null): Extract<api.Resp, { type: 'dir' }> {
  if (dir === null) {
    return { type: 'dir', dir: null, display: 'This computer', parent: null, entries: [{ name: '~', dir: 'dir:~', project: null, repo: false }, { name: '/srv', dir: 'dir:/srv', project: null, repo: false }], truncated: false };
  }
  const path = dir.startsWith('dir:') ? dir.slice(4) : dir;
  const entries = (FOLDERS[path] ?? []).map(([name, project, repo]) => ({ name, dir: `dir:${path}/${name}`, project, repo }));
  const cut = path.lastIndexOf('/');
  const parent = path === '~' || path === '/srv' ? null : `dir:${cut <= 0 ? '~' : path.slice(0, cut)}`;
  return { type: 'dir', dir, display: path, parent, entries, truncated: false };
}

// ---- usage --------------------------------------------------------------------------------------

export function usage(now: number): Extract<api.FeedValue, { feed: 'usage' }> {
  const limit = (id: string, label: string, window: string | null, resetsIn: number | null, used: number, max: number, unit = 'percent'): api.UsageLimitView => ({
    id,
    label,
    window,
    resets_unix_ms: resetsIn === null ? null : now + resetsIn,
    used,
    limit: max,
    remaining: max - used,
    used_fraction: used / max,
    remaining_fraction: (max - used) / max,
    unit,
    status: (max - used) / max < 0.1 ? 'critical' : (max - used) / max < 0.35 ? 'warning' : 'ok',
  });
  return {
    feed: 'usage',
    usage: {
      reports: [
        {
          provider: 'anthropic',
          account: 'axel@example.com',
          plan: 'Max 20×',
          fetched_unix_ms: now - 2 * MIN,
          limits: [limit('five_hour', '5-hour session', '5h', 2 * HOUR + 14 * MIN, 18, 100), limit('seven_day', 'Weekly', '7d', 3 * DAY + 5 * HOUR, 69, 100), limit('seven_day_opus', 'Weekly Opus', '7d', 3 * DAY + 5 * HOUR, 93, 100)],
          notes: ['Opus usage counts against both weekly limits.'],
        },
        {
          provider: 'openai',
          account: 'axel@example.com',
          plan: 'Pro',
          fetched_unix_ms: now - 2 * MIN,
          limits: [limit('primary', 'Primary window', '5h', 3 * HOUR, 41, 100), limit('secondary', 'Weekly', '7d', 6 * DAY, 22, 100)],
          notes: [],
        },
        {
          provider: 'google',
          account: null,
          plan: 'AI Pro',
          fetched_unix_ms: now - 2 * MIN,
          limits: [limit('daily_requests', 'Daily requests', '1d', 9 * HOUR, 312, 1000, 'requests')],
          notes: [],
        },
      ],
      tightest_remaining: 0.07,
      unavailable: ['openrouter'],
    },
    fetched_unix_ms: now - 2 * MIN,
    error: null,
    fetching: false,
    truncated: false,
  };
}

export function activity(now: number, days: number): api.DayCount[] {
  const out: api.DayCount[] = [];
  for (let i = days - 1; i >= 0; i--) {
    const day = new Date(now - i * DAY).toISOString().slice(0, 10);
    const weekday = new Date(now - i * DAY).getUTCDay();
    const wave = Math.abs(Math.sin(i * 0.37) * 0.6 + Math.cos(i * 0.11) * 0.4);
    const idle = weekday === 0 || i % 17 === 5 || i % 29 === 11;
    const requests = idle ? 0 : Math.round(wave * 420) + (i % 7) * 9;
    out.push({ day, cost: idle ? null : Math.round(requests * 0.031 * 100) / 100, requests, tokens: requests * 7_400 });
  }
  return out;
}

// ---- settings -----------------------------------------------------------------------------------

export function ompSettings(): api.SettingsGroup[] {
  const entry = (key: string, kind: api.SettingKind, description: string, value: api.JsonValue, def: api.JsonValue, patch: Partial<api.SettingView> = {}): api.SettingView => ({
    key,
    kind,
    description,
    value,
    default: def,
    modified: JSON.stringify(value) !== JSON.stringify(def),
    secret: false,
    set: true,
    ...patch,
  });
  return [
    {
      id: 'model',
      label: 'Model',
      entries: [
        entry('defaultModel', { type: 'enum', options: ['anthropic/claude-opus-4-5', 'anthropic/claude-sonnet-4-5', 'openai/gpt-5.1', 'google/gemini-3-pro'] }, 'Model new sessions start with.', 'anthropic/claude-opus-4-5', 'anthropic/claude-sonnet-4-5'),
        entry('thinking', { type: 'enum', options: ['off', 'low', 'medium', 'high', 'max'] }, 'Default reasoning effort.', 'high', 'medium'),
        entry('maxTokens', { type: 'number' }, 'Output token cap per turn.', 32_000, 32_000),
      ],
    },
    {
      id: 'behaviour',
      label: 'Behaviour',
      entries: [
        entry('autoCompact', { type: 'bool' }, 'Compact automatically near the context limit.', true, true),
        entry('steeringMode', { type: 'enum', options: ['all', 'one-at-a-time'] }, 'How queued steering messages are delivered.', 'one-at-a-time', 'all'),
        entry('shell', { type: 'string' }, 'Shell used by the bash tool.', '/usr/bin/fish', '/bin/bash'),
        entry('allowedCommands', { type: 'array' }, 'Commands that never ask for approval.', ['git status', 'cargo check', 'bun test'], []),
        entry('env', { type: 'record' }, 'Extra environment for tools.', { RUST_LOG: 'info', NO_COLOR: '1' }, {}),
      ],
    },
    {
      id: 'keys',
      label: 'Provider keys',
      entries: [
        entry('anthropicApiKey', { type: 'string' }, 'Anthropic API key.', null, null, { secret: true, set: true, modified: true }),
        entry('openaiApiKey', { type: 'string' }, 'OpenAI API key.', null, null, { secret: true, set: true, modified: true }),
        entry('exaApiKey', { type: 'string' }, 'Exa search API key.', null, null, { secret: true, set: false, modified: false }),
      ],
    },
    {
      id: 'ui',
      label: 'Interface',
      entries: [
        entry('theme', { type: 'enum', options: ['dark', 'light', 'auto'] }, 'Terminal UI theme.', 'dark', 'auto'),
        entry('showCost', { type: 'bool' }, 'Show the running cost in the status line.', false, true),
      ],
    },
  ];
}

export function updates(now: number, phase: api.UpdatePhase, ompChecking: boolean): Extract<api.FeedValue, { feed: 'updates' }> {
  return {
    feed: 'updates',
    omp: { version: '18.14.0', current: '18.14.0', latest: '18.15.1', error: null, checked_unix_ms: now - 25 * MIN, checking: ompChecking },
    app: {
      install: 'deb',
      apply_allowed: false,
      current: '0.10.0',
      latest: '0.10.2',
      page_url: 'https://github.com/axeleratorx/OmpDeck/releases/tag/v0.10.2',
      asset: 'ompdeck_0.10.2_amd64.deb',
      checked_unix_ms: now - 25 * MIN,
      phase,
    },
  };
}

export const DEFAULT_PREFS: api.PushPrefs = { turn_finished: true, attention: true, ended: false, show_titles: false };

export function backupFeed(now: number, busy: string | null): Extract<api.FeedValue, { feed: 'backup' }> {
  return { feed: 'backup', destination: { kind: 'drive', label: 'Google Drive' }, available: true, connected: true, account: 'axel@example.com', last_backup_unix_ms: now - 5 * HOUR, interval: 'daily', busy, error: null };
}

export function backupList(now: number): api.BackupHost[] {
  const backup = (host: string, n: number, age: number, size: number): api.BackupFileView => ({ id: `drv-${host}-${n}`, name: `ompdeck-settings-${host}-${n}.tar.zst`, size, created_unix_ms: now - age });
  return [
    { host: 'workstation', backups: [backup('workstation', 3, 5 * HOUR, 4_812_331), backup('workstation', 2, 29 * HOUR, 4_790_002), backup('workstation', 1, 8 * DAY, 4_102_118)] },
    { host: 'laptop', backups: [backup('laptop', 3, 2 * DAY, 3_201_774), backup('laptop', 2, 9 * DAY, 3_188_090), backup('laptop', 1, 31 * DAY, 2_904_655)] },
  ];
}

export function backupPlan(): api.RestorePlanView {
  return {
    type: 'backup',
    name: 'ompdeck-settings-laptop-3.tar.zst',
    created_unix_ms: Date.now() - 2 * DAY,
    host: 'laptop',
    replaced: ['~/.omp/agent/settings.json', '~/.omp/agent/keybindings.json', '~/.config/omp-deck/config.toml'],
    added: ['~/.omp/agent/skills/release-notes/SKILL.md', '~/.omp/agent/rules/no-force-push.md'],
    dirs: ['~/.omp/agent/skills/release-notes'],
    removed: ['~/.omp/agent/skills/old-deploy/SKILL.md'],
    safety_display: '~/.local/share/omp-deck/backups/pre-restore-2026-10-30.tar.zst',
  };
}

export const SYNC_PROJECTS = [PROJECTS.ompdeck, PROJECTS.website, PROJECTS.infra] as const;

export function syncRows(now: number): api.SyncRowView[] {
  return [
    { project: PROJECTS.ompdeck.id, display: PROJECTS.ompdeck.display, parts: { omp: true, sessions: true, memory: true }, excluded: ['s-old-2', 's-stub-1'], last_backup_unix_ms: now - 40 * MIN },
    { project: PROJECTS.website.id, display: PROJECTS.website.display, parts: { omp: true, sessions: true, memory: false }, excluded: [], last_backup_unix_ms: now - 3 * HOUR },
    { project: PROJECTS.infra.id, display: PROJECTS.infra.display, parts: { omp: false, sessions: true, memory: false }, excluded: [], last_backup_unix_ms: null },
  ];
}

export function syncBackups(now: number, project: string | null): api.SyncBackupView[] {
  const all: api.SyncBackupView[] = [];
  for (const p of [PROJECTS.ompdeck, PROJECTS.website]) {
    for (const [host, age] of [['workstation', 40 * MIN], ['laptop', 2 * DAY]] as const) {
      all.push({
        id: `sb-${p.id}-${host}`,
        name: `${p.id.slice(2)}-${host}.tar.zst`,
        host,
        display: p.display,
        size: p === PROJECTS.ompdeck ? 38_400_000 : 6_100_000,
        created_unix_ms: now - age,
        omp_bytes: 182_000,
        memory_bytes: p === PROJECTS.ompdeck ? 41_000 : null,
        sessions: p === PROJECTS.ompdeck ? 10 : 3,
        session_bytes: p === PROJECTS.ompdeck ? 37_900_000 : 5_900_000,
      });
    }
  }
  return project === null ? all : all.filter((b) => b.id.startsWith(`sb-${project}-`));
}

export function syncPlan(now: number): api.RestorePlanView {
  const local = (age: number, bytes: number): api.LocalCopyView => ({ project: PROJECTS.ompdeck.id, display: PROJECTS.ompdeck.display, modified_unix_ms: now - age, bytes });
  return {
    type: 'project',
    target_display: PROJECTS.ompdeck.display,
    create_target: false,
    omp_exists: true,
    omp_overwritten: ['.omp/settings.json', '.omp/rules/review.md'],
    omp_added: ['.omp/skills/release/SKILL.md'],
    omp_skipped: [],
    memory_exists: true,
    memory_overwritten: ['MEMORY.md'],
    memory_added: ['memory/decisions.md'],
    memory_skipped: ['memory/scratch.md'],
    sessions: [
      { id: 's-convo', title: 'Ship mobile relay', modified_unix_ms: now - 2 * DAY, bytes: 4_200_000, local: [local(2 * MIN, 4_900_000)], problem: null },
      { id: 's-idle', title: 'Refactor tmux launcher', modified_unix_ms: now - 3 * DAY, bytes: 1_100_000, local: [local(9 * MIN, 1_300_000)], problem: null },
      { id: 's-old-3', title: 'Notification sounds', modified_unix_ms: now - 3 * DAY, bytes: 820_000, local: [local(3 * DAY, 820_000)], problem: null },
      { id: 's-laptop-1', title: 'Laptop-only experiment', modified_unix_ms: now - 5 * DAY, bytes: 310_000, local: [], problem: null },
      { id: 's-laptop-2', title: 'Broken session file', modified_unix_ms: now - 6 * DAY, bytes: 12_000, local: [], problem: 'the session header is not valid JSON' },
    ],
  };
}

export function models(): api.Model[] {
  const model = (provider: string, id: string, name: string, thinking: string[], ctx: number, input: number, output: number): api.Model => ({
    selector: `${provider}/${id}`,
    provider,
    id,
    name,
    context_window: ctx,
    max_tokens: 64_000,
    reasoning: thinking.length > 0,
    thinking,
    input: ['text', 'image'],
    cost_input: input,
    cost_output: output,
  });
  return [
    model('anthropic', 'claude-opus-4-5', 'Claude Opus 4.5', ['low', 'medium', 'high', 'max'], 200_000, 5, 25),
    model('anthropic', 'claude-sonnet-4-5', 'Claude Sonnet 4.5', ['low', 'medium', 'high'], 1_000_000, 3, 15),
    model('anthropic', 'claude-haiku-4-5', 'Claude Haiku 4.5', [], 200_000, 1, 5),
    model('openai', 'gpt-5.1', 'GPT-5.1', ['minimal', 'low', 'medium', 'high'], 400_000, 1.25, 10),
    model('google', 'gemini-3-pro', 'Gemini 3 Pro', ['low', 'high'], 1_000_000, 2, 12),
  ];
}

/** What the terminal view shows first (ANSI). */
export function terminalSnapshot(title: string): string {
  return [
    '\x1b[2J\x1b[H',
    `\x1b[1;35m omp \x1b[0m\x1b[2m· ${title} · anthropic/claude-opus-4-5\x1b[0m\r\n`,
    '\r\n',
    '\x1b[36m❯\x1b[0m Build slice 4 of the phone companion\r\n',
    '\r\n',
    '  \x1b[32m●\x1b[0m read docs/notes/mobile-pwa.md \x1b[2m(412 lines)\x1b[0m\r\n',
    '  \x1b[32m●\x1b[0m edit pwa/src/app/transfer.ts \x1b[2m(+48 −3)\x1b[0m\r\n',
    '  \x1b[31m●\x1b[0m bash bun test \x1b[2m(1 fail)\x1b[0m\r\n',
    '\r\n',
    'Uploads stream in 512 KiB chunks now. The abort test is fixed.\r\n',
    '\r\n',
    '\x1b[2m───────────────────────────────────────\x1b[0m\r\n',
    '\x1b[36m❯\x1b[0m ',
  ].join('');
}
