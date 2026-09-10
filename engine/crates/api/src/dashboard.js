const base = location.pathname.replace(/\/dashboard\/?$/, '');
const sessionKey = `rbe-admin:${location.origin}${base}`;
let auth = null;
let activeTab = 'overview';
let settingsDoc = null;
let settingsRevision = null;
let settingsDirty = false;
let editorMode = 'tree';

const $ = selector => document.querySelector(selector);
const $$ = selector => [...document.querySelectorAll(selector)];
const esc = value => String(value ?? '—').replace(/[&<>"']/g, char => ({
  '&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'
}[char]));
const title = value => String(value).replace(/_/g,' ').replace(/\b\w/g, char => char.toUpperCase());
const num = value => Number(value || 0).toLocaleString();
const duration = value => {
  const seconds = Math.max(0, Number(value || 0));
  const d = Math.floor(seconds / 86400), h = Math.floor(seconds % 86400 / 3600);
  const m = Math.floor(seconds % 3600 / 60), s = Math.floor(seconds % 60);
  return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : m ? `${m}m ${s}s` : `${s}s`;
};
const millis = value => `${Number(value || 0).toFixed(2)} ms`;
const typeOf = value => Array.isArray(value) ? 'array' : value === null ? 'null' : typeof value;
const encodePath = path => encodeURIComponent(JSON.stringify(path));
const decodePath = value => JSON.parse(decodeURIComponent(value));

function loadStoredAuth() {
  try {
    const value = JSON.parse(sessionStorage.getItem(sessionKey) || 'null');
    if (value && typeof value.session === 'string' && typeof value.csrf === 'string') auth = value;
  } catch (_) {}
}
function persistAuth(value) {
  auth = value;
  if (value) sessionStorage.setItem(sessionKey, JSON.stringify(value));
  else sessionStorage.removeItem(sessionKey);
}

async function request(path, options = {}) {
  const headers = new Headers(options.headers || {});
  if (auth?.session) headers.set('x-rbe-admin-session', auth.session);
  if (options.mutation && auth?.csrf) headers.set('x-rbe-admin-csrf', auth.csrf);
  let body = options.body;
  if (body !== undefined && typeof body !== 'string') {
    headers.set('content-type', 'application/json');
    body = JSON.stringify(body);
  }
  const response = await fetch(`${base}/api/${path}`, {
    method: options.method || 'GET',
    headers,
    body,
    cache: 'no-store'
  });
  const text = await response.text();
  let data = {};
  try { data = text ? JSON.parse(text) : {}; } catch (_) { data = { error: text || response.statusText }; }
  if (response.status === 401 && path !== 'login' && path !== 'session') {
    lockDashboard('Session expired. Enter the build-time admin password again.');
  }
  if (!response.ok) {
    const error = new Error(data.error || `${response.status} ${response.statusText}`);
    error.status = response.status;
    error.data = data;
    throw error;
  }
  return data;
}

function showToast(message, tone = 'good') {
  const node = $('#toast');
  node.textContent = message;
  node.className = `toast ${tone}`;
  clearTimeout(showToast.timer);
  showToast.timer = setTimeout(() => node.classList.add('hidden'), 4200);
}

function lockDashboard(message = '') {
  persistAuth(null);
  $('#app').classList.add('hidden');
  $('#login-screen').classList.remove('hidden');
  $('#admin-password').value = '';
  $('#login-error').textContent = message;
  $('#admin-password').focus();
}

function unlockDashboard() {
  $('#login-screen').classList.add('hidden');
  $('#app').classList.remove('hidden');
  $('#login-error').textContent = '';
  refreshActive();
}

async function bootstrap() {
  loadStoredAuth();
  try {
    const state = await request('session');
    if (!state.configured) {
      $('#login-copy').textContent = 'This backend was built without an admin password. Rebuild it with build.ps1 or build.sh to enable the Control Room.';
      $('#admin-password').disabled = true;
      $('#login-button').disabled = true;
      $('#login-error').textContent = 'Admin control plane is locked in this build.';
      return;
    }
    if (state.authenticated) unlockDashboard();
    else lockDashboard();
  } catch (error) {
    $('#login-error').textContent = error.message;
  }
}

