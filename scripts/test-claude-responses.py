#!/usr/bin/env python3
"""Exercise Claude-to-Responses streams through a real gateway and optional Codex CLI.

cargo build --locked --bin io-gateway
python3 scripts/test-claude-responses.py --codex-bin codex

Only fake credentials and a loopback upstream are used. Codex runs in temporary
directories with isolated configuration; its only tool commands print fixtures.
"""

import argparse
import http.server
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import urllib.parse
import urllib.request
import urllib.error


ROOT = Path(__file__).resolve().parents[1]
GATEWAY_KEY = "claude-stream-fixture-gateway-key"
UPSTREAM_KEY = "claude-stream-fixture-upstream-key"
ANSWER = "Gateway fixture answer: café 🐈"
TOOL_ANSWER = "Both fixture commands completed."
RECOVERY_MODEL = "claude-sonnet-4-5-fixture-recovery"
EXHAUSTED_MODEL = "claude-sonnet-4-5-fixture-exhausted"
SLOW_MODEL = "claude-fixture-slow-stream"
TOOL_MODELS = {"claude-fixture-tools", "claude-sonnet-5-5", RECOVERY_MODEL}
MODELS = ["claude-fixture-text", "claude-fixture-tools", "claude-fixture-truncated",
          "claude-fixture-tool-truncated", "claude-fixture-mixed-truncated", "claude-sonnet-5-5"]


def check(condition, message):
    if not condition:
        raise AssertionError(message)
    print("PASS", message, flush=True)


def free_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def shell_call(tools, marker):
    command = "printf '%s\\n' " + marker
    for name in ("exec_command", "shell_command", "shell"):
        for tool in tools:
            if tool.get("name") != name:
                continue
            properties = tool.get("input_schema", {}).get("properties", {})
            if "cmd" in properties:
                return name, {"cmd": command}
            if "command" in properties:
                schema = properties["command"]
                return name, {"command": ["sh", "-c", command] if schema.get("type") == "array" else command}
    raise AssertionError("Codex did not advertise a supported shell function")


