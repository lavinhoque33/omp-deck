// Pairing: name this phone, compare fingerprints with the desktop's approval dialog, and wait
// for approval. Reached by opening the desktop's pairing link (`#pair=`) or pasting it.

import { type ReactNode, useState } from 'react';
import { inviteFromText } from '../nav.ts';
import { useClient, useClientState } from '../ui/hooks.ts';
import { Card, InstallHint, KeyValues } from '../ui/kit.tsx';

function defaultName(): string {
  const ua = navigator.userAgent;
  if (/iPhone/.test(ua)) return 'iPhone';
  if (/iPad/.test(ua)) return 'iPad';
  if (/Android/.test(ua)) return 'Android phone';
  return 'Phone';
}

export function PairingScreen({
  invite,
  onInvite,
  standalone,
}: {
  /** The invite from the opened link, if any. */
  invite: string | null;
  onInvite: (invite: string | null) => void;
  standalone: boolean;
}): ReactNode {
  const client = useClient();
  const flow = useClientState((state) => state.pairingFlow);
  const paired = useClientState((state) => state.pairing);
  const [name, setName] = useState(defaultName);
  const [pasted, setPasted] = useState('');
  const pastedInvite = inviteFromText(pasted);

  if (flow !== null) {
    const waiting = flow.stage !== 'failed';
    return (
      <div className="screen pushed">
        <header className="top">
          <div className="eyebrow">Pairing</div>
          <h2>{flow.stage === 'approval' ? 'Approve on your desktop' : flow.stage === 'failed' ? 'Pairing failed' : 'Connecting…'}</h2>
        </header>
        <div className="body">
          <div className="pair-hero">
            <div className="mark">{waiting ? <span className="busy" style={{ width: 28, height: 28, borderWidth: 3, borderColor: 'currentColor', borderRightColor: 'transparent' }} /> : '!'}</div>
            <p className="muted" style={{ margin: 0 }}>
              {flow.stage === 'approval'
                ? 'OmpDeck on your desktop asks whether to allow this phone. Check that it shows this fingerprint:'
                : flow.stage === 'failed'
                  ? (flow.error ?? 'The desktop did not accept this phone.')
                  : 'Reaching your desktop through the relay…'}
            </p>
          </div>
          <div className="fingerprint" aria-label="This phone’s fingerprint">
            {flow.fingerprint}
          </div>
          <Card title="Details">
            <KeyValues
              rows={[
                ['Phone name', flow.name],
                ['Desktop key', <span className="mono">{flow.gatewayFingerprint}</span>],
                ['Relay', <span className="mono">{flow.relay}</span>],
              ]}
            />
          </Card>
          {flow.stage === 'failed' && (
            <div className="buttons pair" style={{ display: 'grid', gridTemplateColumns: '1fr 1fr' }}>
              <button type="button" className="button" onClick={() => client.dismissPairing()}>
                Back
              </button>
              <button
                type="button"
                className="button primary"
                disabled={invite === null}
                onClick={() => invite !== null && void client.pair(invite, flow.name)}
              >
                Try again
              </button>
            </div>
          )}
        </div>
      </div>
    );
  }

  const active = invite ?? pastedInvite;
  return (
    <div className="screen pushed">
      <header className="top">
        <div className="eyebrow">OmpDeck</div>
        <h2>{paired === null ? 'Pair with your desktop' : 'Pair with another desktop'}</h2>
      </header>
      <div className="body">
        <div className="pair-hero">
          <div className="mark">◆</div>
          <p className="muted" style={{ margin: 0 }}>
            {active !== null
              ? 'Name this phone, then approve it on your desktop.'
              : 'On your desktop, open OmpDeck → Settings → Mobile access → Pair a phone, then open the link on this phone or paste it here.'}
          </p>
        </div>
        <InstallHint standalone={standalone} />
        {paired !== null && (
          <div className="banner warn">
            <span>⚠</span>
            <span className="grow">This phone is paired with {paired.name}. Pairing again replaces that pairing.</span>
          </div>
        )}
        {active === null && (
          <label className="field">
            <span>Pairing link</span>
            <input
              className="input"
              value={pasted}
              onChange={(event) => setPasted(event.target.value)}
              placeholder="https://…/#pair=v1.…"
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
            />
          </label>
        )}
        <label className="field">
          <span>Phone name</span>
          <input className="input" value={name} maxLength={64} onChange={(event) => setName(event.target.value)} />
        </label>
        <div className="buttons pair" style={{ display: 'grid', gridTemplateColumns: paired !== null || invite !== null ? '1fr 1fr' : '1fr' }}>
          {(paired !== null || invite !== null) && (
            <button
              type="button"
              className="button"
              onClick={() => {
                setPasted('');
                onInvite(null);
              }}
            >
              Cancel
            </button>
          )}
          <button
            type="button"
            className="button primary block"
            disabled={active === null || name.trim().length === 0}
            onClick={() => {
              if (active === null) return;
              onInvite(active);
              void client.pair(active, name.trim());
            }}
          >
            Pair this phone
          </button>
        </div>
      </div>
    </div>
  );
}
