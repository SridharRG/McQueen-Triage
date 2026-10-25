import { useState } from 'react';
import type { LogEvent } from '../api';
import { clock } from '../format';
import { Link } from '../router';
import { LevelLozenge } from './Lozenges';

export function LogList({
  events,
  anchorId,
  showTrace = false,
  empty = 'No log lines.',
}: {
  events: LogEvent[];
  anchorId?: number | null;
  showTrace?: boolean;
  empty?: string;
}) {
  const [open, setOpen] = useState<Set<number>>(new Set());
  if (events.length === 0) return <div className="muted">{empty}</div>;
  const toggle = (id: number) =>
    setOpen((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  return (
    <div className="log-list">
      {events.map((e) => {
        const cls = [
          'log-row',
          e.level === 'ERROR' ? 'error' : e.level === 'WARN' ? 'warn' : '',
          e.id === anchorId ? 'anchor' : '',
        ].join(' ');
        return (
          <div key={`${e.id}-${e.correlation ?? ''}`} className={cls} onClick={() => toggle(e.id)}>
            <span className="mono">{clock(e.ts)}</span>
            <span>
              <LevelLozenge level={e.level} />
            </span>
            <span className="mono log-svc" title={e.container}>
              {e.service}
            </span>
            <span className="log-msg">
              {e.correlation && (
                <span className="chip" title="Linked to this request by a shared key, not by trace id">
                  ~ {e.correlation}
                </span>
              )}{' '}
              {e.message}
              {e.target && <div className="log-target">{e.target}</div>}
              {showTrace && e.trace_id && (
                <div onClick={(ev) => ev.stopPropagation()}>
                  <Link to={`/traces/${e.trace_id}`} className="mono">
                    trace {e.trace_id.slice(0, 12)}…
                  </Link>
                </div>
              )}
            </span>
            {open.has(e.id) && (
              <div className="log-fields" onClick={(ev) => ev.stopPropagation()}>
                {JSON.stringify({ container: e.container, spans: e.spans, ...e.fields }, null, 2)}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}
