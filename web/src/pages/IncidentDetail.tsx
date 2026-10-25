import { useCallback, useEffect, useState } from 'react';
import Button from '@atlaskit/button/new';
import { CodeBlock } from '@atlaskit/code';
import Heading from '@atlaskit/heading';
import Lozenge from '@atlaskit/lozenge';
import ProgressBar from '@atlaskit/progress-bar';
import SectionMessage from '@atlaskit/section-message';
import Select from '@atlaskit/select';
import Spinner from '@atlaskit/spinner';
import Tabs, { Tab, TabList, TabPanel } from '@atlaskit/tabs';
import { api, type ContextSnapshot, type Incident, type TriageReport } from '../api';
import { LogList } from '../components/LogList';
import { SeverityLozenge, SourceLozenge, TriageLozenge } from '../components/Lozenges';
import { ServiceGraph } from '../components/ServiceGraph';
import { useIncidentEvents } from '../components/useLive';
import { Waterfall } from '../components/Waterfall';
import { copy, dateTime, num, relTime } from '../format';
import { Link } from '../router';

const STATUS_OPTIONS = [
  { label: 'Open', value: 'open' },
  { label: 'Acknowledged', value: 'acknowledged' },
  { label: 'Resolved', value: 'resolved' },
] as const;

export function IncidentDetailPage({ id }: { id: string }) {
  const [inc, setInc] = useState<Incident | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [tab, setTab] = useState(0);

  const load = useCallback(async () => {
    try {
      setInc(await api.incident(id));
      setError(null);
    } catch (e) {
      setError(String((e as Error).message));
    }
  }, [id]);

  useEffect(() => {
    void load();
  }, [load]);
  useIncidentEvents((changed) => changed === id && void load(), 300);
  // Poll while triage is in flight in case the event stream is buffered by a proxy.
  useEffect(() => {
    if (!inc || !['pending', 'running'].includes(inc.triage_state)) return;
    const t = window.setInterval(load, 4000);
    return () => window.clearInterval(t);
  }, [inc, load]);

  if (error) {
    return (
      <div className="page">
        <SectionMessage appearance="error" title="Incident not available">
          <p>{error}</p>
        </SectionMessage>
      </div>
    );
  }
  if (!inc) {
    return (
      <div className="page" style={{ alignItems: 'center', paddingTop: 80 }}>
        <Spinner size="large" label="Loading incident" />
      </div>
    );
  }

  const report = inc.report as TriageReport | null | undefined;
  const ctx = inc.context as ContextSnapshot | null | undefined;
  const trace = ctx?.trace;

  const retriage = async () => {
    setBusy(true);
    try {
      await api.retriage(inc.id);
      await load();
    } finally {
      setBusy(false);
    }
  };
  const setStatus = async (status: Incident['status']) => {
    setInc(await api.setStatus(inc.id, status));
  };

  return (
    <div className="page">
      <div className="subtlest">
        <Link to="/">Incidents</Link> / {inc.id}
      </div>
      <div className="page-header">
        <div style={{ minWidth: 0, flex: '1 1 600px' }}>
          <div className="row" style={{ marginBottom: 6 }}>
            <SeverityLozenge severity={inc.severity} />
            <TriageLozenge state={inc.triage_state} />
            <SourceLozenge source={inc.source} />
            {report?.category && <Lozenge>{report.category.replace(/_/g, ' ')}</Lozenge>}
          </div>
          <Heading size="large">{report?.title || inc.title}</Heading>
          <div className="subtitle">
            {report?.title && <span className="mono">{inc.title}</span>}
            <div className="row subtlest" style={{ marginTop: 4 }}>
              <span>first seen {dateTime(inc.first_seen)}</span>·<span>last seen {relTime(inc.last_seen)}</span>·
              <span>{num(inc.occurrences)} hits</span>
              {inc.trace_id && (
                <>
                  ·
                  <Link to={`/traces/${inc.trace_id}`} className="mono">
                    trace {inc.trace_id}
                  </Link>
                  <Button appearance="subtle" spacing="compact" onClick={() => copy(inc.trace_id!)}>
                    Copy
                  </Button>
                </>
              )}
            </div>
          </div>
        </div>
        <div className="toolbar">
          <div style={{ width: 180 }}>
            <Select
              inputId="incident-status"
              aria-label="Incident status"
              options={STATUS_OPTIONS as unknown as { label: string; value: string }[]}
              value={STATUS_OPTIONS.find((o) => o.value === inc.status)}
              onChange={(v) => v && setStatus((v as { value: Incident['status'] }).value)}
            />
          </div>
          <Button onClick={retriage} isLoading={busy || inc.triage_state === 'running'} appearance="default">
            {inc.triage_state === 'done' ? 'Re-run triage' : 'Run triage'}
          </Button>
        </div>
      </div>

      {['pending', 'running'].includes(inc.triage_state) && (
        <SectionMessage appearance="information" title={inc.triage_state === 'running' ? 'AI triage in progress' : 'Queued for AI triage'}>
          <p>
            {inc.triage_state === 'running'
              ? 'Assembling the trace, the code at each logging site and the schema of the tables involved, then asking the model. This page updates when it finishes.'
              : 'Waiting for the request to settle so the whole cascade across services is captured.'}
          </p>
        </SectionMessage>
      )}
      {inc.triage_state === 'failed' && (
        <SectionMessage appearance="error" title="Triage failed">
          <p className="mono" style={{ whiteSpace: 'pre-wrap' }}>{inc.error}</p>
          {ctx && <p>The assembled context is still available in the tabs below.</p>}
        </SectionMessage>
      )}
      {inc.triage_state === 'skipped' && (
        <SectionMessage appearance="warning" title="Not triaged automatically">
          <p>{inc.error ?? 'Auto-triage skipped this incident.'} Use “Run triage” to analyse it now.</p>
        </SectionMessage>
      )}

      {report && (
        <SectionMessage
          appearance={report.is_frontend_issue ? 'warning' : 'discovery'}
          title={report.is_frontend_issue ? 'For the frontend developer: this needs a frontend fix' : 'For the frontend developer: this is a backend issue'}
        >
          <p className="prose">{report.frontend_summary}</p>
          {report.frontend_action && (
            <p className="prose">
              <strong>What to do now: </strong>
              {report.frontend_action}
            </p>
          )}
          {report.owner && (
            <p>
              <strong>Owner: </strong>
              {report.owner}
            </p>
          )}
        </SectionMessage>
      )}

      <div className="card">
        <Tabs id="incident-tabs" selected={tab} onChange={setTab}>
          <TabList>
            <Tab>Analysis</Tab>
            <Tab>Request journey</Tab>
            <Tab>Logs{trace ? ` (${trace.events.length + trace.correlated.length})` : ''}</Tab>
            <Tab>Code{ctx ? ` (${ctx.snippets.length})` : ''}</Tab>
            <Tab>Database{ctx ? ` (${ctx.tables.length})` : ''}</Tab>
            <Tab>Prompt</Tab>
          </TabList>
          <TabPanel>
            <div className="tab-body" style={{ width: '100%' }}>
              <Analysis inc={inc} report={report} />
            </div>
          </TabPanel>
          <TabPanel>
            <div className="tab-body stack" style={{ width: '100%' }}>
              {trace ? (
                <>
                  <ServiceGraph services={trace.services} edges={trace.edges} origin={report?.origin_service} />
                  <div className="subtlest">
                    Solid arrows were seen in logs (URLs, proxy slugs); dashed arrows are inferred from timing. Dashed boxes are services linked by a shared key
                    because they logged no trace id.
                  </div>
                  <Waterfall trace={trace} />
                  {trace.gaps.length > 0 && (
                    <SectionMessage appearance="warning" title="Trace propagation gaps">
                      {trace.gaps.map((g) => (
                        <p key={g}>{g}</p>
                      ))}
                    </SectionMessage>
                  )}
                </>
              ) : (
                <div className="muted">The request journey is captured when triage runs.</div>
              )}
            </div>
          </TabPanel>
          <TabPanel>
            <div className="tab-body stack" style={{ width: '100%' }}>
              {trace ? (
                <>
                  <LogList events={[...trace.events, ...trace.correlated].sort((a, b) => a.ts.localeCompare(b.ts) || a.id - b.id)} anchorId={trace.first_error_id} />
                  {trace.nearby_problems.length > 0 && (
                    <>
                      <Heading size="xsmall">Other warnings and errors within ±30 s (not linked to this request)</Heading>
                      <LogList events={trace.nearby_problems} showTrace />
                    </>
                  )}
                </>
              ) : (
                <div className="muted">Logs are attached when triage runs.</div>
              )}
            </div>
          </TabPanel>
          <TabPanel>
            <div className="tab-body stack" style={{ width: '100%' }}>
              {ctx?.snippets.length ? (
                ctx.snippets.map((s) => (
                  <div key={`${s.location.repo}/${s.location.file}:${s.location.line}`} className="stack" style={{ gap: 6 }}>
                    <div className="spread">
                      <span className="mono" style={{ fontWeight: 600 }}>
                        {s.location.repo}/{s.location.file}:{s.location.line}
                      </span>
                      <span className="subtlest">
                        found via {s.location.via} · repo commit {s.commit ?? '?'} · from log #{s.log_id}
                      </span>
                    </div>
                    <pre className="block">{s.code}</pre>
                  </div>
                ))
              ) : (
                <div className="muted">No source location could be resolved from these log lines (unknown crate, or REPOS_DIR not mounted).</div>
              )}
              <div className="subtlest">
                The repo checkout may differ from the deployed image. Compare the commit with the image tag before trusting line numbers.
              </div>
            </div>
          </TabPanel>
          <TabPanel>
            <div className="tab-body stack" style={{ width: '100%' }}>
              {ctx?.tables.length ? (
                <>
                  <div className="row">
                    {ctx.tables.map((t) => (
                      <span key={t} className="chip">
                        {t}
                      </span>
                    ))}
                  </div>
                  <pre className="block">{ctx.schema_ddl}</pre>
                </>
              ) : (
                <div className="muted">No tables were matched for this incident.</div>
              )}
            </div>
          </TabPanel>
          <TabPanel>
            <div className="tab-body stack" style={{ width: '100%' }}>
              {ctx ? (
                <>
                  <div className="subtlest">
                    Exactly what was sent to the model (secrets redacted): {num(ctx.prompt_chars)} characters
                    {ctx.sections_truncated.length > 0 && ` · trimmed to budget: ${ctx.sections_truncated.join(', ')}`}
                  </div>
                  <CodeBlock text={ctx.prompt} language="markdown" showLineNumbers={false} />
                </>
              ) : (
                <div className="muted">No prompt yet.</div>
              )}
            </div>
          </TabPanel>
        </Tabs>
      </div>
    </div>
  );
}

