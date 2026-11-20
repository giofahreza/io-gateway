#!/usr/bin/env node
// Verify quota-only refreshes redraw cards even when account counters stay fixed.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const main = fs.readFileSync(new URL('../src/main.rs',import.meta.url),'utf8');
const elements = new Map(['compatibleCards','compatibleBadgeCount','qwenCards','qwenBadgeCount'].map(id=>[id,{}]));
const account = {account_id:'qwen-fixture',label:'Qwen Cloud Token Plan',file_name:'fixture.json',
  service:'qwen_token_plan',models:['auto'],base_url:'https://token-plan.maas.qwencloudapi.com/compatible-mode/v1'};
const custom = {...account,account_id:'custom-fixture',file_name:'custom.json',service:'custom',label:'Other API',base_url:'https://example.org/v1'};
let quota = {account_id:account.account_id,credits:{balance:15000},limits:[{used_percent:25}]};
const context = vm.createContext({
  Map,Response,console,URL,
  lastQwenQuota:new Map(),buildQwenCard:account=>account.label,
  document:{getElementById:id=>elements.get(id),querySelectorAll:()=>[]},
  dashboardState:{providers:{qwen:[]},quotas:{qwen:new Map()}},providerCardSignatures:new Map(),
  adminFetch:async path=>new Response(JSON.stringify({accounts:path.includes('/quota.')?[quota]:[account,custom]})),
  escapeHtml:value=>String(value).replaceAll('&','&amp;').replaceAll('<','&lt;').replaceAll('"','&quot;'),
  renderAccountHeader:label=>label,renderQuotaBars:value=>'Usage '+value.limits[0].used_percent+'%',updateOverview:()=>{},
  providerLabels:{compatible:'Compatible API'},formatDashboardDateTime:value=>String(value),previewText:text=>text,
  accountDetailStateKey:()=>'',detailToggleAttrs:()=>'',renderAttentionDetails:()=>'',
  renderAccountModels:account=>'<models>'+account.models.join(',')+'</models>',
});
for (const name of ['renderUsageCredits','renderProviderCards','connectionRows','renderMetaLine','renderDateTimeMetaLine','renderLongMetaLine','firstPresent','renderMetaDetails']) {
  const start=main.indexOf('      function '+name+'(');
  const end=main.indexOf('\n      function ',start+1);
  vm.runInContext(main.slice(start,end),context);
}
vm.runInContext(fs.readFileSync(new URL('../src/target/compatible/dashboard.js',import.meta.url),'utf8'),context);
await context.refreshCompatibleAccounts();
assert.match(elements.get('qwenCards').innerHTML,/15000/);
assert.ok(!elements.get('compatibleCards').innerHTML.includes('15000'));
assert.match(elements.get('compatibleCards').innerHTML,/Other API/);
assert.equal(elements.get('qwenBadgeCount').textContent,'1 accounts');
assert.equal(elements.get('compatibleBadgeCount').textContent,'1 accounts');
context.dashboardState.providers.qwen=[{label:'Qwen Chat account'}];
context.renderQwenProviderCards();
assert.match(elements.get('qwenCards').innerHTML,/Qwen Chat account/);
assert.equal(elements.get('qwenBadgeCount').textContent,'2 accounts');
quota={...quota,credits:{balance:0},limits:[{used_percent:100}]};
await context.refreshCompatibleAccounts();
assert.match(elements.get('qwenCards').innerHTML,/> 0</);
assert.match(elements.get('qwenCards').innerHTML,/Usage 100%/);
quota={...quota,credits:{balance:'12.50',cash_balance:'2.50',credit_limit:'10',currency:'USD'}};
await context.refreshCompatibleAccounts();
const html=elements.get('qwenCards').innerHTML;
for (const expected of ['12.50 USD','cash balance','2.50 USD','credit limit','10 USD']) assert.ok(html.includes(expected));
assert.equal((html.match(/quota-status/g)||[]).length,1,'credits show on a single line');
assert.match(html,/Connection details<\/summary><div class="meta-list">.*openai base URL: <\/span><code>https:\/\/token-plan\.maas\.qwencloudapi\.com/s);
assert.match(html,/<models>cmp:auto<\/models>/);
assert.ok(!html.includes('<dl'));
context.renderQwenProviderCards();
assert.match(elements.get('qwenCards').innerHTML,/12.50 USD/);
assert.equal(elements.get('qwenBadgeCount').textContent,'2 accounts');
console.log('Qwen grouping, Chat coexistence, custom separation, balance-only updates, zero, cash and credit limits passed.');