class Upstream(http.server.BaseHTTPRequestHandler):
    calls = []

    def log_message(self, *_):
        pass

    def reply(self, value, status=200):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def stream_reply(self, message, slow=False, broken=False):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()

        def emit(value):
            data = ("data: " + json.dumps(value, ensure_ascii=False) + "\n\n").encode()
            # Network chunks may bisect a UTF-8 codepoint or SSE frame.
            for offset in range(0, len(data), 17):
                self.wfile.write(data[offset:offset + 17])
            self.wfile.flush()

        try:
            start = {**message, "content": [], "stop_reason": None,
                     "usage": {"input_tokens": 5, "output_tokens": 0,
                               "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}
            emit({"type": "message_start", "message": start})
            if slow:
                # Exceeds both the fixture's one-second upstream read timeout
                # and one downstream heartbeat interval, while upstream pings
                # keep each individual read active.
                until = time.monotonic() + 12
                while time.monotonic() < until:
                    emit({"type": "ping"})
                    time.sleep(0.2)
            for index, block in enumerate(message["content"]):
                field, delta_type = {"text": ("text", "text_delta"),
                                     "thinking": ("thinking", "thinking_delta"),
                                     "tool_use": ("input", "input_json_delta")}[block["type"]]
                initial = {**block, field: {} if field == "input" else ""}
                initial.pop("fixture_partial_json", None)
                emit({"type": "content_block_start", "index": index, "content_block": initial})
                text = (block.get("fixture_partial_json", json.dumps(block["input"]))
                        if field == "input" else block[field])
                for offset in range(0, len(text), 5):
                    emit({"type": "content_block_delta", "index": index,
                          "delta": {"type": delta_type,
                                    "partial_json" if field == "input" else field: text[offset:offset + 5]}})
                emit({"type": "content_block_stop", "index": index})
            emit({"type": "message_delta", "delta": {"stop_reason": message["stop_reason"]},
                  "usage": message["usage"]})
            if not broken:
                emit({"type": "message_stop"})
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        if urllib.parse.urlsplit(self.path).path == "/v1/models":
            self.reply({"data": [{"id": model, "display_name": model} for model in MODELS]})
        else:
            self.reply({}, 404)

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if (urllib.parse.urlsplit(self.path).path != "/v1/messages"
                or self.headers.get("Authorization") != "Bearer " + UPSTREAM_KEY):
            self.reply({"error": {"message": "Wrong fixture endpoint or credential"}}, 400)
            return
        model = payload.get("model")
        results = [block for message in payload.get("messages", [])
                   for block in message.get("content", []) if isinstance(block, dict)
                   and block.get("type") == "tool_result"]
        budget = payload.get("max_tokens", 0)
        shape = {key: value for key, value in payload.items() if key != "max_tokens"}
        self.calls.append({"model": model, "tool_results": results, "max_tokens": budget,
                           "stream": payload.get("stream"),
                           "input_hash": hashlib.sha256(json.dumps(shape, sort_keys=True).encode()).hexdigest()})
        stop = "end_turn"
        if model == "claude-fixture-timeout":
            time.sleep(3)
            return
        if model in (RECOVERY_MODEL, EXHAUSTED_MODEL) and not results and (budget == 32000 or model == EXHAUSTED_MODEL):
            name, arguments = shell_call(payload.get("tools", []), "IOGW_FIXTURE_STALE")
            content = [{"type": "tool_use", "id": "toolu_stale", "name": name, "input": arguments},
                       {"type": "tool_use", "id": "toolu_partial", "name": name, "input": {},
                        "fixture_partial_json": '{"cmd":"printf IOGW_FIXTURE_TRUNCATED'}]
            stop = "max_tokens"
        elif model in ("claude-fixture-text", SLOW_MODEL, "claude-fixture-broken", "claude-fixture-json"):
            content = [{"type": "text", "text": ANSWER}]
        elif model == "claude-sonnet-5-5" and budget <= 4096:
            # The production failure consumed the adapter's entire 4096-token
            # default on adaptive thinking, without reaching an answer or tool.
            content, stop = [{"type": "thinking", "thinking": "Still planning the fixture."}], "max_tokens"
        elif model.endswith("truncated"):
            content, stop = [{"type": "text", "text": "Partial fixture answer"}], "max_tokens"
            if model in ("claude-fixture-tool-truncated", "claude-fixture-mixed-truncated"):
                name, arguments = shell_call(payload.get("tools", []), "IOGW_FIXTURE_TRUNCATED")
                content = [{"type": "tool_use", "id": "toolu_truncated", "name": name, "input": arguments}]
                if model == "claude-fixture-mixed-truncated":
                    name, arguments = shell_call(payload.get("tools", []), "IOGW_FIXTURE_ONE")
                    content.insert(0, {"type": "tool_use", "id": "toolu_complete", "name": name, "input": arguments})
        elif model in TOOL_MODELS:
            if results:
                text = json.dumps(results)
                if not all(marker in text for marker in ("IOGW_FIXTURE_ONE", "IOGW_FIXTURE_TWO")):
                    self.reply({"error": {"message": "Missing fixture tool results"}}, 400)
                    return
                content = [{"type": "text", "text": TOOL_ANSWER}]
            else:
                content = [{"type": "thinking", "thinking": "Check both fixtures."},
                           {"type": "text", "text": "Running the fixture commands."}]
                try:
                    for number, marker in enumerate(("IOGW_FIXTURE_ONE", "IOGW_FIXTURE_TWO")):
                        name, arguments = shell_call(payload.get("tools", []), marker)
                        content.append({"type": "tool_use", "id": f"toolu_fixture_{number}",
                                        "name": name, "input": arguments})
                except AssertionError as error:
                    self.reply({"error": {"message": str(error)}}, 400)
                    return
                stop = "tool_use"
        else:
            self.reply({"error": {"message": "Unknown fixture model"}}, 400)
            return
        message = {"id": "msg_fixture", "type": "message", "role": "assistant", "model": model,
                   "content": content, "stop_reason": stop,
                   "usage": {"input_tokens": 5, "output_tokens": 9}}
        if model == "claude-fixture-json":
            self.reply(message)
        else:
            self.stream_reply(message, slow=model == SLOW_MODEL, broken=model == "claude-fixture-broken")


def request(base, path, payload=None, key=GATEWAY_KEY):
    data = None if payload is None else json.dumps(payload).encode()
    req = urllib.request.Request(base + path, data=data, headers={
        "Content-Type": "application/json", "Authorization": "Bearer " + key,
    })
    with urllib.request.urlopen(req, timeout=30) as response:
        return response.headers, response.read().decode()


def sse_events(body):
    return [event for frame in body.strip().split("\n\n")
            if frame and (event := json.loads(frame.split("data: ", 1)[1])).get("type") != "ping"]


def protocol_case(base, model):
    headers, body = request(base, "/codex/responses", {
        "model": "cld:" + model, "input": "Return the fixture answer.", "stream": True,
        "tools": [{"type": "function", "name": "exec_command", "parameters": {
            "type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"],
        }}],
    })
    events = sse_events(body)
    check(headers.get_content_type() == "text/event-stream", model + " returns SSE")
    check([event["sequence_number"] for event in events] == list(range(len(events))),
          model + " has monotonic sequence numbers")
    check(events[0]["response"]["output"] == [] and events[0]["response"]["status"] == "in_progress",
          model + " starts with an empty response")
    final = events[-1]["response"]
    added = [event for event in events if event["type"] == "response.output_item.added"]
    done = [event for event in events if event["type"] == "response.output_item.done"]
    finished = [(index, item) for index, item in enumerate(final["output"])
                if not (item["type"] == "function_call" and item.get("status") == "incomplete")]
    check([event["output_index"] for event in added] == list(range(len(final["output"])))
          and [event["output_index"] for event in done] == [index for index, _ in finished],
          model + " finishes completed items at their own indices")
    check([event["item"] for event in done] == [item for _, item in finished]
          and [event["item"]["id"] for event in added] == [item["id"] for item in final["output"]],
          model + " keeps item IDs and final snapshots consistent")
    check(len({item["id"] for item in final["output"]}) == len(final["output"]),
          model + " gives each item a distinct ID")
    if model.endswith("truncated"):
        check(events[-1]["type"] == "response.incomplete" and final["status"] == "incomplete"
              and final["incomplete_details"]["reason"] == "max_output_tokens"
              and not any(event["type"] == "response.completed" for event in events),
              "token exhaustion is incomplete, never a successful completion")
        if model in ("claude-fixture-tool-truncated", "claude-fixture-mixed-truncated"):
            check(not any(event["type"] == "response.function_call_arguments.done"
                          and event["output_index"] == len(final["output"]) - 1 for event in events),
                  "an unfinished tool is not finalized as executable")
    elif model in TOOL_MODELS:
        check([item["type"] for item in final["output"]] == ["reasoning", "message", "function_call", "function_call"],
              "thinking, text and parallel tools retain upstream order")
        arguments = [event for event in events if event["type"] == "response.function_call_arguments.done"]
        check(len(arguments) == 2 and all(json.loads(event["arguments"]).get("cmd") for event in arguments),
              "both tools finish with valid JSON arguments")
    else:
        check("".join(event["delta"] for event in events if event["type"] == "response.output_text.delta") == ANSWER,
              "text deltas reconstruct the exact Unicode answer once")


def explicit_budget_case(base):
    for limits in ({"max_output_tokens": 64}, {"max_tokens": None, "max_output_tokens": 64}):
        before = len(Upstream.calls)
        _, body = request(base, "/codex/responses", {
            "model": "cld:claude-sonnet-5-5", "input": "Return the fixture answer.",
            "stream": True, **limits,
        })
        events = sse_events(body)
        check(len(Upstream.calls[before:]) == 1 and Upstream.calls[-1]["max_tokens"] == 64,
              "an explicit output budget is preserved without hidden inference retries")
        check(events[-1]["type"] == "response.incomplete",
              "thinking exhaustion with an explicit small budget remains incomplete")


def recovery_case(base):
    for model in (RECOVERY_MODEL, EXHAUSTED_MODEL):
        before = len(Upstream.calls)
        _, body = request(base, "/codex/responses", {
            "model": "cld:" + model, "input": "Return the fixture answer.", "stream": True,
            "tools": [{"type": "function", "name": "exec_command", "parameters": {
                "type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"],
            }}],
        })
        calls = Upstream.calls[before:]
        events = sse_events(body)
        check([call["max_tokens"] for call in calls] == [32000, 64000]
              and len({call["input_hash"] for call in calls}) == 1,
              "truncated default-budget generation is retried once with the same input and a larger cap")
        final = events[-1]["response"]
        if model == RECOVERY_MODEL:
            check(final["status"] == "completed" and len([item for item in final["output"]
                                                         if item["type"] == "function_call"]) == 2,
                  "regeneration replaces the unfinished generation with complete tool calls")
            check("IOGW_FIXTURE_STALE" not in body and "IOGW_FIXTURE_TRUNCATED" not in body,
                  "no stale text, arguments or completed tools escape before regeneration")
        else:
            check(final["status"] == "incomplete"
                  and len([event for event in events if event["type"] == "response.output_item.done"]) == 1,
                  "recovery is bounded and still never finalizes the unfinished call")


