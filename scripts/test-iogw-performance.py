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
from urllib.parse import parse_qs


class Gateway(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.delay = {}
        self.fail = set()
        self.requests = []
        self.posts = []
        self.request_count = 111
        self.login_required = False
        self.logged_in = False
        self.accounts = []
        self.quotas = {}

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
        self.body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
        self.respond()

    def respond(self):
        gateway = self.server
        path = self.path.split("?")[0]
        gateway.requests.append((self.command, path, self.headers.get("Accept-Encoding", "")))
        if self.command == "POST":
            gateway.posts.append((path, self.body))
            if path == "/credentials/toggle":
                form = parse_qs(self.body.decode())
                for account in gateway.accounts:
                    if account.get("credential_file") == form["file_name"][0]:
                        account["enabled"] = form["enabled"][0] == "true"
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
            "/admin/account-routing": {"accounts": gateway.accounts, "settings": {}},
            "/dashboard/snapshot.json": {"quotas": gateway.quotas},
            "/custom-models.json": {"models": [{"alias": "original", "enabled": True}]},
            "/usage/context-history.json": {"labels": ["now"], "buckets": [{"total_tokens": 7}]},
            "/admin/api-keys": {"keys": []},
            "/notifications/settings": {"enabled": True},
            "/notifications/test": {"message": "notification tested"},
            "/admin/login": {"message": "logged in"},
            "/credentials/toggle": {"message": "account updated"},
            "/credentials/delete": {"message": "account deleted"},
            "/custom-models/save": {"message": "model updated"},
            "/models/refresh": {"ok": True, "message": "quotas refreshed"},
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
    def __init__(self, binary, gateway, config_dir=None):
        self.gateway = gateway
        self.config = tempfile.TemporaryDirectory(prefix="iogw-tui-test-")
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 42, 180, 0, 0))
        env = os.environ.copy()
        env.pop("IOGW_ADMIN_API_KEY", None)
        env.pop("IOGW_REDUCED_MOTION", None)
        env.update(TERM="xterm-256color", XDG_CONFIG_HOME=config_dir or self.config.name)
        self.env = env
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
                raise AssertionError("terminal did not reach expected state before deadline\n" + self.screen.text())
            assert self.process.poll() is None, "TUI exited unexpectedly"
            self.pump()
        return time.monotonic()

    def marker(self, marker, offset=0, timeout=4):
        return self.until(lambda: marker.decode() in self.screen.text(), timeout)

    def prompt(self, marker, offset):
        # Line prompts leave the alternate screen. Inspect the literal write:
        # the minimal test emulator deliberately does not model screen switching
        # or deferred autowrap at the final terminal column.
        return self.until(lambda: marker in self.output[offset:])

    def send(self, keys):
        offset = len(self.output)
        started = time.monotonic()
        os.write(self.master, keys)
        return offset, started

    def install_plugin(self, binary, theme="pixel-cafe"):
        pack = Path(__file__).resolve().parents[1] / f"src/bin/iogw/plugins/{theme}.json"
        result = subprocess.run(
            [str(binary), "--json", "plugins", "install", str(pack)], env=self.env,
            capture_output=True, text=True, check=True,
        )
        assert json.loads(result.stdout)["installed"] == theme

    def animated(self):
        text = self.screen.text()
        return any(f"{theme} / " in text for theme in ("Café", "Office", "Station")) and "Commands" not in text

    def drain(self, seconds):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self.pump()

    def help_latency(self, prefix=b""):
        offset, started = self.send(prefix + b"?")
        received = self.marker(b"Commands", offset, timeout=0.5)
        return (received - started) * 1000

    def quit_latency(self):
        _, started = self.send(b"\x03")
        # Keep consuming terminal output like a real emulator; otherwise a full
        # resize/redraw can block stdout before the TUI reads Ctrl+C.
        while self.process.poll() is None and time.monotonic() - started < 0.5:
            self.pump()
        self.process.wait(timeout=0.1)
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
    latency = terminal.help_latency(b"RRR")
    assert gateway.count("/custom-models.json") == 1, "refreshes were not coalesced"
    return {"help_after_refresh_burst_ms": round(latency, 2)}


