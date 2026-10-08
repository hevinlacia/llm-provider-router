import { useState } from 'react';
import { api } from '../../api';
import type { ClassRule, ClassTemplate, ErrorRulesConfig, Tunables, V2Status } from '../../types';

const CLASSES: Array<{ key: keyof ClassTemplate; label: string; hint: string }> = [
  { key: 'invalid', label: '失效 (invalid)', hint: '冻结整把 key（时长见下方参数）→ 换下一把' },
  { key: 'rate_limited', label: '限流 (rate_limited)', hint: '冻结到恢复时刻（retry-after / reset-at / 兜底）→ 换下一把' },
  { key: 'model_unsupported', label: '模型不支持 (model_unsupported)', hint: '直接报错，不切 key（配置错误）' },
  { key: 'transient', label: '其他 (transient)', hint: '同 key 重试 N 次 → 换下一把；同行成功时差分冻结；全池耗尽短熔断' },
];

function ruleSummary(rule: ClassRule): string {
  const parts: string[] = [];
  if (rule.status.length) parts.push(`status ${rule.status.join(',')}`);
  if (rule.keywords_all.length) parts.push(`all: ${rule.keywords_all.join(' + ')}`);
  if (rule.keywords_any.length) parts.push(`any: ${rule.keywords_any.join(' / ')}`);
  return parts.join(' · ') || '任意失败';
}

function RuleEditor({ rules, onChange }: { rules: ClassRule[]; onChange: (next: ClassRule[]) => void }) {
  function update(index: number, patch: Partial<ClassRule>) {
    const next = [...rules];
    next[index] = { ...next[index], ...patch };
    onChange(next);
  }
  return <div className="rule-list">
    {rules.map((rule, index) => <div className="rule-row" key={index}>
      <input className="number-input" style={{ width: 150 }} value={rule.status.join(',')}
        placeholder="状态码, 如 400,401"
        onChange={(e) => update(index, { status: e.target.value.split(',').map((v) => Number(v.trim())).filter((v) => Number.isFinite(v) && v > 0) })} />
      <input style={{ flex: 1 }} value={rule.keywords_all.join(', ')}
        placeholder="AND 关键词（逗号分隔，全部命中才匹配）"
        onChange={(e) => update(index, { keywords_all: e.target.value.split(',').map((v) => v.trim().toLowerCase()).filter(Boolean) })} />
      <input style={{ flex: 1 }} value={rule.keywords_any.join(', ')}
        placeholder="OR 关键词（任一命中即匹配）"
        onChange={(e) => update(index, { keywords_any: e.target.value.split(',').map((v) => v.trim().toLowerCase()).filter(Boolean) })} />
      <span className="muted" style={{ minWidth: 180 }}>{ruleSummary(rule)}</span>
      <button className="secondary" onClick={() => onChange(rules.filter((_, i) => i !== index))}>删除</button>
    </div>)}
    <button className="secondary" onClick={() => onChange([...rules, { status: [], keywords_any: [], keywords_all: [] }])}>添加规则</button>
  </div>;
}

function TemplateEditor({ name, template, onChange }: { name: string; template: ClassTemplate; onChange: (next: ClassTemplate) => void }) {
  return <div className="template-editor">
    <h3>{name}</h3>
    {CLASSES.map(({ key, label, hint }) => <div key={key}>
      <div className="class-head"><strong>{label}</strong><span className="muted">{hint}</span></div>
      <RuleEditor rules={template[key]} onChange={(next) => onChange({ ...template, [key]: next })} />
    </div>)}
  </div>;
}

