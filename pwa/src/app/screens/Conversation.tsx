// A session's conversation: transcript with tool cards and images, approval cards (draft confirms
// and omp's pending UI requests), the composer (send/steer/follow-up/insert with attachments),
// model/effort picker, context meter with breakdown, and runtime and workspace actions.

import { type ReactNode, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { api } from '../../remote/index.ts';
import type { ToolRenderHost } from '../../vendor/omp-collab/tool-render/index.ts';
import { ClientError, type ConfirmView } from '../client.ts';
import { useNav } from '../nav.ts';
import { downloadAndSave } from '../transfer.ts';
import { adaptTranscript } from '../transcript/adapter.ts';
import { perform, shortModel, statusLabel, succeeded, tokens } from '../ui/format.ts';
import { useCan, useClient, useClientState, useFeed, useNow, useTranscript } from '../ui/hooks.ts';
import { ConfirmSheet, Empty, Sheet, StaleBanner, useS4 } from '../ui/kit.tsx';
import '../../styles/convo.css';
import { Composer } from './conversation/Composer.tsx';
import { ContextSheet } from './conversation/ContextSheet.tsx';
import { GoneCard, PendingUiCard } from './conversation/PendingUi.tsx';
import { EMPTY_TRACKER, GONE_MS, dismissGone, noteAnswer, noteSending, trackPending } from './conversation/pending.ts';
import { TranscriptList } from './Transcript.tsx';

const FIRST_TAIL = 120;

/** Finds the live session for a route: by its pinned key, else by the omp session id. */
export function useLive(liveKey: string | null, session: string | null): api.LiveSession | null {
  const feed = useFeed({ feed: 'live' }, 'live');
  return useMemo(() => {
    const sessions = feed?.sessions ?? [];
    if (liveKey !== null) {
      const pinned = sessions.find((entry) => entry.live.key === liveKey);
      if (pinned !== undefined) return pinned;
    }
    if (session === null) return null;
    return sessions.find((entry) => entry.session === session && entry.status.state !== 'exited') ?? null;
  }, [feed, liveKey, session]);
}

/** The model entry a live session's model string names (selector, or provider/id). */
export function findModel(models: readonly api.Model[] | undefined, model: string | null): api.Model | null {
  if (models === undefined || model === null) return null;
  return (
    models.find((entry) => entry.selector === model) ??
    models.find((entry) => `${entry.provider}/${entry.id}` === model) ??
    models.find((entry) => entry.id === model) ??
    null
  );
}

export function ConversationScreen({
  session: routeSession,
  liveKey: routeLiveKey,
  title: routeTitle,
}: {
  session: string | null;
  liveKey: string | null;
  title: string;
}): ReactNode {
  const client = useClient();
  const nav = useNav();
  const can = useCan();
  const now = useNow();
  const [liveKey, setLiveKey] = useState(routeLiveKey);
  const live = useLive(liveKey, routeSession);
  const session = routeSession ?? live?.session ?? null;
  const [tail, setTail] = useState(FIRST_TAIL);
  const transcript = useTranscript(session === null ? null : { feed: 'transcript', session, tail });
  const cards = useFeed(session === null ? null : { feed: 'cards', sessions: [session] }, 'cards');
  const card = cards?.cards.find((entry) => entry.session === session) ?? null;
  const models = useClientState((state) => state.models);
  const online = useClientState((state) => state.conn.phase === 'online');
  const allConfirms = useClientState((state) => state.confirms);
  const confirms = live === null ? [] : allConfirms.filter((confirm) => confirm.live.key === live.live.key);
  const [sheet, setSheet] = useState<'actions' | 'model' | 'meter' | null>(null);
  const pending = live?.pending_ui ?? [];
  const [tracker, setTracker] = useState(EMPTY_TRACKER);

  // Follow omp's pending UI requests: countdown starts, and requests answered elsewhere.
  useEffect(() => {
    setTracker((current) => trackPending(current, pending, Date.now()));
  }, [live?.pending_ui]);
  useEffect(() => {
    if (tracker.gone.length === 0) return;
    const oldest = Math.min(...tracker.gone.map((entry) => entry.atMs));
    const timer = setTimeout(() => setTracker((current) => trackPending(current, live?.pending_ui ?? [], Date.now())), oldest + GONE_MS - Date.now() + 50);
    return () => clearTimeout(timer);
  }, [tracker.gone, live?.pending_ui]);

  useEffect(() => {
    if (online) client.loadModels().catch(() => undefined);
  }, [client, online]);

  const entries = useMemo(
    () => (transcript === undefined ? [] : adaptTranscript(transcript.items, transcript.start)),
    [transcript],
  );
  const host = useMemo<ToolRenderHost>(
    () => ({
      openAgent: (agent: string) => {
        if (session !== null) nav.push({ kind: 'subagent', session, agent });
      },
    }),
    [nav, session],
  );

  // Keep the view pinned to the newest entry while the reader is at the bottom.
  const scroller = useRef<HTMLDivElement>(null);
  const pinned = useRef(true);
  useLayoutEffect(() => {
    const element = scroller.current;
    if (element !== null && pinned.current) element.scrollTop = element.scrollHeight;
  }, [entries, confirms.length, pending.length, tracker.gone.length]);
  // Images, expanding cards and the composer (modes, attachment chips) change the heights after
  // render: stay at the bottom while pinned.
  const flow = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const element = flow.current;
    const outer = scroller.current;
    if (element === null || outer === null || typeof ResizeObserver === 'undefined') return;
    const observer = new ResizeObserver(() => {
      if (pinned.current) outer.scrollTop = outer.scrollHeight;
    });
    observer.observe(element);
    const composer = outer.parentElement?.querySelector('.composer');
    if (composer != null) observer.observe(composer);
    return () => observer.disconnect();
  }, [live === null, session === null]);
  const title = live?.title ?? routeTitle;
  const model = live?.model ?? card?.model ?? null;
  const modelInfo = findModel(models?.models, model);
  const effort = card?.thinking ?? null;
  const contextWindow = modelInfo?.context_window ?? null;
  const contextShare =
    card?.context_tokens != null && contextWindow !== null && contextWindow > 0 ? card.context_tokens / contextWindow : null;
  const busy = live !== null && (live.busy || live.status.state === 'busy');
  const source = useMemo(() => (session === null ? null : { session, agent: null }), [session]);

  return (
    <div className="screen pushed chat">
      <header className="top">
        <div className="line">
          <button type="button" className="back" onClick={nav.pop} aria-label="Back">
            ‹
          </button>
          <h2 className="grow">{title}</h2>
          {busy && <span className="busy" aria-label="Working" />}
          {live !== null && (
            <button type="button" className="icon-btn" onClick={() => nav.push({ kind: 'terminal', liveKey: live.live.key, title })} aria-label="Terminal">
              ⌨
            </button>
          )}
          <button type="button" className="icon-btn" onClick={() => setSheet('actions')} aria-label="Session actions">
            ⋯
          </button>
        </div>
        <div className="line wrap" style={{ marginTop: 8 }}>
          <button
            type="button"
            className="pill"
            onClick={() => setSheet('model')}
            disabled={live === null}
            aria-label="Model and effort"
          >
            {shortModel(model)}
            {effort !== null ? ` · ${effort}` : ''}
          </button>
          <span className="pill" style={{ color: 'var(--accent)' }}>
            {live === null ? 'Not running' : statusLabel(live)}
          </span>
          {live?.project != null && (
            <button
              type="button"
              className="pill"
              onClick={() => nav.push({ kind: 'changes', project: live.project ?? '', display: live.display })}
            >
              ± Changes
            </button>
          )}
          {live?.desktop_controlled === true && <span className="pill warn">Desktop has control</span>}
        </div>
      </header>
      <div
        className="chat-content"
        ref={scroller}
        onScroll={(event) => {
          const element = event.currentTarget;
          pinned.current = element.scrollHeight - element.scrollTop - element.clientHeight < 80;
        }}
      >
        <div ref={flow}>
        <StaleBanner />
        {transcript !== undefined && transcript.start > 0 && tail < api.MAX_TAIL && (
          <button type="button" className="more-row" onClick={() => setTail(api.MAX_TAIL)}>
            Load earlier messages
          </button>
        )}
        {session === null ? (
          <Empty title="Starting the session…">The conversation appears once omp has created it.</Empty>
        ) : transcript === undefined ? (
          <Empty title="Loading the conversation…" />
        ) : entries.length === 0 ? (
          <Empty title="No messages yet">Send the first prompt below.</Empty>
        ) : (
          <TranscriptList entries={entries} busy={busy} host={host} now={now} source={source} />
        )}
        {confirms.map((confirm) => (
          <ApprovalCard key={`${confirm.live.key}:${confirm.request}`} confirm={confirm} />
        ))}
        {live !== null &&
          pending.map((request) => (
            <PendingUiCard
              key={`${live.live.key}:${request.request}`}
              live={live.live}
              request={request}
              seenMs={tracker.seen.get(request.request) ?? now}
              onAnswered={(answered, applied, sending) =>
                setTracker((current) => (sending ? noteSending(current, answered.request) : noteAnswer(current, answered, applied, Date.now())))
              }
            />
          ))}
        {tracker.gone.map((gone) => (
          <GoneCard
            key={`gone:${gone.request.request}`}
            gone={gone}
            onDismiss={() => setTracker((current) => dismissGone(current, gone.request.request))}
          />
        ))}
        </div>
      </div>
      {live !== null ? (
        <Composer live={live} busy={busy} share={contextShare} onMeter={() => setSheet('meter')} />
      ) : session !== null ? (
        <div className="composer">
          <div className="line spread">
            <span className="muted small">This session is not running on the desktop.</span>
            <button
              type="button"
              className="button primary"
              disabled={!can({ op: 'resume', session })}
              onClick={async () => {
                const view = await perform(client, { op: 'resume', session }, `Resume “${title}”`);
                const result = view?.outcome?.state === 'done' ? view.outcome.result : null;
                if (result?.type === 'session') setLiveKey(result.live.key);
              }}
            >
              Resume
            </button>
          </div>
        </div>
      ) : null}
      {sheet === 'actions' && (
        <ActionSheet
          live={live}
          session={session}
          title={title}
          entries={transcript?.items ?? []}
          onTerminal={() => {
            setSheet(null);
            if (live !== null) nav.push({ kind: 'terminal', liveKey: live.live.key, title });
          }}
          onTree={() => {
            setSheet(null);
            if (session !== null) nav.push({ kind: 'tree', session, title });
          }}
          onForked={(forked) => {
            setSheet(null);
            nav.push({ kind: 'conversation', session: forked, liveKey: null, title: `${title} (fork)` });
          }}
          onClose={() => setSheet(null)}
        />
      )}
      {sheet === 'model' && live !== null && (
        <ModelSheet live={live} current={modelInfo} effort={effort} onClose={() => setSheet(null)} />
      )}
      {sheet === 'meter' && (
        <ContextSheet live={live} card={card} model={model} effort={effort} contextWindow={contextWindow} onClose={() => setSheet(null)} />
      )}
    </div>
  );
}

