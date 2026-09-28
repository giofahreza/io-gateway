#!/usr/bin/env node
// Local-only end-to-end smoke test for durable Codex reset-credit automation.
// It creates synthetic gateway credentials and a private fake Codex App
// Server speaking JSON-RPC JSONL on stdio. It never reads real credentials,
// calls a provider, or starts a Wham mock.
//
// Usage: cargo build --bin io-gateway && node scripts/test-codex-reset-credit-automation.mjs
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import fs from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repository = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const gatewayBinary = path.resolve(
  process.env.CODEX_RESET_CREDIT_TEST_GATEWAY_BINARY
    || path.join(repository, 'target/debug/io-gateway'),
);
const temporary = await fs.mkdtemp(path.join(os.tmpdir(), 'io-gateway-reset-credit-test-'));
const authDirectory = path.join(temporary, 'auth');
const profileRoot = path.join(temporary, 'managed-codex-profiles');
const stateFile = path.join(profileRoot, '.fake-app-server-state.json');
const fakeAppServer = path.join(temporary, 'fake-codex-app-server.mjs');
const fakeCodex = path.join(temporary, 'fake-codex');
const configFile = path.join(temporary, 'config.json');
const accounts = {
  normal: 'fixture-reset-credit-normal',
  zero: 'fixture-reset-credit-zero',
  unknown: 'fixture-reset-credit-unknown',
};
const profiles = {
  [accounts.normal]: 'normal',
  [accounts.zero]: 'zero',
  [accounts.unknown]: 'unknown',
};
const emails = {
  [accounts.normal]: 'normal@example.test',
  [accounts.zero]: 'zero@example.test',
  [accounts.unknown]: 'unknown@example.test',
};
const secrets = [
  'fixture-reset-credit-proxy-key',
  'fixture-reset-credit-normal-token',
  'fixture-reset-credit-zero-token',
  'fixture-reset-credit-unknown-token',
];
const normalCredit = 'fixture-normal-credit';
const zeroCredit = 'fixture-zero-count-forged-credit';
const unknownCredit = 'fixture-unknown-natural-reset-credit';
let gateway;
let gatewayBase;
let gatewayLog = '';

const sleep = milliseconds => new Promise(resolve => setTimeout(resolve, milliseconds));

function redact(value) {
  let text = String(value);
  for (const secret of secrets) text = text.replaceAll(secret, '[synthetic credential]');
  return text.replaceAll(temporary, '[private fixture]');
}

async function eventually(predicate, message, timeout = 12_000) {
  const started = Date.now();
  let lastError;
  while (Date.now() - started < timeout) {
    try {
      if (await predicate()) return;
    } catch (error) {
      lastError = error;
    }
    await sleep(30);
  }
  throw new Error(message + (lastError ? ': ' + redact(lastError.message || lastError) : ''));
}

async function unusedPort() {
  const server = net.createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const port = server.address().port;
  await new Promise(resolve => server.close(resolve));
  return port;
}

async function writePrivate(file, value) {
  await fs.writeFile(file, JSON.stringify(value), { mode: 0o600 });
  await fs.chmod(file, 0o600);
}

async function state() {
  return JSON.parse(await fs.readFile(stateFile, 'utf8'));
}

function shellQuote(value) {
  return "'" + String(value).replaceAll("'", "'\"'\"'") + "'";
}

