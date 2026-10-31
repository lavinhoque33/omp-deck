// A live session's terminal: xterm 6 as a renderer only (DOM renderer), fed by the view's
// snapshot and output. Input comes from our own field and key bar, never from xterm, so terminal
// queries are never answered; OSC 52 (clipboard) is swallowed. Observe/Input/Control modes; in
// Control the phone owns the terminal size (`geometry_owned` falls back to Input).

import { FitAddon } from '@xterm/addon-fit';
import { Unicode11Addon } from '@xterm/addon-unicode11';
import { Terminal } from '@xterm/xterm';
import '@xterm/xterm/css/xterm.css';
import { type ReactNode, useCallback, useEffect, useRef, useState } from 'react';
import type { api } from '../../remote/index.ts';
import { ClientError, type TermView } from '../client.ts';
import { useNav } from '../nav.ts';
import { shortModel, statusLabel } from '../ui/format.ts';
import { useClient, useClientState } from '../ui/hooks.ts';
import { Sheet } from '../ui/kit.tsx';
import { useLive } from './Conversation.tsx';

const encoder = new TextEncoder();
const KEYS: readonly (readonly [string, string])[] = [
  ['Esc', '\x1b'],
  ['Tab', '\t'],
  ['↑', '\x1b[A'],
  ['↓', '\x1b[B'],
  ['←', '\x1b[D'],
  ['→', '\x1b[C'],
  ['Enter', '\r'],
];
const MIN_FONT = 6;
const MAX_FONT = 13;
/** JetBrains Mono advance width per pixel of font size. */
const CELL_RATIO = 0.6;

function endText(end: api.TermEnd): string {
  switch (end.reason) {
    case 'exited':
      return `The session exited${end.code === null ? '' : ` (code ${end.code})`}.`;
    case 'closed':
      return 'The terminal was closed.';
    case 'stale':
      return 'The session was replaced.';
    case 'overflow':
      return 'The phone fell behind; reattach to catch up.';
    case 'disconnected':
      return 'The connection dropped.';
    case 'shutdown':
      return 'The desktop is shutting down.';
    case 'detached':
      return 'The session was detached.';
    case 'not_allowed':
      return 'This phone may no longer view it.';
    case 'too_large':
      return 'The screen was too large to send.';
  }
}

function refusalText(reason: api.InputRefusal): string {
  switch (reason) {
    case 'not_allowed':
      return 'Typing is not permitted for this phone.';
    case 'input_held':
      return 'The desktop is holding input right now.';
    case 'exited':
      return 'The session has exited.';
    case 'read_only':
      return 'This view is read-only.';
    case 'detached':
      return 'The session is detached.';
    case 'stale':
      return 'The session was replaced.';
    case 'rejected':
      return 'The input was refused.';
  }
}

