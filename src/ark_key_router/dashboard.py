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
    .shell { display: grid; grid-template-columns: 230px 1fr; min-height: 100vh; }
    aside { background: #070b16; border-right: 1px solid #1f2937; padding: 24px 16px; }
    .brand { font-size: 18px; font-weight: 800; margin: 0 0 22px; }
    nav { display: grid; gap: 8px; }
    .nav-button { background: transparent; color: #94a3b8; text-align: left; width: 100%; }
    .nav-button.active { background: #1e293b; color: #e5e7eb; }
    main { max-width: 1180px; width: 100%; padding: 28px; box-sizing: border-box; }
    header { display: flex; justify-content: space-between; gap: 16px; align-items: center; }
    h1 { margin: 0; font-size: 28px; }
    h2 { margin: 0 0 10px; font-size: 18px; }
    button { border: 0; border-radius: 10px; padding: 10px 14px; background: #38bdf8; color: #04111f; font-weight: 700; cursor: pointer; }
    button.secondary { background: #334155; color: #e5e7eb; }
    .toolbar { display: flex; flex-wrap: wrap; gap: 10px; margin-top: 20px; align-items: end; }
    .field { display: grid; gap: 6px; }
    .field label { color: #94a3b8; font-size: 12px; text-transform: uppercase; letter-spacing: .08em; }
    select, input { background: #111827; border: 1px solid #334155; border-radius: 10px; color: #e5e7eb; padding: 9px 10px; }
    input.weight-input { width: 86px; text-align: right; }
    input.key-input { width: min(420px, 100%); }
    .grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(210px, 1fr)); gap: 14px; margin: 22px 0; }
    .card { background: linear-gradient(180deg, #111827, #0f172a); border: 1px solid #1f2937; border-radius: 16px; padding: 16px; box-shadow: 0 18px 60px rgb(0 0 0 / 22%); }
    .section-title { display: flex; justify-content: space-between; gap: 12px; align-items: baseline; }
    .label { color: #94a3b8; font-size: 12px; text-transform: uppercase; letter-spacing: .08em; }
    .value { font-size: 30px; font-weight: 800; margin-top: 8px; }
    table { width: 100%; border-collapse: collapse; margin-top: 10px; }
    th, td { padding: 9px 10px; text-align: right; border-bottom: 1px solid #1f2937; }
    th:first-child, td:first-child { text-align: left; }
    th { color: #93c5fd; font-size: 12px; text-transform: uppercase; letter-spacing: .06em; }
    .muted { color: #94a3b8; }
    .ok { color: #86efac; }
    .error { color: #fca5a5; }
    .status { display: inline-block; border-radius: 999px; padding: 3px 8px; font-size: 12px; background: #1e293b; color: #cbd5e1; }
    .status.ok { background: #064e3b; color: #bbf7d0; }
    .status.warn { background: #78350f; color: #fde68a; }
    .provider-group { margin-top: 18px; }
    .provider-group h3 { margin: 0 0 4px; font-size: 15px; color: #e0f2fe; }
    .provider-group .muted { font-size: 12px; }
    .page { display: none; }
    .page.active { display: block; }
    @media (max-width: 760px) { .shell { grid-template-columns: 1fr; } aside { position: sticky; top: 0; z-index: 1; } nav { grid-template-columns: repeat(2, minmax(0, 1fr)); } main { padding: 18px; } header { align-items: flex-start; flex-direction: column; } }
  </style>
</head>
<body>
<div class="shell">
  <aside>
    <div class="brand">Ark Key Router</div>
    <nav>
      <button id="nav-home" class="nav-button active" onclick="showPage('home')">Home</button>
      <button id="nav-settings" class="nav-button" onclick="showPage('settings')">Settings</button>
    </nav>
  </aside>
  <main>
    <section id="page-home" class="page active">
      <header>
        <div>
          <h1>Dashboard</h1>
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
      <section class="card"><div class="section-title"><h2>Today by Key</h2><span class="muted" id="today-by-key-note"></span></div><div id="today-by-key"></div></section>
      <section class="card"><h2>Daily Requests</h2><div id="by-day"></div></section>
      <section class="card"><h2>Monthly Requests</h2><div id="by-month"></div></section>
      <section class="card"><h2>Usage by Model</h2><div id="by-model"></div></section>
      <section class="card"><h2>Usage by Key</h2><div id="by-key"></div></section>
      <section class="card"><h2>Usage by Status</h2><div id="by-status"></div></section>
      <section class="card"><h2>Frozen Keys</h2><div id="frozen"></div></section>
    </section>

    <section id="page-settings" class="page">
      <header>
        <div>
          <h1>Settings</h1>
          <div class="muted">Change provider URLs, routing weights, and encrypted keys without restarting the service.</div>
        </div>
        <button class="secondary" onclick="loadSettings()">Refresh</button>
      </header>
      <section class="card"><div class="section-title"><h2>Provider URLs</h2><span class="muted" id="providers-note">Loading providers...</span></div><div id="provider-urls"></div></section>
      <section class="card"><div class="section-title"><h2>Key Weights</h2><span class="muted" id="weights-note">Loading weights...</span></div><div id="key-weights"></div></section>
      <section class="card"><div class="section-title"><h2>API Keys</h2><span class="muted" id="keys-note">Loading key config...</span></div><p class="muted">Values are saved to an encrypted SOPS file. Existing values are never displayed.</p><div id="api-keys"></div></section>
    </section>
  </main>
</div>
<script>
const number = new Intl.NumberFormat();

function showPage(page) {
  for (const name of ['home', 'settings']) {
    document.getElementById(`page-${name}`).classList.toggle('active', name === page);
    document.getElementById(`nav-${name}`).classList.toggle('active', name === page);
  }
  if (page === 'settings') loadSettings();
}

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

function tokenTable(data) {
  const entries = Object.entries(data || {}).sort((left, right) => (right[1].total_tokens || 0) - (left[1].total_tokens || 0));
  if (!entries.length) return '<p class="muted">No token usage today.</p>';
  const rows = entries.map(([name, item]) => `<tr><td>${name}</td><td>${number.format(item.prompt_tokens)}</td><td>${number.format(item.cached_tokens || 0)}</td><td>${formatPercent(item.cache_hit_rate)}</td><td>${number.format(item.completion_tokens)}</td><td>${number.format(item.total_tokens)}</td><td>${number.format(item.requests)}</td></tr>`).join('');
  return `<table><thead><tr><th>Key</th><th>Prompt</th><th>Cached</th><th>Cache Hit</th><th>Completion</th><th>Total Tokens</th><th>Requests</th></tr></thead><tbody>${rows}</tbody></table>`;
}

function weightsTable(config) {
  const weights = config.weights || {};
  const entries = Object.entries(weights).sort((left, right) => left[0].localeCompare(right[0]));
  if (!entries.length) return '<p class="muted">No configurable keys.</p>';
  const total = entries.reduce((sum, [, weight]) => sum + Math.max(0, Number(weight) || 0), 0);
  const rows = entries.map(([name, weight]) => {
    const numericWeight = Math.max(0, Number(weight) || 0);
    const probability = total > 0 ? `${((numericWeight / total) * 100).toFixed(1)}%` : '0.0%';
    return `<tr><td>${name}</td><td><input class="weight-input" data-key="${name}" type="number" min="0" step="1" value="${numericWeight}"></td><td>${probability}</td></tr>`;
  }).join('');
  return `<table><thead><tr><th>Key</th><th>Weight</th><th>Probability</th></tr></thead><tbody>${rows}</tbody></table><div class="toolbar"><button onclick="saveWeights()">Save Weights</button><button class="secondary" onclick="loadWeights()">Reload Weights</button><span id="weights-status" class="muted"></span></div>`;
}

function providersTable(config) {
  const providers = config.providers || [];
  if (!providers.length) return '<p class="muted">No configurable providers.</p>';
  const rows = providers.map((item) => `<tr><td>${item.name}</td><td><input class="provider-input" data-provider="${item.name}" type="url" value="${item.base_url}"></td><td>${item.default_base_url}</td></tr>`).join('');
  return `<table><thead><tr><th>Provider</th><th>Base URL</th><th>Default</th></tr></thead><tbody>${rows}</tbody></table><div class="toolbar"><button onclick="saveProviders()">Save Providers</button><button class="secondary" onclick="loadProviders()">Reload Providers</button><span id="providers-status" class="muted"></span></div>`;
}

function keysTable(config) {
  const entries = config.keys || [];
  if (!entries.length) return '<p class="muted">No configurable API keys.</p>';
  const grouped = entries.reduce((groups, item) => {
    if (!groups[item.provider]) groups[item.provider] = [];
    groups[item.provider].push(item);
    return groups;
  }, {});
  const groups = Object.entries(grouped).sort(([left], [right]) => left.localeCompare(right)).map(([provider, items]) => {
    const rows = items.map((item) => {
      const statusClass = item.configured ? 'ok' : 'warn';
      const statusLabel = item.configured ? item.source : 'missing';
      return `<tr><td>${item.name}</td><td>${billingLabel(item.billing_type)}</td><td>${item.env_var}</td><td><span class="status ${statusClass}">${statusLabel}</span></td><td><input class="key-input" data-key="${item.name}" type="password" autocomplete="off" placeholder="Leave blank to keep current value"></td><td><input class="delete-key" data-key="${item.name}" type="checkbox"></td></tr>`;
    }).join('');
    return `<div class="provider-group"><h3>${provider}</h3><div class="muted">Keys for this provider are routed through its configured base URL.</div><table><thead><tr><th>Key</th><th>Billing</th><th>Env Var</th><th>Status</th><th>New Value</th><th>Delete Encrypted</th></tr></thead><tbody>${rows}</tbody></table></div>`;
  }).join('');
  return `${groups}<div class="toolbar"><button onclick="saveKeys()">Save API Keys</button><button class="secondary" onclick="loadKeys()">Reload Keys</button><span id="keys-status" class="muted"></span></div>`;
}

function billingLabel(value) {
  if (value === 'payg') return 'Pay-as-you-go';
  if (value === 'subscription') return 'Subscription';
  return value || 'Unknown';
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
  const [response, todayResponse] = await Promise.all([
    fetch(usageUrl()),
    fetch('/api/usage?period=today'),
  ]);
  const data = await response.json();
  const todayUsage = await todayResponse.json();
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
  document.getElementById('today-by-key').innerHTML = tokenTable(todayUsage.by_key);
  document.getElementById('today-by-key-note').textContent = `${number.format(todayUsage.total.total_tokens)} tokens today`;
  document.getElementById('by-day').innerHTML = table(data.usage.by_day);
  document.getElementById('by-month').innerHTML = table(data.usage.by_month);
  document.getElementById('by-model').innerHTML = table(data.usage.by_model);
  document.getElementById('by-key').innerHTML = table(data.usage.by_key);
  document.getElementById('by-status').innerHTML = table(data.usage.by_status);
  document.getElementById('frozen').innerHTML = frozenTable(data.frozen);
}

async function loadWeights() {
  const response = await fetch('/api/config/weights');
  const config = await response.json();
  document.getElementById('key-weights').innerHTML = weightsTable(config);
  document.getElementById('weights-note').textContent = config.config_path || '';
}

async function loadProviders() {
  const response = await fetch('/api/config/providers');
  const config = await response.json();
  document.getElementById('provider-urls').innerHTML = providersTable(config);
  document.getElementById('providers-note').textContent = config.config_path || '';
}

async function loadSettings() {
  await Promise.all([loadProviders(), loadWeights(), loadKeys()]);
}

async function loadKeys() {
  const response = await fetch('/api/config/keys');
  const config = await response.json();
  document.getElementById('api-keys').innerHTML = keysTable(config);
  document.getElementById('keys-note').textContent = config.config_path || '';
}

async function saveWeights() {
  const inputs = document.querySelectorAll('.weight-input');
  const weights = {};
  for (const input of inputs) {
    weights[input.dataset.key] = Number(input.value || 0);
  }
  const response = await fetch('/api/config/weights', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ weights }),
  });
  const result = await response.json();
  const status = document.getElementById('weights-status');
  if (!response.ok) {
    status.className = 'error';
    status.textContent = result.detail || 'Failed to save weights';
    return;
  }
  status.className = 'ok';
  status.textContent = 'Saved. New requests use these weights immediately.';
  document.getElementById('key-weights').innerHTML = weightsTable(result);
  await loadData();
}

async function saveProviders() {
  const providers = {};
  for (const input of document.querySelectorAll('.provider-input')) {
    providers[input.dataset.provider] = input.value;
  }
  const response = await fetch('/api/config/providers', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ providers }),
  });
  const result = await response.json();
  const status = document.getElementById('providers-status');
  if (!response.ok) {
    status.className = 'error';
    status.textContent = result.detail || 'Failed to save providers';
    return;
  }
  status.className = 'ok';
  status.textContent = 'Saved. New requests use these provider URLs immediately.';
  document.getElementById('provider-urls').innerHTML = providersTable(result);
}

async function saveKeys() {
  const values = {};
  for (const input of document.querySelectorAll('.key-input')) {
    if (input.value) values[input.dataset.key] = input.value;
  }
  const deleteNames = [];
  for (const input of document.querySelectorAll('.delete-key')) {
    if (input.checked) deleteNames.push(input.dataset.key);
  }
  const response = await fetch('/api/config/keys', {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ keys: values, delete: deleteNames }),
  });
  const result = await response.json();
  const status = document.getElementById('keys-status');
  if (!response.ok) {
    status.className = 'error';
    status.textContent = result.detail || 'Failed to save keys';
    return;
  }
  status.className = 'ok';
  status.textContent = 'Saved encrypted key config. New requests use it immediately.';
  document.getElementById('api-keys').innerHTML = keysTable(result);
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
loadSettings();
setInterval(loadData, 5000);
</script>
</body>
</html>
""".strip()
