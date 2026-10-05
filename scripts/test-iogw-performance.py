#!/usr/bin/env python3
"""Exercise the real TUI against a local gateway, without production credentials.

Run: python3 scripts/test-iogw-performance.py --binary target/release/iogw
Requires a POSIX pseudo-terminal. No additional Python packages are needed.
"""

import argparse
import codecs
import fcntl
import gzip
import json
import os
from pathlib import Path
import pty
import re
import select
import socket
import struct
import subprocess
import tempfile
import termios
import threading
import time
import unicodedata
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Gateway(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.delay = {}
        self.fail = set()
        self.requests = []
        self.request_count = 111
        self.login_required = False
        self.logged_in = False

    def count(self, path):
        return sum(request[1] == path for request in self.requests)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_args):
        pass

    def setup(self):
        super().setup()
        self.connection.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def do_GET(self):
        self.respond()

    def do_POST(self):
        self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.respond()

    def respond(self):
        gateway = self.server
        path = self.path.split("?")[0]
        gateway.requests.append((self.command, path, self.headers.get("Accept-Encoding", "")))
        time.sleep(gateway.delay.get(path, 0))
        authenticated = not gateway.login_required or (
            gateway.logged_in and self.headers.get("Cookie") == "io_gateway_admin_session=test-only"
        )
        bodies = {
            "/admin/session": {
                "enabled": gateway.login_required,
                "authenticated": authenticated,
            },
            "/usage/summary.json": {"totals": {"requests": gateway.request_count}, "providers": {}},
            "/admin/account-routing": {"accounts": [], "settings": {}},
            "/dashboard/snapshot.json": {"quotas": {}},
            "/custom-models.json": {"models": [{"alias": "original", "enabled": True}]},
            "/usage/context-history.json": {"labels": ["now"], "buckets": [{"total_tokens": 7}]},
            "/admin/api-keys": {"keys": []},
            "/notifications/settings": {"enabled": True},
            "/notifications/test": {"message": "notification tested"},
            "/admin/login": {"message": "logged in"},
        }
        status = 500 if path in gateway.fail else 200 if path in bodies else 404
        value = bodies.get(path, {"message": "unexpected endpoint"})
        if status == 500:
            value = {"message": "injected failure"}
        elif path not in ("/admin/session", "/admin/login") and not authenticated:
            status, value = 401, {"message": "login required"}
        payload = json.dumps(value).encode()
        compressed = "gzip" in self.headers.get("Accept-Encoding", "")
        if compressed:
            payload = gzip.compress(payload)
        try:
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            if compressed:
                self.send_header("Content-Encoding", "gzip")
            if path == "/admin/login" and status == 200:
                gateway.logged_in = True
                self.send_header("Set-Cookie", "io_gateway_admin_session=test-only; Path=/")
            self.end_headers()
            self.wfile.write(payload)
        except (BrokenPipeError, ConnectionResetError):
            pass


class Screen:
    """The cursor/erase operations used by Crossterm; SGR does not affect text.

    Ratatui can emit a changed word as several cursor-addressed fragments, so
    assertions must inspect the resulting screen rather than search raw bytes.
    """

    def __init__(self, rows=42, columns=180):
        self.rows, self.columns = rows, columns
        self.cells = [[" "] * columns for _ in range(rows)]
        self.row = self.column = 0
        self.pending = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")

    def feed(self, data):
        self.pending += self.decoder.decode(data)
        index = 0
        while index < len(self.pending):
            char = self.pending[index]
            if char == "\x1b":
                match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending[index:])
                if not match:
                    break
                raw, _, operation = match.groups()
                args = [int(value) if value else 0 for value in raw.lstrip("?").split(";")]
                amount = args[0] or 1
                if operation in "Hf":
                    self.row = (args[0] or 1) - 1
                    self.column = (args[1] if len(args) > 1 and args[1] else 1) - 1
                elif operation == "G":
                    self.column = amount - 1
                elif operation == "A":
                    self.row -= amount
                elif operation == "B":
                    self.row += amount
                elif operation == "C":
                    self.column += amount
                elif operation == "D":
                    self.column -= amount
                elif operation == "J" and args[0] in (2, 3):
                    self.cells = [[" "] * self.columns for _ in range(self.rows)]
                elif operation == "K":
                    start = 0 if args[0] in (1, 2) else self.column
                    end = self.column + 1 if args[0] == 1 else self.columns
                    self.cells[self.row][start:end] = [" "] * (end - start)
                self.row = max(0, min(self.rows - 1, self.row))
                self.column = max(0, min(self.columns - 1, self.column))
                index += len(match.group())
                continue
            if char == "\r":
                self.column = 0
            elif char == "\n":
                self.row = min(self.rows - 1, self.row + 1)
            elif char >= " " and not unicodedata.combining(char):
                self.cells[self.row][self.column] = char
                width = 2 if unicodedata.east_asian_width(char) in ("W", "F") else 1
                self.column = min(self.columns - 1, self.column + width)
            index += 1
        self.pending = self.pending[index:]

    def text(self):
        return "\n".join("".join(row) for row in self.cells)