def partial_failure(terminal, gateway):
    terminal.marker(b"original")
    gateway.request_count = 999
    gateway.fail.add("/custom-models.json")
    offset, started = terminal.send(b"R")
    received = terminal.marker(b"999", offset)
    terminal.marker(b"STALE", offset)
    assert gateway.count("/usage/context-history.json") > 0, "chart was removed"
    return {"fresh_counter_despite_model_error_ms": round((received - started) * 1000, 2)}


def stalled_action(terminal, gateway):
    terminal.marker(b"111")
    terminal.send(b"\t\t")
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


def missing_plugin(terminal, gateway):
    terminal.send(b"\x1bOS")  # F4, xterm encoding.
    terminal.marker(b"animated plugin unavailable")
    return {"missing_plugin_keeps_normal_ui": True, "help_ms": round(terminal.help_latency(), 2)}


def animated_ui(terminal, gateway, binary):
    terminal.marker(b"111")
    terminal.install_plugin(binary)
    _, started = terminal.send(b"\x1bOS")
    entered = terminal.until(terminal.animated, timeout=0.5)
    terminal.send(b"t")
    terminal.marker(b"Office theme unavailable")
    terminal.install_plugin(binary, "pixel-office")
    terminal.send(b"t")
    terminal.marker(b"Office / ")
    preference = Path(terminal.env["XDG_CONFIG_HOME"]) / "iogw/plugins/selected-theme.json"
    assert json.loads(preference.read_text())["theme"] == "pixel-office"
    if gateway.accounts:
        for value in [b"agent00@example.com", b"75%", "◎ Codex".encode()]:
            terminal.marker(value)
        assert "Account 00" not in terminal.screen.text(), "account name shown despite available email"
        terminal.send(b"j\r")
        terminal.marker(b"Account Details")
        terminal.marker(b"Account 01")
        terminal.send(b"\x1b")
        terminal.until(terminal.animated)
        terminal.send(b"k")
        terminal.marker(b"Account Usage")
        terminal.send(b"a")
        terminal.marker(b"quota all limits")
        terminal.marker(b"Gen Weekly")
        assert "Code Gen" not in terminal.screen.text(), "verbose quota label leaked into bubbles"
        terminal.send(b"a")
        terminal.marker(b"quota one limit/account")
    terminal.send(b"t")
    terminal.marker("Café / ".encode())
    terminal.marker(b"Machine")
    for metric in [b"Requests", b"Tokens", b"Accounts", b"Error Rate", b"Context Usage 24h"]:
        terminal.marker(metric)
    if gateway.accounts:
        terminal.send(b"\x1b[F")  # End selects the last account, including unknown quota.
        for value in [b"Usage unavailable", b"Account 24"]:
            terminal.marker(value)
        terminal.until(lambda: "Account Usage · Account 24" in terminal.screen.text())
        # The same fallback appears in the cat's bubble, not just its footer.
        terminal.until(lambda: any("Account 24" in "".join(row) for row in terminal.screen.cells[:-4]))
        terminal.send(b"\x1b[Hl")  # Home is the model cat; l selects account 00.
        terminal.marker(b"75%")

    # Animated h/l and arrows select actors; the leftmost cat is NOT a credential.
    terminal.send(b"\x1b[H")
    terminal.marker(b"Custom Models")
    before = gateway.count("/models/refresh")
    terminal.send(b"r")
    terminal.marker(b"no focused account")
    assert gateway.count("/models/refresh") == before
    if gateway.accounts:
        for key, email in [(b"l", b"agent00@example.com"), (b"l", b"agent01@example.com"),
                           (b"h", b"agent00@example.com"), (b"\x1b[C", b"agent01@example.com"),
                           (b"\x1b[D", b"agent00@example.com")]:
            terminal.send(key)
            terminal.marker(b"Account Usage \xc2\xb7 " + email)
        terminal.send(b"\r")
        terminal.marker(b"Account 00")
        terminal.send(b"\x1b")
        terminal.until(lambda: "Account Details" not in terminal.screen.text())
    terminal.send(b"?")
    terminal.marker(b"Commands")
    terminal.send(b"j\x1b[6~\x1b[H?")
    terminal.until(terminal.animated, timeout=0.5)
    terminal.send(b"\t")  # Other tabs retain their ordinary UI and controls.
    terminal.marker(b"No API keys available.")
    terminal.send(b"\x1b[Z")  # Shift+Tab returns to animated Overview.
    terminal.until(terminal.animated, timeout=0.5)
    terminal.send(b"R")
    before = len(terminal.output)
    terminal.until(lambda: len(terminal.output) > before + 50, timeout=2)
    terminal.send(b" ")
    terminal.marker(b"PAUSED")
    terminal.drain(0.15)
    before = len(terminal.output)
    terminal.drain(0.4)
    # System counters still update while motion is paused; deterministic scene
    # clock freezing is checked by the renderer's unit tests.
    assert "PAUSED" in terminal.screen.text(), "pause state was lost during telemetry update"

    for rows, columns in [(8, 20), (24, 80), (42, 180)]:
        fcntl.ioctl(terminal.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, columns, 0, 0))
        terminal.screen = Screen(rows, columns)
        terminal.until(terminal.animated, timeout=0.5)
        terminal.send(b"t")
        terminal.marker(b"Office / ", timeout=0.5)
        terminal.send(b"t")
        terminal.marker("Café / ".encode(), timeout=0.5)
    terminal.send(b"\x16")
    terminal.until(lambda: not terminal.animated() and " iogw " in terminal.screen.text(), timeout=0.5)
    terminal.send(b"\x16")
    terminal.until(terminal.animated, timeout=0.5)
    terminal.send(b"\x1b")
    terminal.until(lambda: not terminal.animated() and " iogw " in terminal.screen.text(), timeout=0.5)
    return {"animated_switch_ms": round((entered - started) * 1000, 2),
            "email_bubbles_shared_navigation_modals_pause_and_resize": True}


