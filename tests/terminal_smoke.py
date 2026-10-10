"""Focused PTY regressions for terminal restoration, child cleanup, and idle output.

Uses Python's standard library and isolated fake CLIs. State, parsing, keybinding,
and layout coverage lives in the Rust tests; check appearance and real VPN
behavior manually.
"""

import errno
import fcntl
import json
import os
from pathlib import Path
import re
import select
import signal
import struct
import subprocess
import tempfile
import termios
import time
import unicodedata
import unittest


ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("NORTUI_TEST_BINARY", ROOT / "target/debug/nortui"))
FAKE = r'''#!/usr/bin/python3
import json, os, sys, termios, time
from pathlib import Path
root = Path(os.environ["NORTUI_FIXTURE"])
name = Path(sys.argv[0]).name
args = sys.argv[1:]
fd = os.open(root / "commands.jsonl", os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o600)
os.write(fd, (json.dumps([name, *args]) + "\n").encode())
os.close(fd)
if name == "systemctl":
    if args[0] == "show":
        print((root / "service").read_text())
    elif args[0] in ("start", "stop"):
        (root / "service").write_text("active" if args[0] == "start" else "inactive")
        if args[0] == "stop":
            (root / "vpn").write_text("Disconnected")
    else:
        sys.exit(2)
elif name == "sudo":
    if args == ["-n", "-v"]:
        sys.exit(0 if (root / "credentials").exists() else 1)
    if args == ["-v"]:
        (root / "auth.pid").write_text(str(os.getpid()))
        attributes = termios.tcgetattr(0)
        flags = attributes[3]
        assert flags & termios.ICANON and flags & termios.ECHO, "sudo received raw terminal"
        password_attributes = termios.tcgetattr(0)
        password_attributes[3] &= ~termios.ECHO
        termios.tcsetattr(0, termios.TCSANOW, password_attributes)
        try:
            print("AUTH_COOKED password: ", end="", flush=True)
            sys.stdin.readline()
        finally:
            # A killed authentication child cannot execute this restoration.
            termios.tcsetattr(0, termios.TCSANOW, attributes)
        failure = int(os.environ.get("NORTUI_AUTH_FAILURE", "0"))
        if failure:
            sys.exit(failure)
        (root / "credentials").touch()
    else:
        assert args[0] == "-n" and (root / "credentials").exists()
        os.execv(str(root / "systemctl"), [str(root / "systemctl"), *args[2:]])
elif name == "nordvpn":
    assert (root / "service").read_text() == "active", "queried a stopped daemon"
    if args[0] == "status":
        state = (root / "vpn").read_text()
        print("Status: " + state)
        if state == "Connected":
            print("Country: United States\nCity: New York\nIP: 192.0.2.10\nCurrent technology: NORDLYNX")
    elif args[0] == "countries":
        print("United_States\nJapan\nMonaco")
    elif args[0] == "cities":
        if args[1] == "Monaco":
            print("Servers by city are not available for this country.", file=sys.stderr)
            sys.exit(1)
        (root / f"city-{args[1]}.pid").write_text(str(os.getpid()))
        time.sleep(float(os.environ.get(f"NORTUI_{args[1].upper()}_CITY_DELAY", "0")))
        print("New_York\nLos_Angeles" if args[1] == "United_States" else "Tokyo")
    elif args[0] == "connect":
        (root / "connect.pid").write_text(str(os.getpid()))
        time.sleep(float(os.environ.get("NORTUI_CONNECT_DELAY", "0.1")))
        (root / "vpn").write_text("Connected")
        print("You are connected to NordVPN.")
    elif args[0] == "disconnect":
        (root / "vpn").write_text("Disconnected")
        print("You are disconnected.")
    else:
        sys.exit(2)
'''


