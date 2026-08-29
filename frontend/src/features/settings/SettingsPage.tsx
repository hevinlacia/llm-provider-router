import { useCallback, useEffect, useState } from 'react';
import { api } from '../../api';
import type { SearchProvidersConfig, TokenPriceConfig, V2Status } from '../../types';
import { SearchProvidersPanel } from './searchProviders';
import { SupplierModelConfigPanel } from './supplierModels';
import { V2Panel } from './v2';

export function SettingsPage() {
  const [tokenPrices, setTokenPrices] = useState<TokenPriceConfig | null>(null);
  const [v2, setV2] = useState<V2Status | null>(null);
  const [searchProviders, setSearchProviders] = useState<SearchProvidersConfig | null>(null);
  const [status, setStatus] = useState('');
  const [error, setError] = useState('');

  const loadSettings = useCallback(async () => {
    try {
      const [tokenPriceData, v2Data, searchData] = await Promise.all([api.tokenPrices(), api.v2Status(), api.searchProviders()]);
      setTokenPrices(tokenPriceData);
      setV2(v2Data);
      setSearchProviders(searchData);
      setStatus('Settings loaded.');
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  useEffect(() => { void loadSettings(); }, [loadSettings]);

  async function reloadTokenPrices() {
    try {
      setTokenPrices(await api.tokenPrices());
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  return <section className="page active"><header><div><h1>Settings</h1><div className="muted">Manage providers, keys, token prices, and logical model routing.</div>{status && <div className="ok">{status}</div>}{error && <div className="error">{error}</div>}</div><button className="secondary" onClick={() => void loadSettings()}>Refresh</button></header>
    <V2Panel config={v2} onSaved={setV2} onError={setError} />
    <SearchProvidersPanel config={searchProviders} onSaved={setSearchProviders} onError={setError} />
    <SupplierModelConfigPanel v2={v2} tokenPrices={tokenPrices} onSaved={(next) => { setV2(next); setStatus('Supplier model config saved.'); void reloadTokenPrices(); }} onError={setError} />
  </section>;
}
