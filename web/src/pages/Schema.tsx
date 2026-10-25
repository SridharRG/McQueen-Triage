import { useEffect, useState } from 'react';
import DynamicTable from '@atlaskit/dynamic-table';
import Heading from '@atlaskit/heading';
import Textfield from '@atlaskit/textfield';
import { api, type SchemaPackage, type SchemaTableSummary } from '../api';

export function SchemaPage() {
  const [q, setQ] = useState('');
  const [packages, setPackages] = useState<SchemaPackage[]>([]);
  const [tables, setTables] = useState<SchemaTableSummary[]>([]);
  const [selected, setSelected] = useState<string | null>(null);
  const [ddl, setDdl] = useState('');

  useEffect(() => {
    const t = window.setTimeout(() => {
      api.schema(q).then((r) => {
        setPackages(r.packages);
        setTables(r.tables);
      });
    }, 200);
    return () => window.clearTimeout(t);
  }, [q]);

  useEffect(() => {
    if (selected) api.schemaTable(selected).then((r) => setDdl(r.ddl));
  }, [selected]);

  return (
    <div className="page">
      <div className="page-header">
        <div>
          <Heading size="large">Database schema</Heading>
          <div className="subtitle">
            From the architect-sdk packages (JSON files or core's read-only <span className="mono">/config/packages</span>). McQueen never connects to Postgres.
          </div>
        </div>
      </div>
      <div className="grid-4">
        {packages.map((p) => (
          <div key={p.id} className="card">
            <div className="stat-label">{p.name}</div>
            <div className="stat-value">{p.tables}</div>
            <div className="stat-foot">
              tables · schema <span className="mono">{p.schema}</span> · v{p.version ?? '?'}
            </div>
          </div>
        ))}
        {packages.length === 0 && <div className="card muted">No packages loaded. Set SCHEMA_DIR or CORE_PACKAGES_URL.</div>}
      </div>
      <div className="grid-2" style={{ alignItems: 'start' }}>
        <div className="card stack">
          <Textfield aria-label="Search tables" placeholder="Search tables or columns…" value={q} onChange={(e) => setQ((e.target as HTMLInputElement).value)} />
          <DynamicTable
            head={{
              cells: [
                { key: 'name', content: 'Table', isSortable: true },
                { key: 'cols', content: 'Columns', isSortable: true, width: 15 },
              ],
            }}
            rows={tables.map((t) => ({
              key: t.name,
              onClick: () => setSelected(t.name),
              cells: [
                {
                  key: t.name,
                  content: (
                    <div className="cell-title" style={{ cursor: 'pointer' }}>
                      <strong className="mono">{t.name}</strong>
                      {t.api_path && <span className="subtlest mono">{t.api_path}</span>}
                    </div>
                  ),
                },
                { key: t.columns, content: t.columns },
              ],
            }))}
            rowsPerPage={20}
            defaultPage={1}
            highlightedRowIndex={selected ? [tables.findIndex((t) => t.name === selected)] : undefined}
          />
        </div>
        <div className="card stack" style={{ position: 'sticky', top: 16 }}>
          <Heading size="small">{selected ?? 'Select a table'}</Heading>
          {selected ? <pre className="block">{ddl}</pre> : <div className="muted">The DDL the model sees for a table appears here.</div>}
        </div>
      </div>
    </div>
  );
}
