import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';

const source = fs.readFileSync(new URL('../src/main.rs', import.meta.url), 'utf8');
const html = source.slice(source.indexOf('r###"<!doctype html>'), source.indexOf('</html>"###'));
const scripts = [...html.matchAll(/<script(?:\s[^>]*)?>([\s\S]*?)<\/script>/g)].map(match => match[1]).filter(Boolean);

function definition(name) {
  const start = source.search(new RegExp(`      (?:async )?function ${name}\\(`));
  assert.ok(start >= 0, `missing dashboard function ${name}`);
  const end = source.indexOf('\n      }', start);
  assert.ok(end >= 0, `missing dashboard function end ${name}`);
  return source.slice(start, end + '\n      }'.length);
}

function environment(rules = [], timezone = 'UTC') {
  const row = rule => ({ querySelector: selector => ({value: String(rule[{
    '[data-quota-metric]': 'metric', '[data-quota-period]': 'period', '[data-quota-limit]': 'limit',
  }[selector]])}) });
  const context = vm.createContext({
    Intl, Set, Number,
    dashboardState: {apiKeys: [], apiKeyEditingId: ''},
    document: {
      querySelectorAll: () => rules.map(row),
      getElementById: () => ({value: timezone}),
    },
  });
  const names = ['normalizePromptTokenLimit', 'apiKeyRequestLimit', 'apiKeyAccountScope',
    'normalizeInputTokenBudget', 'normalizeApiKeyAccess', 'quotaPolicyFromDom', 'quotaMetricLabel',
    'apiKeyAccessEditorError'];
  new vm.Script(names.map(definition).join('\n')).runInContext(context);
  return context;
}

const plain = value => JSON.parse(JSON.stringify(value));

test('all embedded dashboard scripts parse', () => {
  assert.ok(scripts.length > 0);
  for (const script of scripts) new vm.Script(script);
});

test('scope normalization retains renewable quota and existing provider restrictions', () => {
  const context = environment();
  const quota = {timezone:'Asia/Jakarta',rules:[{metric:'requests',period:'weekly',limit:50}]};
  const access = plain(context.normalizeApiKeyAccess({all:false, quota, providers:[{
    provider:'claude',account_scope:'selected',accounts:['claude:one'],
    max_estimated_input_tokens_per_request:500,account_limits:[],
  }]}));
  assert.deepEqual(access.quota, quota);
  assert.equal(access.all, false);
  assert.deepEqual(access.providers[0].accounts, ['claude:one']);
  assert.equal(access.providers[0].max_estimated_input_tokens_per_request, 500);
});

test('unrestricted keys retain quota independently from access scope', () => {
  const context = environment();
  const quota = {timezone:'UTC',rules:[{metric:'output_tokens',period:'monthly',limit:1000}]};
  assert.deepEqual(plain(context.normalizeApiKeyAccess({all:true,quota})).quota, quota);
});

test('an empty rules editor means no renewable policy', () => {
  assert.equal(environment().quotaPolicyFromDom(), null);
});

test('multiple resources and periods are submitted together', () => {
  const rules = [{metric:'requests',period:'daily',limit:100},
    {metric:'input_tokens',period:'monthly',limit:20000},
    {metric:'cache_read_tokens',period:'weekly',limit:1000}];
  assert.deepEqual(plain(environment(rules,'Asia/Jakarta').quotaPolicyFromDom()), {timezone:'Asia/Jakarta',rules});
});

test('duplicate resource/period rules are rejected', () => {
  const rule = {metric:'requests',period:'daily',limit:1};
  assert.throws(() => environment([rule,rule]).quotaPolicyFromDom(), /only once/);
});

test('zero, negative, fraction, and unsafe numeric quotas are rejected', () => {
  for (const limit of [0,-1,1.5,Number.MAX_SAFE_INTEGER+1,NaN,Infinity]) {
    assert.throws(() => environment([{metric:'requests',period:'daily',limit}]).quotaPolicyFromDom(), /positive whole/);
  }
});

