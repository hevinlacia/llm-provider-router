import { useCallback, useEffect, useState } from 'react';
import { HomePage } from './features/home/HomePage';
import { SettingsPage } from './features/settings/SettingsPage';
import './styles.css';

function pathToPage(pathname: string): 'home' | 'settings' {
  if (pathname === '/settings' || pathname.startsWith('/settings/')) return 'settings';
  return 'home';
}

export default function App() {
  const [page, setPage] = useState<'home' | 'settings'>(() => pathToPage(window.location.pathname));
  const navigate = useCallback((next: 'home' | 'settings') => {
    const path = next === 'settings' ? '/settings' : '/';
    window.history.pushState({}, '', path);
    setPage(next);
  }, []);
  useEffect(() => {
    const onPop = () => setPage(pathToPage(window.location.pathname));
    window.addEventListener('popstate', onPop);
    return () => window.removeEventListener('popstate', onPop);
  }, []);
  return <div className="shell"><aside><div className="brand">LLM Provider Router</div><nav><button className={`nav-button ${page === 'home' ? 'active' : ''}`} onClick={() => navigate('home')}>Dashboard</button><button className={`nav-button ${page === 'settings' ? 'active' : ''}`} onClick={() => navigate('settings')}>Settings</button></nav><div className="side-card"><span>Cost Board</span><strong>当月决策看板</strong><em>by supplier · key · model</em></div></aside><main>{page === 'home' ? <HomePage /> : <SettingsPage />}</main></div>;
}