export function ErrorRulesPanel({ config, onSaved, onError, v2 }: {
  config: ErrorRulesConfig | null;
  v2: V2Status | null;
  onSaved: (value: ErrorRulesConfig) => void;
  onError: (value: string) => void;
}) {
  const [draft, setDraft] = useState<ErrorRulesConfig | null>(config);
  const [selectedTemplate, setSelectedTemplate] = useState('default');
  const [saving, setSaving] = useState(false);
  if (!config) return <section className="card"><h2>报错分类模板</h2><p className="muted">Loading error rules...</p></section>;
  const current = draft ?? config;

  function setTunables(patch: Partial<Tunables>) {
    setDraft({ ...current, tunables: { ...current.tunables, ...patch } });
  }

  async function save() {
    setSaving(true);
    try { onSaved(await api.saveErrorRules(current)); } catch (err) { onError(err instanceof Error ? err.message : String(err)); } finally { setSaving(false); }
  }

  const providerNames = ['default', ...Object.keys(v2?.providers ?? {})];
  const templateNames = Object.keys(current.templates);

  return <section className="card">
    <div className="section-title"><h2>报错分类模板</h2><span className="muted">匹配顺序：失效 &gt; 限流 &gt; 模型不支持 &gt; 其他（兜底）。关键词小写包含匹配。</span></div>
    <div className="tunables-grid">
      <label>失效冻结时长 (s)<input className="number-input" type="number" min="0" value={current.tunables.invalid_freeze_seconds ?? ''} onChange={(e) => setTunables({ invalid_freeze_seconds: Number(e.target.value) || 0 })} /></label>
      <label>差分冻结时长 (s)<input className="number-input" type="number" min="0" value={current.tunables.differential_freeze_seconds ?? ''} onChange={(e) => setTunables({ differential_freeze_seconds: Number(e.target.value) || 0 })} /></label>
      <label>全池熔断时长 (s)<input className="number-input" type="number" min="0" value={current.tunables.transient_exhausted_freeze_seconds ?? ''} onChange={(e) => setTunables({ transient_exhausted_freeze_seconds: Number(e.target.value) || 0 })} /></label>
      <label>每 key 尝试次数<input className="number-input" type="number" min="1" value={current.tunables.transient_attempts_per_key ?? 1} onChange={(e) => setTunables({ transient_attempts_per_key: Number(e.target.value) || 1 })} /></label>
      <label>重试间隔 (ms)<input className="number-input" type="number" min="0" value={current.tunables.transient_retry_interval_ms ?? 1000} onChange={(e) => setTunables({ transient_retry_interval_ms: Number(e.target.value) || 0 })} /></label>
    </div>
    <h3>供应商 → 模板绑定</h3>
    <div className="binding-grid">
      {providerNames.filter((p) => p !== 'default').map((provider) => <label key={provider}>
        <span>{provider}</span>
        <select value={current.provider_bindings[provider] ?? (current.templates[provider] ? provider : 'default')}
          onChange={(e) => setDraft({ ...current, provider_bindings: { ...current.provider_bindings, [provider]: e.target.value } })}>
          {templateNames.map((name) => <option key={name} value={name}>{name}</option>)}
        </select>
      </label>)}
    </div>
    <h3>模板</h3>
    <div className="toolbar">
      <select value={selectedTemplate} onChange={(e) => setSelectedTemplate(e.target.value)}>
        {templateNames.map((name) => <option key={name} value={name}>{name}</option>)}
      </select>
      <button className="secondary" onClick={() => {
        const name = `template-${templateNames.length + 1}`;
        const fresh: ClassTemplate = {
          invalid: [],
          rate_limited: [{ status: [429], keywords_any: [], keywords_all: [] }],
          model_unsupported: [],
          transient: [],
        };
        setDraft({ ...current, templates: { ...current.templates, [name]: fresh } });
        setSelectedTemplate(name);
      }}>新建模板</button>
    </div>
    {current.templates[selectedTemplate]
      ? <TemplateEditor name={selectedTemplate} template={current.templates[selectedTemplate]}
        onChange={(next) => setDraft({ ...current, templates: { ...current.templates, [selectedTemplate]: next } })} />
      : <p className="muted">模板不存在</p>}
    <div className="toolbar"><button disabled={saving} onClick={() => void save()}>保存报错分类规则</button></div>
  </section>;
}
