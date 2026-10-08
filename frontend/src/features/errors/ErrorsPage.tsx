import { useCallback, useEffect, useState } from 'react';
import { api } from '../../api';
import type { RecentErrorEntry } from '../../types';

const CLASS_LABEL: Record<string, string> = {
  invalid: '失效',
  rate_limited: '限流',
  model_unsupported: '模型不支持',
  transient: '其他',
};

function formatTime(ts: number): string {
  const date = new Date(ts * 1000);
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`;
}

export function ErrorsPage() {
  const [errors, setErrors] = useState<RecentErrorEntry[]>([]);
  const [error, setError] = useState('');
  const [classFilter, setClassFilter] = useState('');
  const [autoRefresh, setAutoRefresh] = useState(true);

  const load = useCallback(async () => {
    try {
      const data = await api.recentErrors();
      setErrors(data.errors ?? []);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  useEffect(() => {
    void load();
    if (!autoRefresh) return;
    const timer = setInterval(() => void load(), 5000);
    return () => clearInterval(timer);
  }, [load, autoRefresh]);

  const visible = errors.filter((item) => !classFilter || item.class === classFilter);

  return <section className="page active">
    <header>
      <div>
        <h1>最近报错</h1>
        <div className="muted">上游失败实时视图（内存 ring buffer，最近 200 条，重启清空）。用于诊断并调整设置页的报错分类模板。</div>
        {error && <div className="error">{error}</div>}
      </div>
      <div className="toolbar">
        <label style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
          <input type="checkbox" checked={autoRefresh} onChange={(e) => setAutoRefresh(e.target.checked)} /> 5s 自动刷新
        </label>
        <select value={classFilter} onChange={(e) => setClassFilter(e.target.value)}>
          <option value="">全部分类</option>
          <option value="invalid">失效</option>
          <option value="rate_limited">限流</option>
          <option value="model_unsupported">模型不支持</option>
          <option value="transient">其他</option>
        </select>
        <button className="secondary" onClick={() => void load()}>刷新</button>
      </div>
    </header>
    <section className="card">
      {visible.length === 0 ? <p className="muted">暂无报错记录。</p> : <div className="table-wrap"><table>
        <thead><tr>
          <th>时间</th><th>供应商</th><th>Key</th><th>模型别名</th><th>上游模型</th>
          <th>状态码</th><th>分类</th><th>命中规则</th><th>报错内容</th>
        </tr></thead>
        <tbody>{visible.map((item, index) => <tr key={`${item.ts}-${index}`}>
          <td style={{ whiteSpace: 'nowrap' }}>{formatTime(item.ts)}</td>
          <td>{item.provider}</td>
          <td>{item.key}</td>
          <td>{item.alias}</td>
          <td>{item.model}</td>
          <td className={item.status >= 500 ? '' : 'muted'}>{item.status === 599 ? '连接失败' : item.status}</td>
          <td><span className={`status ${item.class === 'invalid' || item.class === 'rate_limited' ? 'warn' : ''}`}>{CLASS_LABEL[item.class] ?? item.class}</span></td>
          <td className="muted">{item.rule}</td>
          <td style={{ maxWidth: 480, overflowWrap: 'anywhere' }}>{item.message}</td>
        </tr>)}</tbody>
      </table></div>}
    </section>
  </section>;
}
