#!/usr/bin/env node
// Deterministic end-to-end quota tests. Only localhost providers and synthetic
// credentials are used. Each run owns and removes one private temporary tree.
// Usage: cargo build --bins && node scripts/test-renewable-quotas.mjs
// Optional: QUOTA_TEST_FILTER='restart|backup' node scripts/test-renewable-quotas.mjs
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs/promises';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const gatewayBinary = path.resolve(process.env.QUOTA_TEST_GATEWAY_BINARY || path.join(repository, 'target/debug/io-gateway'));
const cliBinary = path.resolve(process.env.QUOTA_TEST_CLI_BINARY || path.join(repository, 'target/debug/iogw'));
const selectedChecks = process.env.QUOTA_TEST_FILTER ? new RegExp(process.env.QUOTA_TEST_FILTER, 'i') : undefined;
const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'io-gateway-renewable-quota-test-'));
await fs.chmod(temporary, 0o700);
const authDirectory = path.join(temporary, 'auth');
await fs.mkdir(authDirectory, { mode: 0o700 });
const configFile = path.join(temporary, 'config.json');
const legacyKey = 'local-fixture-legacy-not-a-real-key';
const secrets = new Set([legacyKey, 'local-fixture-codex', 'local-fixture-claude', 'local-fixture-deepseek-one', 'local-fixture-deepseek-two']);
const checks = [];
const upstreamCalls = [];
const slowResponses = new Map();
const retryCounts = new Map();
const auxiliaryProcesses = new Set();
let gateway;
let gatewayLog = '';
let gatewayBase;

function redacted(value) {
  let result = String(value);
  for (const secret of secrets) result = result.replaceAll(secret, '[fixture credential]');
  return result.replaceAll(temporary, '[private fixture]');
}
const delay = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));
async function eventually(predicate, message, timeout = 8000) {
  const start = Date.now();
  while (Date.now() - start < timeout) {
    if (await predicate()) return;
    await delay(25);
  }
  throw new Error(message);
}
async function check(name, test) {
  if (selectedChecks && !selectedChecks.test(name)) return;
  const start = Date.now();
  try {
    await test();
    checks.push({ name, passed: true, milliseconds: Date.now() - start });
    process.stdout.write(`PASS ${name}\n`);
  } catch (error) {
    const message = redacted(error?.stack || error);
    checks.push({ name, passed: false, milliseconds: Date.now() - start, error: message });
    process.stdout.write(`FAIL ${name}: ${message}\n`);
    for (const held of slowResponses.values()) {
      if (!held.response.writableEnded) held.reply();
    }
  }
}
function writeJson(response, status, value) {
  response.writeHead(status, { 'content-type': 'application/json' });
  response.end(JSON.stringify(value));
}
function usageFor(body, marker) {
  if (marker.includes('missing-usage')) return undefined;
  if (marker.includes('partial-usage')) return { input_tokens: 20, output_tokens: 3 };
  if (marker.includes('explicit-zero')) return { input_tokens: 0, output_tokens: 0, cache_read_input_tokens: 0, cache_creation_input_tokens: 0 };
  return {
    input_tokens: 20,
    cache_read_input_tokens: 6,
    cache_creation_input_tokens: 4,
    output_tokens: marker.includes('reported-overage') ? 13 : Math.min(3, Number(body.max_tokens ?? 3)),
  };
}
function anthropicReply(response, body, marker) {
  const usage = usageFor(body, marker);
  if (body.stream) {
    response.writeHead(200, { 'content-type': 'text/event-stream', 'cache-control': 'no-cache' });
    const event = (name, value) => response.write(`event: ${name}\ndata: ${JSON.stringify(value)}\n\n`);
    const startUsage = usage && { ...usage, output_tokens: 0 };
    event('message_start', { type: 'message_start', message: { id: 'msg-fixture', type: 'message', role: 'assistant', model: body.model, content: [], usage: startUsage } });
    event('content_block_start', { type: 'content_block_start', index: 0, content_block: { type: 'text', text: '' } });
    event('content_block_delta', { type: 'content_block_delta', index: 0, delta: { type: 'text_delta', text: 'fixture ok' } });
    if (marker.includes('stream-disconnect')) {
      slowResponses.set(marker, { response, reply: () => response.end() });
      response.on('close', () => slowResponses.delete(marker));
      return;
    }
    if (marker.includes('stream-error')) {
      event('error', { type: 'error', error: { type: 'overloaded_error', message: 'synthetic stream failure' } });
      response.end();
      return;
    }
    if (marker.includes('stream-incomplete')) {
      response.end();
      return;
    }
    event('content_block_stop', { type: 'content_block_stop', index: 0 });
    event('message_delta', { type: 'message_delta', delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: usage && { output_tokens: usage.output_tokens } });
    event('message_stop', { type: 'message_stop' });
    response.end();
    return;
  }
  writeJson(response, 200, {
    id: 'msg-fixture', type: 'message', role: 'assistant', model: body.model,
    content: [{ type: 'text', text: 'fixture ok' }], stop_reason: 'end_turn', stop_sequence: null,
    ...(usage === undefined ? {} : { usage }),
  });
}

const mock = http.createServer(async (request, response) => {
  try {
    const chunks = [];
    for await (const chunk of request) chunks.push(chunk);
    const text = Buffer.concat(chunks).toString();
    let body;
    try { body = JSON.parse(text || '{}'); } catch { body = {}; }
    if (request.method !== 'POST') {
      if (request.url.includes('models')) {
        writeJson(response, 200, { data: [
          { id: 'deepseek-chat', object: 'model', owned_by: 'deepseek' },
          { id: 'claude-sonnet-4-20250514', type: 'model', display_name: 'Fixture Claude' },
        ], models: [{ slug: 'gpt-5.4', display_name: 'Fixture Codex', supported_in_api: true }] });
      } else {
        writeJson(response, 200, { is_available: true, balance_infos: [], rate_limit: { allowed: true }, credits: [], available_count: 0 });
      }
      return;
    }
    const marker = text.match(/fixture:[a-z0-9-]+/)?.[0] || 'fixture:normal';
    upstreamCalls.push({ path: request.url, body, marker, at: Date.now() });
    if (marker.includes('retry-once')) {
      const count = (retryCounts.get(marker) || 0) + 1;
      retryCounts.set(marker, count);
      if (count === 1) {
        writeJson(response, 503, { error: { message: 'synthetic temporary overload', type: 'overloaded_error' } });
        return;
      }
    }
    const reply = () => {
      if (request.url.includes('messages')) anthropicReply(response, body, marker);
      else {
        const incomplete = marker.includes('terminal-incomplete');
        const value = {
        id: 'resp-fixture', object: 'response', status: incomplete ? 'incomplete' : 'completed', model: body.model,
        ...(incomplete ? { incomplete_details: { reason: 'max_output_tokens' } } : {}),
        output: [{ id: 'msg-fixture', type: 'message', role: 'assistant', status: 'completed', content: [{ type: 'output_text', text: 'fixture ok', annotations: [] }] }],
        usage: { input_tokens: 30, output_tokens: 3, total_tokens: 33, input_tokens_details: { cached_tokens: 6, cache_write_tokens: 4 }, output_tokens_details: { reasoning_tokens: 1 } },
        };
        // Codex transports Responses over SSE even for non-streaming clients.
        response.writeHead(200, { 'content-type': 'text/event-stream' });
        response.write(`event: response.created\ndata: ${JSON.stringify({ type: 'response.created', response: { ...value, status: 'in_progress', output: [] } })}\n\n`);
        const terminal = incomplete ? 'response.incomplete' : 'response.completed';
        response.write(`event: ${terminal}\ndata: ${JSON.stringify({ type: terminal, response: value })}\n\n`);
        response.end();
      }
    };
    if (marker.includes('hold-')) {
      slowResponses.set(marker, { response, reply });
      response.on('close', () => slowResponses.delete(marker));
      return;
    }
    reply();
  } catch (error) {
    if (!response.headersSent) writeJson(response, 500, { error: { message: 'synthetic mock handler failure' } });
    else response.destroy(error);
  }
});