def lifecycle_case(base):
    # An interrupted probe used to leak active_requests and leave probing=true
    # indefinitely, exactly matching the production incident.
    try:
        request(base, "/codex/responses", {"model": "cld:claude-fixture-timeout", "input": "test", "stream": True})
        raise AssertionError("Expected fixture read timeout")
    except urllib.error.HTTPError as error:
        body = error.read().decode()
        check(error.code == 502 and "timeout" in body and "error sending request" in body,
              "transport failures expose their timeout classification and cause")
    runtime = json.loads(request(base, "/claude/accounts.json")[1])["accounts"][0]["runtime"]
    check(runtime["status"] == "cooldown" and 1 <= runtime["cooldown_remaining_seconds"] <= 5,
          "a timeout uses the short cooldown")
    time.sleep(5.1)
    parsed = urllib.parse.urlsplit(base)
    payload = json.dumps({"model": "cld:claude-fixture-timeout", "input": "test", "stream": True}).encode()
    sock = socket.create_connection((parsed.hostname, parsed.port))
    sock.sendall((f"POST /codex/responses HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {GATEWAY_KEY}\r\n"
                  f"Content-Type: application/json\r\nContent-Length: {len(payload)}\r\n\r\n").encode() + payload)
    time.sleep(0.15)
    sock.close()
    time.sleep(0.3)
    runtime = json.loads(request(base, "/claude/accounts.json")[1])["accounts"][0]["runtime"]
    check(runtime["active_requests"] == 0 and runtime["status"] == "healthy",
          "cancelling an in-flight probe releases the account immediately")
    req = urllib.request.Request(base + "/codex/responses", data=json.dumps({
        "model": "cld:" + SLOW_MODEL, "input": "test", "stream": True,
    }).encode(), headers={"Content-Type": "application/json", "Authorization": "Bearer " + GATEWAY_KEY})
    started = time.monotonic()
    with urllib.request.urlopen(req, timeout=15) as response:
        first = response.readline()
        check(first == b"event: ping\n" and time.monotonic() - started < 1,
              "downstream receives a heartbeat before the model finishes")
    time.sleep(0.3)
    runtime = json.loads(request(base, "/claude/accounts.json")[1])["accounts"][0]["runtime"]
    check(runtime["active_requests"] == 0 and runtime["status"] == "healthy",
          "disconnecting a streaming client releases its active account")
    started = time.monotonic()
    _, body = request(base, "/codex/responses", {"model": "cld:" + SLOW_MODEL, "input": "test", "stream": True})
    check(time.monotonic() - started >= 12 and body.count("event: ping") >= 2
          and sse_events(body)[-1]["type"] == "response.completed",
          "long inference survives per-read timeouts and sends regular downstream data heartbeats")
    _, body = request(base, "/codex/responses", {"model": "cld:claude-fixture-broken", "input": "test", "stream": True})
    events = sse_events(body)
    check(events[-1]["type"] == "response.failed" and "message_stop" in events[-1]["response"]["error"]["message"]
          and not any(event["type"] == "response.output_item.done" for event in events),
          "a broken stream reports a failure without releasing any tools")
    time.sleep(2.1)
    _, body = request(base, "/codex/responses", {"model": "cld:claude-fixture-json", "input": "test", "stream": False})
    check(json.loads(body)["status"] == "completed", "non-streaming JSON-compatible upstreams still work")
    _, body = request(base, "/codex/responses", {"model": "cld:claude-fixture-text", "input": "test", "stream": False})
    check(json.loads(body)["status"] == "completed", "non-streaming clients can consume streamed upstream results")