class Screen:
    """Decode the small subset of terminal output used by Ratatui/Crossterm."""

    def __init__(self, width=100, height=24):
        self.width, self.height = width, height
        self.x = self.y = 0
        self.rows = [[" "] * width for _ in range(height)]
        self.pending = ""
        self.cursor_visible = False
        self.alternate_active = False
        self.primary_rows = self.rows
        self.primary_cursor = (0, 0)
        self.restored_screens = []

    def feed(self, text):
        self.pending += text
        while self.pending:
            if self.pending.startswith("\x1b["):
                match = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", self.pending)
                if not match:
                    return
                parameters, _, command = match.groups()
                self.pending = self.pending[match.end():]
                if parameters.startswith("?"):
                    if parameters == "?25":
                        self.cursor_visible = command == "h"
                    if parameters == "?1049":
                        if command == "h" and not self.alternate_active:
                            self.primary_rows = self.rows
                            self.primary_cursor = (self.x, self.y)
                            self.rows = [[" "] * self.width for _ in range(self.height)]
                            self.x = self.y = 0
                            self.alternate_active = True
                        elif command == "l" and self.alternate_active:
                            self.rows = self.primary_rows
                            self.x, self.y = self.primary_cursor
                            self.alternate_active = False
                            self.restored_screens.append(self.text())
                    continue
                values = [int(value or 0) for value in parameters.split(";")]
                if command in "Hf":
                    self.y = (values[0] or 1) - 1
                    self.x = (values[1] if len(values) > 1 else 1) - 1
                elif command == "J" and values[0] in (2, 3):
                    self.rows = [[" "] * self.width for _ in range(self.height)]
                elif command == "K":
                    self.rows[self.y][self.x:] = [" "] * (self.width - self.x)
                elif command == "G":
                    self.x = (values[0] or 1) - 1
                elif command == "C":
                    self.x += values[0] or 1
                elif command == "D":
                    self.x -= values[0] or 1
                elif command == "A":
                    self.y -= values[0] or 1
                elif command == "B":
                    self.y += values[0] or 1
                continue
            if self.pending.startswith("\x1b"):
                if len(self.pending) < 2:
                    return
                self.pending = self.pending[2:]
                continue
            character, self.pending = self.pending[0], self.pending[1:]
            if character == "\r":
                self.x = 0
            elif character == "\n":
                self.y = min(self.y + 1, self.height - 1)
            elif not character.isprintable():
                continue
            else:
                if 0 <= self.x < self.width and 0 <= self.y < self.height:
                    self.rows[self.y][self.x] = character
                self.x += 0 if unicodedata.combining(character) else (2 if unicodedata.east_asian_width(character) in "WF" else 1)

    def text(self):
        return "\n".join("".join(row) for row in self.rows)


class Widget:
    def __init__(self, service="inactive", cached=False, vpn="Disconnected", width=100, height=24, **environment):
        self.directory = tempfile.TemporaryDirectory(prefix="nortui-terminal-")
        self.root = Path(self.directory.name)
        (self.root / "service").write_text(service)
        (self.root / "vpn").write_text(vpn)
        for program in ("nordvpn", "systemctl", "sudo"):
            path = self.root / program
            path.write_text(FAKE)
            path.chmod(0o700)
        if cached:
            path = self.root / "cache/nortui/locations.json"
            path.parent.mkdir(parents=True)
            path.write_text(json.dumps({"version": 1, "updated_at": 0, "countries": {"United_States": ["New_York"]}}))
        self.master, self.slave = os.openpty()
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", height, width, 0, 0))
        self.original_attributes = termios.tcgetattr(self.slave)
        env = dict(os.environ, TERM="xterm-256color", PATH=f"{self.root}:/usr/bin", NORTUI_FIXTURE=str(self.root), XDG_CACHE_HOME=str(self.root / "cache"), **environment)
        self.process = subprocess.Popen([BINARY], stdin=self.slave, stdout=self.slave, stderr=self.slave, env=env, start_new_session=True)
        self.screen = Screen(width, height)
        self.raw = b""

    def pump(self, seconds=0.05):
        if self.master is None:
            return
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            ready, _, _ = select.select([self.master], [], [], max(0, deadline - time.monotonic()))
            if ready:
                try:
                    chunk = os.read(self.master, 65536)
                except OSError as error:
                    if error.errno == errno.EIO:
                        return
                    raise
                self.raw += chunk
                self.screen.feed(chunk.decode("utf-8", errors="replace"))

    def wait_for(self, predicate, timeout=6):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.pump()
            if predicate():
                return
        raise AssertionError(f"Timed out; screen:\n{self.screen.text()}\ncommands: {self.commands()}")

    def text(self):
        return self.screen.text()

    def query(self):
        for line in self.text().splitlines():
            if line.lstrip().startswith("› "):
                return line.lstrip()[2:].rstrip()
        return None

    def footer(self):
        return self.text().splitlines()[-1]

    def send(self, data):
        os.write(self.master, data)

    def commands(self):
        path = self.root / "commands.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def close(self):
        if self.process.poll() is None:
            if self.master is not None:
                self.send(b"\x03")
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.pump()
        if self.master is not None:
            os.close(self.master)
        os.close(self.slave)
        self.directory.cleanup()