function Analysis({ inc, report }: { inc: Incident; report?: TriageReport | null }) {
  const fe = inc.fe_payload;
  const usage = inc.llm_usage;
  return (
    <div className="stack" style={{ gap: 20 }}>
      {fe && (
        <div className="stack" style={{ gap: 6 }}>
          <Heading size="xsmall">What the browser saw</Heading>
          <dl className="kv">
            <dt>Request</dt>
            <dd className="mono">
              {fe.method ?? 'GET'} {fe.url} → {fe.status ?? 'network error'}
              {fe.duration_ms ? ` in ${Math.round(fe.duration_ms)} ms` : ''}
            </dd>
            {fe.page && (
              <>
                <dt>Page</dt>
                <dd className="mono">{fe.page}</dd>
              </>
            )}
            {fe.ui_message && (
              <>
                <dt>UI error</dt>
                <dd>{fe.ui_message}</dd>
              </>
            )}
            {fe.response_body && (
              <>
                <dt>Response body</dt>
                <dd>
                  <pre className="block" style={{ maxHeight: 160 }}>
                    {fe.response_body}
                  </pre>
                </dd>
              </>
            )}
          </dl>
        </div>
      )}

      {!report ? (
        <div className="muted">No AI analysis yet.</div>
      ) : (
        <>
          <div className="grid-2" style={{ alignItems: 'start' }}>
            <div className="stack" style={{ gap: 8 }}>
              <Heading size="xsmall">Root cause</Heading>
              <div className="prose">{report.root_cause}</div>
            </div>
            <div className="stack" style={{ gap: 8 }}>
              <Heading size="xsmall">Failure chain</Heading>
              <div className="chain">
                {report.failure_chain.map((s, i) => (
                  <div key={i} className={`chain-step ${s.service === report.origin_service ? 'origin' : ''}`}>
                    <div className="chain-dot">{i + 1}</div>
                    <div>
                      <div className="mono" style={{ fontWeight: 600 }}>
                        {s.service}
                        {s.service === report.origin_service && (
                          <>
                            {' '}
                            <Lozenge appearance="removed">origin</Lozenge>
                          </>
                        )}
                      </div>
                      <div className="muted">{s.observation}</div>
                    </div>
                  </div>
                ))}
              </div>
            </div>
          </div>

          <div className="stack" style={{ gap: 8 }}>
            <Heading size="xsmall">Suggested fix</Heading>
            <div className="prose">{report.suggested_fix}</div>
            {report.fix_location && (
              <div>
                <span className="subtlest">Location </span>
                <span className="chip">{report.fix_location}</span>
              </div>
            )}
            {report.patch && <CodeBlock text={report.patch} language={guessLang(report.fix_location)} showLineNumbers={false} />}
          </div>

          {report.db_notes && (
            <div className="stack" style={{ gap: 8 }}>
              <Heading size="xsmall">Database notes</Heading>
              <div className="prose">{report.db_notes}</div>
            </div>
          )}

          <div className="grid-2" style={{ alignItems: 'start' }}>
            <div className="stack" style={{ gap: 8 }}>
              <Heading size="xsmall">Evidence</Heading>
              {report.evidence.length ? (
                <ul style={{ margin: 0, paddingLeft: 18 }}>
                  {report.evidence.map((e, i) => (
                    <li key={i} className="mono" style={{ marginBottom: 4, overflowWrap: 'anywhere' }}>
                      {e}
                    </li>
                  ))}
                </ul>
              ) : (
                <span className="muted">—</span>
              )}
              {report.missing_context.length > 0 && (
                <>
                  <Heading size="xxsmall">Would raise confidence</Heading>
                  <ul style={{ margin: 0, paddingLeft: 18 }}>
                    {report.missing_context.map((e, i) => (
                      <li key={i} className="muted">
                        {e}
                      </li>
                    ))}
                  </ul>
                </>
              )}
            </div>
            <div className="stack" style={{ gap: 8 }}>
              <Heading size="xsmall">Confidence</Heading>
              <div className="row" style={{ gap: 12 }}>
                <div style={{ flex: 1 }}>
                  <ProgressBar value={Math.max(0, Math.min(1, report.confidence))} appearance={report.confidence >= 0.7 ? 'success' : 'default'} ariaLabel="Confidence" />
                </div>
                <strong>{Math.round(report.confidence * 100)}%</strong>
              </div>
              {usage && (
                <dl className="kv" style={{ marginTop: 8 }}>
                  <dt>Model</dt>
                  <dd className="mono">{usage.model}</dd>
                  <dt>Tokens</dt>
                  <dd>
                    {num(usage.input_tokens)} in · {num(usage.output_tokens)} out
                    {usage.cost_usd != null && ` · $${usage.cost_usd.toFixed(5)}`}
                  </dd>
                  <dt>Model calls</dt>
                  <dd>
                    {num(usage.model_calls)} in {((usage.elapsed_ms ?? 0) / 1000).toFixed(1)} s{usage.attempts && usage.attempts > 1 ? ` · ${usage.attempts} attempts` : ''}
                  </dd>
                  <dt>Tools used</dt>
                  <dd>
                    {usage.tool_calls?.length ? (
                      <div className="stack" style={{ gap: 2 }}>
                        {usage.tool_calls.map((t, i) => (
                          <span key={i} className="chip">
                            {t}
                          </span>
                        ))}
                      </div>
                    ) : usage.tools_enabled ? (
                      'none needed'
                    ) : (
                      'disabled'
                    )}
                  </dd>
                </dl>
              )}
            </div>
          </div>
        </>
      )}
    </div>
  );
}

function guessLang(loc?: string): 'rust' | 'typescript' | 'python' | 'sql' | 'text' {
  if (!loc) return 'text';
  if (loc.includes('.rs')) return 'rust';
  if (/\.(ts|tsx|js)/.test(loc)) return 'typescript';
  if (loc.includes('.py')) return 'python';
  if (loc.includes('.sql')) return 'sql';
  return 'text';
}