def recovery_quota_case(base):
    for limit in (64000, 100000):
        rules = [{"metric": metric, "period": "daily", "limit": value} for metric, value in
                 (("requests", 1), ("input_tokens", 1_000_000), ("output_tokens", limit))]
        _, created = request(base, "/admin/api-keys/create", {
            "label": "Claude regeneration fixture", "access": {"all": True, "quota": {"timezone": "UTC", "rules": rules}},
        })
        created = json.loads(created)
        before = len(Upstream.calls)
        _, body = request(base, "/codex/responses", {
            "model": "cld:" + RECOVERY_MODEL, "input": "Return the fixture answer.", "stream": True,
            "tools": [{"type": "function", "name": "exec_command", "parameters": {
                "type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"],
            }}],
        }, key=created["plain_text_key"])
        events = sse_events(body)
        count = 1 if limit == 64000 else 2
        check(len(Upstream.calls[before:]) == count,
              "regeneration obtains its own token quota reservation before contacting upstream")
        _, state = request(base, "/admin/api-keys/quotas?id=" + created["key"]["id"])
        rules = json.loads(state)["quota_summaries"][created["key"]["id"]]["rules"]
        expected = {"requests": 1, "input_tokens": 5 * count, "output_tokens": 9 * count}
        check(all(row["confirmed"] == expected[row["metric"]]
                  and row["reserved"] == 0 and row["uncertain"] == 0 for row in rules),
              "each generation settles its tokens exactly once and the client request counts once")
        if limit == 64000:
            check(events[-1]["type"] == "response.failed"
                  and events[-1]["response"]["error"]["code"] == "rate_limit_exceeded"
                  and "quota" in events[-1]["response"]["error"]["message"]
                  and not any(event["type"] == "response.output_item.done" for event in events),
                  "a denied recovery preserves the quota error without releasing stale tools")
        else:
            check(events[-1]["type"] == "response.completed", "allowed recovery completes under renewable quotas")