const start=main.indexOf('      function renderQuotaBars(');
const end=main.indexOf('\n      function ',start+1);
vm.runInContext(main.slice(start,end),context);
const zero=context.renderQuotaBars({limits:[{label:'Monthly',used:0,limit:20000,used_percent:0}]},{provider:'compatible'});
assert.match(zero,/0\/20000/);
const percentage=context.renderQuotaBars({limits:[{label:'Monthly',used_percent:25}]},{provider:'compatible'});
assert.match(percentage,/25.0% used/);
assert.ok(!percentage.includes('>/ <'));
console.log('Real quota renderer preserves zero and shows percentages when allowances are unknown.');
assert.match(context.renderUsageCredits({credits:{balance:36000,estimated:true}}),/≈ 36000/);
assert.doesNotMatch(elements.get('qwenCards').innerHTML,/connect Qwen Cloud account|Edit account/i);

const refreshes=[];
context.refreshModelCatalog=async provider=>refreshes.push(provider);
await context.refreshQwenGroup();
assert.deepEqual(refreshes,['qwen','compatible']);
context.dashboardState.providers.qwen=[];
refreshes.length=0;
await context.refreshQwenGroup();
assert.deepEqual(refreshes,['compatible']);
console.log('Qwen group refresh includes Cloud quotas and skips empty Chat accounts.');

// The editor follows the API key: sk-sp- is Token Plan, any other key is pay-as-you-go.
const nodes=new Map();
const node=id=>{if(!nodes.has(id))nodes.set(id,{value:'',hidden:false,disabled:false,required:false,textContent:'',dataset:{},style:{}});return nodes.get(id);};
const editor=vm.createContext({console,URL,setTimeout,clearTimeout,adminFetch:async()=>({}),document:{getElementById:node}});
vm.runInContext(fs.readFileSync(new URL('../src/target/compatible/dashboard.js',import.meta.url),'utf8'),editor);
const accountType=node('compatibleService'),keyInput=node('compatibleKey'),openai=node('compatibleOpenai');
const typeKey=value=>{keyInput.value=value;editor.detectQwenServiceFromKey();};
accountType.value='qwen_token_plan';
typeKey('sk-ws-abc');
assert.equal(accountType.value,'qwen_payg');
assert.equal(openai.value,'https://ap-southeast-1.maas.qwencloudapi.com/compatible-mode/v1');
assert.equal(node('compatibleTitle').textContent,'Add Qwen Cloud');
openai.value='https://eu-central-1.maas.qwencloudapi.com/compatible-mode/v1';
typeKey('sk-ws-def');
assert.equal(openai.value,'https://eu-central-1.maas.qwencloudapi.com/compatible-mode/v1','a chosen region survives re-typing a key of the same type');
typeKey('sk-sp-xyz');
assert.equal(accountType.value,'qwen_token_plan');
assert.equal(openai.value,'https://token-plan.maas.qwencloudapi.com/compatible-mode/v1');
assert.equal(node('compatibleModels').value,'auto');
accountType.value='custom';
typeKey('sk-ws-other');
assert.equal(accountType.value,'custom','custom compatible APIs are never retyped from their key');
openai.value='https://eu-central-1.maas.qwencloudapi.com/compatible-mode/v1';
editor.detectCompatibleService();
assert.equal(accountType.value,'qwen_payg','a Qwen Cloud host selects its account type');
console.log('Qwen Cloud editor detects Token Plan or pay-as-you-go from the key and host.');
