// Usage: provider quota cards (remaining bars, reset times, the tightest remaining), a refresh,
// and the 53-week activity heatmap (scrolls sideways on a phone, newest week in view).

import { type ReactNode, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import type { api } from '../../../remote/index.ts';
import { age, dollars, tokens } from '../../ui/format.ts';
import { useClient, useFeed, useNow, useOnline } from '../../ui/hooks.ts';
import { Card, Empty, NeedsS4 } from '../../ui/kit.tsx';
import { Bar, SectionScreen, ask } from './common.tsx';
import { ACTIVITY_DAYS, type HeatCell, type Heatmap, buildHeatmap, localDay, percent, remainingTone, untilText } from './logic.ts';

export function UsageSection(): ReactNode {
  return (
    <SectionScreen eyebrow="Settings" title="Usage">
      <NeedsS4 what="Usage">
        <UsageBody />
      </NeedsS4>
    </SectionScreen>
  );
}

function UsageBody(): ReactNode {
  const client = useClient();
  const online = useOnline();
  const now = useNow();
  const feed = useFeed({ feed: 'usage' }, 'usage');
  const [days, setDays] = useState<api.DayCount[] | null>(null);
  const [loading, setLoading] = useState(false);

  const loadActivity = useCallback(async () => {
    setLoading(true);
    const reply = await ask(client, { op: 'usage_activity', days: ACTIVITY_DAYS }, 'Activity');
    setLoading(false);
    if (reply?.type === 'activity') setDays(reply.days);
  }, [client]);
  useEffect(() => {
    if (online && days === null) void loadActivity();
  }, [online, days, loadActivity]);

  const refresh = () => {
    void ask(client, { op: 'usage_refresh' }, 'Refresh');
    void loadActivity();
  };
  const usage = feed?.usage ?? null;
  const fetching = feed?.fetching === true || loading;

  return (
    <>
      <section className="usage-hero card">
        <div className="card-body">
          <div className="line spread">
            <div>
              <div className="eyebrow">Tightest remaining</div>
              <div className={`hero-number ${remainingTone(usage?.tightest_remaining ?? null)}`}>
                {usage?.tightest_remaining == null ? '—' : percent(usage.tightest_remaining)}
              </div>
            </div>
            <button type="button" className="button" disabled={!online || fetching} onClick={refresh}>
              {fetching ? <span className="busy" /> : '↻'} Refresh
            </button>
          </div>
          {usage?.tightest_remaining != null && <Bar fraction={usage.tightest_remaining} tone={remainingTone(usage.tightest_remaining)} />}
          <p className="muted small note">
            {feed?.fetched_unix_ms != null ? `Fetched ${age(feed.fetched_unix_ms, now)} ago` : fetching ? 'Fetching…' : 'Not fetched yet'}
            {usage !== null && ` · ${usage.reports.length} provider${usage.reports.length === 1 ? '' : 's'}`}
          </p>
        </div>
      </section>
      {feed?.error != null && (
        <div className="banner danger" role="alert">
          <span className="grow">{feed.error}</span>
        </div>
      )}
      {usage !== null && usage.unavailable.length > 0 && (
        <div className="banner warn" role="note">
          <span className="grow">No usage data from {usage.unavailable.join(', ')}.</span>
        </div>
      )}
      {usage === null && feed?.error == null && <Empty title={fetching ? 'Fetching usage…' : 'No usage yet'}>Refresh asks every provider again.</Empty>}
      {usage?.reports.map((report) => <ReportCard key={`${report.provider}:${report.account ?? ''}`} report={report} now={now} />)}
      <ActivityCard days={days} loading={loading} />
    </>
  );
}

function ReportCard({ report, now }: { report: api.UsageReportView; now: number }): ReactNode {
  return (
    <Card
      title={report.provider}
      aside={
        <span className="line wrap">
          {report.plan !== null && <span className="chip accent">{report.plan}</span>}
          {report.account !== null && <span className="chip">{report.account}</span>}
        </span>
      }
    >
      {report.limits.length === 0 && <p className="muted small">No limits reported.</p>}
      <div className="quota-list">
        {report.limits.map((limit) => (
          <Quota key={limit.id} limit={limit} now={now} />
        ))}
      </div>
      {report.notes.map((note) => (
        <p key={note} className="muted small note">
          {note}
        </p>
      ))}
    </Card>
  );
}

function Quota({ limit, now }: { limit: api.UsageLimitView; now: number }): ReactNode {
  const remaining = limit.remaining_fraction ?? (limit.used_fraction === null ? null : 1 - limit.used_fraction);
  const tone = remainingTone(remaining);
  const amount =
    limit.used !== null && limit.limit !== null
      ? `${fmtAmount(limit.used)} / ${fmtAmount(limit.limit)} ${limit.unit}`
      : limit.remaining !== null
        ? `${fmtAmount(limit.remaining)} ${limit.unit} left`
        : null;
  return (
    <div className="quota">
      <div className="line spread">
        <b>{limit.label}</b>
        <span className={`quota-left ${tone}`}>{remaining === null ? '—' : `${percent(remaining)} left`}</span>
      </div>
      {remaining !== null && <Bar fraction={remaining} tone={tone} />}
      <div className="quota-meta">
        {limit.window !== null && <span className="chip">{limit.window}</span>}
        {amount !== null && <span className="muted">{amount}</span>}
        {limit.resets_unix_ms !== null && (
          <span className="chip accent" title={new Date(limit.resets_unix_ms).toLocaleString()}>
            resets {untilText(limit.resets_unix_ms, now)}
          </span>
        )}
        {limit.status !== 'ok' && limit.status.length > 0 && <span className={`chip ${tone === 'ok' ? 'warn' : tone}`}>{limit.status}</span>}
      </div>
    </div>
  );
}

function fmtAmount(value: number): string {
  return Number.isInteger(value) ? tokens(value) : value.toFixed(2);
}

const ROW_LABELS = ['Mon', '', 'Wed', '', 'Fri', '', ''];

function ActivityCard({ days, loading }: { days: api.DayCount[] | null; loading: boolean }): ReactNode {
  const today = localDay(new Date());
  const map = useMemo(() => (days === null ? null : buildHeatmap(days, today)), [days, today]);
  const [picked, setPicked] = useState<HeatCell | null>(null);
  return (
    <Card title="Activity" aside={<span className="count">53 weeks</span>}>
      {map === null ? (
        <Empty title={loading ? 'Loading activity…' : 'No activity loaded'} />
      ) : (
        <>
          <ActivityStats map={map} />
          <HeatGrid map={map} picked={picked} onPick={setPicked} />
          <div className="heat-foot">
            <span className="muted small">{map.byCost ? 'By cost' : 'By requests'}</span>
            <span className="heat-legend" aria-hidden="true">
              less
              {[0, 1, 2, 3, 4].map((level) => (
                <i key={level} className={`l${level}`} />
              ))}
              more
            </span>
          </div>
          {picked !== null && (
            <div className="heat-pick" role="status">
              <b>{new Date(`${picked.day}T12:00:00`).toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short', year: 'numeric' })}</b>
              <span className="chip">{picked.requests} requests</span>
              <span className="chip">{tokens(picked.tokens)} tok</span>
              {picked.cost > 0 && <span className="chip accent">{dollars(picked.cost)}</span>}
            </div>
          )}
        </>
      )}
    </Card>
  );
}

function ActivityStats({ map }: { map: Heatmap }): ReactNode {
  const tiles: [string, string, string][] = [
    ['Past year', map.byCost ? dollars(map.totalCost) : `${map.totalRequests}`, map.byCost ? `${map.totalRequests} req · ${tokens(map.totalTokens)} tok` : `${tokens(map.totalTokens)} tokens`],
    ['Active days', `${map.activeDays}`, `longest streak ${map.longestStreak}d`],
    [
      'Busiest',
      map.busiest === null ? '—' : map.byCost ? dollars(map.busiest.cost) : `${map.busiest.requests}`,
      map.busiest === null ? 'no activity' : new Date(`${map.busiest.day}T12:00:00`).toLocaleDateString(undefined, { day: 'numeric', month: 'short' }),
    ],
  ];
  return (
    <div className="stat-tiles">
      {tiles.map(([label, value, sub]) => (
        <div key={label} className="stat-tile">
          <span className="eyebrow">{label}</span>
          <b>{value}</b>
          <small>{sub}</small>
        </div>
      ))}
    </div>
  );
}

function HeatGrid({ map, picked, onPick }: { map: Heatmap; picked: HeatCell | null; onPick: (cell: HeatCell) => void }): ReactNode {
  const scroller = useRef<HTMLDivElement>(null);
  // Start at the newest week (the right end).
  useLayoutEffect(() => {
    const element = scroller.current;
    if (element !== null) element.scrollLeft = element.scrollWidth;
  }, [map]);
  return (
    <div className="heat">
      <div className="heat-rows" aria-hidden="true">
        <span />
        {ROW_LABELS.map((label, row) => (
          <span key={row}>{label}</span>
        ))}
      </div>
      <div className="heat-scroll" ref={scroller}>
        <div className="heat-grid" style={{ gridTemplateColumns: `repeat(${map.weeks.length}, var(--cell))` }}>
          {map.months.map((month, week) => (
            <span key={`m${week}`} className="heat-month" style={{ gridColumn: week + 1, gridRow: 1 }}>
              {month ?? ''}
            </span>
          ))}
          {map.weeks.map((column, week) =>
            column.map((cell, row) =>
              cell === null ? null : (
                <button
                  key={cell.day}
                  type="button"
                  className={`heat-cell l${cell.level}`}
                  style={{ gridColumn: week + 1, gridRow: row + 2 }}
                  aria-pressed={picked?.day === cell.day}
                  aria-label={`${cell.day}: ${cell.requests} requests`}
                  onClick={() => onPick(cell)}
                />
              ),
            ),
          )}
        </div>
      </div>
    </div>
  );
}
