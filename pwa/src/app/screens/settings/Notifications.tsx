// Notifications: Web Push on/off for this phone (permission asked only from a tap), what the
// desktop pushes (turn finished, attention, ended, titles), and the inbox link.

import type { ReactNode } from 'react';
import type { api } from '../../../remote/index.ts';
import { usePush } from '../../push-client.ts';
import { useNav } from '../../nav.ts';
import { useFeed, useOnline } from '../../ui/hooks.ts';
import { Card, KeyValues, NeedsS4 } from '../../ui/kit.tsx';
import { SectionScreen, ToggleRow } from './common.tsx';
import { when } from './logic.ts';

const DEFAULT_PREFS: api.PushPrefs = { turn_finished: true, attention: true, ended: true, show_titles: false };

export function NotificationsSection(): ReactNode {
  return (
    <SectionScreen eyebrow="Settings" title="Notifications">
      <NeedsS4 what="Push notifications">
        <NotificationsBody />
      </NeedsS4>
    </SectionScreen>
  );
}

function NotificationsBody(): ReactNode {
  const nav = useNav();
  const online = useOnline();
  const push = usePush();
  const device = useFeed({ feed: 'device' }, 'device');
  const state = device?.push ?? null;
  const prefs = state?.prefs ?? DEFAULT_PREFS;
  // On when this browser holds a subscription the desktop also knows.
  const on = push.subscribed && state?.subscribed === true;
  const blocked = push.permission === 'denied';
  const canToggle = online && !push.busy && push.support === 'supported' && !blocked;

  const setPref = (key: keyof api.PushPrefs, value: boolean) => {
    void push.setPrefs({ ...prefs, [key]: value });
  };

  return (
    <>
      {push.support === 'ios_home_screen' && (
        <div className="banner warn" role="note">
          <span className="grow">
            <b>Add to Home Screen first.</b> On iPhone and iPad, web apps can show notifications only once installed: tap
            Share (the square with an arrow) in Safari, choose <b>Add to Home Screen</b>, then open OmpDeck from the Home
            Screen and come back here.
          </span>
        </div>
      )}
      {push.support === 'unsupported' && (
        <div className="banner warn" role="note">
          <span className="grow">This browser cannot receive Web Push notifications. Try Chrome, Edge, Firefox or Safari 16.4+.</span>
        </div>
      )}
      {blocked && (
        <div className="banner danger" role="note">
          <span className="grow">
            Notifications are blocked for this site. Allow them in the browser’s site settings, then turn push on here.
          </span>
        </div>
      )}
      {push.error !== null && (
        <div className="banner danger" role="alert">
          <span className="grow">{push.error}</span>
        </div>
      )}

      <Card title="Push" aside={<span className={`chip ${on ? 'ok' : ''}`}>{on ? 'On' : 'Off'}</span>}>
        <ToggleRow
          title="Push notifications"
          hint={
            on
              ? 'This phone is notified while the app is closed or in the background.'
              : 'Asks for permission when you turn it on. Nothing is pushed while the app is open on screen.'
          }
          on={on}
          disabled={!canToggle}
          onChange={(next) => void (next ? push.enable() : push.disable())}
        />
        <KeyValues
          rows={[
            ['Permission', <span className={`chip ${push.permission === 'granted' ? 'ok' : push.permission === 'denied' ? 'danger' : ''}`}>{push.permission}</span>],
            ['On the desktop', <span className={`chip ${state?.subscribed === true ? 'ok' : ''}`}>{state === null ? 'unknown' : state.subscribed ? 'subscribed' : 'not subscribed'}</span>],
            state?.since_unix_ms != null ? ['Since', when(state.since_unix_ms)] : null,
          ]}
        />
      </Card>

      <Card title="Notify me when">
        <ToggleRow
          title="A turn finishes"
          hint="omp finished answering and is waiting for you."
          on={prefs.turn_finished}
          disabled={!online || !on || push.busy}
          onChange={(value) => setPref('turn_finished', value)}
        />
        <ToggleRow
          title="A session needs attention"
          hint="An approval, a question or a dialog is waiting."
          on={prefs.attention}
          disabled={!online || !on || push.busy}
          onChange={(value) => setPref('attention', value)}
        />
        <ToggleRow
          title="A session ends"
          hint="omp exited or crashed."
          on={prefs.ended}
          disabled={!online || !on || push.busy}
          onChange={(value) => setPref('ended', value)}
        />
        <ToggleRow
          title="Show session titles"
          hint={
            <>
              <span className="privacy">Privacy</span> Titles pass through Apple’s, Google’s or Mozilla’s push service
              (encrypted) and show on the lock screen. Off: “A session finished”.
            </>
          }
          on={prefs.show_titles}
          disabled={!online || !on || push.busy}
          onChange={(value) => setPref('show_titles', value)}
        />
      </Card>

      <button type="button" className="button block nav-link" onClick={() => nav.setTab('inbox')}>
        <span className="grow">Open the inbox</span>
        <span className="muted small">finished, failed and waiting sessions</span>
        <span aria-hidden="true">›</span>
      </button>
    </>
  );
}
