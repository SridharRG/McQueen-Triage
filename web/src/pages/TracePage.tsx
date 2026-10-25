import { useEffect, useState } from 'react';
import Button from '@atlaskit/button/new';
import Heading from '@atlaskit/heading';
import SectionMessage from '@atlaskit/section-message';
import Spinner from '@atlaskit/spinner';
import { api, type TraceView } from '../api';
import { LogList } from '../components/LogList';
import { ServiceGraph } from '../components/ServiceGraph';
import { Waterfall } from '../components/Waterfall';
import { dateTime, num } from '../format';
import { Link, navigate } from '../router';

export function TracePage({ traceId }: { traceId: string }) {
  const [trace, setTrace] = useState<TraceView | null>(null);
  const [incidentId, setIncidentId] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setTrace(null);
    api
      .trace(traceId)
      .then((r) => {
        setTrace(r.trace);
        setIncidentId(r.incident_id ?? null);
      })
      .catch((e) => setError(String(e.message)));
  }, [traceId]);

  const triage = async () => {
    setBusy(true);
    try {
      const r = await api.triageTrace(traceId);
      navigate(`/incidents/${r.id}`);
    } catch (e) {
      setError(String((e as Error).message));
      setBusy(false);
    }
  };

  if (error) {
    return (
      <div className="page">
        <SectionMessage appearance="error" title="Trace not available">
          <p>{error}</p>
        </SectionMessage>
      </div>
    );
  }
  if (!trace) {
    return (
      <div className="page" style={{ alignItems: 'center', paddingTop: 80 }}>
        <Spinner size="large" label="Loading trace" />
      </div>
    );
  }
  const all = [...trace.events, ...trace.correlated].sort((a, b) => a.ts.localeCompare(b.ts) || a.id - b.id);
  const errors = all.filter((e) => e.level === 'ERROR').length;

  return (
    <div className="page">
      <div className="subtlest">
        <Link to="/logs">Logs</Link> / trace
      </div>
      <div className="page-header">
        <div>
          <Heading size="large">Request journey</Heading>
          <div className="subtitle mono">{traceId}</div>
          <div className="subtlest" style={{ marginTop: 4 }}>
            {dateTime(trace.start)} · {num(trace.duration_ms)} ms · {trace.services.length} services · {num(all.length)} lines · {errors} errors
          </div>
        </div>
        <div className="toolbar">
          {incidentId && <Button onClick={() => navigate(`/incidents/${incidentId}`)}>Open incident {incidentId}</Button>}
          <Button appearance="primary" onClick={triage} isLoading={busy}>
            {incidentId ? 'Re-run AI triage' : 'Triage with AI'}
          </Button>
        </div>
      </div>

      {all.length === 0 && (
        <SectionMessage appearance="warning" title="No backend logs carry this trace id">
          <p>
            Either the request never reached launchpad, the logs were pruned (retention), or the service that handled it does not propagate <code>traceparent</code>.
          </p>
        </SectionMessage>
      )}

      <div className="card stack">
        <Heading size="small">Services</Heading>
        <ServiceGraph services={trace.services} edges={trace.edges} />
        <Waterfall trace={trace} />
        {trace.gaps.length > 0 && (
          <SectionMessage appearance="warning" title="Trace propagation gaps">
            {trace.gaps.map((g) => (
              <p key={g}>{g}</p>
            ))}
          </SectionMessage>
        )}
      </div>

      <div className="card stack">
        <Heading size="small">Log lines</Heading>
        <LogList events={all} anchorId={trace.first_error_id} />
      </div>

      {trace.nearby_problems.length > 0 && (
        <div className="card stack">
          <Heading size="small">Other problems within ±30 s</Heading>
          <LogList events={trace.nearby_problems} showTrace />
        </div>
      )}
    </div>
  );
}
