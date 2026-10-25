import { useEffect, useState } from 'react';
import Button from '@atlaskit/button/new';
import DynamicTable from '@atlaskit/dynamic-table';
import Heading from '@atlaskit/heading';
import Lozenge from '@atlaskit/lozenge';
import ProgressBar from '@atlaskit/progress-bar';
import SectionMessage from '@atlaskit/section-message';
import Tooltip from '@atlaskit/tooltip';
import { api, type ServiceRow } from '../api';
import { LayerLozenge } from '../components/Lozenges';
import { num, pct, relTime } from '../format';
import { Link } from '../router';

/** Trace propagation verdict for one service over the last 24h. */
function propagation(s: ServiceRow): { label: string; appearance: 'success' | 'moved' | 'removed' | 'default'; hint: string } {
  const st = s.stats;
  if (!st || st.total === 0) return { label: 'no logs', appearance: 'default', hint: 'Nothing logged in the last 24 h.' };
  if (s.layer === 'infra') return { label: 'n/a', appearance: 'default', hint: 'Infrastructure: not expected to carry trace ids.' };
  const ratio = st.with_trace / st.total;
  if (ratio >= 0.9) return { label: 'propagating', appearance: 'success', hint: 'Nearly every line carries the request trace id.' };
  if (st.with_trace > 0) return { label: 'partial', appearance: 'moved', hint: 'Some code paths log outside the request span (background tasks, spawned work, or a missing layer).' };
  if (st.empty_trace_field > 0)
    return { label: 'not receiving', appearance: 'removed', hint: 'Logs have a trace_id field but it is empty: the caller does not send traceparent, or the ingress layer does not read it.' };
  return { label: 'no trace field', appearance: 'removed', hint: 'Logs carry no trace_id at all: add the trace middleware and JSON logging.' };
}

export function ServicesPage() {
  const [rows, setRows] = useState<ServiceRow[]>([]);
  const [crates, setCrates] = useState<[string, string][]>([]);
  const [loading, setLoading] = useState(true);
  const [reloading, setReloading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = () =>
    api
      .services()
      .then((r) => {
        setRows(r.services);
        setCrates(r.crates);
        setError(null);
      })
      .catch((e) => setError(String(e.message)))
      .finally(() => setLoading(false));
  useEffect(() => {
    void load();
  }, []);

  const reload = async () => {
    setReloading(true);
    try {
      await api.reload();
      await load();
    } finally {
      setReloading(false);
    }
  };

  const gaps = rows.filter((r) => ['not receiving', 'no trace field', 'partial'].includes(propagation(r).label));

  const head = {
    cells: [
      { key: 'name', content: 'Service', isSortable: true, width: 18 },
      { key: 'containers', content: 'Containers', width: 18 },
      { key: 'lines', content: 'Lines 24h', isSortable: true, width: 9 },
      { key: 'errors', content: 'Errors / warns', isSortable: true, width: 10 },
      { key: 'trace', content: 'Trace coverage', isSortable: true, width: 18 },
      { key: 'repo', content: 'Source', width: 17 },
      { key: 'last', content: 'Last line', width: 10 },
    ],
  };
  const tableRows = rows.map((s) => {
    const st = s.stats;
    const p = propagation(s);
    const ratio = st && st.total ? st.with_trace / st.total : 0;
    return {
      key: s.name,
      cells: [
        {
          key: s.name,
          content: (
            <div className="cell-title">
              <span className="row" style={{ gap: 6 }}>
                <strong className="mono">{s.name}</strong>
                <LayerLozenge layer={s.layer} />
              </span>
              {s.description && <span className="subtlest">{s.description}</span>}
            </div>
          ),
        },
        {
          key: s.containers.length,
          content: s.containers.length ? (
            <div className="stack" style={{ gap: 2 }}>
              {s.containers.map((c) => (
                <Tooltip key={c.id} content={`${c.image} · ${c.status}`}>
                  <span className="row" style={{ gap: 6 }}>
                    <span className={`status-dot ${c.state === 'running' ? '' : 'off'}`} />
                    <span className="mono">{c.name}</span>
                  </span>
                </Tooltip>
              ))}
            </div>
          ) : (
            <span className="subtlest">not seen</span>
          ),
        },
        { key: st?.total ?? 0, content: num(st?.total) },
        {
          key: (st?.errors ?? 0) * 1e6 + (st?.warns ?? 0),
          content: (
            <span>
              <span style={{ color: st?.errors ? 'var(--ds-text-danger)' : undefined }}>{num(st?.errors ?? 0)}</span> / {num(st?.warns ?? 0)}
            </span>
          ),
        },
        {
          key: ratio,
          content: (
            <Tooltip content={p.hint}>
              <div className="stack" style={{ gap: 4 }}>
                <span className="row" style={{ gap: 6 }}>
                  <Lozenge appearance={p.appearance}>{p.label}</Lozenge>
                  <span className="subtlest">{st ? pct(st.with_trace, st.total) : '—'}</span>
                </span>
                {st && st.total > 0 && s.layer !== 'infra' && <ProgressBar value={ratio} appearance={ratio >= 0.9 ? 'success' : 'default'} ariaLabel={`${s.name} trace coverage`} />}
              </div>
            </Tooltip>
          ),
        },
        {
          key: s.repo?.name ?? '',
          content: s.repo ? (
            <span className="mono">
              {s.repo.name}
              <div className="subtlest">
                {s.repo.branch ?? '—'}@{s.repo.commit ?? '?'} · {num(s.repo.files)} files
              </div>
            </span>
          ) : (
            <span className="subtlest">not mounted</span>
          ),
        },
        { key: st?.last_ts ?? '', content: relTime(st?.last_ts) },
      ],
    };
  });

  return (
    <div className="page">
      <div className="page-header">
        <div>
          <Heading size="large">Services</Heading>
          <div className="subtitle">Watched containers, error volume and how well each service propagates the W3C trace context.</div>
        </div>
        <Button onClick={reload} isLoading={reloading}>
          Re-index repos & schema
        </Button>
      </div>
      {error && (
        <SectionMessage appearance="error" title="Could not load services">
          <p>{error}</p>
        </SectionMessage>
      )}
      {gaps.length > 0 && (
        <SectionMessage appearance="warning" title={`${gaps.length} service${gaps.length > 1 ? 's' : ''} break the trace chain`}>
          <p>
            Requests through {gaps.map((g) => g.name).join(', ')} lose their trace id, so McQueen links their lines by shared keys (user_id, execution_id…) instead. That
            works, but it is less precise. Each service needs the ingress <code>traceparent</code> middleware, and must forward the header on outbound calls.
            Hover a verdict for details.
          </p>
        </SectionMessage>
      )}
      <div className="card">
        <div className="table-scroll">
          <DynamicTable head={head} rows={tableRows} isLoading={loading} defaultSortKey="name" defaultSortOrder="ASC" />
        </div>
      </div>
      <div className="card">
        <Heading size="xsmall">Indexed crates → repo</Heading>
        <div className="row" style={{ marginTop: 8 }}>
          {crates.length ? (
            crates.map(([c, r]) => (
              <span key={c} className="chip">
                {c} → {r}
              </span>
            ))
          ) : (
            <span className="muted">No repos indexed. Mount the service repos at REPOS_DIR.</span>
          )}
        </div>
        <div className="subtlest" style={{ marginTop: 8 }}>
          A log line's tracing target (e.g. <span className="mono">architect_sdk::authrs</span>) resolves to its crate here, which is how code context is found. See{' '}
          <Link to="/schema">schema</Link> for the database side.
        </div>
      </div>
    </div>
  );
}
