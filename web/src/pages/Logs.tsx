import { useCallback, useEffect, useRef, useState } from 'react';
import Button from '@atlaskit/button/new';
import Heading from '@atlaskit/heading';
import Select from '@atlaskit/select';
import Textfield from '@atlaskit/textfield';
import Toggle from '@atlaskit/toggle';
import { api, parseTraceInput, type LogEvent } from '../api';
import { LogList } from '../components/LogList';
import { navigate } from '../router';

type Opt = { label: string; value: string };
const LEVELS: Opt[] = ['TRACE', 'DEBUG', 'INFO', 'WARN', 'ERROR'].map((l) => ({ label: `≥ ${l}`, value: l }));
const LIVE_MAX = 500;

export function LogsPage({ initial }: { initial: URLSearchParams }) {
  const [service, setService] = useState<Opt | null>(initial.get('service') ? { label: initial.get('service')!, value: initial.get('service')! } : null);
  const [level, setLevel] = useState<Opt>(LEVELS.find((l) => l.value === initial.get('level')) ?? LEVELS[2]);
  const [q, setQ] = useState(initial.get('q') ?? '');
  const [traceQ, setTraceQ] = useState('');
  const [logs, setLogs] = useState<LogEvent[]>([]);
  const [services, setServices] = useState<Opt[]>([]);
  const [live, setLive] = useState(false);
  const [loading, setLoading] = useState(false);
  const paused = useRef(false);

  useEffect(() => {
    api.services().then((r) => setServices(r.services.map((s) => ({ label: s.name, value: s.name }))));
  }, []);

  const search = useCallback(async () => {
    setLoading(true);
    try {
      const r = await api.logs({ service: service?.value, level: level.value, q, limit: 300 });
      setLogs(r.logs);
    } finally {
      setLoading(false);
    }
  }, [service, level, q]);

  useEffect(() => {
    if (live) return;
    const t = window.setTimeout(search, 250);
    return () => window.clearTimeout(t);
  }, [search, live]);

  useEffect(() => {
    if (!live) return;
    const params = new URLSearchParams();
    if (service) params.set('service', service.value);
    params.set('level', level.value);
    if (q) params.set('q', q);
    const es = new EventSource(`/api/logs/stream?${params}`);
    es.onmessage = (m) => {
      if (paused.current) return;
      const ev = JSON.parse(m.data) as LogEvent;
      setLogs((prev) => [ev, ...prev].slice(0, LIVE_MAX));
    };
    return () => es.close();
  }, [live, service, level, q]);

  const openTrace = () => {
    const tid = parseTraceInput(traceQ);
    if (tid) navigate(`/traces/${tid}`);
  };

  return (
    <div className="page">
      <div className="page-header">
        <div>
          <Heading size="large">Logs</Heading>
          <div className="subtitle">Every line from the watched containers, normalised, newest first.</div>
        </div>
        <div className="toolbar" style={{ flex: '0 1 460px' }}>
          <div className="grow">
            <Textfield
              aria-label="Open trace"
              isCompact
              placeholder="Jump to trace id / traceparent…"
              value={traceQ}
              onChange={(e) => setTraceQ((e.target as HTMLInputElement).value)}
              onKeyDown={(e) => e.key === 'Enter' && openTrace()}
            />
          </div>
          <Button onClick={openTrace}>Open trace</Button>
        </div>
      </div>
      <div className="card stack">
        <div className="toolbar">
          <div className="fixed">
            <Select inputId="log-service" aria-label="Service" placeholder="All services" isClearable options={services} value={service} onChange={(v) => setService((v as Opt) ?? null)} />
          </div>
          <div style={{ width: 140 }}>
            <Select inputId="log-level" aria-label="Minimum level" options={LEVELS} value={level} onChange={(v) => v && setLevel(v as Opt)} />
          </div>
          <div className="grow">
            <Textfield aria-label="Search text" placeholder="Search message, fields or target…" value={q} onChange={(e) => setQ((e.target as HTMLInputElement).value)} />
          </div>
          <label className="row" style={{ gap: 4 }}>
            <Toggle id="live-toggle" isChecked={live} onChange={() => setLive((v) => !v)} label="Live tail" />
            <span>Live tail</span>
          </label>
          {!live && (
            <Button onClick={search} isLoading={loading}>
              Search
            </Button>
          )}
        </div>
        <div onMouseEnter={() => (paused.current = true)} onMouseLeave={() => (paused.current = false)}>
          <LogList events={logs} showTrace empty={live ? 'Waiting for new lines…' : 'No matching lines.'} />
        </div>
        {live && <div className="subtlest">Streaming. Hovering the list pauses it. Keeps the newest {LIVE_MAX} lines.</div>}
      </div>
    </div>
  );
}
