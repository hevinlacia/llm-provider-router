import { useEffect, useState } from 'react';
import { api } from '../../api';
import type { SearchProvidersConfig } from '../../types';

/** 后端只认识这三个搜索供应商；名字决定 /v1/search 的请求翻译与响应归一化方式。 */
const KNOWN_PROVIDERS: Array<{ name: string; default_base_url: string }> = [
  { name: 'tavily', default_base_url: 'https://api.tavily.com' },
  { name: 'exa', default_base_url: 'https://api.exa.ai' },
  { name: 'brave', default_base_url: 'https://api.search.brave.com' },
];

type KeyDraft = { name: string; env_var: string; weight: number; enabled: boolean };
type ProviderDraft = { base_url: string; keys: KeyDraft[] };

function configToDrafts(config: SearchProvidersConfig): Record<string, ProviderDraft> {
  const drafts: Record<string, ProviderDraft> = {};
  for (const { name, default_base_url } of KNOWN_PROVIDERS) {
    const provider = config.providers?.[name];
    drafts[name] = {
      base_url: provider?.base_url || default_base_url,
      keys: Object.entries(provider?.keys ?? {}).map(([keyName, key]) => ({
        name: keyName,
        env_var: key.env_var,
        weight: key.weight,
        enabled: key.enabled,
      })),
    };
  }
  return drafts;
}

/** GET 返回的每个 key 是否已在运行环境解析到值（configured） */
function configuredMap(config: SearchProvidersConfig): Record<string, Record<string, boolean>> {
  const map: Record<string, Record<string, boolean>> = {};
  for (const [name, provider] of Object.entries(config.providers ?? {})) {
    map[name] = {};
    for (const [keyName, key] of Object.entries(provider.keys ?? {})) {
      map[name][keyName] = Boolean(key.configured);
    }
  }
  return map;
}

export function SearchProvidersPanel({ config, onSaved, onError }: {
  config: SearchProvidersConfig | null;
  onSaved: (value: SearchProvidersConfig) => void;
  onError: (value: string) => void;
}) {
  const [drafts, setDrafts] = useState<Record<string, ProviderDraft> | null>(null);
  const [configured, setConfigured] = useState<Record<string, Record<string, boolean>>>({});
  const [status, setStatus] = useState('');

  useEffect(() => {
    if (config && drafts === null) {
      setDrafts(configToDrafts(config));
      setConfigured(configuredMap(config));
    }
  }, [config, drafts]);

  if (!config || !drafts) {
    return <section className="card"><h2>Search Providers</h2><p className="muted">Loading search provider key pools...</p></section>;
  }

  function updateBaseUrl(provider: string, baseUrl: string) {
    if (!drafts) return;
    setDrafts({ ...drafts, [provider]: { ...drafts[provider], base_url: baseUrl } });
  }

  function updateKey(provider: string, index: number, patch: Partial<KeyDraft>) {
    if (!drafts) return;
    const keys = [...drafts[provider].keys];
    keys[index] = { ...keys[index], ...patch };
    setDrafts({ ...drafts, [provider]: { ...drafts[provider], keys } });
  }

  function addKey(provider: string) {
    if (!drafts) return;
    setDrafts({
      ...drafts,
      [provider]: { ...drafts[provider], keys: [...drafts[provider].keys, { name: '', env_var: '', weight: 1, enabled: true }] },
    });
  }

  function removeKey(provider: string, index: number) {
    if (!drafts) return;
    setDrafts({
      ...drafts,
      [provider]: { ...drafts[provider], keys: drafts[provider].keys.filter((_, i) => i !== index) },
    });
  }

  async function save() {
    if (!drafts) return;
    const providers: Record<string, { base_url?: string | null; keys: Record<string, { env_var: string; weight: number; enabled: boolean }> }> = {};
    for (const { name } of KNOWN_PROVIDERS) {
      const draft = drafts[name];
      const keys: Record<string, { env_var: string; weight: number; enabled: boolean }> = {};
      for (const key of draft.keys) {
        if (!key.name.trim()) continue;
        keys[key.name.trim()] = {
          env_var: key.env_var.trim(),
          weight: Math.max(0, Number(key.weight) || 0),
          enabled: key.enabled,
        };
      }
      providers[name] = { base_url: draft.base_url.trim() || null, keys };
    }
    try {
      const saved = await api.saveSearchProviders(providers);
      setDrafts(configToDrafts(saved));
      setConfigured(configuredMap(saved));
      setStatus('Search providers saved.');
      onSaved(saved);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }

  const keyCount = KNOWN_PROVIDERS.reduce((sum, { name }) => sum + (drafts[name]?.keys.length ?? 0), 0);

  return <section className="card settings-section search-providers-section">
    <div className="section-title settings-title">
      <div><h2>Search Providers</h2><p className="muted">搜索服务提供商 key 池（Tavily / Exa / Brave）。/v1/search 统一入口按各供应商可用 key 的权重加权路由；key 值从运行环境读取，配置里只存 env_var 名。</p></div>
      <div className="title-actions"><span className="muted">{KNOWN_PROVIDERS.length} providers · {keyCount} keys</span></div>
    </div>
    {status && <div className="ok">{status}</div>}
    {KNOWN_PROVIDERS.map(({ name, default_base_url }) => {
      const draft = drafts[name];
      return <div className="provider-group" key={name}>
        <div className="provider-head"><h3>{name}</h3><div className="field base-url-field"><label>Base URL</label><input value={draft.base_url} onChange={(event) => updateBaseUrl(name, event.target.value)} placeholder={default_base_url} /></div></div>
        <div className="table-wrap"><table className="search-keys-table">
          <thead><tr><th>Key Name</th><th>Env Var</th><th>Weight</th><th>Enabled</th><th>Status</th><th></th></tr></thead>
          <tbody>{draft.keys.map((key, index) => <tr key={index} className={key.enabled ? '' : 'disabled-row'}>
            <td><input value={key.name} onChange={(event) => updateKey(name, index, { name: event.target.value })} placeholder="e.g. hevin" /></td>
            <td><input className="env-input" value={key.env_var} onChange={(event) => updateKey(name, index, { env_var: event.target.value })} placeholder={`AGENT_SEARCH_${name.toUpperCase()}_..._API_KEY`} /></td>
            <td><input className="weight-input" type="number" min="0" step="1" value={key.weight} onChange={(event) => updateKey(name, index, { weight: Number(event.target.value) || 0 })} /></td>
            <td><input type="checkbox" checked={key.enabled} onChange={(event) => updateKey(name, index, { enabled: event.target.checked })} /></td>
            <td><span className={`status ${configured[name]?.[key.name] ? 'ok' : 'warn'}`}>{configured[name]?.[key.name] ? 'configured' : 'missing'}</span></td>
            <td><button className="secondary compact-button" onClick={() => removeKey(name, index)}>Delete</button></td>
          </tr>)}</tbody>
        </table></div>
        {draft.keys.length === 0 && <p className="muted small-text">No keys configured for {name} yet.</p>}
        <button className="secondary" onClick={() => addKey(name)}>Add Key</button>
      </div>;
    })}
    <div className="toolbar search-providers-toolbar"><span className="muted small-text">key 明文不落盘：在运行环境的 env 文件（如 ~/.config/opencode/agent-secrets.env）中按 env_var 设置即可，保存后 Status 会实时显示 configured / missing。</span><button onClick={() => void save()}>Save Search Providers</button></div>
  </section>;
}
