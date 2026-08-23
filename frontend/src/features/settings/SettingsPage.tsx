import { useCallback, useEffect, useState } from 'react';
import { api } from '../../api';
import type { ModelEquivalencesConfig, RouterCapabilities, ThinkingMapsConfig, TokenPriceConfig, V2Status } from '../../types';
import { CapabilitiesPanel } from './capabilities';
import { ModelEquivalencesPanel, ModelAliasesPanel, TokenPricesPanel } from './panels';
import { ThinkingLevelMapPanel } from './thinking';
import { V2Panel } from './v2';

export function SettingsPage() {
  const [tokenPrices, setTokenPrices] = useState<TokenPriceConfig | null>(null);
  const [thinkingMaps, setThinkingMaps] = useState<ThinkingMapsConfig | null>(null);
  const [v2, setV2] = useState<V2Status | null>(null);
  const [equivalences, setEquivalences] = useState<ModelEquivalencesConfig | null>(null);
  const [caps, setCaps] = useState<RouterCapabilities | null>(null);
  const [capsError, setCapsError] = useState('');
  const [status, setStatus] = useState('');
  const [error, setError] = useState('');

  const loadSettings = useCallback(async () => {
    try {
      const [tokenPriceData, thinkingData, v2Data, equivData, capsData] = await Promise.all([api.tokenPrices(), api.thinkingMaps().catch(() => null as unknown as ThinkingMapsConfig), api.v2Status(), api.equivalences(), api.routerCapabilities().catch(() => null as unknown as RouterCapabilities)]);
      setTokenPrices(tokenPriceData);
      setThinkingMaps(thinkingData);
      setV2(v2Data);
      setEquivalences(equivData);
      if (capsData) setCaps(capsData);
      setCapsError('');
      setStatus('Settings loaded.');
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  const refreshCaps = useCallback(async () => {
    try {
      setCaps(await api.routerCapabilities());
      setCapsError('');
    } catch (err) {
      setCapsError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  useEffect(() => { void loadSettings(); }, [loadSettings]);

  return <section className="page active"><header><div><h1>Settings</h1><div className="muted">Manage providers, keys, token prices, and logical model routing.</div>{status && <div className="ok">{status}</div>}{error && <div className="error">{error}</div>}</div><button className="secondary" onClick={() => void loadSettings()}>Refresh</button></header>
    <CapabilitiesPanel caps={caps} onRefresh={refreshCaps} error={capsError} />
    <V2Panel config={v2} onSaved={setV2} onError={setError} />
    <TokenPricesPanel config={tokenPrices} v2={v2} equivalences={equivalences} onChange={setTokenPrices} onSaved={(next) => { setTokenPrices(next); setStatus('Token prices saved.'); }} onError={setError} />
    <ThinkingLevelMapPanel config={thinkingMaps} v2={v2} equivalences={equivalences} onChange={setThinkingMaps} onSaved={(next) => { setThinkingMaps(next); setStatus('Thinking maps saved.'); }} onError={setError} />
    <ModelEquivalencesPanel config={equivalences} onSaved={(next) => { setEquivalences(next); setStatus('Equivalences saved.'); }} onError={setError} />
  </section>;
}