class TerminalSmoke(unittest.TestCase):
    def widget(self, **kwargs):
        widget = Widget(**kwargs)
        self.addCleanup(widget.close)
        return widget

    def test_idle_views_do_not_repaint(self):
        widget = self.widget(cached=True)
        widget.wait_for(lambda: "Service stopped" in widget.footer())
        for mode in ("panel", "search"):
            with self.subTest(mode=mode):
                if mode == "search":
                    widget.send(b"i")
                    widget.wait_for(lambda: widget.query() == "Search countries and cities…")
                widget.pump(0.15)
                before = len(widget.raw)
                widget.pump(0.4)
                self.assertEqual(len(widget.raw), before, "Idle view kept redrawing")

    def test_cached_city_authenticates_in_cooked_mode_then_connects(self):
        widget = self.widget(cached=True)
        widget.wait_for(lambda: "Service stopped" in widget.text())
        widget.send(b"/new york\r")
        widget.wait_for(lambda: b"AUTH_COOKED" in widget.raw)
        self.assertFalse(termios.tcgetattr(widget.slave)[3] & termios.ECHO)
        widget.send(b"password\n")
        widget.wait_for(lambda: ["nordvpn", "connect", "United_States", "New_York"] in widget.commands())
        widget.wait_for(lambda: "Connected" in widget.text())
        self.assertIn("192.0.2.10", widget.text())
        self.assertIn("NORDLYNX", widget.text())
        self.assertIn("New York", widget.text())
        self.assertIn("United States", widget.text())
        self.assertNotIn("Search countries", widget.text())
        self.assertIsNone(widget.process.poll())
        self.assertFalse(termios.tcgetattr(widget.slave)[3] & termios.ICANON)
        self.assertNotIn(b"password\r\n", widget.raw)

    def test_authentication_failure_restores_tui_without_connecting(self):
        for failure in ("1", "130"):
            with self.subTest(exit_code=failure):
                widget = self.widget(cached=True, NORTUI_AUTH_FAILURE=failure)
                widget.wait_for(lambda: "Service stopped" in widget.text())
                widget.send(b"/new york\r")
                widget.wait_for(lambda: b"AUTH_COOKED" in widget.raw)
                widget.send(b"password\n")
                widget.wait_for(lambda: "Sudo authentication was canceled or failed" in widget.text())
                self.assertFalse(any(command[:2] == ["nordvpn", "connect"] for command in widget.commands()))
                self.assertEqual((widget.root / "service").read_text(), "inactive")
                self.assertFalse(termios.tcgetattr(widget.slave)[3] & termios.ICANON)
                self.assertIn("VPN is off", widget.text())
                self.assertFalse(widget.screen.cursor_visible)

    def test_quitting_after_authentication_never_reveals_sudo_on_primary_screen(self):
        for failure in ("0", "1", "130"):
            with self.subTest(authentication_exit_code=failure):
                widget = self.widget(cached=True, NORTUI_AUTH_FAILURE=failure)
                widget.wait_for(lambda: "Service stopped" in widget.text())
                # Model existing shell content so clearing the primary screen
                # cannot pass merely by removing the saved sudo prompt.
                shell = "Existing shell output"
                widget.screen.primary_rows[0][:len(shell)] = list(shell)
                original_primary = "\n".join("".join(row) for row in widget.screen.primary_rows)
                widget.send(b"/new york\r")
                widget.wait_for(lambda: "AUTH_COOKED" in widget.text())
                self.assertTrue(termios.tcgetattr(widget.slave)[3] & termios.ICANON)
                widget.send(b"password\n")
                if failure == "0":
                    widget.wait_for(lambda: "Connected" in widget.footer())
                else:
                    widget.wait_for(lambda: "Sudo authentication was canceled or failed" in widget.text())
                self.assertFalse(termios.tcgetattr(widget.slave)[3] & termios.ICANON)
                widget.send(b"q")
                self.assertEqual(widget.process.wait(timeout=3), 0)
                widget.pump()
                self.assertFalse(widget.screen.alternate_active)
                self.assertTrue(widget.screen.restored_screens)
                for restored in widget.screen.restored_screens:
                    self.assertEqual(restored, original_primary)
                    self.assertNotIn("AUTH_COOKED", restored)
                self.assertEqual(widget.text(), original_primary)
                self.assertEqual(termios.tcgetattr(widget.slave), widget.original_attributes)

    def test_signals_during_authentication_exit_and_reap_without_running_action(self):
        for exit_signal in (signal.SIGTERM, signal.SIGHUP, signal.SIGINT):
            with self.subTest(signal=exit_signal):
                widget = self.widget(cached=True)
                widget.wait_for(lambda: "Service stopped" in widget.text())
                shell = "Existing shell output"
                widget.screen.primary_rows[0][:len(shell)] = list(shell)
                original_primary = "\n".join("".join(row) for row in widget.screen.primary_rows)
                widget.send(b"c")
                widget.wait_for(lambda: "AUTH_COOKED" in widget.text())
                pid = int((widget.root / "auth.pid").read_text())
                self.assertTrue(Path(f"/proc/{pid}").exists())
                self.assertFalse(termios.tcgetattr(widget.slave)[3] & termios.ECHO)
                widget.process.send_signal(exit_signal)
                self.assertEqual(widget.process.wait(timeout=3), 0)
                widget.pump()
                self.assertFalse(Path(f"/proc/{pid}").exists())
                self.assertEqual((widget.root / "service").read_text(), "inactive")
                self.assertFalse(any(command[:2] in (["nordvpn", "connect"], ["systemctl", "start"]) for command in widget.commands()))
                self.assertFalse((widget.root / "credentials").exists())
                self.assertEqual(termios.tcgetattr(widget.slave), widget.original_attributes)
                self.assertFalse(widget.screen.alternate_active)
                self.assertTrue(widget.screen.restored_screens)
                for restored in widget.screen.restored_screens:
                    self.assertEqual(restored, original_primary)
                    self.assertNotIn("AUTH_COOKED", restored)
                self.assertEqual(widget.text(), original_primary)

    def test_killed_authentication_child_restores_tui_and_original_terminal_settings(self):
        widget = self.widget(cached=True)
        widget.wait_for(lambda: "Service stopped" in widget.text())
        widget.send(b"c")
        widget.wait_for(lambda: "AUTH_COOKED" in widget.text())
        pid = int((widget.root / "auth.pid").read_text())
        self.assertFalse(termios.tcgetattr(widget.slave)[3] & termios.ECHO)
        os.kill(pid, signal.SIGKILL)
        widget.wait_for(lambda: "Sudo authentication was canceled or failed" in widget.text())
        self.assertFalse(Path(f"/proc/{pid}").exists())
        self.assertFalse(termios.tcgetattr(widget.slave)[3] & termios.ICANON)
        self.assertFalse(widget.screen.cursor_visible)
        self.assertEqual((widget.root / "service").read_text(), "inactive")
        self.assertFalse(any(command[:2] in (["nordvpn", "connect"], ["systemctl", "start"]) for command in widget.commands()))
        widget.send(b"q")
        self.assertEqual(widget.process.wait(timeout=3), 0)
        widget.pump()
        self.assertEqual(termios.tcgetattr(widget.slave), widget.original_attributes)
        self.assertFalse(widget.screen.alternate_active)

    def test_closing_terminal_during_authentication_reaps_without_running_action(self):
        widget = self.widget(cached=True)
        widget.wait_for(lambda: "Service stopped" in widget.text())
        widget.send(b"c")
        widget.wait_for(lambda: "AUTH_COOKED" in widget.text())
        pid = int((widget.root / "auth.pid").read_text())
        os.close(widget.master)
        widget.master = None
        self.assertEqual(widget.process.wait(timeout=3), 0)
        self.assertFalse(Path(f"/proc/{pid}").exists())
        self.assertEqual((widget.root / "service").read_text(), "inactive")
        self.assertFalse(any(command[:2] in (["nordvpn", "connect"], ["systemctl", "start"]) for command in widget.commands()))

    def test_quitting_discovery_flushes_latest_partial_catalog_and_reaps_children(self):
        widget = self.widget(service="active", NORTUI_JAPAN_CITY_DELAY="10")
        widget.wait_for(lambda: "Ready to connect" in widget.text())
        widget.send(b"i")
        worker = widget.root / "city-Japan.pid"
        widget.wait_for(lambda: "Los Angeles / United States" in widget.text() and worker.exists())
        pid = int(worker.read_text())
        self.assertIn("Loading", widget.footer())
        widget.send(b"\x03")
        self.assertEqual(widget.process.wait(timeout=3), 0)
        self.assertFalse(Path(f"/proc/{pid}").exists())
        cache = json.loads((widget.root / "cache/nortui/locations.json").read_text())
        self.assertEqual(cache["countries"]["United_States"], ["Los_Angeles", "New_York"])
        self.assertEqual(cache["countries"]["Japan"], [])
        self.assertIn("Monaco", cache["countries"])
        self.assertEqual(termios.tcgetattr(widget.slave), widget.original_attributes)

    def test_closing_terminal_during_connection_reaps_child(self):
        widget = self.widget(service="active", cached=True, NORTUI_CONNECT_DELAY="10")
        widget.wait_for(lambda: "Disconnected" in widget.text())
        widget.send(b"/new york\r")
        widget.wait_for(lambda: (widget.root / "connect.pid").exists())
        pid = int((widget.root / "connect.pid").read_text())
        widget.pump(0.1)
        os.close(widget.master)
        widget.master = None
        self.assertEqual(widget.process.wait(timeout=3), 0)
        self.assertFalse(Path(f"/proc/{pid}").exists())
        self.assertFalse(any(command[:2] == ["nordvpn", "disconnect"] for command in widget.commands()))


if __name__ == "__main__":
    if not BINARY.exists():
        raise SystemExit("Build the binary first with cargo build")
    unittest.main(verbosity=2)