function ApprovalCard({ confirm }: { confirm: ConfirmView }): ReactNode {
  const client = useClient();
  const can = useCan();
  const [busy, setBusy] = useState(false);
  const answer = async (proceed: boolean) => {
    setBusy(true);
    await perform(
      client,
      { op: 'answer', live: confirm.live, request: confirm.request, proceed },
      proceed ? `Approve “${confirm.label}”` : `Deny “${confirm.label}”`,
    );
    setBusy(false);
  };
  const allowed = can({ op: 'answer', live: confirm.live, request: confirm.request, proceed: true });
  return (
    <section className="approval">
      <header>⚠ APPROVAL REQUIRED</header>
      <div className="approval-body">
        <p>{confirm.label}</p>
        {confirm.draft.length > 0 && <pre>{confirm.draft}</pre>}
        <div className="buttons">
          <button type="button" className="button primary" disabled={!allowed || busy} onClick={() => void answer(true)}>
            ✓ Approve
          </button>
          <button type="button" className="button" disabled={!allowed || busy} onClick={() => void answer(false)}>
            Deny
          </button>
        </div>
      </div>
    </section>
  );
}

type ActionMode = 'menu' | 'compact' | 'handoff' | 'rename' | 'stop' | 'kill' | 'restart' | 'desktop' | 'fork';