$('#login-form').addEventListener('submit', async event => {
  event.preventDefault();
  const button = $('#login-button');
  const password = $('#admin-password').value;
  button.disabled = true;
  $('#login-error').textContent = '';
  try {
    const result = await request('login', { method: 'POST', body: { password } });
    persistAuth({ session: result.session, csrf: result.csrf, expiresAtMs: result.expiresAtMs });
    $('#admin-password').value = '';
    unlockDashboard();
  } catch (error) {
    if (error.status === 429 && error.data?.retryAfterSecs) {
      $('#login-error').textContent = `Too many attempts. Try again in ${error.data.retryAfterSecs}s.`;
    } else {
      $('#login-error').textContent = error.message;
    }
  } finally {
    button.disabled = false;
  }
});

$('#toggle-password').addEventListener('click', () => {
  const field = $('#admin-password');
  field.type = field.type === 'password' ? 'text' : 'password';
  $('#toggle-password').textContent = field.type === 'password' ? 'SHOW' : 'HIDE';
});

$('#logout-button').addEventListener('click', async () => {
  try { await request('logout', { method: 'POST', mutation: true }); } catch (_) {}
  lockDashboard('Dashboard locked.');
});

$$('.nav-item[data-tab]').forEach(button => {
  button.addEventListener('click', () => {
    activeTab = button.dataset.tab;
    $$('.nav-item[data-tab]').forEach(item => item.classList.toggle('active', item === button));
    $$('.tab').forEach(section => section.classList.toggle('active', section.id === activeTab));
    const names = {overview:'Overview',backend:'Backend',container:'Container',security:'Security',settings:'Settings'};
    const eyebrows = {overview:'RUNTIME TELEMETRY',backend:'PROCESS CONTROL',container:'EXECUTION RUNTIME',security:'ABUSE CONTROLS',settings:'CONFIGURATION'};
    $('#page-title').textContent = names[activeTab];
    $('#section-eyebrow').textContent = eyebrows[activeTab];
    refreshActive();
  });
});

