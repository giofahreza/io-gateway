#!/usr/bin/env python3
"""Exercise compatible account management and both wire protocols through the real gateway.

Run after cargo build --locked --bins:
    python3 scripts/test-compatible-apis.py
Use --serve to keep the temporary gateway running for browser/TUI inspection.
No real provider credentials or external inference calls are used.
"""
import argparse
import contextlib
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
UPSTREAM_KEY = 'compatible-test-provider-key'
GATEWAY_KEY = 'compatible-test-gateway-key'
CALLS = []


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


class Upstream(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def send(self, value, content_type='application/json', status=200):
        data = value.encode() if isinstance(value, str) else json.dumps(value).encode()
        self.send_response(status)
        self.send_header('Content-Type', content_type)
        self.send_header('Content-Length', str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        assert self.headers['Authorization'] == 'Bearer ' + UPSTREAM_KEY
        if self.path.endswith('/models'):
            self.send({'object': 'list', 'data': [{'id': 'qwen-test'}, {'id': 'another-model'}]})
        else:
            self.send({'error': 'unknown URL'}, status=404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
        CALLS.append((self.path, dict(self.headers), body))
        assert self.headers['Authorization'] == 'Bearer ' + UPSTREAM_KEY
        assert not any('internal' in key.lower() for key in self.headers)
        assert not body['model'].startswith('cmp:')
        if self.path == '/unavailable/v1/chat/completions':
            self.send({'error': 'Service temporarily unavailable'}, status=503)
            return
        anthropic = self.path == '/anthropic/v1/messages'
        if not anthropic and self.path != '/openai/v1/chat/completions':
            self.send({'error': 'wrong upstream route'}, status=404)
            return
        if anthropic:
            assert self.headers['x-api-key'] == UPSTREAM_KEY
            assert 'built on Anthropic' not in json.dumps(body)
            reply = {'id': 'msg_test', 'type': 'message', 'role': 'assistant', 'model': body['model'],
                     'content': [{'type': 'text', 'text': 'OK'}], 'stop_reason': 'end_turn',
                     'usage': {'input_tokens': 3, 'output_tokens': 2}}
            if body.get('stream'):
                events = [
                    ('message_start', {'type': 'message_start', 'message': {**reply, 'content': [], 'usage': {'input_tokens': 3, 'output_tokens': 0}}}),
                    ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': {'type': 'text', 'text': ''}}),
                    ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': {'type': 'text_delta', 'text': 'OK'}}),
                    ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                    ('message_delta', {'type': 'message_delta', 'delta': {'stop_reason': 'end_turn'}, 'usage': {'output_tokens': 2}}),
                    ('message_stop', {'type': 'message_stop'}),
                ]
                self.send(''.join(f'event: {name}\ndata: {json.dumps(value)}\n\n' for name, value in events), 'text/event-stream')
            else:
                self.send(reply)
        else:
            reply = {'id': 'chatcmpl_test', 'object': 'chat.completion', 'model': body['model'], 'created': 1,
                     'choices': [{'index': 0, 'message': {'role': 'assistant', 'content': 'OK'}, 'finish_reason': 'stop'}],
                     'usage': {'prompt_tokens': 3, 'completion_tokens': 2, 'total_tokens': 5}}
            if body.get('stream'):
                events = [
                    {**reply, 'object': 'chat.completion.chunk', 'choices': [{'index': 0, 'delta': {'role': 'assistant', 'content': 'OK'}, 'finish_reason': None}], 'usage': None},
                    {**reply, 'object': 'chat.completion.chunk', 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}]},
                ]
                self.send(''.join(f'data: {json.dumps(event)}\n\n' for event in events) + 'data: [DONE]\n\n', 'text/event-stream')
            else:
                self.send(reply)


def request(base, path, body=None, key=None, form=None):
    headers = {}
    if key:
        headers['Authorization'] = 'Bearer ' + key
    if form is not None:
        data = urllib.parse.urlencode(form).encode()
        headers['Content-Type'] = 'application/x-www-form-urlencoded'
    else:
        data = None if body is None else json.dumps(body).encode()
        headers['Content-Type'] = 'application/json'
    req = urllib.request.Request(base + path, data=data, headers=headers)
    try:
        response = urllib.request.urlopen(req, timeout=20)
    except urllib.error.HTTPError as error:
        response = error
    text = response.read().decode()
    try:
        value = json.loads(text)
    except ValueError:
        value = text
    return response.status, value


@contextlib.contextmanager
def gateway(binary, auth_enabled=False):
    with tempfile.TemporaryDirectory(prefix='iogw-compatible-') as directory:
        directory = Path(directory)
        base = f'http://127.0.0.1:{free_port()}'
        config = {'listen': base.removeprefix('http://'), 'tokens': [], 'proxy_api_key': GATEWAY_KEY,
                  'upstream_base': 'http://127.0.0.1:1', 'auth_dir': str(directory / 'auths'),
                  'admin_auth': {'enabled': auth_enabled, 'api_key': 'test-admin-key' if auth_enabled else None}}
        (directory / 'config.json').write_text(json.dumps(config))
        with (directory / 'gateway.log').open('w+') as log:
            env = {key: value for key, value in os.environ.items() if not key.startswith(('ADMIN_AUTH_', 'CODEX_GATEWAY_KEY'))}
            proc = subprocess.Popen([str(binary), '--config', str(directory / 'config.json')], cwd=directory,
                                    stdout=log, stderr=log, env=env)
            try:
                for _ in range(100):
                    if proc.poll() is not None:
                        log.seek(0)
                        raise RuntimeError(log.read())
                    try:
                        if request(base, '/health')[0] == 200:
                            break
                    except OSError:
                        time.sleep(.1)
                else:
                    raise RuntimeError('gateway did not start')
                yield base, directory
            finally:
                proc.terminate()
                try:
                    proc.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait()


def check(condition, message):
    if not condition:
        raise AssertionError(message)
    print('PASS', message, flush=True)


def run(binary, serve):
    upstream = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    root = f'http://127.0.0.1:{upstream.server_port}'
    try:
        with gateway(binary, auth_enabled=True) as (base, _):
            for path, payload in [('/compatible/accounts.json', None), ('/compatible/save', {'label': 'denied'}),
                                  ('/compatible/discover', {'label': 'denied'})]:
                check(request(base, path, payload)[0] == 401, path + ' requires administrator authentication')
        with gateway(binary) as (base, directory):
            def save(name, models, openai=True, anthropic=True, **extra):
                body = {'label': name, 'api_key': UPSTREAM_KEY, 'models': models,
                        'base_url': root + '/openai/v1' if openai else None,
                        'anthropic_base_url': root + '/anthropic' if anthropic else None, **extra}
                status, result = request(base, '/compatible/save', body)
                check(status == 200 and result.get('ok'), 'save ' + name)
                return result['file_name']
            file = save('Both protocols', ['qwen-test'])
            check(request(base, '/ready')[0] == 200, 'compatible accounts make the gateway ready')
            save('OpenAI only', ['openai-only'], anthropic=False)
            save('Anthropic only', ['anthropic-only'], openai=False)
            status, listing = request(base, '/compatible/accounts.json')
            check(status == 200 and len(listing['accounts']) == 3 and UPSTREAM_KEY not in json.dumps(listing), 'list accounts without revealing keys')
            check((directory / 'auths' / file).stat().st_mode & 0o777 == 0o600, 'credentials use private file permissions')
            status, discovered = request(base, '/compatible/discover', {'label': '', 'file_name': file, 'base_url': root + '/openai/v1'})
            check(status == 200 and 'qwen-test' in discovered['models'], 'discover models using the saved key')
            status, snapshot = request(base, '/dashboard/snapshot.json')
            check(status == 200 and len(snapshot['providers']['compatible']['accounts']) == 3, 'shared web/TUI snapshot includes compatible accounts')
            # Account routing is the authoritative account list for the TUI.
            status, routing = request(base, '/admin/account-routing')
            check(status == 200 and any(a['provider'] == 'compatible' for a in routing['accounts']), 'TUI routing includes compatible accounts')
            status, models = request(base, '/v1/models', key=GATEWAY_KEY)
            check(status == 200 and any(m['id'] == 'cmp:qwen-test' for m in models['data']), 'model catalog exposes cmp: model IDs')
            for model in ['qwen-test', 'openai-only', 'anthropic-only']:
                for path in ['/v1/chat/completions', '/claude/v1/messages', '/v1/responses']:
                    for stream in [False, True]:
                        body = {'model': 'cmp:' + model, 'stream': stream, 'max_tokens': 64, 'max_output_tokens': 64}
                        body.update({'input': 'Reply OK'} if path.endswith('responses') else {'messages': [{'role': 'user', 'content': 'Reply OK'}]})
                        status, reply = request(base, path, body, key=GATEWAY_KEY)
                        check(status == 200 and 'OK' in (reply if isinstance(reply, str) else json.dumps(reply)), f'{model} {path} stream={stream}')
                        if stream:
                            check(any(terminal in reply for terminal in ['[DONE]', 'message_stop', 'response.completed']), 'stream terminates correctly')
            def managed_key(access):
                status, created = request(base, '/admin/api-keys/create', {'label': 'Compatible scope test', 'access': access})
                check(status == 200 and 'plain_text_key' in created, 'create managed key')
                return created['plain_text_key']
            prompt = {'model': 'cmp:qwen-test', 'messages': [{'role': 'user', 'content': 'hi'}], 'max_tokens': 16}
            denied = managed_key({'all': False, 'providers': [{'provider': 'claude', 'account_scope': 'all'}]})
            before = len(CALLS)
            check(request(base, '/v1/chat/completions', prompt, key=denied)[0] == 403 and len(CALLS) == before, 'provider scope blocks compatible traffic before dispatch')
            selected = managed_key({'all': False, 'providers': [{'provider': 'compatible', 'account_scope': 'selected',
                                    'accounts': ['compatible:account_id:' + json.loads((directory / 'auths' / file).read_text())['account_id']]}]})
            check(request(base, '/v1/chat/completions', prompt, key=selected)[0] == 200, 'selected compatible account is authorized')
            before = len(CALLS)
            other = {**prompt, 'model': 'cmp:openai-only'}
            check(request(base, '/v1/chat/completions', other, key=selected)[0] == 503 and len(CALLS) == before, 'account scope prevents dispatch to other compatible credentials')
            status, scoped_models = request(base, '/v1/models', key=selected)
            check(status == 200 and {m['id'] for m in scoped_models['data']} == {'cmp:qwen-test'}, 'scoped model catalog excludes other accounts')
            limited = managed_key({'all': True, 'quota': {'timezone': 'UTC', 'rules': [{'metric': 'requests', 'period': 'daily', 'limit': 1}]}})
            check(request(base, '/v1/chat/completions', prompt, key=limited)[0] == 200, 'request quota admits compatible traffic')
            before = len(CALLS)
            check(request(base, '/v1/chat/completions', prompt, key=limited)[0] == 429 and len(CALLS) == before, 'request quota blocks a second compatible request')
            status, summary = request(base, '/usage/summary.json')
            check(status == 200 and any(row['usage']['requests'] > 0 for row in summary['providers']['compatible']), 'compatible usage persists under its own provider')
            save('Tagged model', ['qwen:8b'], anthropic=False)
            check(request(base, '/v1/chat/completions', {**prompt, 'model': 'cmp:qwen:8b'}, key=GATEWAY_KEY)[0] == 200 and CALLS[-1][2]['model'] == 'qwen:8b', 'upstream model tags retain their colon')
            save('Full resource URLs', ['full-url'], base_url=root + '/openai/v1/chat/completions', anthropic_base_url=root + '/anthropic/v1/messages')
            for path, body in [('/v1/chat/completions', {**prompt, 'model': 'cmp:full-url'}),
                               ('/claude/v1/messages', {**prompt, 'model': 'cmp:full-url'}),
                               ('/v1/responses', {'model': 'cmp:full-url', 'input': 'Reply OK', 'stream': True, 'max_output_tokens': 16})]:
                status, reply = request(base, path, body, key=GATEWAY_KEY)
                check(status == 200 and 'OK' in (reply if isinstance(reply, str) else json.dumps(reply)), 'full resource URLs support ' + path)
            failing = save('Unavailable endpoint', ['failover-model'], anthropic=False, base_url=root + '/unavailable/v1')
            healthy = save('Failover endpoint', ['failover-model'], anthropic=False)
            failing_id = json.loads((directory / 'auths' / failing).read_text())['account_id']
            healthy_id = json.loads((directory / 'auths' / healthy).read_text())['account_id']
            def prioritize(account_id):
                status, result = request(base, '/admin/account-routing/priority', {'provider': 'compatible', 'account': 'compatible:account_id:' + account_id, 'priority': True})
                check(status == 200 and result.get('ok'), 'set compatible account priority')
            # Existing routing uses only the priority pool while it is usable.
            # Warm the healthy account first so the untried endpoint sorts first
            # when both are in that pool, independent of UUID/file ordering.
            prioritize(healthy_id)
            failover_prompt = {**prompt, 'model': 'cmp:failover-model'}
            check(request(base, '/v1/chat/completions', failover_prompt, key=GATEWAY_KEY)[0] == 200, 'warm healthy priority account')
            prioritize(failing_id)
            before = len(CALLS)
            check(request(base, '/v1/chat/completions', failover_prompt, key=GATEWAY_KEY)[0] == 200 and len(CALLS) == before + 2, 'priority endpoint fails over within the priority pool')
            # Never fall back to an unrelated account that does not offer the model.
            before = len(CALLS)
            status, _ = request(base, '/v1/chat/completions', {'model': 'cmp:unknown-model', 'messages': [{'role': 'user', 'content': 'hi'}]}, key=GATEWAY_KEY)
            check(status == 503 and len(CALLS) == before, 'unknown models do not dispatch to other accounts')
            status, result = request(base, '/compatible/save', {'file_name': file, 'label': 'Renamed account', 'models': ['qwen-test'],
                                                                'base_url': root + '/openai/v1', 'anthropic_base_url': root + '/anthropic'})
            check(status == 200 and result['file_name'] == file, 'editing preserves identity and a blank key')
            saved = json.loads((directory / 'auths' / file).read_text())
            check(saved['api_key'] == UPSTREAM_KEY and saved['label'] == 'Renamed account', 'edited credentials persist')
            status, _ = request(base, '/credentials/toggle', form={'file_name': file, 'enabled': 'false'})
            before = len(CALLS)
            status, _ = request(base, '/v1/chat/completions', {'model': 'cmp:qwen-test', 'messages': [{'role': 'user', 'content': 'hi'}]}, key=GATEWAY_KEY)
            check(status == 503 and len(CALLS) == before, 'disabled accounts cannot receive traffic')
            request(base, '/credentials/toggle', form={'file_name': file, 'enabled': 'true'})
            for invalid in ['file:///etc/passwd', 'https://user:password@example.com/v1', 'https://example.com/v1?key=secret']:
                status, _ = request(base, '/compatible/save', {'label': 'Bad URL', 'api_key': UPSTREAM_KEY, 'models': ['x'], 'base_url': invalid})
                check(status == 400, 'reject invalid or credential-bearing URL')
            status, _ = request(base, '/credentials/delete', form={'file_name': file})
            check(status == 200 and not (directory / 'auths' / file).exists(), 'delete removes saved credentials')
            if serve:
                save('Browser example', ['qwen-test'])
                print('BROWSER_GATEWAY=' + base, flush=True)
                print('UPSTREAM=' + root, flush=True)
                try:
                    while True:
                        time.sleep(1)
                except KeyboardInterrupt:
                    pass
    finally:
        upstream.shutdown()
        upstream.server_close()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, default=ROOT / 'target/debug/io-gateway')
    parser.add_argument('--serve', action='store_true')
    args = parser.parse_args()
    run(args.binary.resolve(), args.serve)
