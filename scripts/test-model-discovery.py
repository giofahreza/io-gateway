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
    def log_message(self, *_):
        pass

    def do_GET(self):
        token = self.headers.get("Authorization", "").removeprefix("Bearer ")
        if urllib.parse.urlsplit(self.path).path == "/models" and token in ("upstream-a", "upstream-b"):
            account = token.removeprefix("upstream-")
            value = {"models": [
                {"slug": f"model-{account}", "display_name": f"Account {account}", "visibility": "list",
                 "base_instructions": "runtime instruction " * 25_000,
                 "model_messages": {"instructions_template": "runtime template " * 25_000}},
                {"slug": f"hidden-{account}", "visibility": "hide", "base_instructions": "hidden instructions"},
            ]}
            status = 200
        else:
            value, status = {"error": "Unknown fixture endpoint"}, 404
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
                    scoped_full = request(base, "/codex/models", selected)[1]
                    check(scoped == {"models": [{k: v for k, v in model.items() if k in FIELDS}
                                                for model in scoped_full["models"]]},
                          "restricted discovery projects the same catalog as restricted full")
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