export function TerminalScreen({ liveKey, title }: { liveKey: string; title: string }): ReactNode {
  const client = useClient();
  const nav = useNav();
  const live = useLive(liveKey, null);
  const online = useClientState((state) => state.conn.phase === 'online');
  const control = useClientState((state) => state.welcome?.permissions.control ?? false);
  const host = useRef<HTMLDivElement>(null);
  const termRef = useRef<Terminal | null>(null);
  const fitRef = useRef<FitAddon | null>(null);
  const viewRef = useRef<TermView | null>(null);
  const [mode, setMode] = useState<api.ViewMode>('observe');
  const [attachEpoch, setAttachEpoch] = useState(0);
  const [ended, setEnded] = useState<string | null>(null);
  const [held, setHeld] = useState(false);
  /** The mode of the view attached now; null while (re)attaching (typing would be lost). */
  const [attached, setAttached] = useState<api.ViewMode | null>(null);
  const [size, setSize] = useState<{ cols: number; rows: number } | null>(null);
  const [ctrl, setCtrl] = useState(false);
  const [line, setLine] = useState('');
  const [modeSheet, setModeSheet] = useState(false);

  // The terminal renderer, created once.
  useEffect(() => {
    const element = host.current;
    if (element === null) return;
    const term = new Terminal({
      allowProposedApi: true,
      disableStdin: true,
      cursorBlink: false,
      fontFamily: 'JetBrains, ui-monospace, monospace',
      fontSize: 12,
      scrollback: 5000,
      theme: { background: '#00000000', foreground: '#b8c9d3' },
      allowTransparency: true,
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    term.loadAddon(new Unicode11Addon());
    term.unicode.activeVersion = '11';
    // OSC 52 would write the phone's clipboard from terminal output: never.
    const osc = term.parser.registerOscHandler(52, () => true);
    term.open(element);
    termRef.current = term;
    fitRef.current = fit;
    return () => {
      osc.dispose();
      term.dispose();
      termRef.current = null;
      fitRef.current = null;
    };
  }, []);

  /** Fits the font so the gateway's columns fill the width (Observe/Input follow the desktop size). */
  const fitFont = useCallback((cols: number) => {
    const term = termRef.current;
    const element = host.current;
    if (term === null || element === null || cols <= 0) return;
    const width = element.clientWidth - 20;
    const font = Math.max(MIN_FONT, Math.min(MAX_FONT, Math.floor((width / cols / CELL_RATIO) * 10) / 10));
    if (term.options.fontSize !== font) term.options.fontSize = font;
  }, []);

  // Attach (again on mode change, reconnect, or a manual reattach).
  useEffect(() => {
    const term = termRef.current;
    if (term === null || live === null || !online) return;
    let cancelled = false;
    let snapshotting = false;
    setEnded(null);
    const sink = {
      snapshot(bytes: Uint8Array, last: boolean) {
        if (!snapshotting) {
          term.reset();
          snapshotting = true;
        }
        term.write(bytes);
        if (last) snapshotting = false;
      },
      output(bytes: Uint8Array) {
        term.write(bytes);
      },
      resize(cols: number, rows: number) {
        setSize({ cols, rows });
        if (term.cols !== cols || term.rows !== rows) term.resize(cols, rows);
        if (mode !== 'control') fitFont(cols);
      },
      end(end: api.TermEnd) {
        if (!cancelled) setEnded(endText(end));
      },
      state(inputHeld: boolean) {
        setHeld(inputHeld);
      },
      refused(reason: api.InputRefusal) {
        client.notify(refusalText(reason), 'warn');
      },
    };
    client.attach(live.live, mode, sink).then(
      (view) => {
        if (cancelled) {
          view.detach();
          return;
        }
        viewRef.current = view;
        setAttached(view.mode);
        if (view.mode === 'control') {
          const fit = fitRef.current;
          term.options.fontSize = 11;
          const proposed = fit?.proposeDimensions();
          if (proposed !== undefined && proposed.cols > 0 && proposed.rows > 0) {
            term.resize(proposed.cols, proposed.rows);
            view.resize(proposed.cols, proposed.rows);
          }
        }
      },
      (error: unknown) => {
        if (cancelled) return;
        if (error instanceof ClientError && error.api?.code === 'geometry_owned') {
          client.notify('Another view controls this terminal’s size; switched to Input.', 'warn');
          setMode('input');
          return;
        }
        setEnded(error instanceof Error ? error.message : String(error));
      },
    );
    return () => {
      cancelled = true;
      viewRef.current?.detach();
      viewRef.current = null;
      setAttached(null);
    };
  }, [client, live?.live.key, live?.live.generation, mode, online, attachEpoch, fitFont]);

  // Control mode follows the phone's own size (rotation, keyboard).
  useEffect(() => {
    const element = host.current;
    if (element === null) return;
    const observer = new ResizeObserver(() => {
      const term = termRef.current;
      const view = viewRef.current;
      if (term === null) return;
      if (view?.mode === 'control') {
        const proposed = fitRef.current?.proposeDimensions();
        if (proposed !== undefined && proposed.cols > 0 && proposed.rows > 0 && (proposed.cols !== term.cols || proposed.rows !== term.rows)) {
          term.resize(proposed.cols, proposed.rows);
          view.resize(proposed.cols, proposed.rows);
        }
      } else {
        fitFont(term.cols);
      }
    });
    observer.observe(element);
    return () => observer.disconnect();
  }, [fitFont]);

  // Touch scrolling through the scrollback.
  useEffect(() => {
    const element = host.current;
    if (element === null) return;
    let lastY: number | null = null;
    let carry = 0;
    const start = (event: TouchEvent) => {
      lastY = event.touches[0]?.clientY ?? null;
      carry = 0;
    };
    const move = (event: TouchEvent) => {
      const term = termRef.current;
      const y = event.touches[0]?.clientY;
      if (term === null || y === undefined || lastY === null) return;
      const lineHeight = (term.options.fontSize ?? 12) * 1.2;
      carry += lastY - y;
      lastY = y;
      const lines = Math.trunc(carry / lineHeight);
      if (lines !== 0) {
        term.scrollLines(lines);
        carry -= lines * lineHeight;
      }
      event.preventDefault();
    };
    element.addEventListener('touchstart', start, { passive: true });
    element.addEventListener('touchmove', move, { passive: false });
    return () => {
      element.removeEventListener('touchstart', start);
      element.removeEventListener('touchmove', move);
    };
  }, []);

  const typing = mode !== 'observe' && attached !== null && attached !== 'observe' && ended === null && online && !held;
  const send = (bytes: Uint8Array<ArrayBuffer>, ctrlC = false) => {
    const view = viewRef.current;
    if (view === null || !view.input(bytes, ctrlC)) client.notify('Not connected to the terminal.', 'warn');
  };
  const sendText = (text: string) => send(encoder.encode(text));
  const onLine = (value: string) => {
    if (ctrl && value.length > line.length) {
      const char = value.slice(-1).toUpperCase();
      const code = char.charCodeAt(0);
      if (code >= 64 && code <= 95) send(Uint8Array.of(code & 0x1f), code === 67);
      setCtrl(false);
      return;
    }
    setLine(value);
  };

  return (
    <div className="screen pushed terminal">
      <header className="top">
        <div className="grow">
          <div className="eyebrow">LIVE TERMINAL</div>
          <h2>
            <button type="button" className="back" onClick={nav.pop} aria-label="Back">
              ‹
            </button>
            {live?.title ?? title}
          </h2>
        </div>
        <button type="button" className="pill mode" onClick={() => setModeSheet(true)}>
          ◉ {mode === 'observe' ? 'Observe' : mode === 'input' ? 'Input' : 'Control'}
        </button>
      </header>
      <div className="term" ref={host}>
        {ended !== null && (
          <div className="banner danger term-ended">
            <span className="grow">{ended}</span>
            <button type="button" className="button" disabled={live === null || !online} onClick={() => setAttachEpoch((n) => n + 1)}>
              Reattach
            </button>
          </div>
        )}
        {live === null && (
          <div className="banner warn term-ended">
            <span className="grow">This session is not running.</span>
          </div>
        )}
      </div>
      <div className="term-status">
        <span>{live === null ? '' : `${shortModel(live.model)} · ${statusLabel(live)}`}</span>
        <span>
          {held ? 'input held by desktop · ' : ''}
          {size === null ? '' : `${size.cols}×${size.rows}`}
        </span>
      </div>
      <div className="keys" role="toolbar" aria-label="Terminal keys">
        {KEYS.slice(0, 2).map(([label, bytes]) => (
          <button key={label} type="button" disabled={!typing} onClick={() => sendText(bytes)}>
            {label}
          </button>
        ))}
        <button type="button" disabled={!typing} aria-pressed={ctrl} onClick={() => setCtrl(!ctrl)}>
          Ctrl
        </button>
        {KEYS.slice(2).map(([label, bytes]) => (
          <button key={label} type="button" disabled={!typing} onClick={() => sendText(bytes)}>
            {label}
          </button>
        ))}
        <button type="button" disabled={!typing} onClick={() => send(Uint8Array.of(3), true)}>
          Ctrl+C
        </button>
      </div>
      <form
        className="term-input"
        onSubmit={(event) => {
          event.preventDefault();
          if (!typing) return;
          sendText(`${line}\r`);
          setLine('');
        }}
      >
        <input
          className="input"
          value={line}
          disabled={!typing}
          onChange={(event) => onLine(event.target.value)}
          placeholder={
            mode === 'observe'
              ? 'Observing: switch to Input to type'
              : attached === null
                ? 'Connecting to the terminal…'
                : ctrl
                  ? 'Ctrl + key…'
                  : 'Type, then Enter'
          }
          autoCapitalize="off"
          autoCorrect="off"
          spellCheck={false}
          aria-label="Line to send to the terminal"
        />
        <button type="submit" className="button primary" disabled={!typing}>
          ⏎
        </button>
      </form>
      {modeSheet && (
        <Sheet title="Terminal mode" icon="◉" onClose={() => setModeSheet(false)}>
          <div className="option-list">
            {(
              [
                ['observe', 'Observe', 'Watch only; nothing you do reaches the session.'],
                ['input', 'Input', 'Type into the session; the desktop keeps its size.'],
                ['control', 'Control', 'Type and size the terminal to this phone.'],
              ] as const
            ).map(([id, label, hint]) => (
              <button
                key={id}
                type="button"
                className="option"
                aria-pressed={mode === id}
                disabled={id !== 'observe' && !control}
                onClick={() => {
                  setModeSheet(false);
                  setMode(id);
                }}
              >
                <span className="grow">
                  {label}
                  <small>{id !== 'observe' && !control ? 'Needs the control permission.' : hint}</small>
                </span>
                {mode === id && <span className="chip accent">✓</span>}
              </button>
            ))}
          </div>
        </Sheet>
      )}
    </div>
  );
}
