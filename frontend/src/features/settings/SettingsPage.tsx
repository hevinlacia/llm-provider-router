import { useCallback, useEffect, useState } from 'react';
import { api } from '../../api';
import type { ModelEquivalencesConfig, TokenPriceConfig, V2Status } from '../../types';
import { ModelEquivalencesPanel, TokenPricesPanel } from './panels';
import { SupplierModelConfigPanel } from './supplierModels';
import { V2Panel } from './v2';

export function SettingsPage() {
  const [tokenPrices, setTokenPrices] = useState<TokenPriceConfig | null>(null);
  const [v2, setV2] = useState<V2Status | null>(null);
  const [equivalences, setEquivalences] = useState<ModelEquivalencesConfig | null>(null);
  const [status, setStatus] = useState('');
  const [error, setError] = useState('');

  const loadSettings = useCallback(async () => {
    try {
      const [tokenPriceData, v2Data, equivData] = await Promise.all([api.tokenPrices(), api.v2Status(), api.equivalences()]);
      setTokenPrices(tokenPriceData);
      setV2(v2Data);
      setEquivalences(equivData);
      setStatus('Settings loaded.');
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  useEffect(() => { void loadSettings(); }, [loadSettings]);

  return <section className="page active"><header><div><h1>Settings</h1><div className="muted">Manage providers, keys, token prices, and logical model routing.</div>{status && <div className="ok">{status}</div>}{error && <div className="error">{error}</div>}</div><button className="secondary" onClick={() => void loadSettings()}>Refresh</button></header>
    <V2Panel config={v2} onSaved={setV2} onError={setError} />
    <TokenPricesPanel config={tokenPrices} v2={v2} equivalences={equivalences} onChange={setTokenPrices} onSaved={(next) => { setTokenPrices(next); setStatus('Token prices saved.'); }} onError={setError} />
    <SupplierModelConfigPanel v2={v2} onSaved={(next) => { setV2(next); setStatus('Supplier model config saved.'); }} onError={setError} />
    <ModelEquivalencesPanel config={equivalences} onSaved={(next) => { setEquivalences(next); setStatus('Equivalences saved.'); }} onError={setError} />
  </section>;
}