def animated_auth(terminal, gateway, binary, theme="pixel-cafe"):
    terminal.marker(b"Admin login required.")
    terminal.install_plugin(binary, theme)
    terminal.send(b"\x16")
    terminal.until(terminal.animated, timeout=0.5)
    assert "AUTH" in terminal.screen.text()
    terminal.marker("Login required · [o] Log in".encode())
    terminal.send(b"b")
    terminal.marker("Login required · [o] Log in".encode())
    terminal.send(b"?")
    terminal.marker(b"Commands")
    terminal.send(b"?o")
    terminal.marker(b"OTP:")
    terminal.send(b"123456\n")
    terminal.marker(b"logged in")
    terminal.until(lambda: "CONNECTED" in terminal.screen.text())
    terminal.marker(b"Welcome back!")
    terminal.until(lambda: "Welcome back!" not in terminal.screen.text())
    assert gateway.count("/admin/login") == 1
    assert gateway.count("/usage/summary.json") > 0
    terminal.send(b"\x16")
    terminal.until(lambda: not terminal.animated() and " iogw " in terminal.screen.text(), timeout=0.5)
    return {"authentication_uses_shared_login_handler": True}


def animated_auth_retry(terminal, gateway, binary):
    terminal.marker(b"Admin login required.")
    for theme in ("pixel-cafe", "pixel-office", "pixel-station"):
        terminal.install_plugin(binary, theme)
    terminal.send(b"\x16")
    terminal.marker("Login required · [o] Log in".encode())
    offset, _ = terminal.send(b"o")
    terminal.prompt(b"OTP:", offset)
    terminal.send(b"123456\n")
    terminal.marker("Logging in…".encode())
    latency = terminal.help_latency()
    terminal.send(b"?t")  # Pending login survives changing to another theme.
    terminal.marker(b"Office / ")
    terminal.marker("Logging in…".encode())
    terminal.marker("Login failed · [o] Retry".encode())
    terminal.marker(b"injected failure")
    assert "Welcome back!" not in terminal.screen.text()
    gateway.fail.remove("/admin/login")
    gateway.delay["/admin/session"] = 0.7
    terminal.send(b" t")  # Pause, then Station. State feedback must still work.
    terminal.marker(b"PAUSED")
    terminal.marker(b"Station / ")
    offset, _ = terminal.send(b"o")
    terminal.prompt(b"OTP:", offset)
    terminal.send(b"654321\n")
    terminal.marker("Logging in…".encode())
    terminal.marker("Verifying session…".encode())
    assert "Welcome back!" not in terminal.screen.text()
    terminal.marker(b"Welcome back!")
    terminal.until(lambda: "Welcome back!" not in terminal.screen.text())
    assert "PAUSED" in terminal.screen.text()
    assert gateway.count("/admin/login") == 2
    assert gateway.count("/usage/summary.json") > 0
    # Expired credentials restore the shared locked state, never an empty strip.
    gateway.logged_in = False
    terminal.send(b"R")
    terminal.marker("Login required · [o] Log in".encode())
    assert "Welcome back!" not in terminal.screen.text()
    return {"failed_retry_verified_session_pause_theme_switch_and_expiry": True,
            "help_during_login_ms": round(latency, 2)}