def codex_case(codex, directory, base, model):
    case = directory / model
    case.mkdir()
    codex_home, project = case / "codex-home", case / "project"
    codex_home.mkdir()
    project.mkdir()
    (codex_home / "config.toml").write_text(f'''model_provider = "fixture"
approval_policy = "never"
web_search = "disabled"
project_doc_max_bytes = 0
[model_providers.fixture]
name = "Loopback fixture"
base_url = "{base}/codex"
env_key = "IOGW_CLAUDE_STREAM_TEST_KEY"
wire_api = "responses"
supports_websockets = false
request_max_retries = 0
stream_max_retries = 0
stream_idle_timeout_ms = 11000
[features]
shell_snapshot = false
''')
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(("CODEX", "OPENAI", "ANTHROPIC", "IOGW_CLAUDE_STREAM_TEST_KEY"))}
    env.update(CODEX_HOME=str(codex_home), IOGW_CLAUDE_STREAM_TEST_KEY=GATEWAY_KEY)
    before = len(Upstream.calls)
    proc = subprocess.run([
        codex, "exec", "--json", "--ephemeral", "--skip-git-repo-check", "--color", "never",
        "--sandbox", "read-only", "-C", str(project), "-m", "cld:" + model,
        "Return the fixture answer. Execute any requested fixture commands first.",
    ], env=env, cwd=project, capture_output=True, text=True, timeout=90)
    events = [json.loads(line) for line in proc.stdout.splitlines() if line.strip().startswith("{")]
    (case / "codex.jsonl").write_text(proc.stdout)
    (case / "codex.stderr").write_text(proc.stderr)
    items = [event["item"] for event in events if event.get("type") == "item.completed"]
    messages = [item.get("text") for item in items if item.get("type") == "agent_message"]
    completed = any(event.get("type") == "turn.completed" for event in events)
    if model.endswith("truncated"):
        check(not completed and any(event.get("type") == "turn.failed" for event in events),
              "real Codex reports truncation as failure, not a successful empty turn")
        commands = [item for item in items if item.get("type") == "command_execution"]
        expected_count = 1 if model == "claude-fixture-mixed-truncated" else 0
        check(len(commands) == expected_count
              and all(command.get("exit_code") == 0
                      and "IOGW_FIXTURE_ONE" in command.get("aggregated_output", "")
                      and "IOGW_FIXTURE_TRUNCATED" not in command.get("aggregated_output", "")
                      for command in commands),
              "real Codex executes only completed tools, preserving the unfinished call")
    else:
        expected = TOOL_ANSWER if model in TOOL_MODELS else ANSWER
        check(proc.returncode == 0 and completed and messages and messages[-1] == expected,
              "real Codex finalizes " + model)
        if model in TOOL_MODELS:
            commands = [item for item in items if item.get("type") == "command_execution"]
            check(len(commands) == 2 and all(command.get("exit_code") == 0 for command in commands),
                  "real Codex executes both harmless commands")
            count = 3 if model == RECOVERY_MODEL else 2
            check(len(Upstream.calls[before:]) == count and len(Upstream.calls[-1]["tool_results"]) == 2,
                  "real Codex returns both results and completes the follow-up turn")
            if model == RECOVERY_MODEL:
                check([call["max_tokens"] for call in Upstream.calls[before:]] == [32000, 64000, 32000]
                      and all("IOGW_FIXTURE_STALE" not in command.get("aggregated_output", "") for command in commands),
                      "real Codex executes only regenerated calls, exactly once")


