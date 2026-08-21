import { formatWindow } from '../../lib/format';
import type { RouterCapabilities } from '../../types';

export function CapabilitiesPanel({ caps, onRefresh, error }: { caps: RouterCapabilities | null; onRefresh: () => void; error?: string }) {
  if (!caps) return <section className="card"><h2>Context Negotiation</h2><p className="muted">Loading dynamic context capabilities...</p></section>;
  if (!caps.v2_enabled) return <section className="card"><h2>Context Negotiation</h2><p className="muted">Layered routing disabled; capabilities unavailable.</p></section>;
  const rows = [...(caps.models ?? [])].sort((a, b) => a.id.localeCompare(b.id));
  return <section className="card settings-section">
    <div className="section-title settings-title"><div><h2>Context Negotiation</h2><p className="muted">Effective context window = conservative min across available physical targets (available=true). Pi拉取此视图热补 compaction阈值。</p></div><div className="title-actions"><span className="muted">{rows.length} logical models · {caps.v2_enabled ? 'v2' : 'legacy'}</span><button className="secondary compact-button" onClick={onRefresh}>Refresh</button></div></div>
    {error && <div className="error">{error}</div>}
    <div className="table-wrap"><table className="settings-table"><thead><tr><th>Model</th><th>Strategy</th><th>Effective Window</th><th>Targets (window / available)</th></tr></thead><tbody>{rows.map((m) => <tr key={m.id}><td className="strong-cell">{m.id}</td><td><span className="status">{m.strategy}</span></td><td className="muted small-text">{formatWindow(m.effective.contextWindow)} / {formatWindow(m.effective.maxTokens)} {m.effective.contextWindow == null ? <span className="status warn">unset</span> : null}</td><td className="muted small-text target-cell">{m.targets.map((t) => <span className="target-pill" key={`${m.id}-${t.id}`} title={`${t.provider}/${t.upstream_model}`}>{t.id} {formatWindow(t.context_window)} {t.available ? '' : '(unavail)'} {t.weight != null ? `w=${t.weight}` : ''}</span>)}</td></tr>)}</tbody></table></div>
    <p className="muted small-text">Header校正：非流式chat completions响应头 <code>x-llm-router-context-window / x-llm-router-max-output</code> 为本次命中物理模型的精确值；流式为首选候选的保守提示。Pi扩展 <code>router-context-sync</code> 已通过 capabilities + 响应头双通道热补。</p>
  </section>;
}