def animated_actions(terminal, gateway, binary):
    terminal.marker(b"111")
    terminal.install_plugin(binary)
    terminal.send(b"\x16")
    terminal.until(terminal.animated)
    terminal.marker(b"agent00@example.com")
    terminal.send(b"j\x0b")  # Select account 01, then Ctrl+K reorders it upward.
    terminal.marker(b"moved account Account 01 up")
    terminal.send(b"\x1b[1;5B")  # Ctrl+Down reorders providers, not accounts.
    terminal.marker(b"moved provider codex down")
    # Dense pages truncate bubble emails; compare unique visible prefixes for
    # spatial order while still requiring the full focused email in the footer.
    def bubble_column(prefix):
        # Cats can sleep/jump at different heights: compare horizontal screen
        # coordinates, not row-major text offsets (which also include footer).
        return min((text.find(prefix) for row in terminal.screen.cells[:-4]
                    if (text := "".join(row)).find(prefix) >= 0), default=-1)

    terminal.until(lambda: 0 <= bubble_column("gemini@") < bubble_column("agent01@"))
    terminal.marker(b"agent01@example.com")
    terminal.send(b"\x1b[1;5A")
    terminal.marker(b"moved provider codex up")
    terminal.send(b"v")
    terminal.marker(b"1 hidden")
    terminal.until(lambda: "agent01@" not in terminal.screen.text())
    terminal.send(b"u")
    terminal.marker(b"0 hidden")
    terminal.marker(b"agent01@example.com")
    terminal.send(b"p")
    terminal.until(lambda: "agent00@" not in terminal.screen.text())
    terminal.marker(b"gemini@example.com")
    terminal.send(b"u\x1b[Hl")
    terminal.marker(b"agent01@example.com")
    terminal.send(b"d")
    terminal.until(lambda: gateway.count("/credentials/toggle") == 1)
    terminal.marker(b"account updated")
    terminal.send(b"e")
    terminal.until(lambda: gateway.count("/credentials/toggle") == 2)
    terminal.marker(b"account updated")
    form = parse_qs(gateway.posts[-1][1].decode())
    assert form == {"file_name": ["account-01.json"], "enabled": ["true"]}
    terminal.send(b"x")
    terminal.marker(b"Delete Account")
    terminal.send(b"n")
    terminal.marker(b"delete cancelled")
    assert gateway.count("/credentials/delete") == 0
    terminal.send(b"xy")  # Confirmation uses the same snapshotted target as normal UI.
    terminal.until(lambda: gateway.count("/credentials/delete") == 1)
    terminal.marker(b"account deleted")
    assert parse_qs(gateway.posts[-1][1].decode())["file_name"] == ["account-01.json"]
    terminal.send(b"n")
    terminal.marker(b"Provider [1=Codex")
    terminal.send(b"\n")  # Invalid/blank choice returns without submitting credentials.
    terminal.marker(b"unknown provider")
    terminal.send(b"h")
    terminal.marker(b"Custom Models \xc2\xb7 Custom Models")
    terminal.send(b"d")
    terminal.until(lambda: gateway.count("/custom-models/save") == 1)
    terminal.marker(b"model updated")
    terminal.send(b"x")
    terminal.marker(b"Delete Custom Model")
    terminal.send(b"n")
    terminal.marker(b"delete cancelled")
    return {"shared_filters_reordering_prompts_and_confirmed_actions": True}


