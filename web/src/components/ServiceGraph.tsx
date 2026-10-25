import type { Edge, ServiceNode } from '../api';

// Left-to-right call graph: each service sits one column right of its deepest caller.
// Solid edges were observed in logs (URL, proxy slug, span ref); dashed ones are
// inferred from timing. Red marks hops that failed.

const NODE_W = 172;
const NODE_H = 64;
const COL_GAP = 70;
const ROW_GAP = 18;
const PAD = 16;

const statusColor = {
  error: 'var(--ds-border-danger, #e2483d)',
  warn: 'var(--ds-border-warning, #e56910)',
  ok: 'var(--ds-border-success, #22a06b)',
};

export function ServiceGraph({ services, edges, origin }: { services: ServiceNode[]; edges: Edge[]; origin?: string }) {
  if (services.length === 0) {
    return <div className="muted" style={{ padding: 16 }}>No services recorded for this request.</div>;
  }

  // Column = longest path from a root (cycles broken by visit order).
  const col = new Map<string, number>();
  const incoming = new Map<string, string[]>();
  for (const e of edges) incoming.set(e.to, [...(incoming.get(e.to) ?? []), e.from]);
  const depth = (name: string, seen: Set<string>): number => {
    if (col.has(name)) return col.get(name)!;
    if (seen.has(name)) return 0;
    seen.add(name);
    const parents = (incoming.get(name) ?? []).filter((p) => services.some((s) => s.name === p));
    const d = parents.length ? Math.max(...parents.map((p) => depth(p, seen) + 1)) : 0;
    col.set(name, d);
    return d;
  };
  services.forEach((s) => depth(s.name, new Set()));

  const columns: ServiceNode[][] = [];
  for (const s of services) {
    const c = col.get(s.name) ?? 0;
    (columns[c] ??= []).push(s);
  }
  const pos = new Map<string, { x: number; y: number }>();
  columns.forEach((nodes, c) =>
    nodes.forEach((n, r) => pos.set(n.name, { x: PAD + c * (NODE_W + COL_GAP), y: PAD + r * (NODE_H + ROW_GAP) })),
  );
  const width = PAD * 2 + columns.length * NODE_W + (columns.length - 1) * COL_GAP;
  const height = PAD * 2 + Math.max(...columns.map((c) => c.length)) * (NODE_H + ROW_GAP) - ROW_GAP;

  return (
    <div className="graph-wrap">
      <svg width={width} height={height} role="img" aria-label="Request journey across services">
        <defs>
          {(['ok', 'error'] as const).map((k) => (
            <marker key={k} id={`arrow-${k}`} viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
              <path d="M0,0 L10,5 L0,10 z" fill={k === 'error' ? statusColor.error : 'var(--ds-border-bold, #758195)'} />
            </marker>
          ))}
        </defs>
        {edges.map((e, i) => {
          const a = pos.get(e.from);
          const b = pos.get(e.to);
          if (!a || !b) return null;
          const x1 = a.x + NODE_W;
          const y1 = a.y + NODE_H / 2;
          const x2 = b.x - 2;
          const y2 = b.y + NODE_H / 2;
          const back = x2 <= x1;
          const d = back
            ? `M${a.x + NODE_W / 2},${a.y + NODE_H} C${a.x + NODE_W / 2},${a.y + NODE_H + 40} ${b.x + NODE_W / 2},${b.y + NODE_H + 40} ${b.x + NODE_W / 2},${b.y + NODE_H + 2}`
            : `M${x1},${y1} C${x1 + COL_GAP / 2},${y1} ${x2 - COL_GAP / 2},${y2} ${x2},${y2}`;
          const color = e.error ? statusColor.error : 'var(--ds-border-bold, #758195)';
          return (
            <g key={i}>
              <path d={d} fill="none" stroke={color} strokeWidth={e.error ? 2 : 1.5} strokeDasharray={e.kind === 'inferred' ? '5 4' : undefined} markerEnd={`url(#arrow-${e.error ? 'error' : 'ok'})`}>
                <title>
                  {`${e.from} → ${e.to} (${e.kind}${e.label ? `: ${e.label}` : ''})`}
                </title>
              </path>
            </g>
          );
        })}
        {services.map((s) => {
          const p = pos.get(s.name)!;
          const isOrigin = origin === s.name;
          return (
            <g key={s.name} transform={`translate(${p.x},${p.y})`}>
              <title>{`${s.name}: ${s.events} lines, ${s.errors} errors, ${s.warns} warnings (via ${s.via})\n${s.operations.join('\n')}`}</title>
              <rect
                width={NODE_W}
                height={NODE_H}
                rx={10}
                fill={s.status === 'error' ? 'var(--ds-background-danger, #ffeceb)' : 'var(--ds-surface-raised, #fff)'}
                stroke={statusColor[s.status]}
                strokeWidth={isOrigin ? 3 : 1.5}
                strokeDasharray={s.via === 'correlated' || s.via === 'referenced' ? '4 3' : undefined}
              />
              <text x={12} y={22} fontSize={13} fontWeight={650} fill="var(--ds-text, #172b4d)">
                {s.name.length > 20 ? s.name.slice(0, 19) + '…' : s.name}
              </text>
              <text x={12} y={39} fontSize={11} fill="var(--ds-text-subtlest, #626f86)">
                {s.layer} · {s.via === 'correlated' ? 'linked by key' : s.via === 'referenced' ? 'called, no logs' : s.via}
              </text>
              <text x={12} y={55} fontSize={11} fill={s.errors ? statusColor.error : 'var(--ds-text-subtle, #44546f)'}>
                {s.events} lines{s.errors ? ` · ${s.errors} err` : ''}
                {s.warns ? ` · ${s.warns} warn` : ''}
              </text>
              {isOrigin && (
                <text x={NODE_W - 10} y={20} fontSize={10} fontWeight={700} textAnchor="end" fill={statusColor.error}>
                  ORIGIN
                </text>
              )}
            </g>
          );
        })}
      </svg>
    </div>
  );
}
