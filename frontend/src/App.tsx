import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { AnalyticsPage } from './features/analytics/AnalyticsPage';
import { HomePage } from './features/home/HomePage';
import { SettingsPage } from './features/settings/SettingsPage';
import { TokenUnitProvider } from './lib/tokenUnit';
import './styles.css';

type Page = 'home' | 'analytics' | 'settings';

const SIDEBAR_COLLAPSED_KEY = 'llm-router.sidebarCollapsed';

function readSidebarCollapsed(): boolean {
  try { return localStorage.getItem(SIDEBAR_COLLAPSED_KEY) === '1' } catch { return false }
}

function persistSidebarCollapsed(collapsed: boolean) {
  try { localStorage.setItem(SIDEBAR_COLLAPSED_KEY, collapsed ? '1' : '0') } catch { /* ignore */ }
}

/** 16px 内联图标（不引入图标库依赖） */
const NAV_ICONS: Record<Page, ReactNode> = {
  home: <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><rect x="1.5" y="1.5" width="5.5" height="5.5" rx="1.4" /><rect x="9" y="1.5" width="5.5" height="5.5" rx="1.4" /><rect x="1.5" y="9" width="5.5" height="5.5" rx="1.4" /><rect x="9" y="9" width="5.5" height="5.5" rx="1.4" /></svg>,
  analytics: <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" aria-hidden="true"><path d="M2.5 13.5v-5" /><path d="M6.5 13.5V5.5" /><path d="M10.5 13.5V8" /><path d="M14.5 13.5V2.5" /></svg>,
  settings: <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><circle cx="8" cy="8" r="2.4" /><path d="M8 1.6v1.9M8 12.5v1.9M1.6 8h1.9M12.5 8h1.9M3.5 3.5l1.35 1.35M11.15 11.15l1.35 1.35M12.5 3.5l-1.35 1.35M4.85 11.15L3.5 12.5" /></svg>,
};

const PAGES: Array<{ page: Page; label: string }> = [
  { page: 'home', label: 'Dashboard' },
  { page: 'analytics', label: 'Analytics' },
  { page: 'settings', label: 'Settings' },
];

function PanelIcon({ open }: { open: boolean }) {
  // PanelLeftClose / PanelLeftOpen（参考 agent-panel 的折叠按钮）
  return open ? (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><rect x="1.5" y="2" width="13" height="12" rx="2" /><path d="M6 2v12" /><path d="M12.5 6.5 10.5 8l2 1.5" /></svg>
  ) : (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><rect x="1.5" y="2" width="13" height="12" rx="2" /><path d="M6 2v12" /><path d="M10.5 6.5 12.5 8l-2 1.5" /></svg>
  );
}

function pathToPage(pathname: string): Page {
  if (pathname.startsWith('/analytics')) return 'analytics';
  if (pathname === '/settings' || pathname.startsWith('/settings/')) return 'settings';
  return 'home';
}

function pageToPath(page: Page): string {
  if (page === 'analytics') return '/analytics';
  if (page === 'settings') return '/settings';
  return '/';
}

export default function App() {
  const [page, setPage] = useState<Page>(() => pathToPage(window.location.pathname));
  const [sidebarCollapsed, setSidebarCollapsed] = useState(readSidebarCollapsed);
  const toggleSidebar = useCallback(() => {
    setSidebarCollapsed((value) => {
      persistSidebarCollapsed(!value);
      return !value;
    });
  }, []);
  const navigate = useCallback((next: Page) => {
    window.history.pushState({}, '', pageToPath(next));
    setPage(next);
  }, []);
  useEffect(() => {
    const onPop = () => setPage(pathToPage(window.location.pathname));
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, []);
  return <TokenUnitProvider><div className={`shell ${sidebarCollapsed ? 'collapsed' : ''}`}><aside><div className="brand"><span className="brand-copy">LLM Router</span><button type="button" className="sidebar-toggle" onClick={toggleSidebar} aria-label={sidebarCollapsed ? '展开侧边栏' : '收起侧边栏'} title={sidebarCollapsed ? '展开侧边栏' : '收起侧边栏'}><PanelIcon open={!sidebarCollapsed} /></button></div><nav>{PAGES.map(({ page: p, label }) => <button key={p} type="button" className={`nav-button ${page === p ? 'active' : ''}`} onClick={() => navigate(p)} title={sidebarCollapsed ? label : undefined}><span className="nav-icon">{NAV_ICONS[p]}</span><span className="nav-label">{label}</span></button>)}</nav><div className="side-card"><span>Cost Board</span><strong>当月决策看板</strong><em>by supplier · key · model</em></div></aside><main>{page === 'home' ? <HomePage /> : page === 'analytics' ? <AnalyticsPage /> : <SettingsPage />}</main></div></TokenUnitProvider>;
}
