import { useEffect, useState } from 'react';
import { api } from '../../api';
import type { DnsOverridesConfig } from '../../types';

type StaticDraft = { domain: string; ips: string };

/** types.ts 的 DnsOverridesConfig 去掉响应包装字段 */
type SavePayload = Omit<DnsOverridesConfig, 'ok' | 'applied' | 'message'>;

function configToDraft(config: DnsOverridesConfig) {
  return {
    enabled: config.enabled,
    nameservers: (config.nameservers ?? []).join('\n'),
    query_timeout_ms: config.query_timeout_ms || 1500,
    cache_ttl_seconds: config.cache_ttl_seconds || 60,
    statics: Object.entries(config.static ?? {}).map(([domain, ips]) => ({
      domain,
      ips: (ips ?? []).join(', '),
    })),
  };
}

/** 上游解析三级兜底（L1 自定义 DNS → L2 静态 IP → L3 系统解析）设置面板。
 * 保存即写盘 + 热生效（运行中槽位无需重启）。 */
export function DnsOverridesPanel({ config, onSaved, onError }: {
  config: DnsOverridesConfig | null;
  onSaved: (value: DnsOverridesConfig) => void;
  onError: (value: string) => void;
}) {
  const [draft, setDraft] = useState<ReturnType<typeof configToDraft> | null>(null);
  const [status, setStatus] = useState('');

  useEffect(() => {
    if (config && draft === null) setDraft(configToDraft(config));
  }, [config, draft]);

  if (!config || !draft) {
    return <section className="card"><h2>DNS Fallback</h2><p className="muted">Loading DNS fallback settings...</p></section>;
  }

  function updateStatics(index: number, patch: Partial<StaticDraft>) {
    if (!draft) return;
    const statics = [...draft.statics];
    statics[index] = { ...statics[index], ...patch };
    setDraft({ ...draft, statics });
  }

  async function save() {
    if (!draft) return;
    const nameservers = draft.nameservers
      .split('\n')
      .map((line) => line.trim())
      .filter(Boolean);
    const staticMap: Record<string, string[]> = {};
    for (const entry of draft.statics) {
      const domain = entry.domain.trim();
      const ips = entry.ips.split(',').map((ip) => ip.trim()).filter(Boolean);
      if (!domain || ips.length === 0) continue;
      staticMap[domain] = ips;
    }
    const payload: SavePayload = {
      enabled: draft.enabled,
      nameservers,
      query_timeout_ms: Math.max(100, Number(draft.query_timeout_ms) || 1500),
      cache_ttl_seconds: Math.max(5, Number(draft.cache_ttl_seconds) || 60),
      static: staticMap,
    };
    try {
      const saved = await api.saveDnsOverrides(payload);
      setDraft(configToDraft(saved));
      setStatus(
        saved.applied
          ? 'DNS fallback saved & applied.'
          : `Saved to disk, not applied: ${saved.message ?? 'restart backend slot'}`,
      );
      onSaved(saved);
    } catch (err) {
      onError(err instanceof Error ? err.message : String(err));
    }
  }

  return <section className="card settings-section dns-fallback-section">
    <div className="section-title settings-title">
      <div><h2>DNS Fallback</h2><p className="muted">上游域名解析三级兜底：L1 自定义 DNS 服务器（UDP 直查，绕开路由器劫持链）→ L2 静态 IP 映射 → L3 系统解析。OpenClash 启动/重启窗口期保证供应商域名可解析。保存即写盘并热生效。</p></div>
      <div className="title-actions"><span className="muted">{draft.enabled ? 'enabled' : 'disabled'} · {draft.statics.length} static</span></div>
    </div>
    {status && <div className="ok">{status}</div>}
    <div className="provider-group">
      <div className="field">
        <label><input type="checkbox" checked={draft.enabled} onChange={(event) => setDraft({ ...draft, enabled: event.target.checked })} /> Enabled（关闭 = 回到系统解析）</label>
      </div>
      <div className="field">
        <label>L1 Nameservers（每行一个，如 223.5.5.5）</label>
        <textarea rows={3} value={draft.nameservers} onChange={(event) => setDraft({ ...draft, nameservers: event.target.value })} placeholder={'223.5.5.5\n119.29.29.29'} />
      </div>
      <div className="field">
        <label>Query Timeout (ms)</label>
        <input className="weight-input" type="number" min="100" step="100" value={draft.query_timeout_ms} onChange={(event) => setDraft({ ...draft, query_timeout_ms: Number(event.target.value) || 1500 })} />
      </div>
      <div className="field">
        <label>Cache TTL (s)</label>
        <input className="weight-input" type="number" min="5" step="5" value={draft.cache_ttl_seconds} onChange={(event) => setDraft({ ...draft, cache_ttl_seconds: Number(event.target.value) || 60 })} />
      </div>
      <div className="table-wrap"><table className="search-keys-table">
        <thead><tr><th>Domain</th><th>IPs（逗号分隔）</th><th></th></tr></thead>
        <tbody>{draft.statics.length === 0
          ? <tr><td colSpan={3} className="muted">无静态映射（L2 兜底为空，仅 L1/L3 生效）</td></tr>
          : draft.statics.map((entry, index) => <tr key={index}>
            <td><input value={entry.domain} onChange={(event) => updateStatics(index, { domain: event.target.value })} placeholder="ark.cn-beijing.volces.com" /></td>
            <td><input value={entry.ips} onChange={(event) => updateStatics(index, { ips: event.target.value })} placeholder="14.103.169.114" /></td>
            <td><button className="secondary compact-button" onClick={() => setDraft({ ...draft, statics: draft.statics.filter((_, i) => i !== index) })}>Delete</button></td>
          </tr>)}</tbody>
      </table></div>
      <button className="secondary compact-button" onClick={() => setDraft({ ...draft, statics: [...draft.statics, { domain: '', ips: '' }] })}>Add Static Mapping</button>
    </div>
    <div className="title-actions"><button onClick={save}>Save DNS Fallback</button></div>
  </section>;
}