const SPLITS: readonly (readonly [api.Split, string, string])[] = [
  ['none', 'Own tab', 'Opens or focuses its tab'],
  ['right', 'Split right', 'Beside the current tab'],
  ['down', 'Split down', 'Below the current tab'],
];

/**
 * Copies text a promise resolves to. Safari only allows clipboard writes inside the tap, so the
 * pending text goes in as a `ClipboardItem` promise where supported.
 */
async function copyLater(text: Promise<string>): Promise<void> {
  if (typeof ClipboardItem !== 'undefined' && typeof navigator.clipboard.write === 'function') {
    const blob = text.then((value) => new Blob([value], { type: 'text/plain' }));
    await navigator.clipboard.write([new ClipboardItem({ 'text/plain': blob })]);
    return;
  }
  await navigator.clipboard.writeText(await text);
}

function errorText(error: unknown): string {
  return error instanceof ClientError || error instanceof Error ? error.message : String(error);
}

function ActionSheet({
  live,
  session,
  title,
  entries,
  onTerminal,
  onTree,
  onForked,
  onClose,
}: {
  live: api.LiveSession | null;
  session: string | null;
  title: string;
  entries: readonly api.Item[];
  onTerminal: () => void;
  onTree: () => void;
  onForked: (session: string) => void;
  onClose: () => void;
}): ReactNode {
  const client = useClient();
  const can = useCan();
  const s4 = useS4();
  const [mode, setMode] = useState<ActionMode>('menu');
  const [focus, setFocus] = useState('');
  const [compactMode, setCompactMode] = useState<api.CompactMode>('default');
  const [name, setName] = useState(title);
  const [split, setSplit] = useState<api.Split>('none');
  const [busy, setBusy] = useState(false);
  const [exportProgress, setExportProgress] = useState<number | null>(null);
  const lastReply = entries.findLast((item) => item.type === 'assistant' && item.text.length > 0);

  const run = async (op: api.Op, label: string, success?: string) => {
    setBusy(true);
    const view = await perform(client, op, label, success);
    setBusy(false);
    if (succeeded(view)) onClose();
    return view;
  };
  const action = (value: api.SessionAction, label: string) =>
    live === null ? undefined : run({ op: 'action', live: live.live, action: value }, label);
  const allowed = (op: api.Op) => live !== null && can(op) && !busy;
  const sample: api.Op | null = live === null ? null : { op: 'action', live: live.live, action: { type: 'plan' } };
  const canAct = sample !== null && allowed(sample);
  const project = live?.project ?? null;

  const copyLastReply = () => {
    let text: Promise<string>;
    if (s4 && session !== null) {
      text = client.request({ op: 'last_reply', session }).then((resp) => {
        if (resp.type !== 'text' || resp.text.length === 0) throw new Error('There is no response to copy yet.');
        if (resp.truncated) client.notify('The response was long; the copy holds its first 256 KB.', 'warn');
        return resp.text;
      });
    } else if (lastReply?.type === 'assistant') {
      text = Promise.resolve(lastReply.text);
    } else {
      return;
    }
    copyLater(text).then(
      () => client.notify('Copied the last response.'),
      (error: unknown) => client.notify(error instanceof DOMException ? 'Copying needs clipboard access.' : errorText(error), 'warn'),
    );
    onClose();
  };

  const exportHtml = async () => {
    if (session === null) return;
    setBusy(true);
    setExportProgress(0);
    try {
      await downloadAndSave(client, { op: 'export', session }, { onProgress: (done, total) => setExportProgress(total > 0 ? done / total : 0) });
      client.notify('Exported the session as HTML.');
      onClose();
    } catch (error) {
      client.notify(`Export failed: ${errorText(error)}`, 'error');
    } finally {
      setBusy(false);
      setExportProgress(null);
    }
  };

  const fork = async () => {
    if (session === null || project === null) return;
    setBusy(true);
    const view = await perform(client, { op: 'copy_session', session, project }, `Fork “${title}”`);
    setBusy(false);
    const result = view?.outcome?.state === 'done' ? view.outcome.result : null;
    if (!succeeded(view)) return;
    if (result?.type === 'file' && result.session !== null) onForked(result.session);
    else {
      client.notify(`Forked “${title}”.`);
      onClose();
    }
  };

  if (live !== null && (mode === 'stop' || mode === 'kill' || mode === 'restart')) {
    const verbs = { stop: 'Stop', kill: 'Kill', restart: 'Restart' } as const;
    const messages = {
      stop: 'omp is asked to exit; work in progress ends.',
      kill: 'The session’s process is terminated at once; anything unsaved is lost.',
      restart: 'omp exits and starts again on the same session.',
    } as const;
    return (
      <ConfirmSheet
        title={`${verbs[mode]} “${title}”?`}
        message={messages[mode]}
        facts={[
          ['Session', title],
          ['Folder', live.display],
          ['Status', statusLabel(live)],
        ]}
        confirm={verbs[mode]}
        busy={busy}
        onConfirm={() => void run({ op: mode, live: live.live }, `${verbs[mode]} “${title}”`)}
        onCancel={() => setMode('menu')}
      />
    );
  }

  return (
    <Sheet
      title={
        {
          menu: 'Session actions',
          compact: 'Compact context',
          handoff: 'Hand off',
          rename: 'Rename',
          desktop: 'Open on desktop',
          fork: 'Fork session',
          stop: '',
          kill: '',
          restart: '',
        }[mode]
      }
      icon="⋯"
      onClose={onClose}
    >
      {mode === 'menu' && (
        <div className="menu">
          {live === null && <p className="muted small">Resume the session to use its actions.</p>}
          <button type="button" className="button" disabled={!canAct} onClick={() => setMode('compact')}>
            ⇲ Compact context<span className="hint">default · soft · snap · focus</span>
          </button>
          <button type="button" className="button" disabled={!canAct} onClick={() => setMode('handoff')}>
            ⇢ Hand off<span className="hint">new session with a summary</span>
          </button>
          <button type="button" className="button" disabled={!canAct} onClick={() => setMode('rename')}>
            ✎ Rename<span className="hint">or auto-name</span>
          </button>
          <button type="button" className="button" disabled={!canAct} onClick={() => void action({ type: 'plan' }, 'Plan mode')}>
            ☰ Plan mode
          </button>
          <button type="button" className="button" disabled={!canAct} onClick={() => void action({ type: 'retry' }, 'Retry')}>
            ↻ Retry last turn
          </button>
          <button
            type="button"
            className="button"
            disabled={s4 ? session === null || !can({ op: 'last_reply', session }) : lastReply === undefined}
            onClick={copyLastReply}
          >
            ⧉ Copy last response
          </button>
          {s4 && (
            <>
              <button
                type="button"
                className="button"
                disabled={session === null || busy || !can({ op: 'export', session })}
                onClick={() => void exportHtml()}
              >
                ⇩ Export as HTML
                <span className="hint">{exportProgress !== null ? `${Math.round(exportProgress * 100)}%` : 'download'}</span>
              </button>
              <button
                type="button"
                className="button"
                disabled={session === null || !can({ op: 'open_on_desktop', session, split: 'none' })}
                onClick={() => setMode('desktop')}
              >
                ⧉ Open on desktop<span className="hint">tab · split right · split down</span>
              </button>
              <button
                type="button"
                className="button"
                disabled={session === null || busy || !can({ op: 'close_on_desktop', session })}
                onClick={() => session !== null && void run({ op: 'close_on_desktop', session }, 'Close on desktop', 'Closed the desktop tab.')}
              >
                ▭ Close on desktop<span className="hint">closes its desktop tab</span>
              </button>
              <button
                type="button"
                className="button"
                disabled={session === null || project === null || !can({ op: 'copy_session', session, project })}
                onClick={() => setMode('fork')}
              >
                ⑂ Fork session<span className="hint">{project === null ? 'resume it first' : 'a copy in its project'}</span>
              </button>
              <button type="button" className="button" disabled={session === null} onClick={onTree}>
                ⌥ Session tree<span className="hint">fork at a message · go here</span>
              </button>
            </>
          )}
          {live !== null && (
            <button type="button" className="button" onClick={onTerminal}>
              ⌨ Open terminal
            </button>
          )}
          {live !== null && (
            <>
              <button type="button" className="button" disabled={!allowed({ op: 'restart', live: live.live })} onClick={() => setMode('restart')}>
                ⟲ Restart omp
              </button>
              <button type="button" className="button" disabled={!allowed({ op: 'stop', live: live.live })} onClick={() => setMode('stop')}>
                ■ Stop session
              </button>
              <button type="button" className="button danger" disabled={!allowed({ op: 'kill', live: live.live })} onClick={() => setMode('kill')}>
                ✕ Kill session
              </button>
            </>
          )}
        </div>
      )}
      {mode === 'desktop' && (
        <>
          <div className="option-list" style={{ marginBottom: 12 }}>
            {SPLITS.map(([id, label, hint]) => (
              <button key={id} type="button" className="option" aria-pressed={split === id} onClick={() => setSplit(id)}>
                <span className="grow">
                  {label}
                  <small>{hint}</small>
                </span>
                {split === id && <span className="chip accent">✓</span>}
              </button>
            ))}
          </div>
          <p className="muted small">The desktop app opens (or starts) and shows this session; the phone stays here.</p>
          <div className="buttons pair">
            <button type="button" className="button" onClick={() => setMode('menu')}>
              Back
            </button>
            <button
              type="button"
              className="button primary"
              disabled={session === null || busy || !can({ op: 'open_on_desktop', session, split })}
              onClick={() => session !== null && void run({ op: 'open_on_desktop', session, split }, 'Open on desktop', 'Opened on the desktop.')}
            >
              Open
            </button>
          </div>
        </>
      )}
      {mode === 'fork' && (
        <>
          <p>A full copy of “{title}” is added to its project as a new session; the original stays as it is.</p>
          <dl className="kv">
            <dt>Session</dt>
            <dd>{title}</dd>
            <dt>Project</dt>
            <dd>
              <span className="chip accent">{live?.display ?? 'unknown'}</span>
            </dd>
            <dt>Messages</dt>
            <dd>{entries.filter((item) => item.type === 'user' || item.type === 'assistant').length} loaded</dd>
          </dl>
          <div className="buttons pair">
            <button type="button" className="button" onClick={() => setMode('menu')}>
              Back
            </button>
            <button type="button" className="button primary" disabled={busy || session === null || project === null} onClick={() => void fork()}>
              {busy ? 'Forking…' : 'Fork'}
            </button>
          </div>
        </>
      )}
      {mode === 'compact' && (
        <>
          <div className="option-list" style={{ marginBottom: 12 }}>
            {(
              [
                ['default', 'Default', 'Summarise and keep going'],
                ['soft', 'Soft', 'Trim tool output, keep the conversation'],
                ['snapcompact', 'Snap', 'Fast snapshot compaction'],
                ['focus', 'Focus', 'Summarise toward a focus you give'],
              ] as const
            ).map(([id, label, hint]) => (
              <button key={id} type="button" className="option" aria-pressed={compactMode === id} onClick={() => setCompactMode(id)}>
                <span className="grow">
                  {label}
                  <small>{hint}</small>
                </span>
                {compactMode === id && <span className="chip accent">✓</span>}
              </button>
            ))}
          </div>
          {compactMode === 'focus' && (
            <label className="field">
              <span>Focus</span>
              <input className="input" value={focus} onChange={(event) => setFocus(event.target.value)} placeholder="What to keep" />
            </label>
          )}
          <div className="buttons pair">
            <button type="button" className="button" onClick={() => setMode('menu')}>
              Back
            </button>
            <button
              type="button"
              className="button primary"
              disabled={!canAct || (compactMode === 'focus' && focus.trim().length === 0)}
              onClick={() =>
                void action(
                  { type: 'compact', mode: compactMode, focus: compactMode === 'focus' ? focus.trim() : null },
                  'Compact context',
                )
              }
            >
              Compact
            </button>
          </div>
        </>
      )}
      {mode === 'handoff' && (
        <>
          <label className="field">
            <span>Focus (optional)</span>
            <input className="input" value={focus} onChange={(event) => setFocus(event.target.value)} placeholder="What the next session should do" />
          </label>
          <div className="buttons pair">
            <button type="button" className="button" onClick={() => setMode('menu')}>
              Back
            </button>
            <button
              type="button"
              className="button primary"
              disabled={!canAct}
              onClick={() => void action({ type: 'handoff', focus: focus.trim().length > 0 ? focus.trim() : null }, 'Hand off')}
            >
              Hand off
            </button>
          </div>
        </>
      )}
      {mode === 'rename' && (
        <>
          <label className="field">
            <span>Name</span>
            <input className="input" value={name} onChange={(event) => setName(event.target.value)} maxLength={200} />
          </label>
          <div className="buttons">
            <button type="button" className="button" onClick={() => setMode('menu')}>
              Back
            </button>
            <button type="button" className="button" disabled={!canAct} onClick={() => void action({ type: 'auto_name' }, 'Auto-name')}>
              Auto-name
            </button>
            <button
              type="button"
              className="button primary"
              disabled={!canAct || name.trim().length === 0}
              onClick={() => void action({ type: 'rename', name: name.trim() }, 'Rename')}
            >
              Rename
            </button>
          </div>
        </>
      )}
    </Sheet>
  );
}