function status(label, tone = 'good') {
  return `<span class="status ${tone}"><i></i>${esc(label)}</span>`;
}
function metric(label, value, meta = '', tone = '', glyph = '•') {
  return `<article class="metric ${tone}">
    <div class="metric-top"><span class="metric-icon">${esc(glyph)}</span><span class="metric-label">${esc(label)}</span></div>
    <div class="metric-value">${esc(value)}</div>${meta ? `<div class="metric-meta">${esc(meta)}</div>` : ''}
  </article>`;
}
function panel(name, subtitle, body) {
  return `<section class="panel"><div class="panel-head"><div><h2>${esc(name)}</h2>${subtitle ? `<p>${esc(subtitle)}</p>` : ''}</div></div>${body}</section>`;
}
function valueText(value) {
  if (Array.isArray(value)) return value.length ? value.join(', ') : 'None';
  if (value && typeof value === 'object') return JSON.stringify(value);
  if (typeof value === 'boolean') return value ? 'Enabled' : 'Disabled';
  return value ?? '—';
}
function kvRows(object) {
  if (!object || !Object.keys(object).length) return '<div class="empty">No data available.</div>';
  return `<div class="kv">${Object.entries(object).map(([key,value]) =>
    `<div class="kv-row"><span>${esc(title(key))}</span><strong>${esc(valueText(value))}</strong></div>`
  ).join('')}</div>`;
}
function raw(data) {
  return `<details class="raw"><summary>Raw payload</summary><pre>${esc(JSON.stringify(data,null,2))}</pre></details>`;
}
function table(headers, rows) {
  if (!rows.length) return '<div class="empty">Nothing here.</div>';
  return `<div class="table-wrap"><table><thead><tr>${headers.map(h => `<th>${esc(title(h))}</th>`).join('')}</tr></thead>
    <tbody>${rows.map(row => `<tr>${headers.map(h => `<td>${esc(valueText(row[h]))}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`;
}
function errorView(error) {
  return `<div class="error-state"><strong>Control request failed</strong><p>${esc(error.message || error)}</p></div>`;
}

async function renderOverview() {
  const d = await request('overview'), b = d.backend, c = d.container, h = c.health || {}, s = d.security, m = d.maintenance, r = b.responses || {};
  const backendGood = /running|ready/i.test(String(b.state));
  const containerGood = !h.error && h.ok !== false;
  const total = Object.values(r).reduce((sum, value) => sum + Number(value || 0), 0);
  const success = total ? `${(Number(r['2xx'] || 0) / total * 100).toFixed(1)}%` : '—';
  $('#overview-view').innerHTML = `
    <div class="page-intro"><div><div class="eyebrow">SYSTEM AT A GLANCE</div><h2>Control plane, not a JSON poster.</h2>
    <p>Authenticated backend, container, request and policy telemetry. Settings are editable from the same local control surface.</p></div>
    ${status(backendGood && containerGood ? 'Systems nominal' : 'Needs attention', backendGood && containerGood ? 'good' : 'warn')}</div>
    <div class="metric-grid">
      ${metric('Backend', b.state, `PID ${b.pid}`, backendGood ? 'good' : 'warn', 'BE')}
      ${metric('Container', containerGood ? 'Healthy' : 'Attention', `PID ${c.pid ?? '—'} · gen ${c.generation}`, containerGood ? 'good' : 'bad', 'CT')}
      ${metric('Uptime', duration(b.uptime_secs), 'backend.exe', '', 'UP')}
      ${metric('Requests', num(b.total_requests), `${num(b.active_requests)} active`, '', 'RQ')}
      ${metric('Avg latency', millis(b.average_latency_ms), 'rolling average', Number(b.average_latency_ms) > 100 ? 'warn' : 'good', 'MS')}
      ${metric('Success rate', success, `${num(r['5xx'])} server errors`, Number(r['5xx']) ? 'warn' : 'good', '2X')}
      ${metric('Banned IPs', num(s.banned_ips), `${num(s.active_strike_buckets)} strike buckets`, s.banned_ips ? 'bad' : 'good', 'IP')}
      ${metric('Generation', num(c.generation), c.control_address || 'container IPC', '', 'GN')}
    </div>
    <div class="two-col">
      ${panel('HTTP responses','Response families since process start',kvRows(r))}
      ${panel('Maintenance','Rolling process refresh activity',kvRows(m))}
    </div>`;
}

async function renderBackend() {
  const d = await request('backend'), r = d.requests || {};
  const good = /running|ready/i.test(String(d.state));
  $('#backend-view').innerHTML = `
    <div class="page-intro"><div><div class="eyebrow">BACKEND.EXE</div><h2>Backend process</h2>
    <p>Current process state, listener configuration and effective runtime settings.</p></div>${status(d.state, good ? 'good' : 'warn')}</div>
    <div class="metric-grid compact">
      ${metric('Process ID', d.pid, 'backend.exe', '', 'ID')}
      ${metric('Uptime', duration(d.uptime_secs), 'process age', '', 'UP')}
      ${metric('Requests', num(r.total), `${num(r.active)} active`, '', 'RQ')}
      ${metric('Latency', millis(r.average_latency_ms), 'average', Number(r.average_latency_ms) > 100 ? 'warn' : 'good', 'MS')}
    </div>
    <div class="three-col">${panel('Requests','Traffic counters',kvRows(r))}${panel('API','Listener and limits',kvRows(d.api))}${panel('Runtime','Process configuration',kvRows(d.runtime))}</div>
    ${raw(d)}`;
}

async function renderContainer() {
  const d = await request('container'), good = d.online !== false, state = d.state || {};
  const config = state.config || {}, runtime = {...state}; delete runtime.config;
  $('#container-view').innerHTML = `
    <div class="page-intro"><div><div class="eyebrow">CONTAINER RUNTIME</div><h2>Execution container</h2>
    <p>Current control endpoint, generation, resolved topology and live runtime state.</p></div>${status(good ? 'Online' : 'Offline', good ? 'good' : 'bad')}</div>
    <div class="metric-grid compact">
      ${metric('Status',good?'Online':'Offline',d.error||'control channel',good?'good':'bad','CT')}
      ${metric('Process ID',d.pid??'—','container process','','ID')}
      ${metric('Generation',d.generation??'—','rolling generation','','GN')}
      ${metric('Control',d.control_address||'—','IPC address','','IP')}
    </div>
    <div class="two-col">${panel('Resolved config','What the process is actually using',kvRows(config))}${panel('Runtime state','Live container internals',kvRows(runtime))}</div>
    ${raw(d)}`;
}

async function renderSecurity() {
  const d = await request('security'), p = d.policy || {}, bans = d.banned_ips || [], strikes = d.strikes || [];
  $('#security-view').innerHTML = `
    <div class="page-intro"><div><div class="eyebrow">SECURITY</div><h2>Abuse controls</h2>
    <p>IP bans, strike buckets and the effective request policy.</p></div>${status(bans.length ? 'Active bans' : 'Clear', bans.length ? 'warn' : 'good')}</div>
    <div class="metric-grid compact">
      ${metric('Banned IPs',bans.length,`${strikes.length} strike buckets`,bans.length?'bad':'good','IP')}
      ${metric('Strike threshold',p.strike_threshold??'—',`${p.strike_window_secs??'—'}s window`,'','ST')}
      ${metric('Ban duration',duration(p.ban_duration_secs),`${p.ban_duration_secs??'—'} seconds`,'','BN')}
      ${metric('Proxy headers',p.trusted_proxy_headers?'Trusted':'Ignored','real IP policy',p.trusted_proxy_headers?'warn':'good','PX')}
    </div>
    <div class="two-col">${panel('Global rate limit','All requests',kvRows(p.global_rate_limit||{}))}${panel('API rate limit','API requests',kvRows(p.api_rate_limit||{}))}</div>
    <div class="two-col">${panel('Bans',`${bans.length} active`,table(['ip','age_secs','remaining_secs'],bans))}${panel('Strikes',`${strikes.length} active`,table(['ip','category','count','age_secs','remaining_window_secs'],strikes))}</div>`;
}

async function refreshActive() {
  if (!auth) return;
  try {
    if (activeTab === 'settings') {
      if (!settingsDoc) await loadSettings();
    } else if (activeTab === 'overview') await renderOverview();
    else if (activeTab === 'backend') await renderBackend();
    else if (activeTab === 'container') await renderContainer();
    else if (activeTab === 'security') await renderSecurity();
    $('#last-refresh').textContent = new Date().toLocaleTimeString();
  } catch (error) {
    const target = $(`#${activeTab}-view`);
    if (target) target.innerHTML = errorView(error);
  }
}

function setDirty(value = true) {
  settingsDirty = value;
  $('#settings-dot').classList.toggle('hidden', !value);
  $('#save-settings').disabled = !value;
  const state = $('#save-state');
  state.textContent = value ? 'Unsaved changes' : 'Clean';
  state.className = `save-state ${value ? 'dirty' : ''}`;
}

async function loadSettings() {
  $('#settings-error').classList.add('hidden');
  const data = await request('settings');
  settingsDoc = data.document;
  settingsRevision = data.revision;
  $('#settings-source').textContent = data.source || 'settings.json';
  $('#raw-editor').value = JSON.stringify(settingsDoc, null, 2);
  renderTree();
  setDirty(false);
}

function getAt(path) {
  let node = settingsDoc;
  for (const segment of path) node = node[segment];
  return node;
}
function parentOf(path) {
  if (!path.length) return [null, null];
  const parent = getAt(path.slice(0,-1));
  return [parent, path[path.length-1]];
}
function setAt(path, value) {
  if (!path.length) settingsDoc = value;
  else {
    const [parent, key] = parentOf(path);
    parent[key] = value;
  }
  setDirty(true);
}
function deleteAt(path) {
  const [parent, key] = parentOf(path);
  if (Array.isArray(parent)) parent.splice(Number(key), 1);
  else if (parent && typeof parent === 'object') delete parent[key];
  setDirty(true);
  renderTree();
}
function renameAt(path, newKey) {
  if (!path.length || !newKey) return;
  const parentPath = path.slice(0,-1), oldKey = path[path.length-1], parent = getAt(parentPath);
  if (!parent || Array.isArray(parent) || typeof parent !== 'object' || newKey === oldKey) return;
  if (Object.prototype.hasOwnProperty.call(parent, newKey)) {
    showToast(`"${newKey}" already exists in this group.`, 'bad');
    renderTree();
    return;
  }
  const entries = Object.entries(parent);
  for (const key of Object.keys(parent)) delete parent[key];
  for (const [key,value] of entries) parent[key === oldKey ? newKey : key] = value;
  setDirty(true);
  renderTree();
}
function defaultValue(kind) {
  return kind === 'object' ? {} : kind === 'array' ? [] : kind === 'number' ? 0 : kind === 'boolean' ? false : kind === 'null' ? null : '';
}
function uniqueKey(object, baseName) {
  if (!(baseName in object)) return baseName;
  let index = 2;
  while (`${baseName}${index}` in object) index++;
  return `${baseName}${index}`;
}
function addChild(path, kind) {
  const node = getAt(path);
  const value = defaultValue(kind);
  if (Array.isArray(node)) node.push(value);
  else if (node && typeof node === 'object') {
    const baseName = kind === 'object' ? 'newGroup' : kind === 'array' ? 'newArray' : 'newValue';
    node[uniqueKey(node, baseName)] = value;
  } else return;
  setDirty(true);
  renderTree();
}

function typeOptions(type) {
  return ['string','number','boolean','null','object','array']
    .map(value => `<option value="${value}"${value === type ? ' selected' : ''}>${value}</option>`).join('');
}
function primitiveInput(value, path) {
  const type = typeOf(value), encoded = encodePath(path);
  if (type === 'boolean') {
    return `<select class="tree-value primitive" data-path="${encoded}" data-value-type="boolean">
      <option value="true"${value ? ' selected' : ''}>true</option><option value="false"${!value ? ' selected' : ''}>false</option></select>`;
  }
  if (type === 'null') return `<input class="tree-value" value="null" disabled>`;
  return `<input class="tree-value primitive" data-path="${encoded}" data-value-type="${type}" ${type === 'number' ? 'type="number" step="any"' : 'type="text"'} value="${esc(value)}">`;
}
function renderNode(value, path, displayKey, parentKind, root = false) {
  const type = typeOf(value), encoded = encodePath(path);
  const keyPart = root
    ? `<span class="tree-label">settings.json</span>`
    : parentKind === 'object'
      ? `<input class="tree-key" data-path="${encoded}" value="${esc(displayKey)}" aria-label="Setting key">`
      : `<span class="tree-label">[${esc(displayKey)}]</span>`;
  const deleteButton = root ? '' : `<button class="node-button delete" data-action="delete" data-path="${encoded}">Delete</button>`;
  const composite = type === 'object' || type === 'array';
  const addButtons = composite ? `
    <button class="node-button" data-action="add" data-kind="string" data-path="${encoded}">+ value</button>
    <button class="node-button" data-action="add" data-kind="object" data-path="${encoded}">+ group</button>
    <button class="node-button" data-action="add" data-kind="array" data-path="${encoded}">+ array</button>` : '';
  const valuePart = composite
    ? `<span class="tree-label">${type === 'array' ? `${value.length} items` : `${Object.keys(value).length} keys`}</span>`
    : primitiveInput(value, path);
  const rowClass = root ? 'tree-row root-row' : 'tree-row';
  let children = '';
  if (type === 'object') {
    children = Object.entries(value).map(([key,child]) => renderNode(child,[...path,key],key,'object')).join('');
  } else if (type === 'array') {
    children = value.map((child,index) => renderNode(child,[...path,index],index,'array')).join('');
  }
  return `<div class="${root ? '' : 'tree-node'}">
    <div class="${rowClass}">
      ${keyPart}
      ${root ? '' : `<select class="tree-type" data-path="${encoded}">${typeOptions(type)}</select>`}
      ${valuePart}
      <div class="node-actions">${addButtons}${deleteButton}</div>
    </div>
    ${composite ? `<div class="children">${children}</div>` : ''}
  </div>`;
}
function renderTree() {
  if (settingsDoc === null) return;
  $('#tree-editor').innerHTML = renderNode(settingsDoc, [], '', '', true);
}

$('#tree-editor').addEventListener('change', event => {
  const target = event.target;
  if (target.classList.contains('tree-key')) {
    const path = decodePath(target.dataset.path);
    renameAt(path, target.value.trim());
    return;
  }
  if (target.classList.contains('tree-type')) {
    const path = decodePath(target.dataset.path);
    setAt(path, defaultValue(target.value));
    renderTree();
    return;
  }
  if (target.classList.contains('primitive')) {
    const path = decodePath(target.dataset.path);
    const kind = target.dataset.valueType;
    let value = target.value;
    if (kind === 'number') {
      value = Number(value);
      if (!Number.isFinite(value)) return;
    } else if (kind === 'boolean') value = value === 'true';
    setAt(path, value);
  }
});
$('#tree-editor').addEventListener('input', event => {
  const target = event.target;
  if (!target.classList.contains('primitive') || target.dataset.valueType === 'boolean') return;
  const path = decodePath(target.dataset.path);
  let value = target.value;
  if (target.dataset.valueType === 'number') {
    value = Number(value);
    if (!Number.isFinite(value)) return;
  }
  setAt(path, value);
});
$('#tree-editor').addEventListener('click', event => {
  const button = event.target.closest('[data-action]');
  if (!button) return;
  const path = decodePath(button.dataset.path);
  if (button.dataset.action === 'delete') deleteAt(path);
  if (button.dataset.action === 'add') addChild(path, button.dataset.kind);
});
$$('[data-root-add]').forEach(button => button.addEventListener('click', () => {
  if (!settingsDoc || Array.isArray(settingsDoc) || typeof settingsDoc !== 'object') {
    showToast('The settings root must be an object.', 'bad');
    return;
  }
  addChild([], button.dataset.rootAdd === 'value' ? 'string' : button.dataset.rootAdd);
}));

$('#raw-editor').addEventListener('input', () => setDirty(true));
$('#tree-mode').addEventListener('click', () => {
  if (editorMode === 'tree') return;
  try {
    settingsDoc = JSON.parse($('#raw-editor').value);
    $('#settings-error').classList.add('hidden');
  } catch (error) {
    showEditorError(`Raw JSON is invalid: ${error.message}`);
    return;
  }
  editorMode = 'tree';
  $('#tree-mode').classList.add('active'); $('#raw-mode').classList.remove('active');
  $('#tree-editor').classList.remove('hidden'); $('#raw-editor').classList.add('hidden');
  renderTree();
});
$('#raw-mode').addEventListener('click', () => {
  if (editorMode === 'raw') return;
  $('#raw-editor').value = JSON.stringify(settingsDoc, null, 2);
  editorMode = 'raw';
  $('#raw-mode').classList.add('active'); $('#tree-mode').classList.remove('active');
  $('#raw-editor').classList.remove('hidden'); $('#tree-editor').classList.add('hidden');
});

function showEditorError(message) {
  const node = $('#settings-error');
  node.textContent = message;
  node.classList.remove('hidden');
}

$('#reload-settings').addEventListener('click', async () => {
  if (settingsDirty && !confirm('Discard unsaved settings changes?')) return;
  try { await loadSettings(); showToast('Settings reloaded.'); }
  catch (error) { showEditorError(error.message); }
});

$('#save-settings').addEventListener('click', async () => {
  $('#settings-error').classList.add('hidden');
  let document = settingsDoc;
  if (editorMode === 'raw') {
    try { document = JSON.parse($('#raw-editor').value); }
    catch (error) { showEditorError(`Raw JSON is invalid: ${error.message}`); return; }
  }
  const button = $('#save-settings');
  button.disabled = true;
  button.textContent = 'Validating…';
  try {
    const result = await request('settings', {
      method: 'PUT',
      mutation: true,
      body: { revision: settingsRevision, document }
    });
    settingsDoc = document;
    settingsRevision = result.revision;
    $('#raw-editor').value = JSON.stringify(settingsDoc, null, 2);
    setDirty(false);
    $('#save-state').textContent = result.restartRequired ? 'Saved · restart pending' : 'Saved';
    $('#save-state').className = 'save-state saved';
    if (result.restartRequired) {
      const paths = result.pendingRestartPaths || [];
      $('#restart-banner').innerHTML = `<strong>Saved successfully.</strong> Runtime restart/refresh required for: ${esc(paths.slice(0,12).join(', '))}${paths.length > 12 ? ` and ${paths.length-12} more` : ''}`;
      $('#restart-banner').classList.remove('hidden');
    } else {
      $('#restart-banner').classList.add('hidden');
    }
    renderTree();
    showToast(result.message || 'Settings saved.');
  } catch (error) {
    if (error.status === 409) {
      showEditorError('settings.json changed outside this editor. Reload before saving so another change is not overwritten.');
    } else if (error.status === 422) {
      showEditorError(`Configuration rejected: ${error.message}`);
    } else showEditorError(error.message);
  } finally {
    button.textContent = 'Save settings';
    button.disabled = !settingsDirty;
  }
});

window.addEventListener('beforeunload', event => {
  if (!settingsDirty) return;
  event.preventDefault();
  event.returnValue = '';
});

setInterval(() => {
  if (auth && activeTab !== 'settings') refreshActive();
}, 4000);

bootstrap();
