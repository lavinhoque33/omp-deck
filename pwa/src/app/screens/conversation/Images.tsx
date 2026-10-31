// Transcript images: thumbnails fetched with `blob` + `download` (transfer.ts) into the shared
// object-URL cache, a tap-to-load state for large images, and a full-screen viewer.

import { type ReactNode, useEffect, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { download, rasterBlob } from '../../transfer.ts';
import { useClient, useOnline } from '../../ui/hooks.ts';
import { fileSize } from './compose.ts';
import { autoLoads, imageKey, imageLoads, transcriptImages } from './images.ts';

export interface ImageSource {
  session: string;
  agent: string | null;
}

export function ImageStrip({ images, source }: { images: readonly api.ImageRef[]; source: ImageSource | null }): ReactNode {
  if (images.length === 0) return null;
  return (
    <div className="img-strip">
      {images.map((image, index) => (
        <BlobImage key={`${image.blob}:${index}`} image={image} source={source} />
      ))}
    </div>
  );
}

type Phase = { state: 'idle' } | { state: 'loading' } | { state: 'ready'; url: string } | { state: 'failed'; message: string };

function BlobImage({ image, source }: { image: api.ImageRef; source: ImageSource | null }): ReactNode {
  const client = useClient();
  const online = useOnline();
  const key = source === null ? null : imageKey(source.session, source.agent, image.blob);
  const cached = key === null ? undefined : transcriptImages.peek(key);
  const [phase, setPhase] = useState<Phase>(cached !== undefined ? { state: 'ready', url: cached } : { state: 'idle' });
  const [wanted, setWanted] = useState(autoLoads(image));
  /** Bumped by a tap to retry a failed load. */
  const [attempt, setAttempt] = useState(0);
  const [open, setOpen] = useState(false);

  useEffect(() => {
    if (key === null || source === null || !wanted) return;
    let alive = true;
    let retained = false;
    const hit = transcriptImages.peek(key);
    if (hit === undefined && !online) return;
    if (hit === undefined) setPhase({ state: 'loading' });
    transcriptImages
      .load(key, async () => {
        const file = await imageLoads.run(() => download(client, { op: 'blob', session: source.session, agent: source.agent, blob: image.blob }));
        const blob = rasterBlob(file);
        if (blob === null) throw new Error('not a PNG, JPEG, GIF or WebP image');
        return blob;
      })
      .then(
        (url) => {
          if (!alive) return;
          transcriptImages.retain(key);
          retained = true;
          setPhase({ state: 'ready', url });
        },
        (error: unknown) => {
          if (alive) setPhase({ state: 'failed', message: error instanceof Error ? error.message : String(error) });
        },
      );
    return () => {
      alive = false;
      if (retained) transcriptImages.release(key);
    };
  }, [client, key, wanted, online, attempt]);

  const label = `${image.mime.replace('image/', '').toUpperCase()}${image.bytes > 0 ? ` · ${fileSize(image.bytes)}` : ''}`;
  if (phase.state === 'ready') {
    return (
      <>
        <button type="button" className="img-thumb" onClick={() => setOpen(true)} aria-label={`Open image (${label})`}>
          <img src={phase.url} alt={`Image, ${label}`} />
        </button>
        {open && (
          <div className="img-viewer" role="dialog" aria-modal="true" aria-label="Image" onClick={() => setOpen(false)}>
            <header>
              <span className="chip accent">{label}</span>
              <button type="button" className="icon-btn" onClick={() => setOpen(false)} aria-label="Close">
                ✕
              </button>
            </header>
            <img src={phase.url} alt={`Image, ${label}`} />
          </div>
        )}
      </>
    );
  }
  return (
    <button
      type="button"
      className={`img-thumb placeholder${phase.state === 'failed' ? ' failed' : ''}`}
      disabled={source === null || phase.state === 'loading' || !online}
      onClick={() => {
        setPhase({ state: 'idle' });
        setWanted(true);
        setAttempt((value) => value + 1);
      }}
      title={phase.state === 'failed' ? phase.message : undefined}
    >
      <span className="img-glyph">{phase.state === 'loading' ? <span className="busy" /> : '▣'}</span>
      <small>{label}</small>
      <small className="muted">
        {source === null
          ? 'Not available'
          : phase.state === 'loading'
            ? 'Loading…'
            : phase.state === 'failed'
              ? 'Failed · tap to retry'
              : !online
                ? 'Offline'
                : 'Tap to load'}
      </small>
    </button>
  );
}