test('timezone must be a supported named zone', () => {
  assert.throws(() => environment([{metric:'requests',period:'monthly',limit:1}], 'not/a-zone').quotaPolicyFromDom(), /valid timezone/);
});

test('labels explain overlapping cache/input and reasoning/output categories', () => {
  const context = environment();
  assert.match(context.quotaMetricLabel('input_tokens'), /incl. cache/);
  assert.match(context.quotaMetricLabel('output_tokens'), /incl. reasoning/);
});

test('unsafe stored limits at every scope reject editing and never submit a write', async () => {
  const large = JSON.parse('9007199254740993');
  const policies = [
    {all:true, max_estimated_input_tokens_per_request:large},
    {all:true, prompt_token_limit:large},
    {all:true, input_token_budget:{limit:large, period:'lifetime'}},
    {all:true, quota:{rules:[{metric:'requests', period:'weekly', limit:large}]}},
    {all:false, providers:[{provider:'claude', account_scope:'all', max_estimated_input_tokens_per_request:large}]},
    {all:false, providers:[{provider:'claude', account_scope:'all', account_limits:[{account:'claude:one', max_estimated_input_tokens_per_request:large}]}]},
  ];
  for (const access of policies) {
    const context = environment();
    new vm.Script(['editApiKeyAccess', 'apiKeyAccessFromDom', 'createApiKey'].map(definition).join('\n')).runInContext(context);
    context.dashboardState.apiKeys = [{id:'key', access}];
    const messages = [];
    let writes = 0;
    context.updateApiKeyStatusText = message => messages.push(message);
    context.adminFetch = () => { writes += 1; throw new Error('must not write'); };
    context.editApiKeyAccess('key');
    assert.equal(context.dashboardState.apiKeyEditingId, '');
    assert.match(messages.at(-1), /cannot represent exactly.*iogw keys update/);
    // Also fail closed if a policy changed after the editor was opened.
    context.dashboardState.apiKeyEditingId = 'key';
    await context.createApiKey();
    assert.equal(writes, 0);
    assert.match(messages.at(-1), /No changes have been saved/);
  }
});

test('safe maximum integer policies remain editable', () => {
  const context = environment();
  assert.equal(context.apiKeyAccessEditorError({all:true,
    max_estimated_input_tokens_per_request:Number.MAX_SAFE_INTEGER,
    input_token_budget:{limit:Number.MAX_SAFE_INTEGER, period:'lifetime'},
    quota:{rules:[{metric:'requests', period:'weekly', limit:Number.MAX_SAFE_INTEGER}]},
  }), '');
});

test('unsafe limit and usage display directs operators to exact CLI/API values', () => {
  const context = environment();
  new vm.Script(['apiKeyAccessSummary', 'apiKeyQuotaUsageHtml'].map(definition).join('\n')).runInContext(context);
  context.providerLabels={};
  context.escapeHtml=value=>String(value);
  context.formatSettingsDateTime=value=>String(value);
  const large=JSON.parse('9007199254740993');
  const access=context.apiKeyAccessSummary({all:true,input_token_budget:{limit:large,period:'lifetime'}});
  assert.match(access,/CLI\/API inspection/);
  assert.doesNotMatch(access,/unlimited|No cumulative/);
  context.dashboardState.apiKeyQuotaSummaries={key:{rules:[{metric:'requests',period:'weekly',limit:large,
    confirmed:0,reserved:0,uncertain:0,remaining:large}]}};
  const usage=context.apiKeyQuotaUsageHtml('key');
  assert.match(usage,/iogw keys quotas.*exact balances/);
  assert.doesNotMatch(usage,/9007199254740992/);
});

