import { createFileManager } from '/assets/file-manager.js';

const $ = id => document.getElementById(id);
const ACTIVE = new Set(['Starting', 'Running', 'Stopping', 'Installing']);
const STATUS = { Stopped: 'Offline', Running: 'Online', Starting: 'Starting', Stopping: 'Stopping', Installing: 'Installing', Crashed: 'Crashed' };
const TABS = ['console', 'files', 'settings', 'properties', 'network'];
const state = {
  authenticated: false, snapshot: null, selected: null, page: 'overview', tab: 'console',
  polling: false, busy: false, logs: [], cursor: 0, settingsDirty: false,
  networkDirty: false, propertiesDirty: false, propertiesLoaded: false, sidebarSignature: '',
};
const rows = new Map();
let notificationTimer;
let lastLogServer;
const versionCatalogs = new Map();
let versionSequence = 0;
let versionsLoading = false;
let versionError = '';

function element(tag, className, content) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (content !== undefined) node.textContent = content;
  return node;
}
function text(id, value) { $(id).textContent = value; }
function bytes(value = 0) {
  return value >= 1073741824 ? `${(value / 1073741824).toFixed(1)} GiB` : `${Math.round(value / 1048576)} MiB`;
}
function uptime(seconds) {
  if (seconds == null) return '—';
  if (seconds >= 3600) return `${Math.floor(seconds / 3600)}h ${Math.floor(seconds / 60) % 60}m`;
  if (seconds >= 60) return `${Math.floor(seconds / 60)}m ${seconds % 60}s`;
  return `${seconds}s`;
}
function selectedServer() { return state.snapshot?.servers.find(server => server.config.id === state.selected); }
function editable(server = selectedServer()) { return !!server && state.authenticated && !state.busy && !ACTIVE.has(server.status); }
function publicAddress(server) { return server?.network?.join_address || null; }
function requestError(payload, fallback) {
  return payload?.error || payload?.Error || fallback;
}

function notify(message, isError = false) {
  clearTimeout(notificationTimer);
  text('notification-text', message);
  $('notification').classList.toggle('error', isError);
  $('notification').hidden = false;
  if (!isError) notificationTimer = setTimeout(() => { $('notification').hidden = true; }, 12000);
}
function hideNotification() { clearTimeout(notificationTimer); $('notification').hidden = true; }
function showConnectionError(message) {
  text('connection-detail', message);
  $('connection-error').hidden = false;
}

function showLogin(message = '') {
  state.authenticated = false;
  state.snapshot = null;
  state.selected = null;
  state.logs = [];
  state.cursor = 0;
  state.busy = false;
  state.settingsDirty = state.propertiesDirty = state.networkDirty = false;
  fileManager.reset();
  $('app-shell').hidden = true;
  $('login-panel').hidden = false;
  $('login-error').hidden = !message;
  text('login-error', message);
  hideNotification();
}