async function writeFakeAppServer() {
  // The gateway clears the child environment. The fake derives shared
  // dummy state only from the private parent of CODEX_HOME; no credential or
  // custom test environment variable reaches the child.
  const source = String.raw`#!/usr/bin/env node
import fs from 'node:fs';
import path from 'node:path';
import readline from 'node:readline';

const accounts = {
  normal: 'fixture-reset-credit-normal',
  zero: 'fixture-reset-credit-zero',
  unknown: 'fixture-reset-credit-unknown',
};
const emails = {
  [accounts.normal]: 'normal@example.test',
  [accounts.zero]: 'zero@example.test',
  [accounts.unknown]: 'unknown@example.test',
};
const normalCredit = 'fixture-normal-credit';
const zeroCredit = 'fixture-zero-count-forged-credit';
const unknownCredit = 'fixture-unknown-natural-reset-credit';
const home = process.env.CODEX_HOME || '';
const profile = path.basename(home);
const account = accounts[profile];
const stateFile = path.join(path.dirname(home), '.fake-app-server-state.json');

function readState() {
  return JSON.parse(fs.readFileSync(stateFile, 'utf8'));
}
function writeState(value) {
  fs.writeFileSync(stateFile, JSON.stringify(value), { mode: 0o600 });
}
function record(event) {
  const value = readState();
  value.events.push(event);
  writeState(value);
}
function fail(message) {
  const value = readState();
  value.failures.push(String(message));
  writeState(value);
  throw new Error(message);
}
function response(id, result) {
  process.stdout.write(JSON.stringify({ id, result }) + '\n');
}
function credit(id) {
  return {
    creditId: id,
    resetType: 'codexRateLimits',
    status: 'available',
    grantedAt: new Date(Date.now() - 60_000).toISOString(),
    expiresAt: new Date(Date.now() + 45 * 60_000).toISOString(),
  };
}
function rateLimitResult() {
  const value = readState();
  const codex = {};
  codex.limitId = 'codex';
  if (account === accounts.normal && value.normalResetApplied) {
    codex.rateLimitReachedType = null;
    codex.primary = { resetAfterSeconds: 20 * 60 };
  } else {
    codex.rateLimitReachedType = 'rate_limit_reached';
    codex.primary = account === accounts.unknown ? {} : { resetAfterSeconds: 20 * 60 };
  }
  let rateLimitResetCredits;
  if (account === accounts.normal) {
    rateLimitResetCredits = value.normalResetApplied
      ? { availableCount: 0, credits: [] }
      : { availableCount: 1, credits: [credit(normalCredit)] };
  } else if (account === accounts.zero) {
    rateLimitResetCredits = { availableCount: 0, credits: [credit(zeroCredit)] };
  } else if (account === accounts.unknown) {
    rateLimitResetCredits = { availableCount: 1, credits: [credit(unknownCredit)] };
  } else {
    fail('unknown managed fake profile');
  }
  return { rateLimitsByLimitId: { codex }, rateLimitResetCredits };
}

if (process.argv.slice(2).join(' ') !== 'app-server --stdio -c cli_auth_credentials_store="file"') {
  fail('gateway did not launch app-server with its fixed file-credential isolation override');
}
if (!account) fail('gateway selected an unknown managed fake profile');
if (process.env.HOME !== home || process.env.CODEX_SQLITE_HOME !== home || process.cwd() !== home) {
  fail('gateway did not isolate the fake child in its managed profile');
}
record({ kind: 'launch', account, profile });

const lines = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
for await (const line of lines) {
  try {
    const request = JSON.parse(line);
    const method = request.method;
    const id = request.id;
    if (method === 'initialized') {
      if (id !== undefined) fail('initialized was not a JSON-RPC notification');
      record({ kind: 'notification', account, method });
      continue;
    }
    if (typeof id !== 'number') fail('expected a numeric JSON-RPC request id');
    if (method === 'initialize') {
      record({ kind: 'request', account, method });
      response(id, { capabilities: {} });
    } else if (method === 'account/read') {
      if (request.params?.refreshToken !== false) fail('account/read requested an unsafe refresh');
      record({ kind: 'request', account, method });
      response(id, { account: { type: 'chatgpt', email: emails[account] } });
    } else if (method === 'account/rateLimits/read') {
      const result = rateLimitResult();
      record({ kind: 'request', account, method, result });
      response(id, result);
    } else if (method === 'account/rateLimitResetCredit/consume') {
      const params = request.params || {};
      if (account !== accounts.normal) fail('only the normal profile may consume');
      if (params.creditId !== normalCredit) fail('gateway sent the wrong credit ID');
      if (typeof params.idempotencyKey !== 'string'
        || !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(params.idempotencyKey)) {
        fail('gateway did not send a UUID v4 idempotency key');
      }
      const value = readState();
      value.normalResetApplied = true;
      value.consumeCalls.push({
        account,
        creditId: params.creditId,
        idempotencyKey: params.idempotencyKey,
      });
      value.events.push({ kind: 'request', account, method });
      writeState(value);
      response(id, { outcome: 'reset' });
    } else {
      record({ kind: 'unexpected_request', account, method });
      process.stdout.write(JSON.stringify({
        id,
        error: { code: -32601, message: 'unsupported fake App Server method' },
      }) + '\n');
    }
  } catch (error) {
    try { fail(error && error.message ? error.message : error); } catch {}
    process.exitCode = 1;
    break;
  }
}
`;
  await fs.writeFile(fakeAppServer, source, { mode: 0o600 });
  await fs.chmod(fakeAppServer, 0o600);
  await fs.writeFile(
    fakeCodex,
    '#!/bin/sh\nexec ' + shellQuote(process.execPath) + ' ' + shellQuote(fakeAppServer) + ' "$@"\n',
    { mode: 0o700 },
  );
  await fs.chmod(fakeCodex, 0o700);
}

async function stop(child, signal = 'SIGTERM') {
  if (!child || child.exitCode !== null || child.signalCode !== null) return;
  child.kill(signal);
  await Promise.race([once(child, 'exit'), sleep(5_000)]);
  if (child.exitCode === null && child.signalCode === null) {
    child.kill('SIGKILL');
    await once(child, 'exit');
  }
}