test('quota-only edits retain capped but unselected accounts without authorizing them', () => {
  const quota = {timezone:'UTC',rules:[{metric:'requests',period:'weekly',limit:50}]};
  const context = environment(quota.rules);
  new vm.Script(['addRequestLimit', 'validateApiKeyEditorLimits', 'apiKeyAccessFromDom'].map(definition).join('\n')).runInContext(context);
  const limit = {value:'25', getAttribute:()=>'claude:unselected', setCustomValidity(){}};
  const fields = {
    apiKeyAccessModeInput:{value:'restricted'},
    apiKeyMaxEstimatedInputTokensInput:{value:'',setCustomValidity(){}},
    apiKeyInputTokenBudgetInput:{value:'',setCustomValidity(){}},
    apiKeyInputTokenBudgetPeriodInput:{value:'lifetime'},
    apiKeyQuotaTimezone:{value:'UTC'},
  };
  const quotaRows = context.document.querySelectorAll();
  context.dashboardProviderKeys = ['claude'];
  context.document = {
    getElementById:id=>fields[id],
    querySelector:selector=>selector.includes('access-provider') ? {checked:false} : {value:''},
    querySelectorAll:selector=>selector === '[data-quota-rule]' ? quotaRows
      : selector.includes(':checked') ? [{value:'claude:selected'}]
      : selector.includes('account-prompt-limit') ? [limit] : [],
  };
  const access = plain(context.apiKeyAccessFromDom());
  assert.deepEqual(access.providers[0].accounts, ['claude:selected']);
  assert.deepEqual(access.providers[0].account_limits,
    [{account:'claude:unselected',max_estimated_input_tokens_per_request:25}]);
  assert.deepEqual(access.quota, quota);
});

test('offline account caps do not check their access checkbox during rendering', () => {
  const context = environment();
  new vm.Script(['apiKeyAccessRule', 'renderApiKeyAccessEditor'].map(definition).join('\n')).runInContext(context);
  const fields = {apiKeyAccessModeInput:{},apiKeyAccessGroups:{}};
  context.document.getElementById = id=>fields[id];
  context.dashboardProviderKeys = ['claude'];
  context.providerLabels = {};
  context.renderApiKeyQuotaEditor = ()=>{};
  context.escapeHtml = value=>String(value);
  context.compactMiddle = value=>value;
  context.renderApiKeyAccessEditor({all:false,providers:[{
    provider:'claude',account_scope:'selected',accounts:['claude:selected'],
    account_limits:[{account:'claude:unselected',max_estimated_input_tokens_per_request:25}],
  }]});
  const checkboxes = [...fields.apiKeyAccessGroups.innerHTML.matchAll(/<input type="checkbox"[^>]+value="([^"]+)"([^>]*)>/g)];
  assert.match(checkboxes.find(match=>match[1] === 'claude:selected')[2], /checked/);
  assert.doesNotMatch(checkboxes.find(match=>match[1] === 'claude:unselected')[2], /checked/);
});

test('balance refresh preserves unsaved policy editor state', async () => {
  const context = environment();
  new vm.Script(definition('refreshApiKeyQuotaSummaries')).runInContext(context);
  context.dashboardState.apiKeyEditingId = 'editing';
  const keys = context.dashboardState.apiKeys;
  const summaries = {editing:{rules:[{metric:'requests',confirmed:2,remaining:8}]}};
  const calls = [];
  context.adminFetch = async url=>{
    calls.push(url);
    return {ok:true,json:async()=>({ok:true,quota_summaries:summaries})};
  };
  let renders = 0;
  context.renderApiKeys = ()=>{renders += 1;};
  context.renderApiKeyAccessEditor = ()=>{throw new Error('must not reset editor');};
  await context.refreshApiKeyQuotaSummaries();
  assert.deepEqual(plain(context.dashboardState.apiKeyQuotaSummaries), summaries);
  assert.equal(context.dashboardState.apiKeyEditingId, 'editing');
  assert.equal(context.dashboardState.apiKeys, keys);
  assert.deepEqual(calls, ['/admin/api-keys/quotas']);
  assert.equal(renders, 1);
});

