#!/usr/bin/env python3
"""Exercise the real 3D TUI against demo accounts; requires a POSIX PTY and GPU.

python3 scripts/test-iogw-3d.py --binary target/release/iogw --python /path/to/python
"""
import argparse
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import signal
import struct
import sys
import tempfile
import termios
import threading

sys.dont_write_bytecode = True
spec = importlib.util.spec_from_file_location(
    "tui_test", Path(__file__).with_name("test-iogw-performance.py"))
tui = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tui)


class SixelScreen(tui.Screen):
    """A screen that sets sixel (DCS) images aside, as a terminal's graphics layer would."""

    def __init__(self, *args):
        super().__init__(*args)
        self.images = []  # (row, column, payload) of every image received
        self.partial = None

    def feed(self, data):
        while data:
            if self.partial is not None:
                self.partial += data
                end = self.partial.find(b"\x1b\\")
                if end < 0:
                    return
                self.images.append((*self.where, bytes(self.partial[:end])))
                data, self.partial = bytes(self.partial[end + 2:]), None
                continue
            start = data.find(b"\x1bP")
            super().feed(data if start < 0 else data[:start])
            if start < 0:
                return
            self.where = (self.row, self.column)
            self.partial, data = bytearray(), data[start + 2:]


def pixel_size(terminal, rows=42, columns=180, cell=(6, 15)):
    """Tell the app how big a cell is in pixels, as a real terminal does."""
    fcntl.ioctl(terminal.master, termios.TIOCSWINSZ,
                struct.pack("HHHH", rows, columns, columns * cell[0], rows * cell[1]))


def label_at(terminal, text):
    """Screen position of an actor's label within the scene rows, or None."""
    for row in range(2, terminal.screen.rows - 4):
        column = "".join(terminal.screen.cells[row]).find(text)
        if column >= 0:
            return column + len(text) // 2, row


def click_at(terminal, x, y):
    terminal.send(f"\x1b[<0;{x+1};{y+1}M\x1b[<0;{x+1};{y+1}m".encode())


def scene_text(terminal):
    return "\n".join("".join(row) for row in terminal.screen.cells[2:terminal.screen.rows - 4])