def independent_theme(terminal, gateway, binary, theme="pixel-office", label="Office"):
    terminal.install_plugin(binary, theme)
    terminal.send(b"\x16")
    terminal.marker(f"{label} / ".encode(), timeout=0.5)
    terminal.send(b"t")
    missing = "Station" if theme == "pixel-office" else "Café"
    terminal.marker(f"{missing} theme unavailable".encode())
    terminal.install_plugin(binary)
    terminal.send(b"tt")
    terminal.marker(f"{label} / ".encode())
    preference = Path(terminal.env["XDG_CONFIG_HOME"]) / "iogw/plugins/selected-theme.json"
    terminal.until(lambda: preference.is_file() and json.loads(preference.read_text())["theme"] == theme)
    restored = Terminal(binary, gateway, config_dir=terminal.env["XDG_CONFIG_HOME"])
    try:
        restored.marker(b"? commands")  # Normal UI still opens first.
        restored.send(b"\x16")
        restored.marker(f"{label} / ".encode(), timeout=0.5)
    finally:
        restored.close()
    return {"independent_theme_and_preference_survive_restart": theme}


def station_theme(terminal, gateway, binary):
    for theme in ("pixel-cafe", "pixel-office", "pixel-station"):
        terminal.install_plugin(binary, theme)
    terminal.send(b"\x16")
    terminal.marker("Café / ".encode())
    terminal.send(b"t")
    terminal.marker(b"Office / ")
    terminal.send(b"t")
    terminal.marker(b"Station / ")
    for value in (b"Machine", b"CPU", b"GPU", b"RAM", b"DISK", b"NET", b"Requests", b"Tokens"):
        terminal.marker(value)
    terminal.send(b"\x1b[Hl")
    terminal.until(lambda: "Account Usage · agent00@example.com" in terminal.screen.text())
    terminal.send(b"\r")
    terminal.marker(b"Account Details")
    terminal.send(b"\x1b")
    terminal.marker(b"Station / ")
    terminal.send(b"b ")  # Hide bubbles and pause all scenery.
    terminal.marker(b"PAUSED")
    terminal.until(lambda: not any("╭" in "".join(row) for row in terminal.screen.cells[:-4]))
    latency = terminal.help_latency()
    terminal.send(b"\x1b")
    terminal.marker(b"Station / ")
    preference = Path(terminal.env["XDG_CONFIG_HOME"]) / "iogw/plugins/selected-theme.json"
    assert json.loads(preference.read_text())["theme"] == "pixel-station"
    terminal.send(b"t")
    terminal.marker("Café / ".encode())
    return {"three_theme_cycle_shared_details_and_pause": True, "help_ms": round(latency, 2)}


