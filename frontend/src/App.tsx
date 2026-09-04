import { useCallback, useEffect, useState } from 'react';
import { AnalyticsPage } from './features/analytics/AnalyticsPage';
import { HomePage } from './features/home/HomePage';
import { SettingsPage } from './features/settings/SettingsPage';
import { TokenUnitProvider } from './lib/tokenUnit';
import './styles.css';

type Page = 'home' | 'analytics' | 'settings';

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
  const navigate = useCallback((next: Page) => {
    window.history.pushState({}, '', pageToPath(next));
    setPage(next);
  }, []);
  useEffect(() => {
    const onPop = () => setPage(pathToPage(window.location.pathname));
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, []);
  return <TokenUnitProvider><div className="shell"><aside><div className="brand">LLM Provider Router</div><nav><button className={`nav-button ${page === 'home' ? 'active' : ''}`} onClick={() => navigate('home')}>Dashboard</button><button className={`nav-button ${page === 'analytics' ? 'active' : ''}`} onClick={() => navigate('analytics')}>Analytics</button><button className={`nav-button ${page === 'settings' ? 'active' : ''}`} onClick={() => navigate('settings')}>Settings</button></nav><div className="side-card"><span>Cost Board</span><strong>当月决策看板</strong><em>by supplier · key · model</em></div></aside><main>{page === 'home' ? <HomePage /> : page === 'analytics' ? <AnalyticsPage /> : <SettingsPage />}</main></div></TokenUnitProvider>;
}
