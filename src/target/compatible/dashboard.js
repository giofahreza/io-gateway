// Uses the dashboard's existing admin session, modal, card, and action controls.
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
      <label for="compatibleName">Name</label>
      <input id="compatibleName" required autocomplete="off" placeholder="e.g. Qwen Coding Plan">
      <label for="compatibleFormat" style="margin-top:12px">API format</label>
      <select id="compatibleFormat"><option value="openai">OpenAI compatible</option><option value="anthropic">Anthropic compatible</option><option value="both">Both formats</option></select>
      <div id="compatibleOpenaiGroup"><label for="compatibleOpenai" style="margin-top:12px">OpenAI base URL</label><input id="compatibleOpenai" type="url" placeholder="https://example.com/v1"></div>
      <div id="compatibleAnthropicGroup"><label for="compatibleAnthropic" style="margin-top:12px">Anthropic base URL</label><input id="compatibleAnthropic" type="url" placeholder="https://example.com/anthropic"></div>
      <label for="compatibleKey" style="margin-top:12px">API key</label>
      <input id="compatibleKey" type="password" autocomplete="new-password" spellcheck="false" placeholder="Enter the service API key">
      <div id="compatibleKeyHelp" class="muted" style="margin-top:6px">The key is stored privately and never shown in account details.</div>
      <label for="compatibleModels" style="margin-top:12px">Model IDs</label>
      <textarea id="compatibleModels" rows="3" required placeholder="qwen3.7-plus, qwen3.6-flash" aria-describedby="compatibleModelsHelp"></textarea>
      <div id="compatibleModelsHelp" class="muted">Separate IDs with commas or new lines, or fetch the service's model list.</div>
      <div class="modal-actions" style="margin-top:12px"><button id="compatibleDiscover" type="button" class="secondary-button">Fetch models</button></div>
      <p id="compatibleStatus" class="muted" role="status" aria-live="polite"></p>
      <div class="modal-actions"><button id="compatibleSave" type="submit">Save account</button><button id="compatibleClose" type="button" class="secondary-button">Cancel</button></div>
    </form></div>`;
  document.body.appendChild(modal);
  document.getElementById('compatibleFormat').addEventListener('change', updateCompatibleFormat);
  document.getElementById('compatibleClose').addEventListener('click', closeCompatibleModal);
  document.getElementById('compatibleForm').addEventListener('submit', saveCompatibleAccount);
  document.getElementById('compatibleDiscover').addEventListener('click', discoverCompatibleModels);
}
function updateCompatibleFormat() {
  const format = document.getElementById('compatibleFormat').value;
  for (const [id, enabled] of [['Openai', format !== 'anthropic'], ['Anthropic', format !== 'openai']]) {
    document.getElementById('compatible' + id + 'Group').hidden = !enabled;
    document.getElementById('compatible' + id).required = enabled;
    document.getElementById('compatible' + id).disabled = !enabled;
  }
}
function openCompatibleAccount(fileName) {
  ensureCompatibleModal();
  const account = fileName && dashboardState.providers.compatible.find(a => a.file_name === fileName);
  if (fileName && !account) return;
  const form = document.getElementById('compatibleForm');
  form.reset();
  form.dataset.fileName = account ? account.file_name : '';
  document.getElementById('compatibleTitle').textContent = account ? 'Edit compatible API' : 'Add compatible API';
  document.getElementById('compatibleName').value = account ? account.label : '';
  document.getElementById('compatibleFormat').value = account ? (account.base_url && account.anthropic_base_url ? 'both' : account.base_url ? 'openai' : 'anthropic') : 'openai';
  document.getElementById('compatibleOpenai').value = account && account.base_url || '';
  document.getElementById('compatibleAnthropic').value = account && account.anthropic_base_url || '';
  document.getElementById('compatibleModels').value = account ? account.models.join('\n') : '';
  document.getElementById('compatibleKey').required = !account;
  document.getElementById('compatibleKey').placeholder = account ? 'Leave blank to keep the saved key' : 'Enter the service API key';
  document.getElementById('compatibleStatus').textContent = '';
  updateCompatibleFormat();
  openModal('compatibleModal');
  document.getElementById('compatibleName').focus();
}
function closeCompatibleModal() {
  document.getElementById('compatibleKey').value = '';
  closeModal('compatibleModal');
}
function compatiblePayload() {
  const format = document.getElementById('compatibleFormat').value;
  return {
    file_name: document.getElementById('compatibleForm').dataset.fileName || null,
    label: document.getElementById('compatibleName').value.trim(),
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
  await compatibleAction('/compatible/save', 'Saving account…', async () => {
    closeCompatibleModal();
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
async function refreshCompatibleAccounts() {
  const response = await adminFetch('/compatible/accounts.json');
  if (!response) return;
  const data = await response.json();
  const accounts = data.accounts || [];
  dashboardState.providers.compatible = accounts;
  const cards = accounts.map(account => {
    const escape = escapeHtml;
    const formats = [account.base_url && 'OpenAI', account.anthropic_base_url && 'Anthropic'].filter(Boolean).join(' + ');
    return '<div class="card">' + renderAccountHeader(account.label, 'compatible', account)
      + '<div class="stat-pills"><span class="stat-pill">' + formats + '</span><span class="stat-pill">' + (account.requests || 0) + ' requests</span><span class="stat-pill">' + (account.errors || 0) + ' errors</span><span class="stat-pill">' + (account.total_tokens || 0) + ' tokens</span></div>'
      + '<dl style="overflow-wrap:anywhere">' + (account.base_url ? '<dt class="muted">OpenAI</dt><dd style="margin-left:0">' + escape(account.base_url) + '</dd>' : '')
      + (account.anthropic_base_url ? '<dt class="muted" style="margin-top:8px">Anthropic</dt><dd style="margin-left:0">' + escape(account.anthropic_base_url) + '</dd>' : '')
      + '</dl><details><summary>' + account.models.length + ' models</summary><div style="overflow-wrap:anywhere">' + account.models.map(id => '<code>' + escape('cmp:' + id) + '</code>').join('<br>') + '</div></details>'
      + (account.last_error_message ? '<p class="muted">' + escape(account.last_error_message) + '</p>' : '')
      + '<button type="button" class="secondary-button" style="margin-top:12px" data-edit-compatible="' + escape(account.file_name) + '">Edit account</button></div>';
  }).join('');
  renderProviderCards('compatible', 'compatibleCards', 'compatibleBadgeCount', accounts, cards, 'No compatible APIs. Add an OpenAI or Anthropic compatible service.');
  document.querySelectorAll('[data-edit-compatible]').forEach(button => button.addEventListener('click', () => openCompatibleAccount(button.dataset.editCompatible)));
  updateOverview();
}