async function startGateway() {
  gatewayLog = '';
  gateway = spawn(gatewayBinary, ['--config', configFile], {
    cwd: temporary,
    env: {
      ...process.env,
      RUST_BACKTRACE: '0',
      HTTP_PROXY: '', HTTPS_PROXY: '', ALL_PROXY: '',
      http_proxy: '', https_proxy: '', all_proxy: '', NO_PROXY: '*',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  const retain = chunk => { gatewayLog = (gatewayLog + chunk.toString()).slice(-12_000); };
  gateway.stdout.on('data', retain);
  gateway.stderr.on('data', retain);
  gateway.on('error', retain);
  await eventually(async () => {
    if (gateway.exitCode !== null || gateway.signalCode !== null) {
      throw new Error('gateway exited during startup: ' + redact(gatewayLog));
    }
    try {
      return (await fetch(gatewayBase + '/health', { signal: AbortSignal.timeout(300) })).ok;
    } catch {
      return false;
    }
  }, 'gateway did not become ready: ' + redact(gatewayLog), 15_000);
}

async function admin(route, body) {
  const response = await fetch(gatewayBase + route, {
    method: body === undefined ? 'GET' : 'POST',
    headers: body === undefined ? undefined : { 'content-type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
    signal: AbortSignal.timeout(12_000),
  });
  const text = await response.text();
  let value;
  try { value = JSON.parse(text); } catch { value = undefined; }
  return { status: response.status, text, value };
}

async function configure(account) {
  const response = await admin('/admin/codex/reset-credit-automation', {
    account_key: 'codex:account_id:' + account,
    enabled: true,
    scan_interval_minutes: 30,
    expiry_window_minutes: 60,
    final_attempt_minutes: 5,
    min_natural_reset_remaining_minutes: 10,
  });
  assert.equal(response.status, 200, 'policy write failed: ' + redact(response.text));
  assert.equal(response.value?.ok, true, 'policy write did not return state: ' + redact(response.text));
}

function actionFor(actions, account) {
  return actions.find(action => action.account_key === 'codex:account_id:' + account);
}

function uuidV4(value) {
  return typeof value === 'string'
    && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(value);
}

try {
  await fs.access(gatewayBinary);
  await fs.chmod(temporary, 0o700);
  await fs.mkdir(authDirectory, { mode: 0o700 });
  await fs.chmod(authDirectory, 0o700);
  await fs.mkdir(profileRoot, { mode: 0o700 });
  await fs.chmod(profileRoot, 0o700);
  for (const profile of Object.values(profiles)) {
    const directory = path.join(profileRoot, profile);
    await fs.mkdir(directory, { mode: 0o700 });
    await fs.chmod(directory, 0o700);
    await writePrivate(path.join(directory, 'auth.json'), {
      // The fake App Server never reads this.  It exists solely to prove the
      // gateway requires a private per-profile credential file before launch.
      synthetic: true,
    });
  }
  await writePrivate(stateFile, {
    normalResetApplied: false,
    consumeCalls: [],
    events: [],
    failures: [],
  });
  await writeFakeAppServer();
  const gatewayPort = await unusedPort();
  gatewayBase = 'http://127.0.0.1:' + gatewayPort;

  await writePrivate(path.join(authDirectory, 'normal.json'), {
    type: 'codex', account_id: accounts.normal, label: 'Synthetic normal',
    access_token: 'fixture-reset-credit-normal-token',
  });
  await writePrivate(path.join(authDirectory, 'zero.json'), {
    type: 'codex', account_id: accounts.zero, label: 'Synthetic zero',
    access_token: 'fixture-reset-credit-zero-token',
  });
  await writePrivate(path.join(authDirectory, 'unknown.json'), {
    type: 'codex', account_id: accounts.unknown, label: 'Synthetic unknown',
    access_token: 'fixture-reset-credit-unknown-token',
  });
  await writePrivate(configFile, {
    listen: '127.0.0.1:' + gatewayPort,
    // Automation must use the local App Server, not legacy HTTP. This
    // unreachable loopback value makes any accidental HTTP fallback harmless.
    upstream_base: 'http://127.0.0.1:9/codex',
    proxy_api_key: 'fixture-reset-credit-proxy-key',
    tokens: [],
    auth_dir: authDirectory,
    codex_reset_credit_app_server: {
      experimental_opt_in: true,
      command: fakeCodex,
      profile_root: profileRoot,
      profiles: Object.values(accounts).map(account => ({
        account_key: 'codex:account_id:' + account,
        profile: profiles[account],
        expected_email: emails[account],
      })),
    },
    max_concurrent_requests: 16,
    upstream_connect_timeout_seconds: 1,
    upstream_read_timeout_seconds: 1,
    upstream_first_event_timeout_seconds: 1,
  });

  // Persist policies after the first empty worker tick. The first live pass
  // exercises the whole JSONL flow without racing a process shutdown midway
  // through a short durable scan lease.
  await startGateway();
  const unsafeGet = await fetch(
    gatewayBase + '/codex/rate-limit-reset-credit/consume?file_name=normal.json&credit_id=' + normalCredit,
    { redirect: 'manual', signal: AbortSignal.timeout(2_000) },
  );
  assert.equal(unsafeGet.status, 405, 'reset-credit redemption must reject state-changing GET requests');
  // The worker's first tick is intentionally immediate. Let the empty pass
  // finish before adding policies so this test models a clean restart rather
  // than killing a worker while it legitimately owns a short scan lease.
  await sleep(500);
  for (const account of Object.values(accounts)) await configure(account);

  let snapshot;
  await eventually(async () => {
    const response = await admin('/admin/codex/reset-credit-automation');
    assert.equal(response.status, 200, 'automation snapshot failed: ' + redact(response.text));
    const actions = response.value?.actions;
    if (!Array.isArray(actions)) return false;
    const normal = actionFor(actions, accounts.normal);
    const unknown = actionFor(actions, accounts.unknown);
    const zero = actionFor(actions, accounts.zero);
    if (normal?.state !== 'verified') {
      throw new Error('normal action has unexpected state: ' + JSON.stringify(normal || null));
    }
    if (unknown?.state !== 'deferred' || unknown?.last_outcome !== 'deferred_natural_reset_unknown') {
      throw new Error('unknown-reset action has unexpected state: ' + JSON.stringify({
        action: unknown || null,
        fake: await state(),
        gatewayLog: redact(gatewayLog),
      }));
    }
    if (zero !== undefined) throw new Error('availableCount=0 forged detail created an action');
    snapshot = response.value;
    return true;
  }, 'worker did not settle expected App Server actions');

  // A restart after durable settlement must neither create another action nor
  // replay the already-consumed credit.
  const consumesBeforeRestart = (await state()).consumeCalls.length;
  await stop(gateway);
  gateway = undefined;
  await startGateway();
  await sleep(500);
  const afterRestart = await admin('/admin/codex/reset-credit-automation');
  assert.equal(afterRestart.status, 200, 'restart snapshot failed: ' + redact(afterRestart.text));
  assert.equal(actionFor(afterRestart.value?.actions || [], accounts.normal)?.state, 'verified');
  assert.equal((await state()).consumeCalls.length, consumesBeforeRestart,
    'restart replayed an already-settled reset credit');
  snapshot = afterRestart.value;

  const fake = await state();
  assert.deepEqual(fake.failures, [], 'fake App Server observed invalid protocol behavior');
  assert.equal(fake.consumeCalls.length, 1, 'exactly one App Server consume request is permitted');
  const [consume] = fake.consumeCalls;
  assert.equal(consume.account, accounts.normal);
  assert.equal(consume.creditId, normalCredit);
  assert.ok(uuidV4(consume.idempotencyKey), 'gateway did not generate a UUID v4 idempotency key');
  assert.ok(fake.events.some(event => event.method === 'account/rateLimits/read'));
  assert.ok(fake.events.some(event => event.method === 'account/rateLimitResetCredit/consume'));
  assert.equal(fake.events.some(event => event.kind === 'unexpected_request'), false);
  assert.ok(fake.events.filter(event => event.kind === 'launch')
    .every(event => Object.values(accounts).includes(event.account)));

  const actions = snapshot.actions;
  const normal = actionFor(actions, accounts.normal);
  const unknown = actionFor(actions, accounts.unknown);
  assert.equal(actions.length, 2, 'only normal and unknown profiles may create actions');
  assert.equal(normal.credit_id, normalCredit);
  assert.equal(normal.state, 'verified');
  assert.ok(normal.verified_at);
  assert.equal(Object.hasOwn(normal, 'idempotency_key'), false);
  assert.equal(unknown.credit_id, unknownCredit);
  assert.equal(unknown.state, 'deferred');
  assert.equal(unknown.last_outcome, 'deferred_natural_reset_unknown');
  assert.equal(unknown.submitted_at, null);

  process.stdout.write('PASS Codex reset-credit local App Server JSONL smoke test\n');
  process.stdout.write('Verified one exact-credit UUID-idempotent JSON-RPC consume, zero-count fail-closed behavior, and unknown-reset deferral.\n');
  process.stdout.write('No production credentials, provider quota, Wham endpoint, or non-loopback host were used.\n');
} finally {
  await stop(gateway);
  assert.equal(path.dirname(temporary), os.tmpdir());
  assert.ok(path.basename(temporary).startsWith('io-gateway-reset-credit-test-'));
  await fs.rm(temporary, { recursive: true, force: true });
}