def quota_summary_details(terminal, gateway, binary):
    terminal.marker(b"111")
    terminal.until(lambda: gateway.count("/dashboard/snapshot.json") > 0)
    # First check an older gateway's balance shape through the local projection.
    terminal.send(b"\r")
    terminal.marker(b"Account Details")
    terminal.marker(b"balance 4.50")
    terminal.send(b"\x1b")
    terminal.until(lambda: "Account Details" not in terminal.screen.text())
    # Then supply the new wire format, with enough wrapping to require scroll.
    gateway.quotas["deepseek"]["accounts"][0]["quota_summary"] = [
        {"label": "Balance / USD", "value": "balance 4.50; granted 1.00; paid 3.50"},
        *[{"label": f"Window {i}", "value": "reported quota window and reset metadata " * 4}
          for i in range(8)],
        {"label": "Quota note", "value": "END_SUMMARY"},
    ]
    terminal.send(b"R")
    terminal.install_plugin(binary)
    terminal.send(b"\x16")
    terminal.until(terminal.animated)
    terminal.marker(b"deepseek@example.com")
    # Wait for the explicit refresh instead of accidentally testing old data.
    terminal.until(lambda: gateway.count("/dashboard/snapshot.json") >= 2)
    terminal.drain(0.1)
    fcntl.ioctl(terminal.master, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    terminal.screen = Screen(24, 80)
    terminal.until(terminal.animated)
    terminal.send(b"\r")
    terminal.marker(b"balance 4.50")
    for _ in range(50):
        if "END_SUMMARY" in terminal.screen.text():
            break
        terminal.send(b"\x1b[6~")
        terminal.drain(0.02)
    terminal.marker(b"END_SUMMARY")
    assert "SECRET_TEST_ONLY" not in terminal.screen.text()
    terminal.send(b"\x1b")
    terminal.until(terminal.animated)
    return {"old_gateway_balance_new_summary_and_wrapped_modal_scrolling": True}


def refresh_and_bubbles(terminal, gateway, binary):
    terminal.install_plugin(binary)
    terminal.marker(b"111")
    terminal.send(b"\x1bOS")
    terminal.marker(b"agent00@example.com")
    for marker in [b"Machine", b"CPU", b"GPU", b"RAM", b"DISK /", b"NET"]:
        terminal.marker(marker)
    terminal.marker(b"Machine")
    terminal.marker(b"75%")
    terminal.until(lambda: "% used" not in terminal.screen.text())
    terminal.send(b"b")
    terminal.until(lambda: "▗" not in terminal.screen.text())
    terminal.send(b"j")
    terminal.send(b"b")
    terminal.until(lambda: "▗" in terminal.screen.text())
    terminal.marker(b"agent01@example.com")
    gateway.delay["/models/refresh"] = 0.6
    terminal.send(b"rrrj")
    terminal.until(lambda: gateway.count("/models/refresh") == 1)
    terminal.marker(b"[o o]")
    terminal.marker(b"ACCOUNT")
    assert parse_qs(gateway.posts[-1][1].decode()) == {"provider": ["cod"], "file_name": ["account-01.json"]}
    latency = terminal.help_latency()
    terminal.send(b"?")
    terminal.marker(b"quota refresh complete")
    terminal.send(b"R")
    terminal.marker(b"ALL")
    terminal.until(lambda: gateway.count("/admin/api-keys") > 0 and gateway.count("/notifications/settings") > 0)
    assert gateway.count("/models/refresh") == 1, "R unexpectedly forced provider API calls"
    terminal.send(b"\x12")  # Ctrl+R; legacy terminals also encode Ctrl+Shift+R this way.
    terminal.until(lambda: gateway.count("/models/refresh") == 2)
    terminal.marker(b"HARD")
    assert gateway.posts[-1] == ("/models/refresh", b"")
    terminal.marker(b"quota refresh complete")
    gateway.fail.add("/models/refresh")
    terminal.send(b"r")
    terminal.marker(b"injected failure")
    gateway.fail.remove("/models/refresh")
    terminal.send(b"r")
    terminal.marker(b"quota refresh complete")
    return {"bubble_toggle_scoped_all_hard_refresh_and_error_recovery": True, "help_during_refresh_ms": round(latency, 2)}


def plugin_lifecycle(binary):
    pack = Path(__file__).resolve().parents[1] / "src/bin/iogw/plugins/pixel-cafe.json"
    with tempfile.TemporaryDirectory(prefix="iogw-plugin-test-") as directory:
        env = dict(os.environ, XDG_CONFIG_HOME=directory)

        def command(*args, success=True):
            result = subprocess.run(
                [str(binary), "--base-url", "not-a-valid-gateway-url", "--json", "plugins", *args],
                env=env, capture_output=True, text=True, timeout=3,
            )
            assert (result.returncode == 0) == success, result.stderr
            return json.loads(result.stdout) if success else None

        assert command("list")["plugins"] == []
        installed = command("install", str(pack))
        target = Path(installed["path"])
        original = target.read_bytes()
        command("install", str(pack))  # Atomic update also works over an existing file.
        invalid = Path(directory) / "invalid.json"
        value = json.loads(original)
        value["schema_version"] = 999
        invalid.write_text(json.dumps(value))
        command("install", str(invalid), success=False)
        assert target.read_bytes() == original, "invalid pack overwrote a working installation"
        assert command("list")["plugins"][0]["id"] == "pixel-cafe"
        office = command("install", str(pack.with_name("pixel-office.json")))
        assert [item["id"] for item in command("list")["plugins"]] == ["pixel-cafe", "pixel-office"]
        command("remove", "../not-a-plugin", success=False)
        assert target.exists()
        command("remove", "pixel-cafe")
        assert [item["id"] for item in command("list")["plugins"]] == ["pixel-office"]
        assert Path(office["path"]).exists()
        command("remove", "pixel-office")
        assert command("list")["plugins"] == []
        station = command("install", str(pack.with_name("pixel-station.json")))
        assert [item["id"] for item in command("list")["plugins"]] == ["pixel-station"]
        assert Path(station["path"]).exists()
        command("remove", "pixel-station")
        assert command("list")["plugins"] == []
        command("remove", "pixel-cafe")  # Removing an absent pack is harmless.
    print(json.dumps({"case": "plugin lifecycle", "offline_install_update_validation_and_remove": True}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/release/iogw"))
    binary = parser.parse_args().binary.resolve(strict=True)
    plugin_lifecycle(binary)
    run_case(binary, "stalled startup", lambda g: g.delay.update({"/admin/session": 1.5}), stalled_startup)
    run_case(binary, "stalled models and refresh burst", lambda g: g.delay.update({"/custom-models.json": 1.5}), stalled_models)
    run_case(binary, "independent updates", lambda g: None, partial_failure)
    run_case(binary, "stalled management action", lambda g: g.delay.update({"/notifications/test": 1.5}), stalled_action)

    def login_setup(gateway):
        gateway.login_required = True
        gateway.delay["/admin/login"] = 1.5

    run_case(binary, "stalled login", login_setup, stalled_login)
    run_case(binary, "login completes", lambda g: setattr(g, "login_required", True), completed_login)
    run_case(binary, "missing optional plugin", lambda g: None, missing_plugin)
    run_case(binary, "animated UI", lambda g: None, lambda t, g: animated_ui(t, g, binary))
    run_case(binary, "animated auth", lambda g: setattr(g, "login_required", True),
             lambda t, g: animated_auth(t, g, binary))
    for theme in ("pixel-office", "pixel-station"):
        run_case(binary, f"animated auth {theme}", lambda g: setattr(g, "login_required", True),
                 lambda t, g, theme=theme: animated_auth(t, g, binary, theme))

    def failed_login(gateway):
        gateway.login_required = True
        gateway.fail.add("/admin/login")
        gateway.delay["/admin/login"] = 1.5

    run_case(binary, "animated auth failure retry and expiry", failed_login,
             lambda t, g: animated_auth_retry(t, g, binary))
    run_case(binary, "independent office theme", lambda g: None,
             lambda t, g: independent_theme(t, g, binary))
    run_case(binary, "independent station theme", lambda g: None,
             lambda t, g: independent_theme(t, g, binary, "pixel-station", "Station"))
    run_case(binary, "animated failed section", lambda g: g.fail.add("/custom-models.json"),
             lambda t, g: animated_ui(t, g, binary))

    def many_accounts(gateway):
        gateway.accounts = [
            {"provider": "codex", "key": f"account-{index:02}", "label": f"Account {index:02}",
             "credential_file": f"account-{index:02}.json",
             "enabled": index % 3 != 0}
            for index in range(25)
        ]
        gateway.quotas = {"codex": {"accounts": [
            {"key": f"account-{index:02}", "email": f"agent{index:02}@example.com",
             "code_generation": {"five_hour": {"used_percent": 75 - index, "reset_label": "resets in 1h 23m"},
                                 "weekly": {"used_percent": 10, "reset_label": "resets in 1d 2h"}}}
            for index in range(24)
        ]}}

    run_case(binary, "animated account pagination", many_accounts,
             lambda t, g: animated_ui(t, g, binary))
    run_case(binary, "station theme and three-theme cycling", many_accounts,
             lambda t, g: station_theme(t, g, binary))
    run_case(binary, "animated telemetry bubbles and refresh keys", many_accounts,
             lambda t, g: refresh_and_bubbles(t, g, binary))

    def action_accounts(gateway):
        many_accounts(gateway)
        gateway.accounts.append({"provider": "gemini", "key": "gemini-account", "label": "Gemini Tester", "enabled": True})
        gateway.quotas["gemini"] = {"accounts": [{"key": "gemini-account", "email": "gemini@example.com", "models": []}]}

    run_case(binary, "animated Overview actions", action_accounts,
             lambda t, g: animated_actions(t, g, binary))

    def summary_account(gateway):
        gateway.accounts = [{"provider": "deepseek", "key": "balance.json",
                             "credential_file": "balance.json", "label": "Balance account", "enabled": True}]
        gateway.quotas = {"deepseek": {"accounts": [{"file_name": "balance.json",
            "email": "deepseek@example.com", "balances": [{"currency": "USD", "total_balance": "4.50"}],
            "api_key": "SECRET_TEST_ONLY"}]}}

    run_case(binary, "provider quota summaries", summary_account,
             lambda t, g: quota_summary_details(t, g, binary))


if __name__ == "__main__":
    main()