def drag(terminal, x, y, columns, steps=2):
    """Press at (x, y), move by `columns` in steps, then release."""
    send = lambda code, column, final: terminal.send(
        f"\x1b[<{code};{column+1};{y+1}{final}".encode())
    send(0, x, "M")
    for step in range(1, steps + 1):
        send(32, x + columns * step // steps, "M")
    send(0, x + columns, "m")


def click(terminal, text):
    """Cats walk, so click a label where it is now rather than at a fixed point."""
    terminal.until(lambda: label_at(terminal, text))
    position = label_at(terminal, text)
    click_at(terminal, *position)
    return position


def focus(terminal, index):
    terminal.marker(f"Account Usage · agent{index:02}@example.com".encode())


def child_pids(terminal):
    root = Path(f"/proc/{terminal.process.pid}/task")
    return {int(pid) for item in root.glob("*/children") for pid in item.read_text().split()}


def cpu(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(fields[11]) + int(fields[12])


def real_gpu(terminal, gateway):
    terminal.marker(b"111")
    terminal.until(lambda: gateway.count("/dashboard/snapshot.json") > 0)
    terminal.send(b"\x1b[15~")  # F5
    terminal.marker(b"agent05@example.com", timeout=15)
    terminal.send(b"\x1b[H")  # Models, then a real account by mouse.
    position = click(terminal, "agent05@example.com")
    focus(terminal, 5)
    terminal.send(b"\r")
    terminal.marker(b"Account Details")
    click_at(terminal, *position)  # Clicks behind the details modal must do nothing.
    terminal.send(b"\x1b")
    terminal.until(lambda: "Account Details" not in terminal.screen.text())
    focus(terminal, 5)
    terminal.send(b"d")
    terminal.until(lambda: gateway.count("/credentials/toggle") == 1)
    posted = tui.parse_qs(gateway.posts[-1][1].decode())
    assert posted["file_name"] == ["account-05.json"]
    terminal.marker(b"Status Disabled")  # Wait for the shared action to finish.
    terminal.send(b"e")
    terminal.until(lambda: gateway.count("/credentials/toggle") == 2)
    terminal.send(b"\x1b[6~")
    terminal.marker(b"page 2/3")
    focus(terminal, 6)
    terminal.send(b"\x1b[6~")
    terminal.marker(b"page 3/3")
    focus(terminal, 13)
    terminal.send(b"\x1b[F")
    focus(terminal, 15)
    terminal.send(b"\x1b[H")
    terminal.marker(b"Custom Models")
    terminal.marker(b"page 1/3")
    terminal.drain(.15)
    click(terminal, "Custom Models")
    terminal.send(b"r")
    terminal.marker(b"no focused account")
    assert gateway.count("/models/refresh") == 0
    terminal.send(b"\r")
    terminal.marker(b"Custom Model Details")
    terminal.send(b"\x1b")
    terminal.until(lambda: "Custom Model Details" not in terminal.screen.text())
    terminal.send(b"]")
    terminal.drain(.25)
    click(terminal, "agent04@example.com")
    focus(terminal, 4)
    terminal.send(b"0")
    terminal.drain(.25)
    click(terminal, "agent01@example.com")
    focus(terminal, 1)
    terminal.send(b" ")
    terminal.marker(b"PAUSED")
    terminal.drain(.5)
    # Dragging orbits the camera and must not select the actor under the press.
    names = [f"agent0{index}@example.com" for index in range(6)]
    before = [label_at(terminal, name) for name in names]
    drag(terminal, *label_at(terminal, "agent04@example.com"), -16)
    terminal.drain(.5)
    assert before != [label_at(terminal, name) for name in names], "drag did not orbit"
    assert "Account Usage \u00b7 agent04@example.com" not in terminal.screen.text()
    focus(terminal, 1)
    terminal.send(b"0")  # Reset the camera.
    terminal.drain(.5)
    # `a` switches the quota view; labels carry the same rows as the animated bubbles.
    assert "Codex" in scene_text(terminal) and "Credits" not in scene_text(terminal)
    terminal.send(b"a")
    terminal.until(lambda: "Credits Not reported" in scene_text(terminal))
    terminal.send(b"a")
    terminal.until(lambda: "Credits" not in scene_text(terminal))
    pids = child_pids(terminal)
    assert len(pids) == 1, pids
    pid = next(iter(pids))
    before = cpu(pid)
    terminal.drain(.7)
    assert cpu(pid) - before <= 2, "paused renderer still consumed substantial CPU"
    terminal.send(b" ")
    terminal.until(lambda: "PAUSED" not in terminal.screen.text())
    # Cats patrol: at least one visible label must have moved a few seconds later.
    before = [label_at(terminal, name) for name in names]
    terminal.drain(3.5)
    assert before != [label_at(terminal, name) for name in names], "cats are not walking"
    # The compositor and picker adapt to the actual terminal size.
    fcntl.ioctl(terminal.master, termios.TIOCSWINSZ, struct.pack("HHHH", 54, 160, 0, 0))
    terminal.screen = tui.Screen(54, 160)
    terminal.marker(b"agent05@example.com")
    terminal.drain(.25)
    click(terminal, "agent03@example.com")
    focus(terminal, 3)
    latency = terminal.help_latency()
    assert latency < 300, latency
    terminal.send(b"\x1b")
    terminal.until(lambda: "Commands" not in terminal.screen.text())
    # A helper crash returns to the normal TUI and reaps the renderer.
    os.kill(pid, signal.SIGKILL)
    terminal.marker(b"3D unavailable")
    assert not Path(f"/proc/{pid}").exists()
    assert b"\x1b[?1003l" in terminal.output  # Mouse capture restored.
    terminal.send(b"\x07")  # Ctrl+G starts a new helper.
    terminal.marker(b"agent05@example.com", timeout=15)
    terminal.send(b"\x1b")
    terminal.until(lambda: "3D Caf" not in terminal.screen.text())
    assert not child_pids(terminal)
    return {"gpu_clicks_details_actions_paging_camera_resize_pause_crash_reentry": True,
            "help_ms": round(latency, 2)}


def sixel_gpu(terminal, gateway):
    terminal.marker(b"111")
    pixel_size(terminal)
    terminal.screen = SixelScreen(42, 180)
    terminal.send(b"\x1b[15~")  # F5
    terminal.marker("\u00b7 sixel".encode(), timeout=15)
    terminal.marker(b"agent05@example.com", timeout=15)  # labels stay real text
    terminal.until(lambda: len(terminal.screen.images) > 3, timeout=10)
    assert "\u2580" not in scene_text(terminal), "half-blocks drawn in sixel mode"
    # Images are pixel-sized: a 180-column scene is 1080 px wide, never wider.
    widths = [int(payload[payload.index(b'"') + 1:].split(b";")[2])
              for _, _, payload in terminal.screen.images]
    assert max(widths) <= 1080 and max(widths) > 100, widths
    # Selecting by label still works, and an orbit sends new pixels.
    click(terminal, "agent05@example.com")
    focus(terminal, 5)
    count = len(terminal.screen.images)
    terminal.send(b"]")
    terminal.until(lambda: len(terminal.screen.images) > count)
    # A paused scene sends nothing at all.
    terminal.send(b" ")
    terminal.marker(b"PAUSED")
    terminal.drain(.6)
    count = len(terminal.screen.images)
    terminal.drain(1.0)
    assert len(terminal.screen.images) == count, "a paused scene kept sending images"
    return {"sixel_images_labels_picking_orbit_and_pause": True, "images": count}


def sixel_unavailable(terminal, gateway):
    terminal.marker(b"111")  # this terminal reports no pixel size
    terminal.send(b"\x1b[15~")
    terminal.marker(b"sixel needs the terminal's pixel size", timeout=15)
    terminal.until(lambda: "\u2580" in scene_text(terminal))
    return {"sixel_without_pixel_size_falls_back_to_half_blocks": True}


def sixel_in_tmux(terminal, gateway):
    terminal.marker(b"111")
    pixel_size(terminal)
    terminal.send(b"\x1b[15~")
    terminal.marker(b"allow-passthrough on", timeout=15)
    terminal.until(lambda: "\u2580" in scene_text(terminal))
    return {"sixel_in_tmux_without_passthrough_falls_back_and_says_why": True}


def missing_python(terminal, gateway):
    terminal.marker(b"111")
    terminal.send(b"\x07")
    terminal.marker(b"3D unavailable")
    assert terminal.help_latency() < 300
    return {"missing_python_preserves_normal_ui": True}


def stalled_helper(terminal, gateway):
    terminal.marker(b"111")
    terminal.send(b"\x07")
    terminal.marker(b"starting GPU")
    assert terminal.help_latency() < 300
    pids = child_pids(terminal)
    terminal.send(b"\x1b")  # Close help.
    terminal.until(lambda: "Commands" not in terminal.screen.text())
    terminal.send(b"\x1b")  # Exit 3D while the pipe worker is blocked.
    terminal.until(lambda: "3D Caf" not in terminal.screen.text())
    assert all(not Path(f"/proc/{pid}").exists() for pid in pids)
    return {"stalled_helper_does_not_block_help_or_exit": True}

def software_gpu(terminal, gateway):
    terminal.marker(b"111")
    terminal.send(b"\x07")
    terminal.marker(b"3D unavailable", timeout=15)
    assert not child_pids(terminal)
    return {"software_renderer_rejected_and_normal_ui_preserved": True}


def authenticated_gpu(terminal, gateway):
    terminal.marker(b"Admin login required.")
    terminal.send(b"\x07")
    terminal.marker("Login required · [o] Log in".encode(), timeout=15)
    assert gateway.count("/admin/account-routing") == 0
    offset, _ = terminal.send(b"o")
    terminal.prompt(b"OTP:", offset)
    terminal.send(b"123456\n")
    terminal.marker(b"logged in")
    terminal.marker(b"agent05@example.com", timeout=15)
    assert gateway.count("/admin/login") == 1
    assert "Gateway locked" not in terminal.screen.text()
    return {"3d_uses_shared_login_and_authentication_gates": True}


def startup(binary):
    gateway = tui.Gateway()
    accounts(gateway)
    threading.Thread(target=gateway.serve_forever, daemon=True).start()
    terminal = tui.Terminal(binary, gateway, arguments=("tui", "--3d"))
    try:
        terminal.marker(b"agent05@example.com", timeout=15)
        assert "3D Caf" in terminal.screen.text()
        print(json.dumps({"case":"3D CLI startup","explicit_startup_flag":True}),flush=True)
    finally:
        terminal.close()
        gateway.shutdown()
        gateway.server_close()


def accounts(gateway):
    gateway.accounts = [{"provider": "codex", "key": f"account-{i:02}",
        "label": f"Account {i:02}", "credential_file": f"account-{i:02}.json",
        "enabled": True} for i in range(16)]
    gateway.quotas = {"codex": {"accounts": [{"key": f"account-{i:02}",
        "email": f"agent{i:02}@example.com",
        "code_generation": {"five_hour": {"used_percent": 25+i, "reset_label": "resets in 1h"}}}
        for i in range(16)]}}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--python", default=os.environ.get("IOGW_PANDA3D_PYTHON", "python3"))
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    os.environ.pop("NO_COLOR", None)
    os.environ["COLORTERM"] = "truecolor"
    original = os.environ.get("IOGW_PANDA3D_PYTHON")
    try:
        os.environ["IOGW_PANDA3D_PYTHON"] = "/nonexistent/iogw-python"
        tui.run_case(binary, "3D missing runtime", accounts, missing_python)
        with tempfile.TemporaryDirectory(prefix="iogw-stalled-renderer-") as directory:
            helper = Path(directory) / "python"
            helper.write_text("#!/bin/sh\nexec sleep 60\n")
            helper.chmod(0o700)
            os.environ["IOGW_PANDA3D_PYTHON"] = str(helper)
            tui.run_case(binary, "3D stalled helper", accounts, stalled_helper)
        os.environ["IOGW_PANDA3D_PYTHON"] = args.python
        tui.run_case(binary, "3D real GPU", accounts, real_gpu)
        saved = {name: os.environ.get(name) for name in ("IOGW_3D_SIXEL", "TMUX", "TMUX_PANE")}
        try:
            os.environ["IOGW_3D_SIXEL"] = "1"
            os.environ.pop("TMUX", None)
            os.environ.pop("TMUX_PANE", None)
            tui.run_case(binary, "3D sixel", accounts, sixel_gpu)
            tui.run_case(binary, "3D sixel without a pixel size", accounts, sixel_unavailable)
            # A tmux that cannot be reached has no passthrough: sixel must be refused.
            os.environ["TMUX"], os.environ["TMUX_PANE"] = "/nonexistent/iogw-tmux,1,0", "%0"
            tui.run_case(binary, "3D sixel in tmux without passthrough", accounts, sixel_in_tmux)
        finally:
            for name, value in saved.items():
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value
        previous_software = os.environ.get("LIBGL_ALWAYS_SOFTWARE")
        os.environ["LIBGL_ALWAYS_SOFTWARE"] = "1"
        try:
            tui.run_case(binary, "3D software GPU rejected", accounts, software_gpu)
        finally:
            if previous_software is None:
                os.environ.pop("LIBGL_ALWAYS_SOFTWARE", None)
            else:
                os.environ["LIBGL_ALWAYS_SOFTWARE"] = previous_software
        def locked(gateway):
            accounts(gateway)
            gateway.login_required = True
        tui.run_case(binary, "3D authentication", locked, authenticated_gpu)
        startup(binary)
    finally:
        if original is None:
            os.environ.pop("IOGW_PANDA3D_PYTHON", None)
        else:
            os.environ["IOGW_PANDA3D_PYTHON"] = original


if __name__ == "__main__":
    main()
