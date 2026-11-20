#!/usr/bin/env node
// The Test API model list must name every model, even when the catalog has no separate display name.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';
const main = fs.readFileSync(new URL('../src/main.rs', import.meta.url), 'utf8');
const list = { innerHTML: '' };
const input = { value: '' };
const context = vm.createContext({
  document: { getElementById: id => ({ testApiModelList: list, testApiModelInput: input })[id] },
  dashboardState: {
    testApiModelOptions: [
      { provider: 'cmp', id: 'cmp:qwen3.7-plus', model: 'qwen3.7-plus', display_name: 'cmp:qwen3.7-plus' },
      { provider: 'cod', id: 'cod:gpt-5', model: 'gpt-5', display_name: 'GPT-5' },
      { provider: 'ctm', id: 'ctm:workhorse', model: 'workhorse', display_name: 'ctm:workhorse' },
    ],
    customModelModelOptions: [],
    customModels: [],
  },
  providerLabels: { cmp: 'Compatible API', cod: 'Codex', ctm: 'Custom model' },
  escapeHtml: value => String(value).replaceAll('&', '&amp;').replaceAll('"', '&quot;').replaceAll('<', '&lt;'),
  normalizeCustomAlias: value => String(value || '').trim(),
});
for (const name of ['testApiModelEntries', 'renderTestApiModelOptions']) {
  const start = main.indexOf('      function ' + name + '(');
  const end = main.indexOf('\n      function ', start + 1);
  assert.ok(start >= 0, name + ' not found');
  vm.runInContext(main.slice(start, end), context);
}
context.renderTestApiModelOptions();
const options = [...list.innerHTML.matchAll(/value="([^"]*)" label="([^"]*)"/g)].map(match => [match[1], match[2]]);
assert.deepEqual(options, [
  ['cmp:qwen3.7-plus', 'Compatible API - qwen3.7-plus'],
  ['cod:gpt-5', 'Codex - GPT-5'],
  ['ctm:workhorse', 'Custom model - workhorse'],
]);
assert.equal(input.value, 'cmp:qwen3.7-plus', 'the first model is preselected');
console.log('Test API model options name every model.');
