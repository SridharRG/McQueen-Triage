// Typed client for the McQueen API (mirrors the Rust models).

export type Level = 'TRACE' | 'DEBUG' | 'INFO' | 'WARN' | 'ERROR';

export interface LogEvent {
  id: number;
  ts: string;
  service: string;
  container: string;
  level: Level;
  target?: string;
  message: string;
  trace_id?: string;
  fields: Record<string, unknown>;
  spans?: string[];
  correlation?: string;
}

export interface ChainStep {
  service: string;
  observation: string;
}

export interface TriageReport {
  title: string;
  severity: 'critical' | 'high' | 'medium' | 'low';
  category: string;
  origin_service: string;
  failure_chain: ChainStep[];
  root_cause: string;
  frontend_summary: string;
  frontend_action: string;
  is_frontend_issue: boolean;
  owner: string;
  suggested_fix: string;
  fix_location: string;
  patch: string;
  db_notes: string;
  confidence: number;
  evidence: string[];
  missing_context: string[];
}

export interface FrontendError {
  trace_id: string;
  method?: string;
  url?: string;
  status?: number;
  response_body?: string;
  ui_message?: string;
  page?: string;
  app_version?: string;
  user_agent?: string;
  duration_ms?: number;
}

export interface ServiceNode {
  name: string;
  layer: string;
  first_ts: string;
  last_ts: string;
  events: number;
  errors: number;
  warns: number;
  status: 'ok' | 'warn' | 'error';
  operations: string[];
  via: 'trace' | 'correlated' | 'jaeger' | 'frontend' | 'referenced';
}

export interface Edge {
  from: string;
  to: string;
  kind: 'observed' | 'inferred';
  label?: string | null;
  error: boolean;
}

export interface TraceView {
  trace_id?: string | null;
  start?: string | null;
  end?: string | null;
  duration_ms: number;
  services: ServiceNode[];
  edges: Edge[];
  events: LogEvent[];
  correlated: LogEvent[];
  nearby_problems: LogEvent[];
  first_error_id?: number | null;
  gaps: string[];
}

export interface Snippet {
  location: { repo: string; file: string; line: number; via: string };
  start_line: number;
  end_line: number;
  code: string;
  commit?: string | null;
  log_id: number;
}

export interface ContextSnapshot {
  trace: TraceView;
  snippets: Snippet[];
  tables: string[];
  schema_ddl: string;
  frontend?: FrontendError | null;
  prompt: string;
  prompt_chars: number;
  sections_truncated: string[];
}

export interface Usage {
  model?: string;
  input_tokens?: number;
  output_tokens?: number;
  total_tokens?: number;
  cost_usd?: number | null;
  model_calls?: number;
  tools_enabled?: boolean;
  tool_calls?: string[];
  elapsed_ms?: number;
  attempts?: number;
}

export interface Incident {
  id: string;
  fingerprint: string;
  trace_id?: string | null;
  source: 'backend' | 'frontend' | 'manual';
  service?: string | null;
  anchor_log_id?: number | null;
  title: string;
  level: Level;
  status: 'open' | 'acknowledged' | 'resolved';
  triage_state: 'pending' | 'running' | 'done' | 'failed' | 'skipped';
  severity?: TriageReport['severity'] | null;
  first_seen: string;
  last_seen: string;
  occurrences: number;
  fe_payload?: FrontendError | null;
  report?: Partial<TriageReport> | null;
  context?: ContextSnapshot | null;
  error?: string | null;
  llm_usage?: Usage | null;
}

export interface ServiceStats {
  service: string;
  container: string;
  total: number;
  with_trace: number;
  empty_trace_field: number;
  errors: number;
  warns: number;
  last_ts?: string | null;
}

