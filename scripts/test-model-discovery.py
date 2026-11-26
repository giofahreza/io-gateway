#!/usr/bin/env python3
"""Test compact discovery through the real gateway using local fake accounts.

Run after `cargo build --locked --bin io-gateway`:
    python3 scripts/test-model-discovery.py
No real credentials or external provider calls are used.
"""

import argparse
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


GATEWAY_KEY = "discovery-test-gateway-key"
FIELDS = {"id", "slug", "display_name", "visibility"}


class Upstream(http.server.BaseHTTPRequestHandler):
    claude_modes = {"unavailable": "error"}

    def log_message(self, *_):
        pass

    def do_GET(self):
        token = self.headers.get("Authorization", "").removeprefix("Bearer ")
        path = urllib.parse.urlsplit(self.path).path
        if path == "/models" and token in ("upstream-a", "upstream-b"):
            account = token.removeprefix("upstream-")
            value = {"models": [
                {"slug": f"model-{account}", "display_name": f"Account {account}", "visibility": "list",
                 "base_instructions": "runtime instruction " * 25_000,
                 "model_messages": {"instructions_template": "runtime template " * 25_000}},
                {"slug": f"hidden-{account}", "visibility": "hide", "base_instructions": "hidden instructions"},
            ]}
            status = 200
        elif path == "/v1/models" and token.startswith("claude-"):
            account = token.removeprefix("claude-")
            mode = self.claude_modes.get(account)
            if mode == "slow":
                time.sleep(4)
            status = 503 if mode == "error" else 200
            value = {"data": [] if mode == "empty" else [
                {"id": "claude-shared", "display_name": "Shared Claude"},
                {"id": f"claude-private-{account}", "display_name": f"Claude {account}"},
            ]}
        else:
            value, status = {"error": "Unknown fixture endpoint"}, 404
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError):
            pass  # A timed-out catalog request intentionally closes its connection.

    def do_POST(self):
        token = self.headers.get("Authorization", "").removeprefix("Bearer ")
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        if (urllib.parse.urlsplit(self.path).path == "/v1/messages"
                and token == "claude-b" and payload.get("model") == "claude-private-b"):
            status = 200
            value = {"id": "msg-fixture", "type": "message", "role": "assistant",
                     "model": payload["model"], "content": [{"type": "text", "text": "Fixture Claude reply"}],
                     "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}}
        else:
            status, value = 400, {"error": "Wrong fixture model, account, or endpoint"}
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def request(base, path, key=None, payload=None):
    headers = {"Content-Type": "application/json"}
    if key:
        headers["Authorization"] = "Bearer " + key
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(base + path, data=data, headers=headers)
    try:
        response = urllib.request.urlopen(req, timeout=10)
    except urllib.error.HTTPError as error:
        response = error
    body = response.read()
    try:
        value = json.loads(body)
    except ValueError:
        value = body.decode()
    return response.status, value, len(body)


def check(condition, message):
    if not condition:
        raise AssertionError(message)
    print("PASS", message, flush=True)


def run(binary):
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="iogw-discovery-") as directory:
            directory = Path(directory)
            auths = directory / "auths"
            auths.mkdir()
            for account in ("a", "b"):
                (auths / f"{account}.json").write_text(json.dumps({
                    "type": "codex", "account_id": f"account-{account}",
                    "access_token": f"upstream-{account}", "email": f"{account}@example.test",
                }))
            for account in ("a", "b", "disabled", "unavailable"):
                (auths / f"claude-{account}.json").write_text(json.dumps({
                    "type": "claude", "organization_uuid": f"org-{account}",
                    "account_id": f"claude-{account}", "access_token": f"claude-{account}",
                    "expires_at": "2099-01-01T00:00:00Z",
                    "api_base_url": f"http://127.0.0.1:{upstream.server_port}",
                    "models": [] if account == "unavailable" else [
                        {"id": f"claude-saved-{account}", "display_name": "Saved Claude"}],
                }))
            (auths / "custom-models.json").write_text(json.dumps({"models": [
                {"alias": f"custom-{account}", "display_name": f"Custom {account}",
                 "routes": [{"targets": [{"model": f"cod:model-{account}",
                                           "account": f"codex:account_id:account-{account}"}]}]}
                for account in ("a", "b")
            ]}))
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            base = f"http://127.0.0.1:{port}"
            config = directory / "config.json"
            config.write_text(json.dumps({
                "listen": f"127.0.0.1:{port}", "tokens": [], "proxy_api_key": GATEWAY_KEY,
                "upstream_base": f"http://127.0.0.1:{upstream.server_port}",
                "auth_dir": str(auths), "admin_auth": {"enabled": False},
                "disabled_files": ["claude-disabled.json"],
            }))
            env = {k: v for k, v in os.environ.items() if not k.startswith(("ADMIN_AUTH_", "CODEX_GATEWAY_KEY"))}
            with (directory / "gateway.log").open("w+") as log:
                proc = subprocess.Popen([str(binary), "--config", str(config)], cwd=directory,
                                        stdout=log, stderr=log, env=env)
                try:
                    for _ in range(100):
                        if proc.poll() is not None:
                            log.seek(0)
                            raise RuntimeError(log.read())
                        try:
                            if request(base, "/health")[0] == 200:
                                break
                        except OSError:
                            time.sleep(0.1)
                    else:
                        raise RuntimeError("Gateway did not start")

                    path = "/codex/models?view=discovery"
                    check(request(base, path)[0] == 401, "discovery requires authentication")
                    check(request(base, path, "invalid-key")[0] == 401, "discovery rejects invalid keys")
                    status, full, full_size = request(base, "/codex/models", GATEWAY_KEY)
                    check(status == 200 and full_size > 1_500_000, "default catalog retains large runtime instructions")
                    check(request(base, "/codex/models?view=full", GATEWAY_KEY)[1] == full, "explicit full catalog matches default")
                    status, compact, compact_size = request(base, path, GATEWAY_KEY)
                    expected = {"models": [{k: v for k, v in model.items() if k in FIELDS} for model in full["models"]]}
                    check(status == 200 and compact == expected, "discovery preserves exact entries, aliases, order and visibility")
                    check({"ctm:custom-a", "ctm:custom-b"}.issubset({m["slug"] for m in compact["models"]}),
                          "discovery includes permitted custom models")
                    claude_ids = {"cld:claude-shared", "cld:claude-private-a", "cld:claude-private-b"}
                    check(claude_ids == {m["slug"] for m in compact["models"] if m["slug"].startswith("cld:")},
                          "Codex discovery includes live models from all enabled Claude accounts")
                    check(sum(m["slug"] == "cld:claude-shared" for m in compact["models"]) == 1,
                          "Claude models shared by multiple accounts are deduplicated")
                    claude_model = next(m for m in full["models"] if m["slug"] == "cld:claude-private-a")
                    check(claude_model["display_name"] == "Claude a" and claude_model["visibility"] == "list"
                          and claude_model["context_window"] == 200_000
                          and claude_model["supported_reasoning_levels"] == []
                          and "instructions_template" in claude_model["model_messages"],
                          "Claude full catalog includes CLI metadata without unsupported reasoning controls")
                    check(compact_size < 4096 and compact_size < full_size / 100, "discovery size is independent of runtime instructions")
                    check(any(m.get("visibility") == "hide" for m in compact["models"]), "hidden entries retain their visibility marker")
                    check(request(base, "/codex/models", GATEWAY_KEY)[1] == full, "discovery does not mutate the cached full catalog")

                    def managed_key(providers):
                        status, created, _ = request(base, "/admin/api-keys/create", payload={
                            "label": "Discovery fixture", "access": {"all": False, "providers": providers},
                        })
                        check(status == 200, "create scoped fixture key")
                        return created["plain_text_key"]

                    denied = managed_key([{"provider": "claude", "account_scope": "all"}])
                    check(request(base, path, denied)[0] == 403, "discovery enforces Codex provider access")
                    selected = managed_key([{"provider": "codex", "account_scope": "selected",
                                             "accounts": ["codex:account_id:account-b"]}])
                    status, scoped, _ = request(base, path, selected)
                    slugs = {m["slug"] for m in scoped["models"]}
                    check(status == 200 and "model-b" in slugs and "cod:model-b" in slugs
                          and "model-a" not in slugs and "cod:model-a" not in slugs,
                          "restricted key uses its own account, not the primed global catalog")
                    check("ctm:custom-b" in slugs and "ctm:custom-a" not in slugs,
                          "custom models obey the restricted key's account scope")
                    check(not any(slug.startswith("cld:") for slug in slugs),
                          "Codex-only keys cannot discover Claude models")
                    scoped_full = request(base, "/codex/models", selected)[1]
                    check(scoped == {"models": [{k: v for k, v in model.items() if k in FIELDS}
                                                for model in scoped_full["models"]]},
                          "restricted discovery projects the same catalog as restricted full")

                    def codex_and_claude_key(account):
                        return managed_key([
                            {"provider": "codex", "account_scope": "selected",
                             "accounts": ["codex:account_id:account-b"]},
                            {"provider": "claude", "account_scope": "selected",
                             "accounts": [f"claude:organization:org-{account}"]},
                        ])

                    claude_selected = codex_and_claude_key("b")
                    status, scoped, _ = request(base, path, claude_selected)
                    check(status == 200 and {m["slug"] for m in scoped["models"] if m["slug"].startswith("cld:")}
                          == {"cld:claude-shared", "cld:claude-private-b"},
                          "Claude discovery uses only the key's selected account after an unrestricted catalog")
                    scoped_full = request(base, "/codex/models", claude_selected)[1]
                    check(scoped == {"models": [{k: v for k, v in model.items() if k in FIELDS}
                                                for model in scoped_full["models"]]},
                          "Claude-scoped discovery matches its scoped full catalog")
                    status, response, _ = request(base, "/codex/responses", claude_selected, {
                        "model": "cld:claude-private-b", "input": "Fixture request", "stream": False,
                    })
                    check(status == 200 and response.get("object") == "response"
                          and any(part.get("text") == "Fixture Claude reply"
                                  for item in response.get("output", []) for part in item.get("content", [])),
                          "a discovered Claude model executes through Codex using its permitted fake account")
                    disabled = codex_and_claude_key("disabled")
                    check(not any(m["slug"].startswith("cld:") for m in request(base, path, disabled)[1]["models"]),
                          "disabled Claude accounts never contribute models")
                    unavailable = codex_and_claude_key("unavailable")
                    check(not any(m["slug"].startswith("cld:") for m in request(base, path, unavailable)[1]["models"]),
                          "unavailable accounts without saved models do not advertise invented Claude models")

                    for mode in ("error", "slow", "empty"):
                        Upstream.claude_modes["b"] = mode
                        try:
                            started = time.monotonic()
                            status, fallback, _ = request(base, path, claude_selected)
                            elapsed = time.monotonic() - started
                            ids = {m["slug"] for m in fallback["models"] if m["slug"].startswith("cld:")}
                            expected_ids = {"cld:claude-saved-b"}
                            check(status == 200 and ids == expected_ids,
                                  f"Claude {mode} catalog preserves account scope and uses the correct fallback")
                            if mode == "slow":
                                check(elapsed < 5, "slow Claude discovery stays within Workbench's catalog deadline")
                        finally:
                            Upstream.claude_modes.pop("b", None)
                    print(f"Payload: full={full_size} bytes; discovery={compact_size} bytes", flush=True)
                finally:
                    proc.terminate()
                    try:
                        proc.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        proc.kill()
                        proc.wait()
    finally:
        upstream.shutdown()
        upstream.server_close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path,
                        default=Path(__file__).resolve().parents[1] / "target/debug/io-gateway")
    run(parser.parse_args().binary.resolve())
