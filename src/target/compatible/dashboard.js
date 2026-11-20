// Uses the dashboard's existing admin session, modal, card, and action controls.
let qwenCloudLogin = null;
let qwenCloudConnectGeneration = 0;
function ensureCompatibleModal() {
  if (document.getElementById('compatibleModal')) return;
  const modal = document.createElement('div');
  modal.id = 'compatibleModal';
  modal.className = 'modal';
  modal.setAttribute('role', 'dialog');
  modal.setAttribute('aria-modal', 'true');
  modal.setAttribute('aria-labelledby', 'compatibleTitle');
  modal.setAttribute('aria-hidden', 'true');
  modal.style.display = 'none';
  modal.innerHTML = `<div class="modal-card">
    <h2 id="compatibleTitle" style="margin-top:0">Add compatible API</h2>
    <p class="muted">Connect a service that accepts OpenAI or Anthropic requests. Use its upstream model IDs here; clients select them with <code>cmp:</code>.</p>
    <form id="compatibleForm">
      <label for="compatibleService">Account type</label>
      <select id="compatibleService"><option value="custom">Custom compatible API</option><option value="qwen_token_plan">Qwen Cloud Token Plan</option><option value="qwen_payg">Qwen Cloud Pay-as-you-go</option></select>
      <div id="compatibleServiceHint" class="muted" style="margin:6px 0 12px" hidden></div>
      <label for="compatibleName">Name</label>
      <input id="compatibleName" required autocomplete="off" placeholder="e.g. Qwen Coding Plan">
      <label for="compatibleFormat" style="margin-top:12px">API format</label>
      <select id="compatibleFormat"><option value="openai">OpenAI compatible</option><option value="anthropic">Anthropic compatible</option><option value="both">Both formats</option></select>
      <div id="compatibleOpenaiGroup"><label for="compatibleOpenai" style="margin-top:12px">OpenAI base URL</label><input id="compatibleOpenai" type="url" placeholder="https://example.com/v1"></div>
      <div id="compatibleAnthropicGroup"><label for="compatibleAnthropic" style="margin-top:12px">Anthropic base URL</label><input id="compatibleAnthropic" type="url" placeholder="https://example.com/anthropic"></div>
      <label for="compatibleKey" style="margin-top:12px">API key</label>
      <input id="compatibleKey" type="password" autocomplete="new-password" spellcheck="false" placeholder="Enter the service API key">
      <div id="compatibleKeyHelp" class="muted" style="margin-top:6px">The key is stored privately and never shown in account details.</div>
      <div id="compatibleConsoleGroup" hidden>
        <div style="border-top:1px solid var(--border);margin-top:18px;padding-top:14px">
          <strong>Qwen Cloud account</strong>
          <p class="muted" style="margin:6px 0">Optional to show quota. Your API key works without connecting.</p>
          <p id="qwenCloudConnectionStatus" class="muted" role="status" aria-live="polite"></p>
          <div class="modal-actions"><button id="qwenCloudConnect" type="button" class="secondary-button">Connect Qwen Cloud account</button><button id="qwenCloudDisconnect" type="button" class="secondary-button" hidden>Disconnect</button></div>
          <p class="muted">Connecting saves this API configuration first. Sign in to the Qwen Cloud account that owns this API key.</p>
          <a id="qwenCloudVerification" target="_blank" rel="noopener noreferrer" hidden>Open Qwen Cloud sign-in ↗</a>
          <button id="qwenCloudCancel" type="button" class="secondary-button" hidden>Cancel sign-in</button>
        </div>
        <details style="margin-top:12px"><summary class="muted">Advanced: console cookie</summary>
        <label for="compatibleConsoleCookie" style="margin-top:12px">Console Cookie (optional alternative)</label>
        <input id="compatibleConsoleCookie" type="password" autocomplete="new-password" spellcheck="false">
        <p class="muted">For credits and limits, log in at <a href="https://home.qwencloud.com" target="_blank" rel="noopener noreferrer">Qwen Cloud</a>, open browser Developer Tools → Network, and copy a request’s Cookie header. The inference API key cannot read billing. Use the console account that owns this API key. Blank keeps the saved session.</p>
        <label><input id="compatibleClearCookie" type="checkbox"> Remove saved console session</label>
        <p id="compatibleCookieStatus" class="muted"></p>
        </details>
      </div>
      <label for="compatibleModels" style="margin-top:12px">Model IDs</label>
      <textarea id="compatibleModels" rows="3" required placeholder="qwen3.7-plus, qwen3.6-flash" aria-describedby="compatibleModelsHelp"></textarea>
      <div id="compatibleModelsHelp" class="muted">Separate IDs with commas or new lines, or fetch the service's model list.</div>
      <div class="modal-actions" style="margin-top:12px"><button id="compatibleDiscover" type="button" class="secondary-button">Fetch models</button></div>
      <p id="compatibleStatus" class="muted" role="status" aria-live="polite"></p>
      <div class="modal-actions"><button id="compatibleSave" type="submit">Save account</button><button id="compatibleClose" type="button" class="secondary-button">Cancel</button></div>
    </form></div>`;
  document.body.appendChild(modal);
  if (!modalIds.includes('compatibleModal')) modalIds.push('compatibleModal');
  document.getElementById('compatibleFormat').addEventListener('change', updateCompatibleFormat);
  document.getElementById('compatibleService').addEventListener('change', () => updateCompatibleService(true));
  for (const id of ['compatibleOpenai', 'compatibleAnthropic']) document.getElementById(id).addEventListener('change', detectCompatibleService);
  document.getElementById('compatibleKey').addEventListener('input', detectQwenServiceFromKey);
  document.getElementById('compatibleClose').addEventListener('click', closeCompatibleModal);
  document.getElementById('compatibleForm').addEventListener('submit', saveCompatibleAccount);
  document.getElementById('compatibleDiscover').addEventListener('click', discoverCompatibleModels);
  document.getElementById('qwenCloudConnect').addEventListener('click', connectQwenCloudAccount);
  document.getElementById('qwenCloudDisconnect').addEventListener('click', disconnectQwenCloudAccount);
  document.getElementById('qwenCloudCancel').addEventListener('click', () => {
    cancelQwenCloudSignIn();
    document.getElementById('qwenCloudConnectionStatus').textContent = 'Sign-in cancelled. Optional to show quota.';
  });
}
function isQwenCloudAccount(account) {
  return account && ['qwen_token_plan', 'qwen_payg'].includes(account.service);
}
// A Qwen Cloud API key is Token Plan when it starts with sk-sp-; every other
// Qwen Cloud key (sk-ws- or the older sk-) is pay-as-you-go. The URL host is
// checked first because the host decides which endpoint a key can reach.
function detectQwenServiceFromKey() {
  const select = document.getElementById('compatibleService');
  const key = document.getElementById('compatibleKey').value.trim();
  if (select.value === 'custom' || !key) return;
  const service = key.startsWith('sk-sp-') ? 'qwen_token_plan' : 'qwen_payg';
  if (service === select.value) return;
  select.value = service;
  updateCompatibleService(true);
}
function detectCompatibleService() {
  for (const id of ['compatibleOpenai', 'compatibleAnthropic']) {
    let host;
    try { host = new URL(document.getElementById(id).value).hostname; } catch (_) { continue; }
    const service = host === 'token-plan.maas.qwencloudapi.com' ? 'qwen_token_plan'
      : host === 'maas.qwencloudapi.com' || host.endsWith('.maas.qwencloudapi.com') ? 'qwen_payg' : null;
    if (!service) continue;
    document.getElementById('compatibleService').value = service;
    const openai = document.getElementById('compatibleOpenai');
    const anthropic = document.getElementById('compatibleAnthropic');
    if (!openai.value) openai.value = 'https://' + host + '/compatible-mode/v1';
    if (!anthropic.value) anthropic.value = 'https://' + host + '/apps/anthropic';
    updateCompatibleService(false);
    return;
  }
}
function updateCompatibleService(preset) {
  if (preset) cancelQwenCloudSignIn();
  const service = document.getElementById('compatibleService').value;
  document.getElementById('compatibleConsoleGroup').hidden = service === 'custom';
  const label = service === 'qwen_token_plan' ? 'Qwen Cloud Token Plan' : service === 'qwen_payg' ? 'Qwen Cloud Pay-as-you-go' : 'compatible API';
  const isNew = !document.getElementById('compatibleForm').dataset.fileName;
  document.getElementById('compatibleTitle').textContent = (isNew ? 'Add ' : 'Edit ') + (isNew && service !== 'custom' ? 'Qwen Cloud' : label);
  const hint = document.getElementById('compatibleServiceHint');
  hint.hidden = service === 'custom';
  hint.textContent = service === 'custom' ? '' : 'Detected from your API key: sk-sp- keys are Token Plan, other keys are Pay-as-you-go. Change the account type to override.'
    + (service === 'qwen_payg' ? ' Pay-as-you-go keys only work in the region they were created in; Singapore is filled in, so edit the URLs for another region.' : '');
  if (preset && service !== 'custom') {
    const host = service === 'qwen_token_plan' ? 'token-plan' : 'ap-southeast-1';
    document.getElementById('compatibleOpenai').value = 'https://' + host + '.maas.qwencloudapi.com/compatible-mode/v1';
    document.getElementById('compatibleAnthropic').value = 'https://' + host + '.maas.qwencloudapi.com/apps/anthropic';
    document.getElementById('compatibleFormat').value = 'both';
    const name = document.getElementById('compatibleName');
    if (!name.value || ['Qwen Cloud Token Plan', 'Qwen Cloud Pay-as-you-go'].includes(name.value)) name.value = label;
    const models = document.getElementById('compatibleModels');
    if (service === 'qwen_token_plan' && !models.value) models.value = 'auto';
    if (service === 'qwen_payg' && models.value.trim() === 'auto') models.value = '';
    updateCompatibleFormat();
  }
}
function updateCompatibleFormat() {
  const format = document.getElementById('compatibleFormat').value;
  for (const [id, enabled] of [['Openai', format !== 'anthropic'], ['Anthropic', format !== 'openai']]) {
    document.getElementById('compatible' + id + 'Group').hidden = !enabled;
    document.getElementById('compatible' + id).required = enabled;
    document.getElementById('compatible' + id).disabled = !enabled;
  }
}
function openCompatibleAccount(fileName, service) {
  ensureCompatibleModal();
  cancelQwenCloudSignIn();
  const account = fileName && dashboardState.providers.compatible.find(a => a.file_name === fileName);
  if (fileName && !account) return;
  const form = document.getElementById('compatibleForm');
  form.reset();
  form.dataset.fileName = account ? account.file_name : '';
  document.getElementById('compatibleTitle').textContent = account ? 'Edit compatible API' : 'Add compatible API';
  document.getElementById('compatibleName').value = account ? account.label : '';
  document.getElementById('compatibleService').value = account && account.service || service || 'custom';
  document.getElementById('compatibleFormat').value = account ? (account.base_url && account.anthropic_base_url ? 'both' : account.base_url ? 'openai' : 'anthropic') : 'openai';
  document.getElementById('compatibleOpenai').value = account && account.base_url || '';
  document.getElementById('compatibleAnthropic').value = account && account.anthropic_base_url || '';
  document.getElementById('compatibleModels').value = account ? account.models.join('\n') : '';
  document.getElementById('compatibleKey').required = !account;
  document.getElementById('compatibleKey').placeholder = account ? 'Leave blank to keep the saved key' : 'Enter the service API key';
  document.getElementById('compatibleStatus').textContent = '';
  document.getElementById('compatibleCookieStatus').textContent = account && account.has_console_cookie ? 'A console session is saved. Replace it here when it expires.' : 'No console session saved. Credits and limits will be unavailable until connected.';
  renderQwenCloudConnection(account);
  detectCompatibleService();
  updateCompatibleService(!account && !!service);
  updateCompatibleFormat();
  openModal('compatibleModal');
  document.getElementById('compatibleName').focus();
}
function closeCompatibleModal() {
  cancelQwenCloudSignIn();
  document.getElementById('compatibleKey').value = '';
  document.getElementById('compatibleConsoleCookie').value = '';
  closeModal('compatibleModal');
}
function renderQwenCloudConnection(account) {
  const connection = account && account.quota_connection || {};
  const connected = connection.status === 'connected';
  const expired = connection.status === 'expired';
  const expiry = connection.expires_at && new Date(connection.expires_at);
  const expiryLabel = expiry && Number.isFinite(expiry.getTime()) ? ' Sign-in expires ' + expiry.toLocaleString() + '.' : '';
  document.getElementById('qwenCloudConnectionStatus').textContent = connected
    ? 'Connected' + (connection.email ? ' as ' + connection.email : '') + '. Quota refreshes automatically.' + expiryLabel
    : expired ? 'Sign-in expired. Reconnect to show quota.' : connection.status === 'cookie' ? 'A console cookie is saved for quota.' : 'No account connected. Optional to show quota.';
  document.getElementById('qwenCloudConnect').textContent = connected || expired ? 'Reconnect Qwen Cloud account' : 'Connect Qwen Cloud account';
  document.getElementById('qwenCloudConnect').disabled = false;
  document.getElementById('qwenCloudDisconnect').hidden = !connected && !expired && connection.status !== 'cookie';
}
function cancelQwenCloudSignIn() {
  qwenCloudConnectGeneration++;
  if (qwenCloudLogin) {
    clearTimeout(qwenCloudLogin.timer);
    adminFetch('/compatible/qwen/cancel', {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify({login_id:qwenCloudLogin.id})}).catch(() => {});
  }
  qwenCloudLogin = null;
  for (const id of ['qwenCloudVerification', 'qwenCloudCancel']) {
    const element = document.getElementById(id);
    if (element) element.hidden = true;
  }
  const button = document.getElementById('qwenCloudConnect');
  if (button) button.disabled = false;
}
async function qwenCloudRequest(path, body) {
  const response = await adminFetch('/compatible/qwen/' + path, {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(body)});
  if (!response) throw new Error('Administrator login required.');
  const data = await response.json();
  if (!response.ok || !data.ok) throw new Error(data.message || 'Could not connect Qwen Cloud account.');
  return data;
}
async function connectQwenCloudAccount() {
  const form = document.getElementById('compatibleForm');
  if (!form.reportValidity() || document.getElementById('compatibleService').value === 'custom') return;
  cancelQwenCloudSignIn();
  const generation = qwenCloudConnectGeneration;
  const status = document.getElementById('qwenCloudConnectionStatus');
  const buttons = ['qwenCloudConnect', 'qwenCloudDisconnect', 'compatibleSave', 'compatibleDiscover'].map(id => document.getElementById(id));
  buttons.forEach(button => button.disabled = true);
  status.textContent = 'Saving API configuration and starting sign-in…';
  try {
    const response = await adminFetch('/compatible/save', {method:'POST', headers:{'Content-Type':'application/json'}, body:JSON.stringify(compatiblePayload())});
    if (!response) throw new Error('Administrator login required.');
    const saved = await response.json();
    if (!response.ok || !saved.ok) throw new Error(saved.message || 'Could not save API account.');
    if (generation !== qwenCloudConnectGeneration) return;
    form.dataset.fileName = saved.file_name;
    document.getElementById('compatibleKey').value = '';
    document.getElementById('compatibleKey').required = false;
    document.getElementById('compatibleConsoleCookie').value = '';
    document.getElementById('compatibleClearCookie').checked = false;
    const login = await qwenCloudRequest('connect', {file_name:saved.file_name});
    if (generation !== qwenCloudConnectGeneration) {
      await qwenCloudRequest('cancel', {login_id:login.login_id});
      return;
    }
    const url = new URL(login.verification_url);
    if (url.protocol !== 'https:' || !(url.hostname === 'qwencloud.com' || url.hostname.endsWith('.qwencloud.com'))) throw new Error('Invalid Qwen Cloud sign-in link.');
    const link = document.getElementById('qwenCloudVerification');
    link.href = url.href;
    link.hidden = false;
    document.getElementById('qwenCloudCancel').hidden = false;
    status.textContent = 'Open Qwen Cloud sign-in and approve access. Waiting for authorization…';
    qwenCloudLogin = {id:login.login_id, generation, deadline:Date.now() + login.expires_in * 1000};
    window.open(url.href, '_blank', 'noopener,noreferrer');
    scheduleQwenCloudPoll(login.interval);
  } catch (error) {
    if (generation === qwenCloudConnectGeneration) {
      cancelQwenCloudSignIn();
      status.textContent = error.message || 'Could not connect Qwen Cloud account.';
    }
  } finally {
    buttons.forEach(button => button.disabled = false);
    if (qwenCloudLogin) document.getElementById('qwenCloudConnect').disabled = true;
  }
}
function scheduleQwenCloudPoll(interval) {
  if (qwenCloudLogin) qwenCloudLogin.timer = setTimeout(pollQwenCloudConnection, Math.max(5, Number(interval) || 5) * 1000);
}
async function pollQwenCloudConnection() {
  const login = qwenCloudLogin;
  if (!login) return;
  try {
    if (Date.now() >= login.deadline) throw new Error('Sign-in expired. Connect Qwen Cloud account again.');
    const data = await qwenCloudRequest('status', {login_id:login.id});
    if (qwenCloudLogin !== login) return;
    if (data.status === 'connected') {
      qwenCloudLogin = null;
      cancelQwenCloudSignIn();
      renderQwenCloudConnection({quota_connection:{status:'connected', email:data.email, expires_at:data.expires_at}});
      await adminFetch('/compatible/quota.json');
      await refreshDashboardSnapshot(true);
      await refreshCompatibleAccounts();
    } else scheduleQwenCloudPoll(data.interval);
  } catch (error) {
    if (qwenCloudLogin !== login) return;
    cancelQwenCloudSignIn();
    document.getElementById('qwenCloudConnectionStatus').textContent = error.message || 'Could not check sign-in. Connect again.';
  }
}
async function disconnectQwenCloudAccount() {
  cancelQwenCloudSignIn();
  const button = document.getElementById('qwenCloudDisconnect');
  button.disabled = true;
  try {
    await qwenCloudRequest('disconnect', {file_name:document.getElementById('compatibleForm').dataset.fileName});
    document.getElementById('compatibleConsoleCookie').value = '';
    document.getElementById('compatibleClearCookie').checked = false;
    document.getElementById('compatibleCookieStatus').textContent = 'No console session saved.';
    renderQwenCloudConnection(null);
    await adminFetch('/compatible/quota.json');
    await refreshDashboardSnapshot(true);
    await refreshCompatibleAccounts();
  } catch (error) {
    document.getElementById('qwenCloudConnectionStatus').textContent = error.message || 'Could not disconnect.';
  } finally { button.disabled = false; }
}
function compatiblePayload() {
  const format = document.getElementById('compatibleFormat').value;
  return {
    file_name: document.getElementById('compatibleForm').dataset.fileName || null,
    label: document.getElementById('compatibleName').value.trim(),
    service: document.getElementById('compatibleService').value,
    console_cookie: document.getElementById('compatibleClearCookie').checked ? '' : document.getElementById('compatibleConsoleCookie').value.trim() || null,
    api_key: document.getElementById('compatibleKey').value.trim() || null,
    base_url: format === 'anthropic' ? null : document.getElementById('compatibleOpenai').value.trim(),
    anthropic_base_url: format === 'openai' ? null : document.getElementById('compatibleAnthropic').value.trim(),
    models: document.getElementById('compatibleModels').value.split(/[,\n]/).map(id => id.trim()).filter(Boolean)
  };
}
async function compatibleAction(path, pending, onSuccess) {
  const status = document.getElementById('compatibleStatus');
  const buttons = ['compatibleSave', 'compatibleDiscover', 'compatibleClose'].map(id => document.getElementById(id));
  buttons.forEach(button => button.disabled = true);
  status.textContent = pending;
  try {
    const response = await adminFetch(path, {method: 'POST', headers: {'Content-Type': 'application/json'}, body: JSON.stringify(compatiblePayload())});
    if (!response) throw new Error('Administrator login required.');
    const data = await response.json();
    if (!response.ok || !data.ok) throw new Error(data.message || 'Request failed.');
    await onSuccess(data);
  } catch (error) {
    status.textContent = error.message || 'Could not reach the gateway.';
  } finally {
    buttons.forEach(button => button.disabled = false);
  }
}
async function saveCompatibleAccount(event) {
  event.preventDefault();
  cancelQwenCloudSignIn();
  await compatibleAction('/compatible/save', 'Saving account…', async () => {
    closeCompatibleModal();
    await adminFetch('/compatible/quota.json?after_save=1');
    await refreshDashboardSnapshot(true);
    await refreshCompatibleAccounts();
    refreshCustomModels();
  });
}
async function discoverCompatibleModels() {
  await compatibleAction('/compatible/discover', 'Fetching models…', data => {
    document.getElementById('compatibleModels').value = data.models.join('\n');
    document.getElementById('compatibleStatus').textContent = data.models.length + ' models found. Review the list, then save.';
  });
}
function buildCompatibleCard(account) {
    const key = account.file_name || account.account_id || account.label;
    const quota = dashboardState.quotas.compatible.get(account.account_id);
    const credits = (quota && quota.credits) || {};
    const currency = credits.currency ? ' ' + credits.currency : '';
    const connection = account.quota_connection || {};
    const connectionLabel = connection.status === 'connected'
      ? 'Connected' + (connection.email ? ' as ' + connection.email : '')
      : connection.status === 'expired' ? 'Sign-in expired. Reconnect to show quota.' : '';
    const formats = [account.base_url && 'OpenAI', account.anthropic_base_url && 'Anthropic'].filter(Boolean).join(' + ');
    const pill = (value, label) => '<span class="stat-pill"><span class="stat-pill-value">' + value + '</span><span class="stat-pill-label">' + label + '</span></span>';
    // Same shape as the other provider cards: URLs live under Connection details and models carry their cmp: prefix.
    const shown = Object.assign({}, account, {base_url: '', openai_base_url: account.base_url, models: account.models.map(id => 'cmp:' + id)});
    const rows = connectionRows('compatible', shown, quota).concat([
      renderMetaLine('service', account.service_label, false),
      renderMetaLine('quota connection', connectionLabel, false),
      // Secondary amounts are noise at zero; the main balance above always shows.
      renderMetaLine('cash balance', credits.cash_balance != null && Number(credits.cash_balance) !== 0 ? credits.cash_balance + currency : '', false),
      renderMetaLine('credit limit', credits.credit_limit != null && Number(credits.credit_limit) !== 0 ? credits.credit_limit + currency : '', false),
      renderLongMetaLine('credit note', credits.note)
    ]).filter(Boolean);
    return '<div class="card">' + renderAccountHeader(account.label, 'compatible', account)
      + '<div class="stat-pills">'
      + '<span class="stat-pill"><span class="stat-pill-label">format</span><span class="stat-pill-value">' + escapeHtml(formats) + '</span></span>'
      + pill(account.requests || 0, 'req') + pill(account.errors || 0, 'err')
      + pill(account.input_tokens || 0, 'in tok') + pill(account.output_tokens || 0, 'out tok') + pill(account.total_tokens || 0, 'total tok')
      + '</div>'
      + (quota
        ? renderQuotaBars(quota, {provider:'compatible',key:key})
          + renderUsageCredits({credits: {balance: credits.balance, currency: credits.currency, estimated: credits.estimated, unlimited: credits.unlimited, has_credits: credits.has_credits}})
          + (quota.notes || []).map(note => '<p class="muted">' + escapeHtml(note) + '</p>').join('')
        : '')
      + renderAccountModels(shown, quota, 'compatible', key)
      + renderMetaDetails(rows, 'Connection details', 'compatible', account, key)
      + renderAttentionDetails('compatible', account, key)
      + '</div>';
}
async function refreshQwenGroup() {
  if ((dashboardState.providers.qwen || []).length || !(dashboardState.providers.compatible || []).some(isQwenCloudAccount)) await refreshModelCatalog('qwen');
  if ((dashboardState.providers.compatible || []).some(isQwenCloudAccount)) await refreshModelCatalog('compatible');
}
function renderQwenProviderCards() {
  const native = dashboardState.providers.qwen || [];
  const cloud = (dashboardState.providers.compatible || []).filter(isQwenCloudAccount);
  const quotas = cloud.map(account => dashboardState.quotas.compatible.get(account.account_id));
  const cards = native.map(account => buildQwenCard(account, lastQwenQuota.get(account.file_name || account.label))).join('')
    + cloud.map(buildCompatibleCard).join('');
  renderProviderCards('qwen', 'qwenCards', 'qwenBadgeCount', native.concat(cloud), cards, 'No Qwen accounts', quotas);
}
async function refreshCompatibleAccounts() {
  const [response, quotaResponse] = await Promise.all([adminFetch('/compatible/accounts.json'), adminFetch('/compatible/quota.json')]);
  if (!response) return;
  const data = await response.json();
  const accounts = data.accounts || [];
  dashboardState.providers.compatible = accounts;
  const quotas = quotaResponse && quotaResponse.ok ? (await quotaResponse.json()).accounts || [] : [];
  dashboardState.quotas.compatible = new Map(quotas.map(quota => [quota.account_id, quota]));
  const custom = accounts.filter(account => !isQwenCloudAccount(account));
  renderProviderCards('compatible', 'compatibleCards', 'compatibleBadgeCount', custom, custom.map(buildCompatibleCard).join(''), 'No custom compatible APIs.');
  renderQwenProviderCards();
  updateOverview();
}
