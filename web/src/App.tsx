import { useEffect, useState } from 'react';
import { ButtonItem, NavigationContent, NavigationFooter, NavigationHeader, Section, SideNavigation } from '@atlaskit/side-navigation';
import AlertIcon from '@atlaskit/icon/core/alert';
import DatabaseIcon from '@atlaskit/icon/core/database';
import DataFlowIcon from '@atlaskit/icon/core/data-flow';
import ListBulletedIcon from '@atlaskit/icon/core/list-bulleted';
import NodeIcon from '@atlaskit/icon/core/node';
import { IncidentDetailPage } from './pages/IncidentDetail';
import { IncidentsPage } from './pages/Incidents';
import { LogsPage } from './pages/Logs';
import { SchemaPage } from './pages/Schema';
import { ServicesPage } from './pages/Services';
import { TracePage } from './pages/TracePage';
import { match, navigate, useLocation } from './router';

function useHealth() {
  const [health, setHealth] = useState<{ docker: boolean; llm_configured: boolean } | null>(null);
  useEffect(() => {
    const load = () =>
      fetch('/api/health')
        .then((r) => r.json())
        .then(setHealth)
        .catch(() => setHealth(null));
    load();
    const t = window.setInterval(load, 20000);
    return () => window.clearInterval(t);
  }, []);
  return health;
}

export function App() {
  const { path, search } = useLocation();
  const health = useHealth();

  let page;
  let p: Record<string, string> | null;
  if ((p = match('/incidents/:id', path))) page = <IncidentDetailPage key={p.id} id={p.id} />;
  else if ((p = match('/traces/:tid', path))) page = <TracePage key={p.tid} traceId={p.tid} />;
  else if (path === '/services') page = <ServicesPage />;
  else if (path === '/logs') page = <LogsPage initial={search} />;
  else if (path === '/schema') page = <SchemaPage />;
  else page = <IncidentsPage />;

  const section = path.startsWith('/incidents') || path === '/' ? 'incidents' : path.startsWith('/traces') ? 'logs' : path.slice(1);
  const item = (key: string, label: string, Icon: typeof AlertIcon, to: string) => (
    <ButtonItem isSelected={section === key} iconBefore={<Icon label="" />} onClick={() => navigate(to)}>
      {label}
    </ButtonItem>
  );

  return (
    <div className="shell">
      <nav className="shell-nav" aria-label="McQueen">
        <SideNavigation label="McQueen">
          <NavigationHeader>
            <div className="nav-brand">
              <div className="nav-brand-mark">
                <svg width="18" height="18" viewBox="0 0 32 32" aria-hidden="true">
                  <path d="M5 23l6-13 5 9 4-5 6 9" stroke="white" strokeWidth="3" fill="none" strokeLinecap="round" strokeLinejoin="round" />
                </svg>
              </div>
              <div>
                <div style={{ fontWeight: 700 }}>McQueen</div>
                <div className="subtlest">AI triage · regere</div>
              </div>
            </div>
          </NavigationHeader>
          <NavigationContent>
            <Section title="Triage">
              {item('incidents', 'Incidents', AlertIcon, '/')}
              {item('logs', 'Logs & traces', ListBulletedIcon, '/logs')}
            </Section>
            <Section title="Context" hasSeparator>
              {item('services', 'Services', NodeIcon, '/services')}
              {item('schema', 'Database schema', DatabaseIcon, '/schema')}
            </Section>
          </NavigationContent>
          <NavigationFooter>
            <div className="stack" style={{ gap: 4, fontSize: 12, padding: '8px 0' }}>
              <span className="row" style={{ gap: 6 }}>
                <span className={`status-dot ${health?.docker ? '' : 'off'}`} />
                Docker {health?.docker ? 'connected' : 'not connected'}
              </span>
              <span className="row" style={{ gap: 6 }}>
                <span className={`status-dot ${health?.llm_configured ? '' : 'off'}`} />
                LLM {health?.llm_configured ? 'configured' : 'key missing'}
              </span>
              <span className="row subtlest" style={{ gap: 6 }}>
                <DataFlowIcon label="" size="small" /> W3C traceparent
              </span>
            </div>
          </NavigationFooter>
        </SideNavigation>
      </nav>
      <main className="shell-main">{page}</main>
    </div>
  );
}