def run(binary, codex):
    upstream = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="iogw-claude-responses-") as directory:
            directory = Path(directory)
            auths = directory / "auths"
            auths.mkdir()
            (auths / "claude.json").write_text(json.dumps({
                "type": "claude", "access_token": UPSTREAM_KEY, "organization_uuid": "org-fixture",
                "account_id": "claude-fixture", "expires_at": "2099-01-01T00:00:00Z",
                "api_base_url": f"http://127.0.0.1:{upstream.server_port}",
            }))
            base = f"http://127.0.0.1:{free_port()}"
            config = directory / "config.json"
            config.write_text(json.dumps({
                "listen": base.removeprefix("http://"), "auth_dir": str(auths), "tokens": [],
                "proxy_api_key": GATEWAY_KEY, "upstream_base": f"http://127.0.0.1:{upstream.server_port}",
                "admin_auth": {"enabled": False},
                "upstream_read_timeout_seconds": 1,
            }))
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(("ADMIN_AUTH_", "CODEX_GATEWAY_KEY"))}
            with (directory / "gateway.log").open("w+") as log:
                proc = subprocess.Popen([str(binary), "--config", str(config)], cwd=directory,
                                        stdout=log, stderr=log, env=env)
                try:
                    for _ in range(100):
                        if proc.poll() is not None:
                            log.seek(0)
                            raise RuntimeError(log.read())
                        try:
                            request(base, "/health")
                            break
                        except OSError:
                            time.sleep(0.1)
                    else:
                        raise RuntimeError("Gateway did not start")
                    for model in MODELS:
                        protocol_case(base, model)
                    explicit_budget_case(base)
                    recovery_case(base)
                    recovery_quota_case(base)
                    lifecycle_case(base)
                    if codex:
                        for model in [*MODELS, RECOVERY_MODEL, SLOW_MODEL]:
                            codex_case(codex, directory, base, model)
                except Exception:
                    if codex:
                        for path in directory.glob("*/codex.jsonl"):
                            print(path.parent.name, path.read_text(), path.with_name("codex.stderr").read_text())
                    raise
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
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/io-gateway")
    parser.add_argument("--codex-bin", help="Also exercise a real Codex CLI against the fake upstream")
    args = parser.parse_args()
    codex = shutil.which(args.codex_bin) if args.codex_bin else None
    if args.codex_bin and not codex:
        parser.error("Codex executable not found: " + args.codex_bin)
    run(args.binary.resolve(), codex)