class Terminal:
    def __init__(self, binary, gateway):
        self.gateway = gateway
        self.config = tempfile.TemporaryDirectory(prefix="iogw-tui-test-")
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 42, 180, 0, 0))
        env = os.environ.copy()
        env.pop("IOGW_ADMIN_API_KEY", None)
        env.update(TERM="xterm-256color", XDG_CONFIG_HOME=self.config.name)
        self.output = b""
        self.screen = Screen()
        self.started = time.monotonic()
        self.process = subprocess.Popen(
            [str(binary), "--base-url", f"http://127.0.0.1:{gateway.server_port}"],
            stdin=slave, stdout=slave, stderr=slave, env=env, start_new_session=True,
        )
        os.close(slave)

    def pump(self, timeout=0.01):
        readable, _, _ = select.select([self.master], [], [], timeout)
        if readable:
            try:
                data = os.read(self.master, 262144)
                self.output += data
                self.screen.feed(data)
            except OSError:
                pass

    def until(self, predicate, timeout=4):
        deadline = time.monotonic() + timeout
        while not predicate():
            if time.monotonic() >= deadline:
                raise AssertionError("terminal did not reach expected state before deadline")
            assert self.process.poll() is None, "TUI exited unexpectedly"
            self.pump()
        return time.monotonic()

    def marker(self, marker, offset=0, timeout=4):
        return self.until(lambda: marker.decode() in self.screen.text(), timeout)

    def send(self, keys):
        offset = len(self.output)
        started = time.monotonic()
        os.write(self.master, keys)
        return offset, started

    def help_latency(self, prefix=b""):
        offset, started = self.send(prefix + b"?")
        received = self.marker(b"Commands", offset, timeout=0.5)
        return (received - started) * 1000

    def quit_latency(self):
        _, started = self.send(b"\x03")
        self.process.wait(timeout=0.5)
        assert self.process.returncode == 0
        return (time.monotonic() - started) * 1000

    def close(self):
        if self.process.poll() is None:
            try:
                self.quit_latency()
            except (AssertionError, subprocess.TimeoutExpired):
                self.process.terminate()
                self.process.wait(timeout=2)
        os.close(self.master)
        self.config.cleanup()


def run_case(binary, name, setup, check):
    gateway = Gateway()
    setup(gateway)
    thread = threading.Thread(target=gateway.serve_forever, kwargs={"poll_interval": 0.02}, daemon=True)
    thread.start()
    terminal = Terminal(binary, gateway)
    try:
        first = terminal.marker(b"? commands", timeout=0.5)
        result = {"case": name, "first_frame_ms": round((first - terminal.started) * 1000, 2)}
        result.update(check(terminal, gateway))
        assert gateway.count("/usage/history.json") == 0, "unused history was fetched"
        assert all("gzip" in encoding for _, _, encoding in gateway.requests), "gzip was not negotiated"
        result["quit_ms"] = round(terminal.quit_latency(), 2)
        print(json.dumps(result), flush=True)
    finally:
        terminal.close()
        gateway.shutdown()
        gateway.server_close()


def stalled_startup(terminal, gateway):
    terminal.until(lambda: gateway.count("/admin/session") == 1)
    return {"help_ms": round(terminal.help_latency(), 2)}


def stalled_models(terminal, gateway):
    terminal.until(lambda: gateway.count("/custom-models.json") == 1)
    terminal.marker(b"111")  # Server sends gzip; visible counters verify decoding.
    assert gateway.count("/admin/api-keys") == 0
    assert gateway.count("/notifications/settings") == 0
    latency = terminal.help_latency(b"rrr")
    assert gateway.count("/custom-models.json") == 1, "refreshes were not coalesced"
    return {"help_after_refresh_burst_ms": round(latency, 2)}


def partial_failure(terminal, gateway):
    terminal.marker(b"original")
    gateway.request_count = 999
    gateway.fail.add("/custom-models.json")
    offset, started = terminal.send(b"r")
    received = terminal.marker(b"999", offset)
    terminal.marker(b"STALE", offset)
    assert gateway.count("/usage/context-history.json") > 0, "chart was removed"
    return {"fresh_counter_despite_model_error_ms": round((received - started) * 1000, 2)}


def stalled_action(terminal, gateway):
    terminal.marker(b"111")
    terminal.send(b"\t\t\t")
    terminal.until(lambda: gateway.count("/notifications/settings") == 1)
    terminal.send(b"t")
    terminal.until(lambda: gateway.count("/notifications/test") == 1)
    latency = terminal.help_latency()
    assert gateway.count("/notifications/test") == 1
    return {"help_during_action_ms": round(latency, 2)}


def stalled_login(terminal, gateway):
    terminal.marker(b"Admin login required.")
    assert gateway.count("/usage/summary.json") == 0
    offset, _ = terminal.send(b"o")
    terminal.marker(b"OTP:", offset)
    terminal.send(b"123456\n")
    terminal.until(lambda: gateway.count("/admin/login") == 1)
    return {"help_during_login_ms": round(terminal.help_latency(), 2)}


def completed_login(terminal, gateway):
    terminal.marker(b"Admin login required.")
    offset, _ = terminal.send(b"o")
    terminal.marker(b"OTP:", offset)
    terminal.send(b"123456\n")
    terminal.marker(b"logged in")
    terminal.marker(b"111")
    sessions = list(Path(terminal.config.name).glob("iogw/*.session"))
    assert len(sessions) == 1 and sessions[0].read_text().strip() == "io_gateway_admin_session=test-only"
    return {"login_cookie_saved_and_used": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/iogw"))
    binary = parser.parse_args().binary.resolve(strict=True)
    run_case(binary, "stalled startup", lambda g: g.delay.update({"/admin/session": 1.5}), stalled_startup)
    run_case(binary, "stalled models and refresh burst", lambda g: g.delay.update({"/custom-models.json": 1.5}), stalled_models)
    run_case(binary, "independent updates", lambda g: None, partial_failure)
    run_case(binary, "stalled management action", lambda g: g.delay.update({"/notifications/test": 1.5}), stalled_action)

    def login_setup(gateway):
        gateway.login_required = True
        gateway.delay["/admin/login"] = 1.5

    run_case(binary, "stalled login", login_setup, stalled_login)
    run_case(binary, "login completes", lambda g: setattr(g, "login_required", True), completed_login)


if __name__ == "__main__":
    main()
