import Lozenge from '@atlaskit/lozenge';
import Heading from '@atlaskit/heading';
import Tooltip from '@atlaskit/tooltip';
import type { Overview } from '../api';
import { num } from '../format';

/** The context-assembly pipeline, stage by stage, with live counters. */
export function Pipeline({ o }: { o: Overview }) {
  const p = o.pipeline;
  const c = o.config;
  const stages = [
    {
      name: 'Docker daemon',
      sub: '/var/run/docker.sock (via read-only proxy)',
      metric: `${p.docker.tailing}/${p.docker.containers}`,
      hint: p.docker.ok ? 'containers tailed' : p.docker.error ?? 'not connected',
      active: p.docker.ok,
    },
    { name: 'Log stream', sub: 'bollard · JSON + text', metric: num(p.ingested), hint: 'lines ingested since start', active: p.ingested > 0 },
    { name: 'Detector', sub: 'errors · warn rules · 5xx', metric: num(p.problems), hint: `problems → ${num(p.incidents)} incidents`, active: p.problems > 0 },
    {
      name: 'Context',
      sub: `trace + code (${p.repos} repos) + schema (${p.tables} tables)`,
      metric: num(p.contexts),
      hint: 'contexts assembled',
      active: p.contexts > 0,
    },
    {
      name: 'Rig agent',
      sub: c.model,
      metric: num(p.triaged),
      hint: `triaged · ${p.triage_failed} failed · ${p.queued} queued · $${p.llm_cost_usd.toFixed(4)}`,
      active: p.triaged > 0,
    },
    {
      name: 'Alert report',
      sub: [c.notifications.google_chat && 'Google Chat', c.notifications.slack && 'Slack', 'UI'].filter(Boolean).join(' · '),
      metric: num(p.notified),
      hint: `alerts sent (≥ ${c.notifications.min_severity})`,
      active: p.notified > 0,
    },
  ];
  const live = p.docker.ok && c.llm_configured;

  return (
    <div className="card">
      <div className="card-title">
        <div>
          <Heading size="small">Triage pipeline</Heading>
          <div className="subtlest">Context assembly & inference, live counters since {new Date(p.started_at).toLocaleString()}</div>
        </div>
        <div className="row">
          {!c.auto_triage && <Lozenge appearance="moved">auto-triage off</Lozenge>}
          <Tooltip content={`${p.budget_left} of ${c.max_per_hour} auto-triages left this hour`}>
            <Lozenge appearance="default">budget {p.budget_left}/{c.max_per_hour}</Lozenge>
          </Tooltip>
          <Lozenge appearance={live ? 'success' : 'removed'} isBold>
            {live ? 'pipeline active' : !p.docker.ok ? 'docker disconnected' : 'LLM key missing'}
          </Lozenge>
        </div>
      </div>
      <div className="pipeline">
        {stages.map((s) => (
          <Tooltip key={s.name} content={s.hint}>
            <div className={`stage ${s.active ? 'active' : ''}`}>
              <div className="stage-name">{s.name}</div>
              <div className="stage-sub" title={s.sub}>
                {s.sub}
              </div>
              <div className="stage-metric">{s.metric}</div>
            </div>
          </Tooltip>
        ))}
      </div>
    </div>
  );
}