test('stale summary responses cannot overwrite a newer mutation or refresh', async () => {
  for (const replacement of ['mutation', 'refresh']) {
    const context = environment();
    new vm.Script(definition('refreshApiKeyQuotaSummaries')).runInContext(context);
    let finish;
    context.adminFetch = () => new Promise(resolve=>{finish=resolve;});
    context.renderApiKeys = ()=>{throw new Error('stale response must be ignored');};
    const pending = context.refreshApiKeyQuotaSummaries();
    const newer = {key:{rules:[{confirmed:2}]}};
    context.dashboardState.apiKeyQuotaSummaries = newer;
    if (replacement === 'mutation') context.dashboardState.apiKeys = [];
    else context.dashboardState.apiKeyQuotaRefreshId += 1;
    finish({ok:true,json:async()=>({ok:true,quota_summaries:{key:{rules:[{confirmed:1}]}}})});
    await pending;
    assert.equal(context.dashboardState.apiKeyQuotaSummaries, newer);
  }
});

test('failed balance refresh marks usage unavailable rather than showing stale allowance', async () => {
  const context = environment();
  new vm.Script(definition('refreshApiKeyQuotaSummaries')).runInContext(context);
  context.dashboardState.apiKeys = [{id:'limited',access:{quota:{rules:[]}}}];
  context.dashboardState.apiKeyQuotaSummaries = {limited:{rules:[{remaining:100}]}};
  context.adminFetch = async()=>{throw new Error('storage unavailable');};
  context.renderApiKeys = ()=>{};
  const messages=[];
  context.updateApiKeyStatusText = message=>messages.push(message);
  await context.refreshApiKeyQuotaSummaries();
  assert.deepEqual(plain(context.dashboardState.apiKeyQuotaSummaries), {limited:{error:true}});
  assert.match(messages.at(-1), /unavailable/);
});

test('managed Test API refreshes quota balances after success and failure', async () => {
  for (const ok of [true, false]) {
    const context = environment();
    new vm.Script(definition('sendTestApi')).runInContext(context);
    const fields = {
      testApiModelInput:{value:'claude/test'},testApiPromptInput:{value:'hello'},
      testApiInstructionsInput:{value:''},testApiMaxOutputInput:{value:'5'},
      testApiApiKeyProfileInput:{value:'managed-key'},
    };
    context.document.getElementById = id=>fields[id];
    for (const name of ['setTestApiBusy','setText','renderTestApiResult','invalidateDashboardSnapshot','refreshContextChart']) {
      context[name] = ()=>{};
    }
    context.adminFetch = async()=>({text:async()=>JSON.stringify({ok}),status:ok?200:503});
    context.refreshDashboardSnapshot = async()=>null;
    let refreshes=0;
    context.refreshApiKeyQuotaSummaries = async()=>{refreshes += 1;};
    await context.sendTestApi();
    assert.equal(refreshes, 1);
  }
});

test('periodic dashboard refresh updates open settings quota balances', async () => {
  const context = environment();
  new vm.Script(definition('startDashboard')).runInContext(context);
  context.dashboardIntervalsStarted=false;
  context.adminAuthenticated=true;
  context.refreshDashboardSnapshot=async()=>({});
  context.renderDashboardSnapshot=()=>{};
  context.refreshContextChart=()=>{};
  context.refreshCustomModels=()=>{};
  const intervals=[];
  context.setInterval=(callback,delay)=>intervals.push({callback,delay});
  const modal={style:{display:'block'}};
  context.document.getElementById=()=>modal;
  let refreshes=0;
  context.refreshApiKeyQuotaSummaries=()=>{refreshes+=1;};
  await context.startDashboard();
  intervals.find(interval=>interval.delay===10000).callback();
  await Promise.resolve();
  assert.equal(refreshes,1);
  modal.style.display='none';
  intervals.find(interval=>interval.delay===10000).callback();
  await Promise.resolve();
  assert.equal(refreshes,1);
});
