import { useCallback, useEffect, useMemo, useState } from 'react';
import Button from '@atlaskit/button/new';
import DynamicTable from '@atlaskit/dynamic-table';
import EmptyState from '@atlaskit/empty-state';
import Heading from '@atlaskit/heading';
import SectionMessage from '@atlaskit/section-message';
import Select from '@atlaskit/select';
import Textfield from '@atlaskit/textfield';
import Tooltip from '@atlaskit/tooltip';
import { api, parseTraceInput, type Incident, type Overview } from '../api';
import { Pipeline } from '../components/Pipeline';
import { SeverityLozenge, SourceLozenge, StatusLozenge, TriageLozenge } from '../components/Lozenges';
import { useIncidentEvents } from '../components/useLive';
import { num, pct, relTime } from '../format';
import { Link, navigate } from '../router';

type Opt = { label: string; value: string };
const STATUS_OPTIONS: Opt[] = [
  { label: 'Open', value: 'open' },
  { label: 'Acknowledged', value: 'acknowledged' },
  { label: 'Resolved', value: 'resolved' },
  { label: 'All', value: 'all' },
];

export function IncidentsPage() {
  const [overview, setOverview] = useState<Overview | null>(null);
  const [incidents, setIncidents] = useState<Incident[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<Opt>(STATUS_OPTIONS[0]);
  const [service, setService] = useState<Opt | null>(null);
  const [q, setQ] = useState('');
  const [traceInput, setTraceInput] = useState('');
  const [traceBusy, setTraceBusy] = useState(false);
  const [traceError, setTraceError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [o, list] = await Promise.all([
        api.overview(),
        api.incidents({ status: status.value, service: service?.value, q }),
      ]);
      setOverview(o);
      setIncidents(list.incidents);
      setError(null);
    } catch (e) {
      setError(String((e as Error).message ?? e));
    } finally {
      setLoading(false);
    }
  }, [status, service, q]);

  useEffect(() => {
    const t = window.setTimeout(load, q ? 250 : 0);
    return () => window.clearTimeout(t);
  }, [load, q]);
  useEffect(() => {
    const t = window.setInterval(load, 15000);
    return () => window.clearInterval(t);
  }, [load]);
  useIncidentEvents(() => void load());

  const triageTrace = async () => {
    const tid = parseTraceInput(traceInput);
    if (!tid) {
      setTraceError('Paste a 32-character trace id or a full traceparent header (00-<trace>-<span>-01).');
      return;
    }
    setTraceBusy(true);
    setTraceError(null);
    try {
      const res = await api.triageTrace(tid);
      navigate(`/incidents/${res.id}`);
    } catch (e) {
      setTraceError(String((e as Error).message));
    } finally {
      setTraceBusy(false);
    }
  };

  const services = useMemo(() => {
    const names = new Set<string>();
    overview?.services.forEach((s) => names.add(s.service));
    incidents.forEach((i) => i.service && names.add(i.service));
    return [...names].sort().map((n) => ({ label: n, value: n }));
  }, [overview, incidents]);

  const totals = useMemo(() => {
    const all = overview?.services ?? [];
    const svc = all.filter((x) => x.layer !== 'infra');
    const infra = all.filter((x) => x.layer === 'infra' && x.errors > 0);
    const sum = (list: typeof all, f: (x: (typeof all)[number]) => number) => list.reduce((a, x) => a + f(x), 0);
    return {
      lines: sum(svc, (x) => x.total),
      errors: sum(svc, (x) => x.errors),
      warns: sum(svc, (x) => x.warns),
      infraErrors: sum(infra, (x) => x.errors),
      infraNames: infra.map((x) => x.service),
      traced: sum(svc, (x) => x.with_trace),
      tracedTotal: sum(svc, (x) => x.total),
    };
  }, [overview]);

  const head = {
    cells: [
      { key: 'severity', content: 'Severity', isSortable: true, width: 9 },
      { key: 'title', content: 'Incident', width: 40 },
      { key: 'service', content: 'Origin', isSortable: true, width: 12 },
      { key: 'triage', content: 'AI triage', width: 11 },
      { key: 'source', content: 'Source', width: 8 },
      { key: 'seen', content: 'Last seen', isSortable: true, width: 9 },
      { key: 'count', content: 'Hits', isSortable: true, width: 5 },
      { key: 'status', content: 'Status', width: 9 },
    ],
  };
  const sevRank: Record<string, number> = { critical: 4, high: 3, medium: 2, low: 1 };
  const rows = incidents.map((i) => ({
    key: i.id,
    cells: [
      { key: sevRank[i.severity ?? ''] ?? 0, content: <SeverityLozenge severity={i.severity} /> },
      {
        key: i.title,
        content: (
          <div className="cell-title">
            <Link to={`/incidents/${i.id}`}>{i.report?.title || i.title}</Link>
            <span className="subtlest">
              {i.id}
              {i.trace_id && <> · trace <span className="mono">{i.trace_id.slice(0, 12)}…</span></>}
            </span>
            {i.report?.frontend_summary && <span className="muted" style={{ fontSize: 12 }}>{i.report.frontend_summary}</span>}
          </div>
        ),
      },
      { key: i.report?.origin_service ?? i.service ?? '', content: <span className="mono">{i.report?.origin_service || i.service || '—'}</span> },
      { key: i.triage_state, content: <TriageLozenge state={i.triage_state} /> },
      { key: i.source, content: <SourceLozenge source={i.source} /> },
      { key: i.last_seen, content: <Tooltip content={new Date(i.last_seen).toLocaleString()}><span>{relTime(i.last_seen)}</span></Tooltip> },
      { key: i.occurrences, content: num(i.occurrences) },
      { key: i.status, content: <StatusLozenge status={i.status} /> },
    ],
  }));

  return (
    <div className="page">
      <div className="page-header">
        <div>
          <Heading size="large">Incidents</Heading>
          <div className="subtitle">Backend failures traced across services and explained for frontend developers.</div>
        </div>
        <div className="stack" style={{ minWidth: 360, flex: '0 1 520px', gap: 6 }}>
          <div className="toolbar">
            <div className="grow">
              <Textfield
                aria-label="Trace id or traceparent"
                placeholder="Paste a trace id or traceparent from DevTools…"
                value={traceInput}
                onChange={(e) => setTraceInput((e.target as HTMLInputElement).value)}
                onKeyDown={(e) => e.key === 'Enter' && triageTrace()}
                isCompact
              />
            </div>
            <Button appearance="primary" onClick={triageTrace} isLoading={traceBusy}>
              Triage trace
            </Button>
          </div>
          {traceError && <div style={{ color: 'var(--ds-text-danger)', fontSize: 12 }}>{traceError}</div>}
        </div>
      </div>

      {error && (
        <SectionMessage appearance="error" title="Could not reach the McQueen API">
          <p>{error}</p>
        </SectionMessage>
      )}

      {overview && <Pipeline o={overview} />}

      {overview && (
        <div className="grid-4">
          <div className="card">
            <div className="stat-label">Open incidents</div>
            <div className="stat-value">{num(overview.incidents.open)}</div>
            <div className="stat-foot">
              {num(overview.incidents.acknowledged)} acknowledged · {num(overview.incidents.running)} analysing · {num(overview.incidents.queued)} queued
            </div>
          </div>
          <div className="card">
            <div className="stat-label">Service errors · 24h</div>
            <div className="stat-value" style={{ color: totals.errors ? 'var(--ds-text-danger)' : undefined }}>{num(totals.errors)}</div>
            <div className="stat-foot">
              {num(totals.warns)} warnings across {num(totals.lines)} lines
              {totals.infraErrors > 0 && (
                <>
                  {' · '}
                  <Link to={`/logs?service=${encodeURIComponent(totals.infraNames[0])}&level=ERROR`}>
                    +{num(totals.infraErrors)} infra ({totals.infraNames.join(', ')})
                  </Link>
                </>
              )}
            </div>
          </div>
          <div className="card">
            <div className="stat-label">AI triaged</div>
            <div className="stat-value">{num(overview.incidents.triaged)}</div>
            <div className="stat-foot">
              {overview.config.model} · {num(overview.pipeline.llm_tokens)} tokens this run
            </div>
          </div>
          <div className="card">
            <div className="stat-label">Trace coverage · 24h</div>
            <div className="stat-value">{pct(totals.traced, totals.tracedTotal)}</div>
            <div className="stat-foot">
              of service log lines carry a trace id · <Link to="/services">gaps by service</Link>
            </div>
          </div>
        </div>
      )}

      <div className="card">
        <div className="toolbar" style={{ marginBottom: 12 }}>
          <div className="fixed">
            <Select inputId="status-filter" aria-label="Status" options={STATUS_OPTIONS} value={status} onChange={(v) => v && setStatus(v as Opt)} />
          </div>
          <div className="fixed">
            <Select
              inputId="service-filter"
              aria-label="Service"
              placeholder="All services"
              isClearable
              options={services}
              value={service}
              onChange={(v) => setService((v as Opt) ?? null)}
            />
          </div>
          <div className="grow">
            <Textfield aria-label="Search incidents" placeholder="Search title, id or trace…" value={q} onChange={(e) => setQ((e.target as HTMLInputElement).value)} />
          </div>
          <Button onClick={load}>Refresh</Button>
        </div>
        <div className="table-scroll">
        <DynamicTable
          head={head}
          rows={rows}
          rowsPerPage={25}
          defaultPage={1}
          isLoading={loading}
          defaultSortKey="seen"
          defaultSortOrder="DESC"
          emptyView={
            <EmptyState
              header={status.value === 'open' ? 'No open incidents' : 'No incidents'}
              description="Errors in the watched containers, frontend reports and manual trace triages appear here."
            />
          }
        />
        </div>
      </div>
    </div>
  );
}