function ModelSheet({
  live,
  current,
  effort,
  onClose,
}: {
  live: api.LiveSession;
  current: api.Model | null;
  effort: string | null;
  onClose: () => void;
}): ReactNode {
  const client = useClient();
  const can = useCan();
  const models = useClientState((state) => state.models);
  const [filter, setFilter] = useState('');
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    client.loadModels(true).catch((error: unknown) => client.notify(error instanceof Error ? error.message : String(error), 'error'));
  }, [client]);
  const run = async (op: api.Op, label: string) => {
    setBusy(true);
    const view = await perform(client, op, label);
    setBusy(false);
    if (succeeded(view)) onClose();
  };
  const needle = filter.trim().toLowerCase();
  const list = (models?.models ?? []).filter(
    (model) => needle.length === 0 || `${model.name} ${model.selector}`.toLowerCase().includes(needle),
  );
  const canSwitch = can({ op: 'switch_model', live: live.live, selector: '' }) && !busy;
  const pending = live.pending_switch;
  return (
    <Sheet title="Model and effort" icon="◇" onClose={onClose}>
      {pending !== null && (
        <div className="banner">
          <span className="busy" />
          <span className="grow">
            Switching to {pending.selector ?? 'a model'}
            {pending.level !== null ? ` · ${pending.level}` : ''}…
          </span>
        </div>
      )}
      {current !== null && current.thinking.length > 0 && (
        <label className="field">
          <span>Effort</span>
          <div className="line wrap">
            {current.thinking.map((level) => (
              <button
                key={level}
                type="button"
                className="chip"
                aria-pressed={effort === level}
                disabled={!canSwitch}
                onClick={() => void run({ op: 'set_effort', live: live.live, level }, `Effort ${level}`)}
              >
                {level}
              </button>
            ))}
          </div>
        </label>
      )}
      <input className="input" placeholder="Filter models" value={filter} onChange={(event) => setFilter(event.target.value)} style={{ marginBottom: 10 }} />
      {models === null ? (
        <p className="muted small">Loading models…</p>
      ) : (
        <div className="option-list">
          {list.map((model) => (
            <button
              key={model.selector}
              type="button"
              className="option"
              aria-pressed={current?.selector === model.selector}
              disabled={!canSwitch}
              onClick={() =>
                void run(
                  { op: 'switch_model', live: live.live, selector: model.selector, level: effort !== null && model.thinking.includes(effort) ? effort : null },
                  `Switch to ${model.name}`,
                )
              }
            >
              <span className="grow">
                {model.name}
                <small>
                  {model.provider}
                  {model.context_window !== null ? ` · ${tokens(model.context_window)} ctx` : ''}
                  {model.thinking.length > 0 ? ` · ${model.thinking.length} effort levels` : ''}
                </small>
              </span>
              {current?.selector === model.selector && <span className="chip accent">current</span>}
            </button>
          ))}
          {models.truncated && <p className="muted small">The list was cut; filter to find others.</p>}
        </div>
      )}
    </Sheet>
  );
}