export interface Overview {
  incidents: { open: number; acknowledged: number; resolved: number; triaged: number; queued: number; running: number };
  histogram: { service: string; bucket: string; total: number; errors: number; warns: number }[];
  services: (ServiceStats & { layer: string })[];
  pipeline: {
    docker: { ok: boolean; error?: string | null; containers: number; tailing: number };
    ingested: number;
    problems: number;
    incidents: number;
    contexts: number;
    triaged: number;
    triage_failed: number;
    notified: number;
    queued: number;
    budget_left: number;
    llm_tokens: number;
    llm_cost_usd: number;
    repos: number;
    tables: number;
    started_at: string;
  };
  config: {
    model: string;
    llm_configured: boolean;
    tools: boolean;
    max_turns: number;
    context_budget_chars: number;
    auto_triage: boolean;
    max_per_hour: number;
    settle_secs: number;
    jaeger: boolean;
    notifications: { google_chat: boolean; slack: boolean; min_severity: string };
    ingest_key_required: boolean;
    retention_hours: number;
  };
}

export interface ContainerInfo {
  id: string;
  name: string;
  service: string;
  image: string;
  state: string;
  status: string;
  project?: string | null;
  watched: boolean;
}

export interface ServiceRow {
  name: string;
  layer: string;
  description: string;
  owner?: string | null;
  containers: ContainerInfo[];
  repo?: { name: string; branch?: string | null; commit?: string | null; files: number } | null;
  stats?: ServiceStats | null;
}

export interface SchemaTableSummary {
  name: string;
  package: string;
  columns: number;
  api_path?: string | null;
  comment?: string | null;
}

export interface SchemaPackage {
  id: string;
  name: string;
  version?: string | null;
  schema: string;
  tables: number;
  source: string;
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, {
    ...init,
    headers: { 'content-type': 'application/json', ...(init?.headers ?? {}) },
  });
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    throw new Error((body as { error?: string }).error ?? `${res.status} ${res.statusText}`);
  }
  return body as T;
}

const qs = (params: Record<string, string | number | undefined | null>) => {
  const s = new URLSearchParams();
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== '') s.set(k, String(v));
  }
  const out = s.toString();
  return out ? `?${out}` : '';
};

export const api = {
  overview: () => request<Overview>('/api/overview'),
  services: () => request<{ services: ServiceRow[]; crates: [string, string][] }>('/api/services'),
  incidents: (p: { status?: string; service?: string; q?: string }) =>
    request<{ incidents: Incident[] }>(`/api/incidents${qs(p)}`),
  incident: (id: string) => request<Incident>(`/api/incidents/${encodeURIComponent(id)}`),
  setStatus: (id: string, status: Incident['status']) =>
    request<Incident>(`/api/incidents/${encodeURIComponent(id)}`, { method: 'PATCH', body: JSON.stringify({ status }) }),
  retriage: (id: string) => request<{ id: string }>(`/api/incidents/${encodeURIComponent(id)}/triage`, { method: 'POST' }),
  trace: (tid: string) => request<{ trace: TraceView; incident_id?: string | null }>(`/api/traces/${encodeURIComponent(tid)}`),
  triageTrace: (tid: string) =>
    request<{ id: string; trace_id: string }>(`/api/traces/${encodeURIComponent(tid)}/triage`, { method: 'POST' }),
  logs: (p: { service?: string; level?: string; q?: string; trace_id?: string; before_id?: number; limit?: number }) =>
    request<{ logs: LogEvent[] }>(`/api/logs${qs(p)}`),
  schema: (q?: string) => request<{ packages: SchemaPackage[]; tables: SchemaTableSummary[] }>(`/api/schema${qs({ q })}`),
  schemaTable: (name: string) => request<{ ddl: string }>(`/api/schema/tables/${encodeURIComponent(name)}`),
  reload: () => request<{ repos: number; tables: number }>('/api/reload', { method: 'POST' }),
};

/** Accepts a bare 32-hex trace id or a full traceparent header. */
export function parseTraceInput(input: string): string | null {
  const s = input.trim().toLowerCase();
  if (/^[0-9a-f]{32}$/.test(s) && !/^0+$/.test(s)) return s;
  const m = s.match(/00-([0-9a-f]{32})-[0-9a-f]{16}-[0-9a-f]{2}/);
  return m && !/^0+$/.test(m[1]) ? m[1] : null;
}
