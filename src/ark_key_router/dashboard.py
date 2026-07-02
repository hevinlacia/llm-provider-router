from __future__ import annotations


DASHBOARD_HTML = """
<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Ark Key Router</title>
  <style>
    :root { color-scheme: dark; font-family: Inter, ui-sans-serif, system-ui, sans-serif; }
    body { margin: 0; background: #0b1020; color: #e5e7eb; }
    main { max-width: 1180px; margin: 0 auto; padding: 28px; }
    header { display: flex; justify-content: space-between; gap: 16px; align-items: center; }
    h1 { margin: 0; font-size: 28px; }
    h2 { margin: 0 0 10px; font-size: 18px; }
    button { border: 0; border-radius: 10px; padding: 10px 14px; background: #38bdf8; color: #04111f; font-weight: 700; cursor: pointer; }
    button.secondary { background: #334155; color: #e5e7eb; }
    .toolbar { display: flex; flex-wrap: wrap; gap: 10px; margin-top: 20px; align-items: end; }
    .field { display: grid; gap: 6px; }
    .field label { color: #94a3b8; font-size: 12px; text-transform: uppercase; letter-spacing: .08em; }
    select, input { background: #111827; border: 1px solid #334155; border-radius: 10px; color: #e5e7eb; padding: 9px 10px; }
    .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(210px, 1fr)); gap: 14px; margin: 22px 0; }
    .card { background: linear-gradient(180deg, #111827, #0f172a); border: 1px solid #1f2937; border-radius: 16px; padding: 16px; box-shadow: 0 18px 60px rgb(0 0 0 / 22%); }
    .label { color: #94a3b8; font-size: 12px; text-transform: uppercase; letter-spacing: .08em; }
    .value { font-size: 30px; font-weight: 800; margin-top: 8px; }
    table { width: 100%; border-collapse: collapse; margin-top: 10px; }
    th, td { padding: 9px 10px; text-align: right; border-bottom: 1px solid #1f2937; }
    th:first-child, td:first-child { text-align: left; }
    th { color: #93c5fd; font-size: 12px; text-transform: uppercase; letter-spacing: .06em; }
    .muted { color: #94a3b8; }
    .ok { color: #86efac; }
    @media (max-width: 640px) { main { padding: 18px; } header { align-items: flex-start; flex-direction: column; } }
  </style>
</head>
<body>
<main>
  <header>
    <div>
      <h1>Ark Key Router</h1>
      <div class="muted" id="subtitle">Loading usage metrics...</div>
    </div>
    <div>
      <button class="secondary" onclick="loadData()">Refresh</button>
      <button onclick="resetUsage()">Reset Usage</button>
    </div>
  </header>

  <section class="toolbar">
    <div class="field">
      <label for="period">Range</label>
      <select id="period" onchange="loadData()">
        <option value="all">All</option>
        <option value="today">Today</option>
        <option value="day">Last 24h</option>
        <option value="month">This Month</option>
      </select>
    </div>
    <div class="field">
      <label for="start">Start</label>
      <input id="start" type="date" onchange="loadData()">
    </div>
    <div class="field">
      <label for="end">End</label>
      <input id="end" type="date" onchange="loadData()">
    </div>
    <button class="secondary" onclick="clearRange()">Clear Range</button>
  </section>

  <section class="grid" id="summary"></section>
  <section class="card"><h2>Daily Requests</h2><div id="by-day"></div></section>
  <section class="card"><h2>Monthly Requests</h2><div id="by-month"></div></section>
  <section class="card"><h2>Usage by Model</h2><div id="by-model"></div></section>
  <section class="card"><h2>Usage by Key</h2><div id="by-key"></div></section>
  <section class="card"><h2>Usage by Status</h2><div id="by-status"></div></section>
  <section class="card"><h2>Frozen Keys</h2><div id="frozen"></div></section>
</main>
<script>
const number = new Intl.NumberFormat();

function card(label, value) {
  const display = typeof value === 'string' ? value : number.format(value || 0);
  return `<div class="card"><div class="label">${label}</div><div class="value">${display}</div></div>`;
}

function table(data) {
  const entries = Object.entries(data || {});
  if (!entries.length) return '<p class="muted">No data yet.</p>';
  const rows = entries.map(([name, item]) => `<tr><td>${name}</td><td>${number.format(item.requests)}</td><td>${number.format(item.errors)}</td><td>${number.format(item.prompt_tokens)}</td><td>${number.format(item.cached_tokens || 0)}</td><td>${formatPercent(item.cache_hit_rate)}</td><td>${number.format(item.completion_tokens)}</td><td>${number.format(item.total_tokens)}</td></tr>`).join('');
  return `<table><thead><tr><th>Name</th><th>Requests</th><th>Errors</th><th>Prompt</th><th>Cached</th><th>Cache Hit</th><th>Completion</th><th>Total</th></tr></thead><tbody>${rows}</tbody></table>`;
}

function formatPercent(value) {
  return `${((value || 0) * 100).toFixed(1)}%`;
}

function usageUrl() {
  const params = new URLSearchParams();
  const start = document.getElementById('start').value;
  const end = document.getElementById('end').value;
  if (start || end) {
    if (start) params.set('start', start);
    if (end) params.set('end', end);
  } else {
    params.set('period', document.getElementById('period').value);
  }
  const query = params.toString();
  return `/api/state${query ? `?${query}` : ''}`;
}

function frozenTable(data) {
  const entries = Object.entries(data || {});
  if (!entries.length) return '<p class="ok">No frozen keys.</p>';
  const rows = entries.map(([name, item]) => `<tr><td>${name}</td><td>${item.reason}</td><td>${number.format(item.seconds_remaining)}s</td></tr>`).join('');
  return `<table><thead><tr><th>Key</th><th>Reason</th><th>Remaining</th></tr></thead><tbody>${rows}</tbody></table>`;
}

async function loadData() {
  const response = await fetch(usageUrl());
  const data = await response.json();
  const usage = data.usage.total;
  document.getElementById('subtitle').textContent = `Uptime ${number.format(data.usage.uptime_seconds)}s · ${data.bindings} active bindings · ${data.usage.range.period} range`;
  document.getElementById('summary').innerHTML = [
    card('Requests', usage.requests),
    card('Errors', usage.errors),
    card('Prompt Tokens', usage.prompt_tokens),
    card('Cached Tokens', usage.cached_tokens),
    card('Cache Hit Rate', formatPercent(usage.cache_hit_rate)),
    card('Completion Tokens', usage.completion_tokens),
    card('Total Tokens', usage.total_tokens),
  ].join('');
  document.getElementById('by-day').innerHTML = table(data.usage.by_day);
  document.getElementById('by-month').innerHTML = table(data.usage.by_month);
  document.getElementById('by-model').innerHTML = table(data.usage.by_model);
  document.getElementById('by-key').innerHTML = table(data.usage.by_key);
  document.getElementById('by-status').innerHTML = table(data.usage.by_status);
  document.getElementById('frozen').innerHTML = frozenTable(data.frozen);
}

async function resetUsage() {
  await fetch('/api/usage/reset', { method: 'POST' });
  await loadData();
}

function clearRange() {
  document.getElementById('start').value = '';
  document.getElementById('end').value = '';
  loadData();
}

loadData();
setInterval(loadData, 5000);
</script>
</body>
</html>
""".strip()