async function request(path, options = {}) {
  const { timeout = 20000, allowUnauthorized = false, ...fetchOptions } = options;
  const headers = new Headers(fetchOptions.headers || {});
  const method = (fetchOptions.method || 'GET').toUpperCase();
  if (!['GET', 'HEAD', 'OPTIONS'].includes(method)) headers.set('X-Dawg-Request', '1');
  const controller = timeout ? new AbortController() : null;
  const timer = controller ? setTimeout(() => controller.abort(), timeout) : null;
  let response;
  try {
    response = await fetch(path, { ...fetchOptions, headers, credentials: 'same-origin', signal: fetchOptions.signal || controller?.signal });
  } catch (error) {
    throw new Error(error.name === 'AbortError' ? 'The request timed out. Check the connection before trying again.' : 'Cannot reach Dawg. Check your connection and the server.');
  } finally {
    if (timer) clearTimeout(timer);
  }
  if (response.status === 401 && !allowUnauthorized) {
    showLogin('Your session has expired. Sign in again.');
    throw new Error('Sign in to continue.');
  }
  if (!response.ok) {
    let message = `Request failed (${response.status}).`;
    try {
      const body = await response.text();
      try { message = requestError(JSON.parse(body), message); }
      catch { if (body && body.length < 500) message = body; }
    } catch { /* Preserve the HTTP outcome if reading its error fails. */ }
    throw new Error(message);
  }
  return response;
}
async function jsonRequest(path, method, body, options = {}) {
  const response = await request(path, { ...options, method, headers: { 'Content-Type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body) });
  return response.status === 204 ? null : response.json();
}
async function action(value) {
  const result = await jsonRequest('/api/action', 'POST', value);
  if (result && typeof result === 'object' && 'Error' in result) throw new Error(result.Error);
  return result;
}
const fileManager = createFileManager({ request, notify, onUnauthorized: () => showLogin('Your session has expired. Sign in again.') });

function dirtyForms() { return state.settingsDirty || state.propertiesDirty || state.networkDirty; }
function leaveCurrent() {
  if (state.tab === 'files' && state.page === 'server' && !fileManager.leave()) return false;
  if (dirtyForms() && !window.confirm('Discard your unsaved changes?')) return false;
  state.settingsDirty = state.propertiesDirty = state.networkDirty = false;
  return true;
}
function closeSidebar() {
  $('sidebar').classList.remove('open');
  $('sidebar-shade').hidden = true;
  $('open-sidebar').setAttribute('aria-expanded', 'false');
}
function showPage(page) {
  state.page = page;
  $('overview-panel').hidden = page !== 'overview';
  $('create-panel').hidden = page !== 'create';
  $('server-workspace').hidden = page !== 'server';
  $('all-servers').classList.toggle('active', page === 'overview');
  renderSidebar();
  closeSidebar();
}
function switchServer(id, checkDirty = true) {
  if (id === state.selected && state.page === 'server') { closeSidebar(); return; }
  if (checkDirty && !leaveCurrent()) return;
  state.selected = id;
  try { localStorage.setItem('dawg.selectedServer', id); } catch { /* Storage can be disabled. */ }
  state.logs = [];
  state.cursor = 0;
  lastLogServer = null;
  state.propertiesLoaded = false;
  state.propertiesDirty = state.settingsDirty = state.networkDirty = false;
  $('console-command').value = '';
  $('console-output').replaceChildren();
  $('properties-editor').value = '';
  showPage('server');
  populateSettings();
  populateNetwork();
  setTab('console', false);
  renderServer();
  void poll();
}
function setTab(tab, checkDirty = true) {
  if (!TABS.includes(tab)) return;
  if (tab === state.tab && state.page === 'server') return;
  if (checkDirty && !leaveCurrent()) return;
  state.tab = tab;
  document.querySelectorAll('[data-tab]').forEach(button => {
    const active = button.dataset.tab === tab;
    button.classList.toggle('active', active);
    if (active) button.setAttribute('aria-current', 'page'); else button.removeAttribute('aria-current');
  });
  TABS.forEach(name => { $(`${name}-panel`).hidden = name !== tab; });
  if (tab === 'files') fileManager.mount($('files-panel'), selectedServer());
  if (tab === 'settings') populateSettings();
  if (tab === 'network') populateNetwork();
  if (tab === 'properties' && !state.propertiesLoaded) void loadProperties();
  updateEnabled();
}

function renderSidebar() {
  const servers = state.snapshot?.servers || [];
  const signature = JSON.stringify([state.page, state.selected, servers.map(server => [server.config.id, server.config.name, server.status])]);
  if (signature === state.sidebarSignature) return;
  state.sidebarSignature = signature;
  const focused = document.activeElement?.dataset?.serverId;
  const fragment = document.createDocumentFragment();
  for (const server of servers) {
    const button = element('button', 'server-nav');
    button.type = 'button';
    button.dataset.serverId = server.config.id;
    button.title = server.config.name;
    button.classList.toggle('active', state.page === 'server' && state.selected === server.config.id);
    button.append(element('span', 'server-nav-name', server.config.name));
    const line = element('span', 'server-nav-state');
    const dot = element('span', `status-dot ${knownStatus(server.status)}`);
    dot.setAttribute('aria-hidden', 'true');
    line.append(dot, document.createTextNode(STATUS[server.status] || server.status));
    button.append(line);
    button.addEventListener('click', () => switchServer(server.config.id));
    fragment.append(button);
  }
  if (!servers.length) fragment.append(element('span', 'muted small', 'No servers'));
  $('server-list').replaceChildren(fragment);
  if (focused) [...$('server-list').querySelectorAll('button')].find(button => button.dataset.serverId === focused)?.focus();
}
function knownStatus(status) { return Object.hasOwn(STATUS, status) ? status.toLowerCase() : 'unknown'; }
function fillStatus(node, status) {
  node.className = `server-status ${knownStatus(status)}`;
  const dot = element('span', `status-dot ${knownStatus(status)}`);
  dot.setAttribute('aria-hidden', 'true');
  node.replaceChildren(dot, document.createTextNode(STATUS[status] || status));
}
function renderOverview() {
  const servers = state.snapshot?.servers || [];
  $('overview-empty').hidden = servers.length !== 0;
  $('server-table-wrap').hidden = servers.length === 0;
  const existing = new Set(servers.map(server => server.config.id));
  for (const [id, entry] of rows) if (!existing.has(id)) { entry.row.remove(); rows.delete(id); }
  servers.forEach((server, index) => {
    let entry = rows.get(server.config.id);
    if (!entry) {
      const row = element('tr');
      const nameCell = element('td');
      const name = element('button', 'quiet'); name.type = 'button';
      name.addEventListener('click', () => switchServer(server.config.id));
      nameCell.append(name);
      const software = element('td'); const address = element('td');
      const statusCell = element('td'); const status = element('span', 'server-status'); statusCell.append(status);
      const memory = element('td');
      row.append(nameCell, software, address, statusCell, memory);
      entry = { row, name, software, address, status, memory };
      rows.set(server.config.id, entry);
    }
    entry.name.textContent = server.config.name;
    entry.software.textContent = `${server.config.kind} ${server.config.minecraft_version}`;
    entry.address.textContent = publicAddress(server) || 'Not configured';
    entry.memory.textContent = server.pid ? bytes(server.memory_bytes) : '—';
    fillStatus(entry.status, server.status);
    if ($('server-table').children[index] !== entry.row) $('server-table').insertBefore(entry.row, $('server-table').children[index] || null);
  });
}
function lifecycleButton(label, value, kind = '') {
  const button = element('button', kind, label);
  button.type = 'button';
  button.disabled = state.busy;
  button.addEventListener('click', () => void mutate(value));
  return button;
}
let lifecycleSignature = '';
function renderServer() {
  const server = selectedServer();
  if (!server) return;
  const config = server.config;
  text('server-name', config.name);
  $('server-name').title = config.name;
  fillStatus($('server-status'), server.status);
  text('server-software', `${config.kind} ${config.minecraft_version}`);
  const address = publicAddress(server);
  text('join-address', address || 'Public access not configured');
  $('copy-address').hidden = !address;
  text('cpu-value', server.pid ? `${server.cpu_percent.toFixed(1)}%` : '—');
  text('memory-value', server.pid ? `${bytes(server.memory_bytes)} / ${config.memory_mb} MiB` : `${config.memory_mb} MiB configured`);
  text('uptime-value', uptime(server.uptime_seconds));
  const signature = JSON.stringify([config.id, server.status, server.installed, state.busy]);
  if (signature !== lifecycleSignature) {
    lifecycleSignature = signature;
    const actions = $('lifecycle-actions'); actions.replaceChildren();
    if (ACTIVE.has(server.status) && server.status !== 'Installing') {
      const restart = lifecycleButton('Restart', { RestartServer: config.id }); restart.disabled ||= server.status !== 'Running';
      const stop = lifecycleButton('Stop', { StopServer: config.id }, 'danger'); stop.disabled ||= !['Running', 'Starting'].includes(server.status);
      actions.append(restart, stop);
    } else if (server.installed) {
      const start = lifecycleButton('Start', { StartServer: config.id }, 'primary'); start.disabled ||= server.status === 'Installing'; actions.append(start);
    } else {
      const install = lifecycleButton(server.status === 'Installing' ? 'Installing…' : 'Install', { InstallServer: config.id }, 'primary');
      install.disabled ||= server.status === 'Installing'; actions.append(install);
    }
  }
  $('server-last-error').hidden = !server.last_error;
  text('server-last-error', server.last_error || '');
  renderNetworkStatus(server);
  fileManager.setServer(server);
  updateEnabled();
  if (!state.logs.length) showConsoleEmpty(server);
}
function showConsoleEmpty(server) {
  if (state.logs.length) return;
  const message = !server.installed ? 'Install server software before starting this server.' : ACTIVE.has(server.status) ? 'Waiting for server output…' : 'Start the server to see its output here.';
  const existing = $('console-output').querySelector('.console-empty');
  if (existing) existing.textContent = message;
  else $('console-output').replaceChildren(element('p', 'console-empty muted', message));
}
function appendLogs(entries) {
  if (!entries.length) return;
  const output = $('console-output');
  output.querySelector('.console-empty')?.remove();
  const fragment = document.createDocumentFragment();
  for (const entry of entries) {
    if (entry.seq <= state.cursor) continue;
    state.cursor = entry.seq;
    state.logs.push(entry);
    const kind = entry.stream === 'error' || entry.line.includes('ERROR') || entry.line.includes('Exception') ? 'error' : entry.line.includes('WARN') ? 'warning' : entry.stream === 'command' ? 'command' : '';
    const line = element('div', `log-line ${kind}`);
    const date = new Date(entry.timestamp_ms);
    const timestamp = Number.isFinite(date.getTime()) ? date.toISOString().slice(11, 19) : '—';
    line.append(element('span', 'log-time', timestamp), element('span', 'log-text', entry.line));
    fragment.append(line);
  }
  output.append(fragment);
  let removedHeight = 0;
  while (state.logs.length > 1500) { state.logs.shift(); const first = output.firstElementChild; if (first) { removedHeight += first.getBoundingClientRect().height; first.remove(); } }
  if ($('follow-output').checked) output.scrollTop = output.scrollHeight;
  else if (removedHeight) output.scrollTop = Math.max(0, output.scrollTop - removedHeight);
}

async function poll() {
  if (!state.authenticated || state.polling) return;
  state.polling = true;
  try {
    const result = await action('Snapshot');
    const snapshot = result?.Snapshot;
    if (!snapshot || !Array.isArray(snapshot.servers)) throw new Error('Dawg returned an invalid server list. Reload after updating the application.');
    state.snapshot = snapshot;
    $('connection-error').hidden = true;
    if (state.selected && !selectedServer()) {
      state.selected = null;
      state.settingsDirty = state.networkDirty = state.propertiesDirty = false;
    }
    if (!state.selected && snapshot.servers.length && state.page !== 'create') {
      let preferred;
      try { preferred = localStorage.getItem('dawg.selectedServer'); } catch { /* Storage can be disabled. */ }
      switchServer(snapshot.servers.find(server => server.config.id === preferred)?.config.id || snapshot.servers[0].config.id, false);
    } else if (!snapshot.servers.length && state.page !== 'create') showPage('overview');
    renderSidebar(); renderOverview(); renderServer();
    if (state.selected) {
      const id = state.selected;
      if (lastLogServer !== id) { state.logs = []; state.cursor = 0; lastLogServer = id; $('console-output').replaceChildren(); }
      for (let batch = 0; batch < 8; batch++) {
        const response = await action({ Logs: { id, after: state.cursor } });
        if (id !== state.selected || !state.authenticated) break;
        const entries = response?.Logs?.entries;
        if (!Array.isArray(entries)) throw new Error('Dawg returned invalid console output.');
        appendLogs(entries);
        if (entries.length < 64) break;
      }
      if (!state.logs.length && selectedServer()) showConsoleEmpty(selectedServer());
    }
  } catch (error) {
    if (state.authenticated) showConnectionError(error.message);
  } finally { state.polling = false; updateEnabled(); }
}

async function mutate(value, options = {}) {
  if (state.busy || !state.authenticated) return null;
  state.busy = true; updateEnabled(); hideNotification();
  try {
    const response = await action(value);
    if (response?.Server && state.snapshot) {
      const index = state.snapshot.servers.findIndex(server => server.config.id === response.Server.config.id);
      if (index < 0) state.snapshot.servers.push(response.Server); else state.snapshot.servers[index] = response.Server;
    }
    if (response?.BackupCreated) notify(`Backup created on this host: ${response.BackupCreated.path}`);
    else if (options.message) notify(options.message);
    return response;
  } catch (error) { if (state.authenticated) notify(error.message, true); return null; }
  finally { state.busy = false; lifecycleSignature = ''; renderServer(); void poll(); }
}

function populateSettings() {
  const server = selectedServer(); if (!server) return;
  const form = $('settings-form'); const config = server.config;
  form.elements.name.value = config.name;
  form.elements.memory.value = config.memory_mb;
  form.elements.port.value = config.port;
  form.elements.java.value = config.java_path;
  form.elements.directory.value = config.directory;
  state.settingsDirty = false; $('settings-dirty').hidden = true;
}
function networkMode(config) { return config?.tunnel_enabled ? 'tunnel' : config?.upnp_enabled ? 'upnp' : 'manual'; }
function populateNetwork() {
  const server = selectedServer(); if (!server) return;
  const form = $('network-form'); const config = server.config.network || {};
  form.elements.mode.value = networkMode(config);
  form.elements.host.value = config.public_host || '';
  form.elements.external_port.value = config.external_port || '';
  state.networkDirty = false; $('network-dirty').hidden = true;
  updateNetworkModeHelp(); updateEnabled();
}
function updateNetworkModeHelp() {
  const mode = $('network-form').elements.mode.value;
  text('network-mode-help', mode === 'tunnel'
    ? 'Playit provides a persistent join address without buying a domain. Link the provider account when prompted. The tunnel also works behind CGNAT.'
    : mode === 'upnp'
      ? 'Dawg asks your router to forward the same public TCP port and restores it after restarts. UPnP needs router support and cannot bypass upstream NAT.'
      : 'Use a public VPS address, or configure your router and DNS/DDNS separately. A saved IP address does not make an ISP address permanent.');
}
function renderNetworkStatus(server) {
  const network = server.network || {};
  text('network-join', network.join_address || 'Not configured');
  text('network-lan', network.lan_address || 'Not detected');
  text('network-external', network.external_ip || 'Not detected');
  const labels = { Disabled: 'Disabled', Pending: 'Setting up', Active: networkMode(server.config.network) === 'tunnel' ? 'Tunnel connected' : 'Port mapping active', Unavailable: 'Unavailable', Error: 'Needs attention', NeedsSetup: 'Setup required' };
  text('network-forwarding', labels[network.forwarding] || network.forwarding || 'Disabled');
  text('network-detail', network.detail || 'Public reachability has not been verified from outside this network.');
  const setup = $('network-setup'); setup.hidden = true;
  if (network.setup_url) {
    try { const url = new URL(network.setup_url); if (url.protocol === 'https:') { setup.href = url.href; setup.hidden = false; } } catch { /* Invalid setup links remain hidden. */ }
  }
}
function updateEnabled() {
  const server = selectedServer(); const canEdit = editable(server);
  const running = !!server && server.status === 'Running' && !state.busy && state.authenticated;
  $('console-command').disabled = !running;
  $('console-command').placeholder = running ? 'Command, e.g. list or say Hello' : 'Start the server to send commands';
  $('send-command').disabled = !running || !$('console-command').value.trim();
  for (const control of $('settings-form').elements) if (control.name !== 'directory') control.disabled = !canEdit;
  $('save-settings').disabled = !canEdit || !$('settings-form').checkValidity();
  $('settings-dirty').hidden = !state.settingsDirty;
  text('settings-readonly', canEdit ? 'Changes apply the next time the server starts.' : 'Stop the server before editing its settings.');
  $('backup-server').disabled = !canEdit || !server?.installed;
  $('reinstall-server').disabled = !canEdit;
  $('remove-server').disabled = !canEdit;
  $('properties-editor').readOnly = !canEdit || !state.propertiesLoaded;
  $('save-properties').disabled = !canEdit || !state.propertiesLoaded || !state.propertiesDirty;
  $('reload-properties').disabled = state.busy || state.propertiesDirty || !server;
  $('properties-dirty').hidden = !state.propertiesDirty;
  text('properties-help', canEdit ? 'Changes take effect the next time the server starts.' : 'Read only. Stop the server to edit its properties.');
  const networkForm = $('network-form');
  for (const control of networkForm.elements) control.disabled = !canEdit;
  const tunnel = networkForm.elements.mode.value === 'tunnel';
  networkForm.elements.host.disabled = !canEdit || tunnel;
  networkForm.elements.external_port.disabled = !canEdit || tunnel;
  $('save-network').disabled = !canEdit || !networkForm.checkValidity();
  $('network-dirty').hidden = !state.networkDirty;
  text('network-readonly', canEdit ? 'Save your connection method, then start the server to apply it.' : 'Stop the server before changing public access.');
  $('new-server').disabled = state.busy || !state.authenticated;
  $('first-server').disabled = state.busy || !state.authenticated;
}
async function loadProperties() {
  const id = state.selected; if (!id) return;
  try {
    const result = await action({ GetProperties: id });
    if (id !== state.selected || !result?.Properties) return;
    $('properties-editor').value = result.Properties.content;
    state.propertiesLoaded = true; state.propertiesDirty = false; updateEnabled();
  } catch (error) { notify(error.message, true); }
}
function openCreate() {
  if (!leaveCurrent()) return;
  $('create-form').reset();
  updateJarImport();
  const ports = new Set((state.snapshot?.servers || []).map(server => server.config.port));
  let port = 25565; while (ports.has(port) && port < 65535) port++;
  $('create-form').elements.port.value = port;
  hideNotification(); showPage('create'); validateCreate();
  $('create-form').elements.name.focus();
  void loadVersionCatalog();
}
function createVersion() {
  const form = $('create-form');
  return form.elements.version.value === 'custom' ? form.elements.manual_version.value.trim() : form.elements.version.value;
}
function updateJarImport() {
  const form = $('create-form');
  const forge = form.elements.kind.value === 'Forge';
  form.elements.jar.disabled = forge;
  if (forge) form.elements.jar.value = '';
  text('jar-help', forge ? 'Forge uses its official installer and libraries. After installation, upload mods using Files.' : 'Choose a JAR from this computer, or leave empty and install official server software after creating.');
}
function updateManualVersion() {
  const form = $('create-form');
  const manual = form.elements.version.value === 'custom';
  $('manual-version-field').hidden = !manual;
  form.elements.manual_version.disabled = !manual;
  form.elements.manual_version.required = manual;
}
function renderVersions() {
  const form = $('create-form');
  const catalog = versionCatalogs.get(form.elements.kind.value);
  const previous = form.elements.version.value;
  const channel = form.elements.channel.value;
  const versions = catalog?.versions?.filter(version => channel === 'All' || version.channel === channel) || [];
  const options = versions.map(version => {
    const option = element('option', '', channel === 'All' ? `${version.id} (${version.channel.toLowerCase()})` : version.id);
    option.value = version.id;
    return option;
  });
  const custom = element('option', '', 'Enter a version manually…'); custom.value = 'custom'; options.push(custom);
  form.elements.version.replaceChildren(...options);
  const latest = channel === 'Snapshot' ? catalog?.latest_snapshot : catalog?.latest_release;
  form.elements.version.value = versions.some(version => version.id === previous) || previous === 'custom' ? previous : versions.some(version => version.id === latest) ? latest : versions[0]?.id || 'custom';
  form.elements.version.disabled = versionsLoading;
  text('version-status', catalog ? versions.length ? `${versions.length} ${channel === 'All' ? '' : `${channel.toLowerCase()} `}versions available from the official ${catalog.kind} catalog.` : `No ${channel.toLowerCase()} versions are available for ${catalog.kind}. Choose another channel or enter an exact version.` : `${versionError ? `${versionError} ` : ''}Retry the official list, or enter an exact version manually.`);
  updateManualVersion(); validateCreate();
}
async function loadVersionCatalog(force = false) {
  const kind = $('create-form').elements.kind.value;
  const sequence = ++versionSequence;
  if (versionCatalogs.has(kind) && !force) { versionsLoading = false; $('reload-versions').hidden = true; renderVersions(); return; }
  versionsLoading = true; versionError = ''; $('create-form').elements.version.disabled = true;
  text('version-status', `Loading official ${kind} versions…`); $('reload-versions').hidden = true; validateCreate();
  try {
    const result = await action({ VersionCatalog: { kind } });
    if (sequence !== versionSequence) return;
    const catalog = result?.VersionCatalog;
    if (!catalog || catalog.kind !== kind || !Array.isArray(catalog.versions)) throw new Error('The version catalog response was invalid.');
    versionCatalogs.set(kind, catalog);
  } catch (error) {
    if (sequence !== versionSequence || !state.authenticated) return;
    versionError = error.message;
    $('reload-versions').hidden = false;
  } finally {
    if (sequence === versionSequence) { versionsLoading = false; renderVersions(); }
  }
}
function validateCreate() {
  const form = $('create-form');
  let reason = '';
  if (!form.elements.name.value.trim()) reason = 'Give your server a name.';
  else if (versionsLoading) reason = 'Wait for the version list to load.';
  else if (!createVersion() || !form.checkValidity()) reason = !form.elements.eula.checked ? 'Read and accept the Minecraft EULA before creating a server.' : 'Check the Minecraft version, memory, port and Java executable.';
  else if (form.elements.jar.files[0]?.size > 1073741824) reason = 'Choose a JAR smaller than 1 GiB.';
  text('create-validation', reason);
  $('create-submit').disabled = !!reason || state.busy;
}

$('login-form').addEventListener('submit', async event => {
  event.preventDefault(); $('login-submit').disabled = true; $('login-error').hidden = true;
  try {
    await jsonRequest('/api/login', 'POST', { username: $('login-username').value, password: $('login-password').value }, { allowUnauthorized: true });
    $('login-password').value = '';
    await activate();
  } catch (error) { text('login-error', error.message); $('login-error').hidden = false; }
  finally { $('login-submit').disabled = false; }
});
$('logout').addEventListener('click', async () => {
  if (!leaveCurrent()) return;
  try { await jsonRequest('/api/logout', 'POST', {}); showLogin(); }
  catch (error) { notify(error.message, true); }
});
$('brand-home').addEventListener('click', event => { event.preventDefault(); if (state.selected) switchServer(state.selected); else if (leaveCurrent()) showPage('overview'); });
$('all-servers').addEventListener('click', () => { if (leaveCurrent()) { showPage('overview'); renderOverview(); } });
$('new-server').addEventListener('click', openCreate);
$('first-server').addEventListener('click', openCreate);
$('cancel-create').addEventListener('click', () => { if (state.selected) { showPage('server'); setTab('console', false); } else showPage('overview'); });
$('create-form').addEventListener('input', validateCreate);
$('create-form').addEventListener('change', validateCreate);
$('create-form').elements.kind.addEventListener('change', () => { $('create-form').elements.version.value = ''; updateJarImport(); void loadVersionCatalog(); });
$('create-form').elements.channel.addEventListener('change', () => { $('create-form').elements.version.value = ''; renderVersions(); });
$('create-form').elements.version.addEventListener('change', () => { updateManualVersion(); validateCreate(); });
$('reload-versions').addEventListener('click', () => void loadVersionCatalog(true));
$('create-form').addEventListener('submit', async event => {
  event.preventDefault(); validateCreate(); if ($('create-submit').disabled) return;
  const form = $('create-form'); const jar = form.elements.jar.files[0];
  state.busy = true; validateCreate(); text('create-submit', jar ? 'Creating and uploading…' : 'Creating…');
  try {
    const result = await action({ CreateServer: { name: form.elements.name.value.trim(), kind: form.elements.kind.value, minecraft_version: createVersion(), memory_mb: Number(form.elements.memory.value), port: Number(form.elements.port.value), java_path: form.elements.java.value.trim(), eula_accepted: form.elements.eula.checked, import_jar: null, network: { public_host: null, external_port: null, upnp_enabled: false, tunnel_enabled: false } } });
    if (!result?.Server) throw new Error('The server was not created. Check the manager log.');
    if (state.snapshot) state.snapshot.servers.push(result.Server);
    switchServer(result.Server.config.id, false);
    if (jar) {
      await request(`/api/servers/${encodeURIComponent(result.Server.config.id)}/files/content?path=server.jar`, { method: 'PUT', headers: { 'Content-Type': 'application/octet-stream' }, body: jar, timeout: 0 });
      notify('Server JAR uploaded. Start the server when you are ready.');
    } else notify('Server created. Choose Install to download its software.');
  } catch (error) { notify(error.message, true); }
  finally { state.busy = false; text('create-submit', 'Create server'); validateCreate(); updateEnabled(); void poll(); }
});
$('workspace-tabs').addEventListener('click', event => { const button = event.target.closest('[data-tab]'); if (button) setTab(button.dataset.tab); });
$('command-form').addEventListener('submit', async event => {
  event.preventDefault(); const server = selectedServer(); const input = $('console-command'); const command = input.value.trim();
  if (!server || !command || state.busy || server.status !== 'Running') return;
  const response = await mutate({ SendCommand: { id: server.config.id, command } });
  if (response !== null) input.value = '';
  updateEnabled(); input.focus();
});
$('console-command').addEventListener('input', updateEnabled);
$('follow-output').addEventListener('change', () => { if ($('follow-output').checked) $('console-output').scrollTop = $('console-output').scrollHeight; });
$('settings-form').addEventListener('input', () => { state.settingsDirty = true; updateEnabled(); });
$('settings-form').addEventListener('submit', async event => {
  event.preventDefault(); const server = selectedServer(); if (!editable(server)) return;
  const form = $('settings-form');
  const config = { ...server.config, name: form.elements.name.value.trim(), memory_mb: Number(form.elements.memory.value), port: Number(form.elements.port.value), java_path: form.elements.java.value.trim() };
  const response = await mutate({ UpdateServer: config }, { message: 'Server settings saved.' });
  if (response) { state.settingsDirty = false; populateSettings(); updateEnabled(); }
});
$('backup-server').addEventListener('click', () => { const server = selectedServer(); if (server) void mutate({ BackupServer: server.config.id }); });
$('reinstall-server').addEventListener('click', () => { const server = selectedServer(); if (server && window.confirm(`Replace ${server.config.name}'s server JAR with an official ${server.config.kind} download? World files stay in place. Back up first.`)) void mutate({ InstallServer: server.config.id }); });
$('remove-server').addEventListener('click', async () => {
  const server = selectedServer(); if (!server || !window.confirm(`Remove “${server.config.name}” from Dawg? Its files and backups stay on disk.`)) return;
  const response = await mutate({ RemoveServer: server.config.id });
  if (response) { state.selected = null; state.settingsDirty = state.networkDirty = state.propertiesDirty = false; state.snapshot.servers = state.snapshot.servers.filter(item => item.config.id !== server.config.id); if (state.snapshot.servers.length) switchServer(state.snapshot.servers[0].config.id, false); else showPage('overview'); notify('Server removed. Its files stay on disk.'); }
});
$('properties-editor').addEventListener('input', () => { state.propertiesDirty = true; updateEnabled(); });
$('reload-properties').addEventListener('click', () => void loadProperties());
$('save-properties').addEventListener('click', async () => {
  const server = selectedServer(); if (!editable(server) || !state.propertiesDirty) return;
  const response = await mutate({ SaveProperties: { id: server.config.id, content: $('properties-editor').value } }, { message: 'server.properties saved. Changes apply on the next start.' });
  if (response !== null) { state.propertiesDirty = false; updateEnabled(); }
});
$('network-form').addEventListener('input', () => { state.networkDirty = true; updateNetworkModeHelp(); updateEnabled(); });
$('network-form').addEventListener('change', () => { state.networkDirty = true; updateNetworkModeHelp(); updateEnabled(); });
$('network-form').addEventListener('submit', async event => {
  event.preventDefault(); const server = selectedServer(); if (!editable(server)) return;
  const form = $('network-form'); const mode = form.elements.mode.value;
  const network = { ...(server.config.network || {}), public_host: mode === 'tunnel' ? null : form.elements.host.value.trim() || null, external_port: mode === 'tunnel' ? null : Number(form.elements.external_port.value) || null, upnp_enabled: mode === 'upnp', tunnel_enabled: mode === 'tunnel' };
  const response = await mutate({ UpdateServer: { ...server.config, network } }, { message: 'Network settings saved.' });
  if (response) { state.networkDirty = false; populateNetwork(); updateEnabled(); }
});
$('copy-address').addEventListener('click', async () => {
  const address = publicAddress(selectedServer()); if (!address) return;
  try {
    if (navigator.clipboard?.writeText) await navigator.clipboard.writeText(address);
    else { const field = element('textarea'); field.value = address; field.style.position = 'fixed'; field.style.opacity = '0'; document.body.append(field); field.select(); const copied = document.execCommand('copy'); field.remove(); if (!copied) throw new Error('Clipboard unavailable'); }
    notify('Join address copied.');
  } catch { notify('Clipboard access is unavailable. Select the join address to copy it.', true); }
});
$('dismiss-notification').addEventListener('click', hideNotification);
$('reconnect').addEventListener('click', () => { state.cursor = 0; state.logs = []; lastLogServer = null; void poll(); });
$('open-sidebar').addEventListener('click', () => { $('sidebar').classList.add('open'); $('sidebar-shade').hidden = false; $('open-sidebar').setAttribute('aria-expanded', 'true'); });
$('close-sidebar').addEventListener('click', closeSidebar);
$('sidebar-shade').addEventListener('click', closeSidebar);
document.addEventListener('keydown', event => { if (event.key === 'Escape') closeSidebar(); });
window.addEventListener('beforeunload', event => { if (dirtyForms() || fileManager.hasUnsavedChanges?.()) { event.preventDefault(); event.returnValue = ''; } });

async function activate() {
  state.authenticated = true; state.page = 'overview'; state.selected = null; state.sidebarSignature = ''; lifecycleSignature = '';
  $('login-panel').hidden = true; $('app-shell').hidden = false; $('connection-error').hidden = true;
  showPage('overview'); await poll();
}
async function boot() {
  try {
    const session = await (await request('/api/session', { allowUnauthorized: true })).json();
    if (session.authenticated) await activate(); else showLogin();
  } catch (error) { showLogin(error.message); }
}
setInterval(() => { void poll(); }, 1000);
void boot();
