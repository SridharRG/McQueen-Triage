import Lozenge from '@atlaskit/lozenge';
import Spinner from '@atlaskit/spinner';
import type { Incident, Level } from '../api';

type Appearance = 'default' | 'inprogress' | 'moved' | 'new' | 'removed' | 'success';

export function SeverityLozenge({ severity }: { severity?: string | null }) {
  if (!severity) return <Lozenge>untriaged</Lozenge>;
  const map: Record<string, Appearance> = { critical: 'removed', high: 'moved', medium: 'new', low: 'default' };
  return (
    <Lozenge appearance={map[severity] ?? 'default'} isBold={severity === 'critical' || severity === 'high'}>
      {severity}
    </Lozenge>
  );
}

export function LevelLozenge({ level }: { level: Level | string }) {
  const map: Record<string, Appearance> = { ERROR: 'removed', WARN: 'moved', INFO: 'inprogress', DEBUG: 'default', TRACE: 'default' };
  return <Lozenge appearance={map[level] ?? 'default'}>{level}</Lozenge>;
}

export function TriageLozenge({ state }: { state: Incident['triage_state'] }) {
  if (state === 'running') {
    return (
      <span className="row" style={{ gap: 6 }}>
        <Spinner size="small" label="Triage running" />
        <Lozenge appearance="inprogress">analysing</Lozenge>
      </span>
    );
  }
  const map: Record<string, Appearance> = { pending: 'new', done: 'success', failed: 'removed', skipped: 'default' };
  const label: Record<string, string> = { pending: 'queued', done: 'triaged', failed: 'failed', skipped: 'skipped' };
  return <Lozenge appearance={map[state] ?? 'default'}>{label[state] ?? state}</Lozenge>;
}

export function StatusLozenge({ status }: { status: Incident['status'] }) {
  const map: Record<string, Appearance> = { open: 'removed', acknowledged: 'inprogress', resolved: 'success' };
  return <Lozenge appearance={map[status] ?? 'default'}>{status}</Lozenge>;
}

export function LayerLozenge({ layer }: { layer: string }) {
  const map: Record<string, Appearance> = { app: 'inprogress', platform: 'new', frontend: 'success', infra: 'default' };
  return <Lozenge appearance={map[layer] ?? 'default'}>{layer}</Lozenge>;
}

export function SourceLozenge({ source }: { source: Incident['source'] }) {
  const map: Record<string, Appearance> = { backend: 'default', frontend: 'new', manual: 'inprogress' };
  return <Lozenge appearance={map[source] ?? 'default'}>{source}</Lozenge>;
}
