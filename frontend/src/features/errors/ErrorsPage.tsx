import { useCallback, useEffect, useState } from 'react';
import { api } from '../../api';
import { KpiCard } from '../../components/tables';
import type {
  ConnectFailureKeyRow,
  ConnectFailureSummary,
  RecentErrorEntry,
} from '../../types';

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

/** 把「距最近一次失败多少秒」渲染成人读相对时间。 */
function formatAgo(seconds: number): string {
  if (seconds < 60) return `${seconds} 秒前`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)} 分钟前`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)} 小时前`;
  return `${Math.floor(seconds / 86400)} 天前`;
}

export function ErrorsPage() {
  const [errors, setErrors] = useState<RecentErrorEntry[]>([]);
  const [connSummary, setConnSummary] = useState<ConnectFailureSummary | null>(null);
  const [connKeys, setConnKeys] = useState<ConnectFailureKeyRow[]>([]);
  const [error, setError] = useState('');
  const [classFilter, setClassFilter] = useState('');
  const [autoRefresh, setAutoRefresh] = useState(true);

  const load = useCallback(async () => {
    try {
      // 独立 catch：前端可能先于后端部署（旧后端无 connect-stats 端点）。
      const [data, stats] = await Promise.all([
        api.recentErrors(),
        api.connectFailureStats().catch(() => null),
      ]);
      setErrors(data.errors ?? []);
      setConnSummary(stats?.summary ?? null);
      setConnKeys(stats?.keys ?? []);
      setError('');
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }, []);

  // 页面在后台时暂停自动刷新（浏览器仍会触发定时器，白白消耗带宽）。
  const [pageVisible, setPageVisible] = useState(() => document.visibilityState === 'visible');

  useEffect(() => {
    const onVisibility = () => {
      const visible = document.visibilityState === 'visible';
      setPageVisible(visible);
      if (visible) void load();
    };
    document.addEventListener('visibilitychange', onVisibility);
    return () => document.removeEventListener('visibilitychange', onVisibility);
  }, [load]);

  useEffect(() => {
    void load();
    if (!autoRefresh || !pageVisible) return;
    const timer = setInterval(() => void load(), 5000);
    return () => clearInterval(timer);
  }, [load, autoRefresh, pageVisible]);

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
    {connSummary && <section className="card">
      <h2 style={{ marginTop: 0 }}>上游连接失败统计</h2>
      <div className="muted" style={{ marginBottom: 10 }}>
        只统计「重试后仍失败」的上游建连错误（status 599），与 key 失效无关。
        持续增长通常意味着路由器 OpenClash 透明代理链路不稳（节点抖动 / 连接被 RST），
        而非 key 过期；内存态，重启归零。
      </div>
      <div className="kpi-grid">
        <KpiCard label="累计连接失败" value={String(connSummary.total)} accent="kpi-primary" />
        <KpiCard label="近 1 小时" value={String(connSummary.recent_1h)} />
        <KpiCard label="近 24 小时" value={String(connSummary.recent_24h)} />
        <KpiCard label="涉及 key" value={String(connSummary.affected_keys)} />
      </div>
      {connKeys.length === 0 ? <p className="muted">暂无连接失败记录（链路正常）。</p> : <div className="table-wrap"><table>
        <thead><tr>
          <th>Key</th><th>累计</th><th>近 1h</th><th>近 24h</th><th>最近失败</th><th>最近错误</th>
        </tr></thead>
        <tbody>{connKeys.map((row) => <tr key={row.key}>
          <td>{row.key}</td>
          <td>{row.total}</td>
          <td>{row.recent_1h > 0 ? <span className="status warn">{row.recent_1h}</span> : <span className="muted">0</span>}</td>
          <td>{row.recent_24h > 0 ? <span className="status warn">{row.recent_24h}</span> : <span className="muted">0</span>}</td>
          <td style={{ whiteSpace: 'nowrap' }} title={formatTime(row.last_at)}>
            {formatAgo(row.seconds_since_last)}
          </td>
          <td style={{ maxWidth: 420, overflowWrap: 'anywhere' }} className="muted">{row.last_message}</td>
        </tr>)}</tbody>
      </table></div>}
    </section>}
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
