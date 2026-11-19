#!/usr/bin/env node
// Exercise the real dashboard renderers without provider credentials or DOM writes.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const source = fs.readFileSync(new URL('../src/main.rs', import.meta.url), 'utf8');
const names = ['renderUsageCredits','renderCodexResetCredits'];
const context = vm.createContext({
  escapeHtml: value => String(value).replaceAll('&','&amp;').replaceAll('<','&lt;').replaceAll('"','&quot;'),
  accountLabel:a=>a.label, jsString:JSON.stringify, accountDetailStateKey:()=>'', detailToggleAttrs:()=>'',
  formatResetCreditExpiry:c=>c.expires_at || 'Expiry not reported', resetCreditDisplayName:c=>c.title || c.id,
  compactMiddle:id=>id,
});
for (const name of names) {
  const start=source.indexOf('      function '+name+'(');
  const end=source.indexOf('\n      function ',start+1);
  vm.runInContext(source.slice(start,end),context);
}
const render=context.renderUsageCredits;
assert.match(render(null),/Not reported/);
assert.match(render({credits:{balance:'0'}}),/> 0</);
assert.match(render({credits:{balance:'12.50',currency:'USD',enabled:false}}),/12.50 USD/);
assert.match(render({credits:{enabled:false}}),/Disabled/);
assert.match(render({credits:{unlimited:true}}),/Unlimited/);
assert.match(render({credits:{balance:'<script>'}}),/&lt;script>/);
const account={file_name:'test.json',label:'Test',enabled:true};
const quota={rate_limit_reset_credits:{available_count:1,credits:[{id:'full-reset',program:'cedar_ember',status:'available',usable_now:true,title:'Full reset',remaining:1,description:'Clears weekly limit'}]}};
assert.match(context.renderCodexResetCredits(account,quota,'claude'),/redeemUsageReset/);
assert.match(context.renderCodexResetCredits(account,quota,'claude'),/Clears weekly limit/);
assert.doesNotMatch(context.renderCodexResetCredits({...account,enabled:false},quota,'claude'),/onclick="redeem/);
assert.doesNotMatch(context.renderCodexResetCredits(account,{rate_limit_reset_credits:{...quota.rate_limit_reset_credits,available_count:0}},'codex'),/onclick="redeem/);
assert.doesNotMatch(context.renderCodexResetCredits(account,{...quota,last_reset_action:{state:'manual_review',updated_at:'now'}},'claude'),/onclick="redeem/);
quota.rate_limit_reset_credits.credits[0].usable_now=false;
assert.doesNotMatch(context.renderCodexResetCredits(account,quota,'claude'),/onclick="redeem/);
assert.match(context.renderCodexResetCredits(account,null,'claude'),/not reported/);
assert.doesNotMatch(context.renderCodexResetCredits(account,{rate_limit_reset_credits:{available_count:2}},'codex'),/onclick="redeem/);
console.log('15 account credit and reset rendering checks passed.');
