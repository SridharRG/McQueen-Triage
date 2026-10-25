import type { TraceView } from '../api';

/** Per-service activity across the request: bar = first..last log line, ticks = errors. */
export function Waterfall({ trace }: { trace: TraceView }) {
  const services = trace.services.filter((s) => s.via !== 'frontend' && s.via !== 'referenced');
  if (!trace.start || !trace.end || services.length === 0) return null;
  const t0 = new Date(trace.start).getTime();
  const span = Math.max(new Date(trace.end).getTime() - t0, 1);
  const at = (iso: string) => ((new Date(iso).getTime() - t0) / span) * 100;
  const errors = [...trace.events, ...trace.correlated].filter((e) => e.level === 'ERROR');

  return (
    <div className="waterfall">
      <div className="subtlest">service</div>
      <div className="subtlest">0 ms → {trace.duration_ms.toLocaleString()} ms</div>
      <div className="subtlest wf-dur">active</div>
      {services.map((s) => {
        const left = at(s.first_ts);
        const width = Math.max(at(s.last_ts) - left, 0.6);
        const dur = new Date(s.last_ts).getTime() - new Date(s.first_ts).getTime();
        return [
          <div key={`${s.name}-n`} className="mono" title={s.operations.join('\n')}>
            {s.name}
          </div>,
          <div key={`${s.name}-t`} className="wf-track">
            <div
              className={`wf-bar ${s.status} ${s.via === 'correlated' ? 'correlated' : ''}`}
              style={{ left: `${left}%`, width: `${width}%` }}
              title={`${s.name}: +${Math.round(at(s.first_ts) * span / 100)} ms, active ${dur} ms`}
            />
            {errors
              .filter((e) => e.service === s.name)
              .map((e) => (
                <div key={e.id} className="wf-tick" style={{ left: `${at(e.ts)}%` }} title={e.message} />
              ))}
          </div>,
          <div key={`${s.name}-d`} className="subtlest wf-dur">
            {dur} ms
          </div>,
        ];
      })}
    </div>
  );
}