async function unusedPort() {
  const server = net.createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}
async function writeFixture(relative, value) {
  await fs.writeFile(path.join(temporary, relative), JSON.stringify(value), { mode: 0o600 });
}
async function stopGateway(signal = 'SIGTERM') {
  const processToStop = gateway;
  gateway = undefined;
  await stopProcess(processToStop, signal);
}
async function stopProcess(processToStop, signal = 'SIGTERM') {
  if (!processToStop || processToStop.exitCode !== null || processToStop.signalCode !== null) return;
  processToStop.kill(signal);
  await Promise.race([once(processToStop, 'exit'), delay(5000)]);
  if (processToStop.exitCode === null && processToStop.signalCode === null) {
    processToStop.kill('SIGKILL');
    await once(processToStop, 'exit');
  }
}
async function startAuxiliary(configuration, base) {
  const child = spawn(gatewayBinary, ['--config', configuration], { cwd: temporary, stdio: ['ignore', 'pipe', 'pipe'] });
  auxiliaryProcesses.add(child);
  child.once('exit', () => auxiliaryProcesses.delete(child));
  let output = '';
  child.stdout.on('data', chunk => { output = (output + chunk).slice(-12000); });
  child.stderr.on('data', chunk => { output = (output + chunk).slice(-12000); });
  await eventually(async () => {
    if (child.exitCode !== null || child.signalCode !== null) throw new Error(`secondary gateway failed: ${redacted(output)}`);
    try { return (await fetch(`${base}/health`, { signal: AbortSignal.timeout(300) })).ok; } catch { return false; }
  }, 'secondary gateway failed readiness', 15000);
  return child;
}
async function startGateway(configuration = configFile) {
  gatewayLog = '';
  const env = { ...process.env, RUST_BACKTRACE: '0', HTTP_PROXY: '', HTTPS_PROXY: '', ALL_PROXY: '', http_proxy: '', https_proxy: '', all_proxy: '', NO_PROXY: '*' };
  gateway = spawn(gatewayBinary, ['--config', configuration], { cwd: temporary, env, stdio: ['ignore', 'pipe', 'pipe'] });
  const retain = chunk => { gatewayLog = (gatewayLog + chunk.toString()).slice(-12000); };
  gateway.stdout.on('data', retain);
  gateway.stderr.on('data', retain);
  gateway.on('error', retain);
  await eventually(async () => {
    if (gateway.exitCode !== null || gateway.signalCode !== null) throw new Error(`gateway failed to start: ${redacted(gatewayLog)}`);
    try { return (await fetch(`${gatewayBase}/health`, { signal: AbortSignal.timeout(300) })).ok; } catch { return false; }
  }, `gateway readiness timeout: ${redacted(gatewayLog)}`, 15000);
}
async function call(route, { method = 'GET', key, body, signal, base = gatewayBase } = {}) {
  const headers = {};
  if (key) headers.authorization = `Bearer ${key.plain || key}`;
  if (body !== undefined) headers['content-type'] = 'application/json';
  const response = await fetch(`${base}${route}`, {
    method, headers, body: body === undefined ? undefined : JSON.stringify(body),
    signal: signal || AbortSignal.timeout(12000),
  });
  const text = await response.text();
  let value;
  try { value = JSON.parse(text); } catch { value = null; }
  return { status: response.status, headers: response.headers, text, value };
}
async function rawCall(route, { method = 'POST', key, body = '', headers = {} } = {}) {
  const bytes = Buffer.isBuffer(body) ? body : Buffer.from(body);
  return new Promise((resolve, reject) => {
    const outgoing = http.request(`${gatewayBase}${route}`, {
      method,
      headers: { authorization: `Bearer ${key.plain}`, 'content-type': 'application/json', 'content-length': bytes.length, ...headers },
      timeout: 12000,
    }, incoming => {
      const chunks = [];
      incoming.on('data', chunk => chunks.push(chunk));
      incoming.on('end', () => resolve({ status: incoming.statusCode, text: Buffer.concat(chunks).toString() }));
      incoming.on('error', reject);
    });
    outgoing.on('timeout', () => outgoing.destroy(new Error('raw fixture request timed out')));
    outgoing.on('error', reject);
    outgoing.end(bytes);
  });
}
const rule = (metric, period = 'daily', limit = 10000) => ({ metric, period, limit });
const policy = (rules, timezone = 'UTC') => ({ timezone, rules });
async function createKey(rules, extras = {}, timezone = 'UTC') {
  const access = { all: true, ...extras, ...(rules ? { quota: policy(rules, timezone) } : {}) };
  const response = await call('/admin/api-keys/create', { method: 'POST', body: { label: `localhost-quota-fixture-${checks.length}`, access } });
  assert.equal(response.status, 200, `create key returned ${response.status}: ${response.text}`);
  assert.ok(response.value?.plain_text_key, 'create response must return the one-time credential');
  secrets.add(response.value.plain_text_key);
  return { id: response.value.key.id, plain: response.value.plain_text_key, access };
}
async function summary(key) {
  const response = await call(`/admin/api-keys/quotas?id=${encodeURIComponent(key.id)}`);
  assert.equal(response.status, 200);
  const result = response.value?.quota_summaries?.[key.id];
  assert.ok(result && !result.error, 'quota summary must exist and be readable');
  return result;
}
function balance(summary, metric = 'requests', period = 'daily') {
  const result = summary.rules.find(rule => rule.metric === metric && rule.period === period);
  assert.ok(result, `summary missing ${metric}/${period}`);
  return result;
}
async function request(key, marker = 'normal', extra = {}, base = gatewayBase) {
  return call('/v1/responses', { method: 'POST', key, base, body: { model: 'dsk:deepseek-chat', input: `fixture:${marker}`, max_output_tokens: 8, stream: false, ...extra } });
}
async function claudeRequest(key, marker = 'normal', extra = {}) {
  return call('/claude/v1/messages', { method: 'POST', key, body: { model: 'claude-sonnet-4-20250514', messages: [{ role: 'user', content: `fixture:${marker}` }], max_tokens: 8, stream: false, ...extra } });
}
async function deniedWithoutUpstream(key, marker, extra, expected = 429) {
  const before = upstreamCalls.length;
  const response = await request(key, marker, extra);
  assert.equal(response.status, expected, response.text);
  assert.equal(upstreamCalls.length, before, 'local denial must never call the upstream');
  return response;
}
async function cli(args) {
  const child = spawn(cliBinary, ['--base-url', gatewayBase, '--json', ...args], {
    cwd: temporary, env: { ...process.env, XDG_CONFIG_HOME: path.join(temporary, 'cli-config'), IOGW_BASE_URL: gatewayBase },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  let stdout = '', stderr = '';
  child.stdout.on('data', chunk => { stdout += chunk; });
  child.stderr.on('data', chunk => { stderr += chunk; });
  const [exit] = await once(child, 'exit');
  assert.equal(exit, 0, `CLI failed: ${redacted(stderr)}`);
  return JSON.parse(stdout);
}
async function gatewayCommand(args) {
  const child = spawn(gatewayBinary, args, { cwd: temporary, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout.on('data', chunk => { output += chunk; });
  child.stderr.on('data', chunk => { output += chunk; });
  const timeout = setTimeout(() => child.kill('SIGKILL'), 12000);
  try {
    const [exit] = await once(child, 'exit');
    return { exit, output: redacted(output) };
  } finally { clearTimeout(timeout); }
}

try {
  await fs.access(gatewayBinary);
  await fs.access(cliBinary);
  mock.listen(0, '127.0.0.1');
  await once(mock, 'listening');
  const mockBase = `http://127.0.0.1:${mock.address().port}`;
  const gatewayPort = await unusedPort();
  gatewayBase = `http://127.0.0.1:${gatewayPort}`;
  await writeFixture('auth/deepseek-one.json', { type: 'deepseek', account_id: 'fixture-deepseek-one', label: 'Fixture DeepSeek One', api_key: 'local-fixture-deepseek-one', base_url: `${mockBase}/deepseek-one` });
  await writeFixture('auth/deepseek-two.json', { type: 'deepseek', account_id: 'fixture-deepseek-two', label: 'Fixture DeepSeek Two', api_key: 'local-fixture-deepseek-two', base_url: `${mockBase}/deepseek-two` });
  await writeFixture('auth/claude.json', { type: 'claude', account_id: 'fixture-claude', label: 'Fixture Claude', access_token: 'local-fixture-claude', api_base_url: `${mockBase}/claude`, expires_at: '2099-01-01T00:00:00Z' });
  await writeFixture('auth/custom-models.json', { models: [
    { alias: 'quota-fixture', enabled: true, routes: [{ targets: [{ model: 'dsk:deepseek-chat', enabled: true }] }] },
    { alias: 'quota-fallback', enabled: true, routes: [{ targets: [{ model: 'cod:gpt-5.4', enabled: true }] }, { targets: [{ model: 'dsk:deepseek-chat', enabled: true }] }] },
  ] });
  await writeFixture('config.json', { listen: `127.0.0.1:${gatewayPort}`, upstream_base: `${mockBase}/codex`, proxy_api_key: legacyKey, tokens: ['local-fixture-codex'], auth_dir: authDirectory, max_concurrent_requests: 64, upstream_read_timeout_seconds: 20, upstream_first_event_timeout_seconds: 10 });
  await startGateway();
  assert.equal((await call('/admin/api-keys/quotas')).status, 200,
    'this binary lacks the new quota endpoint; run cargo build --bins before the matrix');

  await check('unconfigured keys retain working generation without renewable quota', async () => {
    const key = await createKey();
    assert.equal((await request(key)).status, 200);
    assert.equal((await call('/admin/api-keys/quotas')).value.quota_summaries[key.id], undefined);
  });

  for (const period of ['daily', 'weekly', 'monthly']) {
    await check(`${period} requests: creation is unanchored, first accepted request counts, second rejects`, async () => {
      const key = await createKey([rule('requests', period, 1)]);
      const unused = await summary(key);
      assert.equal(unused.anchor, null);
      assert.equal(balance(unused, 'requests', period).remaining, 1);
      const before = Date.now();
      assert.equal((await request(key)).status, 200);
      const after = await summary(key);
      const row = balance(after, 'requests', period);
      assert.equal(row.confirmed, 1);
      assert.equal(row.reserved, 0);
      assert.equal(row.uncertain, 0);
      assert.equal(row.remaining, 0);
      assert.ok(Date.parse(after.anchor) >= before - 1000 && Date.parse(after.anchor) <= Date.now());
      if (period !== 'monthly') assert.equal(Date.parse(row.reset_at) - Date.parse(row.window_start), period === 'daily' ? 86400000 : 604800000);
      else {
        const expected = new Date(after.anchor);
        const day = expected.getUTCDate();
        expected.setUTCDate(1);
        expected.setUTCMonth(expected.getUTCMonth() + 1);
        const lastDay = new Date(Date.UTC(expected.getUTCFullYear(), expected.getUTCMonth() + 1, 0)).getUTCDate();
        expected.setUTCDate(Math.min(day, lastDay));
        assert.equal(Date.parse(row.reset_at), expected.getTime());
      }
      const denied = await deniedWithoutUpstream(key, 'after-exhaustion', {}, 429);
      assert.equal(denied.value.error.code, 'quota_exceeded');
      assert.ok(Number(denied.headers.get('retry-after')) > 0);
      assert.equal(denied.headers.get('x-quota-reset-at'), row.reset_at);
      assert.equal(denied.headers.get('x-quota-remaining'), '0');
      assert.equal((await summary(key)).anchor, after.anchor);
    });
  }

  await check('all seven resources across all three periods settle native usage without cache double counting', async () => {
    const expected = { input_tokens: 30, uncached_input_tokens: 20, output_tokens: 3, cache_read_tokens: 6, cache_write_tokens: 4, cache_tokens: 10, requests: 1 };
    const key = await createKey(Object.keys(expected).flatMap(metric => ['daily', 'weekly', 'monthly'].map(period => rule(metric, period))));
    assert.equal((await request(key)).status, 200);
    const state = await summary(key);
    assert.equal(state.rules.length, 21);
    for (const row of state.rules) {
      assert.equal(row.confirmed, expected[row.metric], `${row.metric}/${row.period}`);
      assert.equal(row.reserved, 0);
      assert.equal(row.uncertain, 0);
      assert.equal(row.remaining, row.limit - expected[row.metric]);
      assert.equal(row.window_start, state.anchor);
    }
  });

  await check('named monthly timezone persists alongside the common first-use anchor', async () => {
    const key = await createKey([rule('requests', 'monthly', 2)], {}, 'Asia/Jakarta');
    assert.equal((await summary(key)).timezone, 'Asia/Jakarta');
    assert.equal((await request(key, 'jakarta-anchor')).status, 200);
    const state = await summary(key);
    assert.equal(state.timezone, 'Asia/Jakarta');
    assert.equal(balance(state, 'requests', 'monthly').window_start, state.anchor);
    const listed = (await call('/admin/api-keys')).value.keys.find(record => record.id === key.id);
    assert.equal(listed.access.quota.timezone, 'Asia/Jakarta');
  });

  await check('20 concurrent requests with capacity one admit exactly one upstream attempt', async () => {
    const key = await createKey([rule('requests', 'weekly', 1)]);
    const before = upstreamCalls.length;
    const responses = await Promise.all(Array.from({ length: 20 }, () => request(key, 'concurrent')));
    assert.equal(responses.filter(response => response.status === 200).length, 1);
    assert.equal(responses.filter(response => response.status === 429).length, 19);
    assert.equal(upstreamCalls.length - before, 1);
    assert.equal(balance(await summary(key), 'requests', 'weekly').confirmed, 1);
  });

  await check('a failing resource leaves every resource uncharged and does not start first-use timer', async () => {
    const key = await createKey([rule('requests', 'daily', 5), rule('input_tokens'), rule('output_tokens', 'monthly', 1)]);
    await deniedWithoutUpstream(key, 'atomic-denial', { max_output_tokens: 2 });
    const state = await summary(key);
    assert.equal(state.anchor, null);
    for (const row of state.rules) assert.equal(row.confirmed + row.reserved + row.uncertain, 0);
  });

  await check('strict output boundary reserves the provider cap and refunds only trustworthy unused tokens', async () => {
    const key = await createKey([rule('output_tokens', 'daily', 5)]);
    assert.equal((await request(key, 'output-five', { max_output_tokens: 5 })).status, 200);
    assert.equal(balance(await summary(key), 'output_tokens').confirmed, 3);
    assert.equal(balance(await summary(key), 'output_tokens').remaining, 2);
    await deniedWithoutUpstream(key, 'output-over', { max_output_tokens: 3 });
    assert.equal((await request(key, 'output-two', { max_output_tokens: 2 })).status, 200);
    assert.equal(balance(await summary(key), 'output_tokens').confirmed, 5);
    await deniedWithoutUpstream(key, 'output-exhausted', { max_output_tokens: 1 });
  });

  await check('an anomalous provider overage is recorded honestly rather than clamped to the rule', async () => {
    const key = await createKey([rule('output_tokens', 'daily', 8)]);
    assert.equal((await request(key, 'reported-overage', { max_output_tokens: 8 })).status, 200);
    const state = balance(await summary(key), 'output_tokens');
    assert.equal(state.confirmed, 13);
    assert.equal(state.remaining, 0);
    await deniedWithoutUpstream(key, 'after-anomalous-overage', { max_output_tokens: 1 });
  });

  await check('in-flight input is reserved separately, then actual below the reservation is reconciled', async () => {
    const key = await createKey([rule('input_tokens'), rule('requests')]);
    const ongoing = request(key, 'hold-observe');
    ongoing.catch(() => {}); // Keep a failed assertion from producing an unhandled rejection.
    await eventually(() => slowResponses.has('fixture:hold-observe'), 'mock must receive held request');
    const held = await summary(key);
    const before = balance(held, 'input_tokens');
    assert.ok(before.reserved > 30, 'serialized upper bound must exceed the small fixture actual');
    assert.equal(before.confirmed, 0);
    assert.equal(before.uncertain, 0);
    slowResponses.get('fixture:hold-observe').reply();
    assert.equal((await ongoing).status, 200);
    const after = balance(await summary(key), 'input_tokens');
    assert.equal(after.confirmed, 30);
    assert.equal(after.reserved, 0);
    assert.equal(after.uncertain, 0);
    assert.equal(after.remaining, after.limit - 30);
  });

  await check('missing usage retains uncertain consumption rather than fabricating zero', async () => {
    const key = await createKey([rule('input_tokens'), rule('output_tokens'), rule('requests')]);
    assert.equal((await request(key, 'missing-usage')).status, 200);
    const state = await summary(key);
    assert.ok(balance(state, 'input_tokens').uncertain > 0);
    assert.equal(balance(state, 'output_tokens').uncertain, 8);
    assert.equal(balance(state).confirmed, 1);
    for (const row of state.rules) assert.equal(row.reserved, 0);
  });

  await check('partial native cache breakdown retains unknown input but settles known output', async () => {
    const key = await createKey([rule('input_tokens'), rule('uncached_input_tokens'), rule('output_tokens'), rule('cache_tokens')]);
    assert.equal((await request(key, 'partial-usage')).status, 200);
    const state = await summary(key);
    assert.ok(balance(state, 'input_tokens').uncertain > 0);
    assert.ok(balance(state, 'cache_tokens').uncertain > 0);
    assert.equal(balance(state, 'uncached_input_tokens').confirmed, 20);
    assert.equal(balance(state, 'output_tokens').confirmed, 3);
  });

  await check('explicit reported zero differs from omitted usage', async () => {
    const key = await createKey([rule('input_tokens'), rule('output_tokens'), rule('cache_tokens'), rule('requests')]);
    assert.equal((await request(key, 'explicit-zero')).status, 200);
    const state = await summary(key);
    for (const row of state.rules.filter(row => row.metric !== 'requests')) {
      assert.equal(row.confirmed + row.reserved + row.uncertain, 0);
      assert.equal(row.remaining, row.limit);
    }
    assert.equal(balance(state).confirmed, 1);
  });

  await check('native Claude success preserves all cache categories', async () => {
    const key = await createKey([rule('input_tokens'), rule('uncached_input_tokens'), rule('output_tokens'), rule('cache_read_tokens'), rule('cache_write_tokens'), rule('cache_tokens'), rule('requests')]);
    const result = await claudeRequest(key);
    assert.equal(result.status, 200, result.text);
    const state = await summary(key);
    assert.equal(balance(state, 'input_tokens').confirmed, 30);
    assert.equal(balance(state, 'cache_read_tokens').confirmed, 6);
    assert.equal(balance(state, 'cache_write_tokens').confirmed, 4);
    assert.equal(balance(state, 'cache_tokens').confirmed, 10);
    assert.equal(balance(state, 'output_tokens').confirmed, 3);
  });

  await check('native Claude quota failures are JSON Anthropic errors with reset headers', async () => {
    const key = await createKey([rule('requests', 'daily', 1)]);
    assert.equal((await claudeRequest(key)).status, 200);
    const before = upstreamCalls.length;
    const denied = await claudeRequest(key);
    assert.equal(denied.status, 429);
    assert.match(denied.headers.get('content-type'), /application\/json/);
    assert.equal(denied.value.type, 'error');
    assert.equal(denied.value.error.type, 'rate_limit_error');
    assert.ok(denied.headers.get('retry-after'));
    assert.equal(upstreamCalls.length, before);
  });

  await check('completed Claude SSE merges input/cache start with final output exactly once', async () => {
    const key = await createKey([rule('input_tokens'), rule('output_tokens'), rule('cache_tokens'), rule('requests')]);
    const result = await claudeRequest(key, 'stream-complete', { stream: true });
    assert.equal(result.status, 200);
    assert.match(result.text, /message_stop/);
    await eventually(async () => balance(await summary(key)).reserved === 0, 'SSE settlement must complete');
    const state = await summary(key);
    assert.equal(balance(state, 'input_tokens').confirmed, 30);
    assert.equal(balance(state, 'output_tokens').confirmed, 3);
    assert.equal(balance(state, 'cache_tokens').confirmed, 10);
  });

  for (const marker of ['stream-error', 'stream-incomplete']) {
    await check(`Claude ${marker} retains uncertain token consumption`, async () => {
      // Account-health cooldown from the previous intentionally failed stream
      // must not mask quota behavior in the next independent failure scenario.
      await stopGateway();
      await startGateway();
      const key = await createKey([rule('input_tokens'), rule('output_tokens'), rule('requests')]);
      const result = await claudeRequest(key, marker, { stream: true });
      assert.equal(result.status, 200);
      await eventually(async () => balance(await summary(key), 'input_tokens').reserved === 0, 'incomplete stream must settle conservatively');
      const state = await summary(key);
      assert.ok(balance(state, 'input_tokens').uncertain > 0);
      assert.equal(balance(state, 'output_tokens').uncertain, 8);
      assert.equal(balance(state).confirmed, 1);
    });
  }

  await check('client cancellation during a Claude stream retains a durable uncertain charge', async () => {
    await stopGateway();
    await startGateway();
    const key = await createKey([rule('input_tokens'), rule('output_tokens'), rule('requests', 'daily', 1)]);
    const controller = new AbortController();
    const response = await fetch(`${gatewayBase}/claude/v1/messages`, {
      method: 'POST', signal: AbortSignal.any([controller.signal, AbortSignal.timeout(12000)]),
      headers: { authorization: `Bearer ${key.plain}`, 'content-type': 'application/json' },
      body: JSON.stringify({ model: 'claude-sonnet-4-20250514', max_tokens: 8, stream: true,
        messages: [{ role: 'user', content: 'fixture:stream-disconnect' }] }),
    });
    assert.equal(response.status, 200);
    const reader = response.body.getReader();
    assert.ok((await reader.read()).value.length > 0);
    await reader.cancel();
    controller.abort();
    await eventually(async () => balance(await summary(key), 'input_tokens').uncertain > 0,
      'cancelled stream must settle its durable reservation conservatively');
    const state = await summary(key);
    assert.equal(balance(state, 'input_tokens').reserved, 0);
    assert.equal(balance(state, 'output_tokens').uncertain, 8);
    assert.equal(balance(state).confirmed, 1);
    await deniedWithoutUpstream(key, 'after-client-cancel');
  });

  await check('unsupported strict Codex output is rejected before upstream contact or request charge', async () => {
    const key = await createKey([rule('requests'), rule('output_tokens')]);
    const result = await deniedWithoutUpstream(key, 'codex-output', { model: 'cod:gpt-5.4' }, 400);
    assert.equal(result.value.error.code, 'quota_measurement_required');
    assert.equal((await summary(key)).anchor, null);
  });

  await check('request-only Codex policy remains usable', async () => {
    const key = await createKey([rule('requests', 'daily', 1)]);
    const result = await request(key, 'codex-request', { model: 'cod:gpt-5.4' });
    assert.equal(result.status, 200, result.text);
    await eventually(async () => balance(await summary(key)).reserved === 0, 'Codex settlement must complete');
    assert.equal(balance(await summary(key)).confirmed, 1);
    await deniedWithoutUpstream(key, 'codex-request-over', { model: 'cod:gpt-5.4' });
  });

  for (const marker of ['native-responses', 'terminal-incomplete']) {
    await check(`Codex ${marker} preserves authoritative input and cache subsets through SSE conversion`, async () => {
      const key = await createKey([rule('input_tokens'), rule('uncached_input_tokens'), rule('cache_read_tokens'), rule('cache_write_tokens'), rule('cache_tokens'), rule('requests')]);
      const response = await request(key, marker, { model: 'cod:gpt-5.4' });
      assert.equal(response.status, 200, response.text);
      const state = await summary(key);
      for (const [metric, expected] of Object.entries({ input_tokens: 30, uncached_input_tokens: 20, cache_read_tokens: 6, cache_write_tokens: 4, cache_tokens: 10, requests: 1 })) {
        const row = balance(state, metric);
        assert.equal(row.confirmed, expected, metric);
        assert.equal(row.reserved + row.uncertain, 0, metric);
      }
    });
  }

  await check('input quota rejects media and retained-context payloads before upstream contact', async () => {
    const key = await createKey([rule('input_tokens'), rule('requests')]);
    for (const extra of [
      { input: [{ role: 'user', content: [{ type: 'input_image', image_url: 'https://invalid.example/image.png' }] }] },
      { previous_response_id: 'retained-fixture-context' },
      { input: [{ type: 'compaction', encrypted_content: 'opaque-fixture-context' }] },
    ]) await deniedWithoutUpstream(key, 'unmeasurable', extra, 400);
    assert.equal((await summary(key)).anchor, null);
  });

  await check('native Claude remote MCP context fails closed while empty connectors remain usable', async () => {
    const key = await createKey([rule('input_tokens'), rule('requests')]);
    const before = upstreamCalls.length;
    const denied = await claudeRequest(key, 'native-mcp', {
      mcp_servers: [{ type: 'url', name: 'fixture', url: 'https://invalid.example/mcp' }],
    });
    assert.equal(denied.status, 400, denied.text);
    assert.equal(upstreamCalls.length, before);
    assert.equal((await summary(key)).anchor, null);
    for (const mcp_servers of [[], null]) {
      const accepted = await claudeRequest(key, 'empty-mcp', { mcp_servers });
      assert.equal(accepted.status, 200, accepted.text);
    }
    assert.equal(balance(await summary(key)).confirmed, 2);
  });

  await check('native Claude literal tool arguments remain measurable without hiding adjacent remote context', async () => {
    const key = await createKey([rule('input_tokens'), rule('requests')], {
      max_estimated_input_tokens_per_request: 10000,
    });
    const toolUse = { type: 'tool_use', id: 'fixture-call', name: 'lookup', input: {
      context: 'a plaintext query', file_id: 'a literal label',
    } };
    const messages = [
      { role: 'user', content: 'fixture:literal-arguments' },
      { role: 'assistant', content: [toolUse] },
      { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'fixture-call', content: 'literal result' }] },
    ];
    const accepted = await claudeRequest(key, 'literal-arguments', { messages });
    assert.equal(accepted.status, 200, accepted.text);
    assert.equal(balance(await summary(key)).confirmed, 1);
    const before = upstreamCalls.length;
    const denied = await claudeRequest(key, 'literal-arguments-with-media', { messages: [
      ...messages, { role: 'user', content: [{ type: 'image', source: {
        type: 'url', url: 'https://invalid.example/image.png',
      } }] },
    ] });
    assert.equal(denied.status, 400, denied.text);
    assert.equal(upstreamCalls.length, before);
    assert.equal(balance(await summary(key)).confirmed, 1);
  });

  await check('too-small input rule rejects complete prepared request without charging', async () => {
    const key = await createKey([rule('input_tokens', 'daily', 1), rule('requests')]);
    await deniedWithoutUpstream(key, 'input-over');
    assert.equal((await summary(key)).anchor, null);
  });

  for (const metric of ['uncached_input_tokens', 'cache_read_tokens', 'cache_write_tokens', 'cache_tokens']) {
    await check(`${metric} reserves its conservative bound and rejects insufficient allowance`, async () => {
      const key = await createKey([rule(metric, 'weekly', 1), rule('requests')]);
      await deniedWithoutUpstream(key, 'cache-boundary');
      const state = await summary(key);
      assert.equal(state.anchor, null);
      assert.equal(balance(state, metric, 'weekly').remaining, 1);
      assert.equal(balance(state).confirmed + balance(state).reserved, 0);
    });
  }

  await check('old per-request input caps remain enforced alongside renewable request quotas', async () => {
    const key = await createKey([rule('requests')], { max_estimated_input_tokens_per_request: 1 });
    await deniedWithoutUpstream(key, 'old-cap');
    assert.equal((await summary(key)).anchor, null);
  });

  await check('custom alias uses the same quota enforcement and error status', async () => {
    const key = await createKey([rule('requests', 'daily', 1)]);
    assert.equal((await request(key, 'alias', { model: 'ctm:quota-fixture' })).status, 200);
    const denied = await deniedWithoutUpstream(key, 'alias-over', { model: 'ctm:quota-fixture' });
    assert.equal(denied.value.error.code, 'quota_exceeded');
    assert.equal(balance(await summary(key)).confirmed, 1);
  });

  await check('custom alias skips unsupported Codex strict output and counts its DeepSeek fallback once', async () => {
    const key = await createKey([rule('requests', 'daily', 1), rule('output_tokens')]);
    const before = upstreamCalls.length;
    const result = await request(key, 'alias-supported-fallback', { model: 'ctm:quota-fallback' });
    assert.equal(result.status, 200, result.text);
    assert.equal(upstreamCalls.length - before, 1);
    assert.match(upstreamCalls.at(-1).path, /deepseek/);
    assert.equal(balance(await summary(key)).confirmed, 1);
    assert.equal(balance(await summary(key), 'output_tokens').confirmed, 3);
  });

  await check('managed-key dashboard Test API consumes and enforces the identical request quota', async () => {
    const key = await createKey([rule('requests', 'weekly', 1)]);
    const body = { model: 'dsk:deepseek-chat', prompt: 'fixture:managed-test', max_output_tokens: 8, api_key_id: key.id };
    const first = await call('/admin/test-api', { method: 'POST', body });
    assert.equal(first.status, 200, first.text);
    assert.equal(first.value.policy_mode, 'managed_api_key');
    const before = upstreamCalls.length;
    const second = await call('/admin/test-api', { method: 'POST', body });
    assert.equal(second.status, 429, second.text);
    assert.equal(upstreamCalls.length, before);
    assert.equal(balance(await summary(key), 'requests', 'weekly').confirmed, 1);
  });

  await check('health, model listing, malformed and administrative reads do not anchor quota', async () => {
    const key = await createKey([rule('requests', 'daily', 1)]);
    assert.equal((await call('/health', { key })).status, 200);
    assert.equal((await call('/v1/models', { key })).status, 200);
    assert.equal((await call('/admin/api-keys')).status, 200);
    assert.equal((await call('/v1/responses', { method: 'POST', key, body: { input: 'no model' } })).status, 400);
    const state = await summary(key);
    assert.equal(state.anchor, null);
    assert.equal(balance(state).remaining, 1);
  });

  await check('request-only policies reject malformed/nonobject JSON without an anchor or upstream call', async () => {
    const key = await createKey([rule('requests', 'daily', 1)]);
    const before = upstreamCalls.length;
    for (const body of ['{', 'null', '[]', '"opaque"', '42']) {
      const malformed = await rawCall('/v1/responses', { key, body });
      assert.equal(malformed.status, 400, malformed.text);
    }
    assert.equal(upstreamCalls.length, before);
    const state = await summary(key);
    assert.equal(state.anchor, null);
    assert.equal(balance(state).remaining, 1);
  });

  await check('request-only policies reject gzip input without an anchor or upstream call', async () => {
    const key = await createKey([rule('requests', 'daily', 1)]);
    const before = upstreamCalls.length;
    const compressed = await rawCall('/v1/responses', { key,
      body: gzipSync(JSON.stringify({ model: 'dsk:deepseek-chat', input: 'fixture:compressed', max_output_tokens: 8 })),
      headers: { 'content-encoding': 'gzip' },
    });
    assert.ok([400, 415].includes(compressed.status), compressed.text);
    assert.equal(upstreamCalls.length, before);
    const state = await summary(key);
    assert.equal(state.anchor, null);
    assert.equal(balance(state).remaining, 1);
  });

  await check('ignored GET and DELETE bodies do not reserve generation allowance', async () => {
    const key = await createKey([rule('requests', 'daily', 1), rule('input_tokens')]);
    const before = upstreamCalls.length;
    const body = JSON.stringify({ model: 'cod:gpt-5.4', input: 'fixture:ignored-body' });
    for (const method of ['GET', 'DELETE']) {
      await rawCall('/v1/videos/fixture-nonexistent-job', { key, method, body });
    }
    assert.equal(upstreamCalls.length, before);
    const state = await summary(key);
    assert.equal(state.anchor, null);
    for (const row of state.rules) assert.equal(row.confirmed + row.reserved + row.uncertain, 0);
  });

  await check('ordinary limit edits preserve anchor and accumulated usage', async () => {
    const key = await createKey([rule('requests', 'weekly', 1)]);
    assert.equal((await request(key)).status, 200);
    const before = await summary(key);
    const access = { ...key.access, quota: policy([rule('requests', 'weekly', 2)]) };
    assert.equal((await call('/admin/api-keys/access', { method: 'POST', body: { id: key.id, access } })).status, 200);
    const edited = await summary(key);
    assert.equal(edited.anchor, before.anchor);
    assert.equal(balance(edited, 'requests', 'weekly').confirmed, 1);
    assert.equal(balance(edited, 'requests', 'weekly').remaining, 1);
    assert.equal((await request(key)).status, 200);
    await deniedWithoutUpstream(key, 'edited-exhausted');
    assert.equal((await call('/admin/api-keys/access', { method: 'POST', body: { id: key.id, access: key.access } })).status, 200);
    const lowered = balance(await summary(key), 'requests', 'weekly');
    assert.equal(lowered.limit, 1);
    assert.equal(lowered.confirmed, 2, 'lowering a rule must not clamp or erase already consumed usage');
    assert.equal(lowered.remaining, 0);
  });

  await check('activated policy cannot be reshaped to silently erase usage', async () => {
    const key = await createKey([rule('requests', 'daily', 1)]);
    assert.equal((await request(key)).status, 200);
    for (const quota of [policy([rule('requests', 'weekly', 1)]), policy([rule('requests', 'daily', 1)], 'Asia/Jakarta')]) {
      const result = await call('/admin/api-keys/access', { method: 'POST', body: { id: key.id, access: { all: true, quota } } });
      assert.equal(result.status, 400, 'activated structural change must be a clear client error');
    }
    assert.equal(balance(await summary(key)).confirmed, 1);
    await deniedWithoutUpstream(key, 'shape-still-exhausted');
  });

  await check('disable and re-enable preserves prior usage and schedule', async () => {
    const key = await createKey([rule('requests', 'daily', 1)]);
    assert.equal((await request(key)).status, 200);
    const original = await summary(key);
    assert.equal((await call('/admin/api-keys/access', { method: 'POST', body: { id: key.id, access: { all: true, quota: null } } })).status, 200);
    assert.equal((await call('/admin/api-keys/access', { method: 'POST', body: { id: key.id, access: key.access } })).status, 200);
    const restored = await summary(key);
    assert.equal(restored.anchor, original.anchor);
    assert.equal(balance(restored).confirmed, 1);
    await deniedWithoutUpstream(key, 'reactivated-exhausted');
  });

  await check('invalid policies reject zero, negatives, overflow, duplicate rules, unknown metrics and zones', async () => {
    const invalid = [
      policy([rule('requests', 'daily', 0)]), policy([rule('requests', 'daily', -1)]),
      policy([rule('requests', 'daily', 1.5)]), policy([rule('requests', 'daily', 1e20)]),
      policy([rule('requests'), rule('requests')]), policy([]),
      policy([rule('not_a_metric')]), policy([rule('requests', 'rolling')]),
      policy([rule('requests')], 'Not/AZone'), { ...policy([rule('requests')]), typo: true },
    ];
    const before = (await call('/admin/api-keys')).value.keys.length;
    for (const quota of invalid) {
      const response = await call('/admin/api-keys/create', { method: 'POST', body: { label: 'invalid fixture', access: { all: true, quota } } });
      assert.ok(response.status >= 400 && response.status < 500, `invalid policy must produce a client error, got ${response.status}`);
    }
    assert.equal((await call('/admin/api-keys')).value.keys.length, before);
  });

  await check('CLI create, quota summary and access update agree with HTTP authority', async () => {
    const created = await cli(['keys', 'create', '--label', 'CLI fixture', '--quota-json', JSON.stringify(policy([rule('requests', 'monthly', 1)]))]);
    const plain = created.plain_text_key;
    assert.ok(plain, 'CLI JSON create must expose one-time credential');
    secrets.add(plain);
    const key = { id: created.key.id, plain };
    assert.equal((await request(key)).status, 200);
    const response = await cli(['keys', 'quotas', '--id', key.id]);
    assert.equal(balance(response.quota_summaries[key.id], 'requests', 'monthly').confirmed, 1);
    await cli(['keys', 'update', key.id, '--access-json', JSON.stringify({ all: true, quota: policy([rule('requests', 'monthly', 2)]) })]);
    assert.equal(balance(await summary(key), 'requests', 'monthly').remaining, 1);
  });

  await check('admin policy typos fail before key creation or access mutation', async () => {
    const key = await createKey([rule('requests', 'daily', 1)]);
    const original = (await call('/admin/api-keys')).value.keys;
    const invalidPayloads = [
      { accesss: key.access },
      { access: { all: true, quotas: key.access.quota } },
      { access: { all: true, input_token_budget: { limit: 10, period: 'lifetime', limt: 1 } } },
      { access: { all: false, providers: [{ provider: 'claude', account_scope: 'all', max_input_tokens: 1 }] } },
      { access: { all: true, quota: key.access.quota }, unexpected: true },
    ];
    for (const payload of invalidPayloads) {
      const created = await call('/admin/api-keys/create', { method: 'POST', body: { label: 'typo fixture', ...payload } });
      assert.ok(created.status >= 400 && created.status < 500, created.text);
      const updated = await call('/admin/api-keys/access', { method: 'POST', body: { id: key.id, ...payload } });
      assert.ok(updated.status >= 400 && updated.status < 500, updated.text);
    }
    assert.deepEqual((await call('/admin/api-keys')).value.keys, original);
    assert.equal((await summary(key)).anchor, null);
  });

  await check('CLI explicit cap overrides access JSON without losing renewable quotas', async () => {
    const created = await cli(['keys', 'create', '--label', 'CLI combined flags fixture',
      '--access-json', JSON.stringify({ all: true, prompt_token_limit: 999 }),
      '--quota-json', JSON.stringify(policy([rule('requests', 'weekly', 1)])),
      '--max-estimated-input-tokens-per-request', '1']);
    assert.ok(created.plain_text_key);
    secrets.add(created.plain_text_key);
    const key = { id: created.key.id, plain: created.plain_text_key };
    await deniedWithoutUpstream(key, 'cli-combined-flags');
    const state = await summary(key);
    assert.equal(state.anchor, null);
    assert.equal(balance(state, 'requests', 'weekly').remaining, 1);
  });

  await check('retry reserves tokens for each upstream attempt but counts the client request only once', async () => {
    const key = await createKey([rule('input_tokens'), rule('output_tokens'), rule('requests', 'daily', 1)]);
    const before = upstreamCalls.length;
    const result = await request(key, 'retry-once-main');
    assert.equal(result.status, 200, result.text);
    assert.equal(upstreamCalls.length - before, 2);
    const state = await summary(key);
    assert.equal(balance(state).confirmed, 1);
    assert.equal(balance(state).uncertain, 0);
    assert.equal(balance(state, 'input_tokens').confirmed, 30);
    assert.ok(balance(state, 'input_tokens').uncertain > 0);
    assert.equal(balance(state, 'output_tokens').confirmed, 3);
    assert.equal(balance(state, 'output_tokens').uncertain, 8);
    await deniedWithoutUpstream(key, 'retry-exhausted');
  });

  await check('completed quota usage, anchor and denial survive clean server restart', async () => {
    const key = await createKey([rule('requests', 'daily', 1), rule('input_tokens')]);
    assert.equal((await request(key)).status, 200);
    const before = await summary(key);
    await stopGateway();
    await startGateway();
    assert.deepEqual(await summary(key), before);
    await deniedWithoutUpstream(key, 'restart-exhausted');
  });

  await check('two gateway processes share atomic admission, live edits and revocation authority', async () => {
    const secondaryPort = await unusedPort();
    const secondaryBase = `http://127.0.0.1:${secondaryPort}`;
    const config = JSON.parse(await fs.readFile(configFile, 'utf8'));
    await writeFixture('secondary-config.json', { ...config, listen: `127.0.0.1:${secondaryPort}` });
    const secondary = await startAuxiliary(path.join(temporary, 'secondary-config.json'), secondaryBase);
    try {
      const key = await createKey([rule('requests', 'weekly', 1)]);
      const before = upstreamCalls.length;
      const responses = await Promise.all(Array.from({ length: 20 }, (_, index) =>
        request(key, 'two-process-concurrent', {}, index % 2 ? gatewayBase : secondaryBase)));
      assert.equal(responses.filter(response => response.status === 200).length, 1);
      assert.equal(responses.filter(response => response.status === 429).length, 19);
      assert.equal(upstreamCalls.length - before, 1);
      assert.equal(balance(await summary(key), 'requests', 'weekly').confirmed, 1);

      const warm = await createKey([rule('requests', 'daily', 2)]);
      assert.equal((await request(warm, 'secondary-cache-warmup', {}, secondaryBase)).status, 200);
      assert.equal((await call('/admin/api-keys/access', { method: 'POST', body: {
        id: warm.id, access: { all: true, quota: policy([rule('requests', 'daily', 1)]) },
      } })).status, 200);
      const beforeDenials = upstreamCalls.length;
      assert.equal((await request(warm, 'secondary-after-edit', {}, secondaryBase)).status, 429);
      assert.equal((await call('/admin/api-keys/revoke', { method: 'POST', body: { id: warm.id } })).status, 200);
      assert.equal((await request(warm, 'secondary-after-revoke', {}, secondaryBase)).status, 401);
      assert.equal(upstreamCalls.length, beforeDenials);
      assert.equal(balance(await summary(warm)).confirmed, 1);
    } finally { await stopProcess(secondary); }
  });

  await check('SIGKILL after dispatch cannot restore held allowance or change the first-use anchor', async () => {
    const key = await createKey([rule('requests', 'weekly', 1), rule('input_tokens')]);
    const ongoing = request(key, 'hold-crash').catch(() => null);
    await eventually(() => slowResponses.has('fixture:hold-crash'), 'in-flight request must reach mock');
    const before = await summary(key);
    assert.equal(balance(before, 'requests', 'weekly').remaining, 0);
    assert.ok(balance(before, 'input_tokens').reserved > 0);
    await stopGateway('SIGKILL');
    await ongoing;
    for (const [marker, held] of slowResponses) {
      if (marker === 'fixture:hold-crash') held.response.destroy();
    }
    await startGateway();
    const after = await summary(key);
    assert.equal(after.anchor, before.anchor);
    for (const row of after.rules) {
      const previous = balance(before, row.metric, row.period);
      assert.equal(row.confirmed + row.reserved + row.uncertain, previous.confirmed + previous.reserved + previous.uncertain);
      assert.equal(row.remaining, previous.remaining);
    }
    await deniedWithoutUpstream(key, 'crash-held-exhausted');
  });

  await check('established private authority and WAL metadata have owner-only permissions', async () => {
    const names = await fs.readdir(authDirectory);
    for (const name of names.filter(name => name.startsWith('api-key') || name.startsWith('api-keys'))) {
      const stat = await fs.stat(path.join(authDirectory, name));
      assert.equal(stat.mode & 0o077, 0, `${name} must not be group/world accessible`);
    }
    assert.ok(names.includes('api-key-policy.sqlite3'));
  });

  await check('online backup includes durable WAL accounting and restores identical quota authority', async () => {
    const key = await createKey([rule('requests', 'monthly', 1), rule('input_tokens')]);
    assert.equal((await request(key, 'backup-usage')).status, 200);
    const before = await summary(key);
    const backupDirectory = path.join(temporary, 'policy-backup');
    const result = await gatewayCommand(['--config', configFile, '--backup-policy', backupDirectory]);
    assert.equal(result.exit, 0, result.output);
    assert.equal((await fs.stat(backupDirectory)).mode & 0o077, 0);
    for (const name of await fs.readdir(backupDirectory)) {
      assert.equal((await fs.stat(path.join(backupDirectory, name))).mode & 0o077, 0, name);
    }
    const manifest = JSON.parse(await fs.readFile(path.join(backupDirectory, 'backup-manifest.json'), 'utf8'));
    assert.equal(manifest.restore_requires_usage_reconciliation, true);
    assert.ok(manifest.excludes.includes('provider_credentials'));
    const overwritten = await gatewayCommand(['--config', configFile, '--backup-policy', backupDirectory]);
    assert.notEqual(overwritten.exit, 0, 'backup must not overwrite a prior snapshot');
    for (const name of ['deepseek-one.json', 'deepseek-two.json', 'claude.json', 'custom-models.json']) {
      await fs.copyFile(path.join(authDirectory, name), path.join(backupDirectory, name));
    }
    const config = JSON.parse(await fs.readFile(configFile, 'utf8'));
    await writeFixture('restore-config.json', { ...config, auth_dir: backupDirectory });
    await stopGateway();
    try {
      await startGateway(path.join(temporary, 'restore-config.json'));
      assert.deepEqual(await summary(key), before);
      await deniedWithoutUpstream(key, 'restore-exhausted');
    } finally {
      await stopGateway();
      await startGateway();
    }
  });

  await check('missing established database refuses startup instead of restoring spent allowance', async () => {
    const directory = path.join(temporary, 'missing-store-backup');
    const snapshot = await gatewayCommand(['--config', configFile, '--backup-policy', directory]);
    assert.equal(snapshot.exit, 0, snapshot.output);
    const database = path.join(directory, 'api-key-policy.sqlite3');
    await fs.rename(database, path.join(directory, 'withheld-original.sqlite3'));
    const config = JSON.parse(await fs.readFile(configFile, 'utf8'));
    await writeFixture('missing-config.json', { ...config, auth_dir: directory });
    const startup = await gatewayCommand(['--config', path.join(temporary, 'missing-config.json')]);
    assert.notEqual(startup.exit, 0);
    assert.match(startup.output, /missing|absent|refusing/i);
    assert.equal(await fs.access(database).then(() => true, () => false), false,
      'failed startup must not initialize a replacement empty database');
  });

  for (const damage of ['mismatched identity', 'corrupt database']) {
    await check(`${damage} refuses startup without replacing established accounting`, async () => {
      const directory = path.join(temporary, `damaged-${damage.replaceAll(' ', '-')}`);
      const snapshot = await gatewayCommand(['--config', configFile, '--backup-policy', directory]);
      assert.equal(snapshot.exit, 0, snapshot.output);
      const database = path.join(directory, 'api-key-policy.sqlite3');
      if (damage === 'mismatched identity') {
        const marker = `${database}.identity`;
        const identity = JSON.parse(await fs.readFile(marker, 'utf8'));
        identity.database_id = '00000000-0000-4000-8000-000000000001';
        await fs.writeFile(marker, JSON.stringify(identity), { mode: 0o600 });
      } else {
        await fs.writeFile(database, 'synthetic broken SQLite database', { mode: 0o600 });
      }
      const config = JSON.parse(await fs.readFile(configFile, 'utf8'));
      const damagedConfigName = `damaged-${damage.replaceAll(' ', '-')}-config.json`;
      await writeFixture(damagedConfigName, { ...config, auth_dir: directory });
      const startup = await gatewayCommand(['--config', path.join(temporary, damagedConfigName)]);
      assert.notEqual(startup.exit, 0);
      assert.match(startup.output, damage === 'mismatched identity' ? /identity|mismatch/i : /database|sqlite/i);
      if (damage === 'corrupt database') assert.equal(await fs.readFile(database, 'utf8'), 'synthetic broken SQLite database');
    });
  }

  const failed = checks.filter(check => !check.passed);
  process.stdout.write(`\n${checks.length - failed.length}/${checks.length} end-to-end quota checks passed; ${upstreamCalls.length} synthetic localhost generation attempts.\n`);
  process.stdout.write('No production credentials, provider quota, repository configuration or live gateway were used.\n');
  if (failed.length) process.exitCode = 1;
} finally {
  await stopGateway();
  for (const child of auxiliaryProcesses) await stopProcess(child);
  for (const { response } of slowResponses.values()) response.destroy();
  mock.closeAllConnections();
  if (mock.listening) await new Promise(resolve => mock.close(resolve));
  // Only this run's validated mkdtemp directory is removed.
  assert.equal(path.dirname(temporary), os.tmpdir());
  assert.ok(path.basename(temporary).startsWith('io-gateway-renewable-quota-test-'));
  await fs.rm(temporary, { recursive: true, force: true });
}
