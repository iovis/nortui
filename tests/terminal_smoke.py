"""Full-binary tests using only Python's standard library and isolated fake CLIs."""

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
SPINNER_FRAMES = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
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
        flags = termios.tcgetattr(0)[3]
        assert flags & termios.ICANON and flags & termios.ECHO, "sudo received raw terminal"
        print("AUTH_COOKED password: ", end="", flush=True)
        sys.stdin.readline()
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
        time.sleep(float(os.environ.get("NORTUI_CITY_DELAY", "0")))
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
                    if parameters == "?1049" and command == "h":
                        self.rows = [[" "] * self.width for _ in range(self.height)]
                        self.x = self.y = 0
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
        self.original_flags = termios.tcgetattr(self.slave)[3]
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
        try:
            flags = termios.tcgetattr(self.slave)[3]
        except termios.error:
            flags = None  # A closed terminal no longer accepts terminal ioctls.
        if self.master is not None:
            os.close(self.master)
        os.close(self.slave)
        self.directory.cleanup()
        return flags


class TerminalSmoke(unittest.TestCase):
    def widget(self, **kwargs):
        widget = Widget(**kwargs)
        self.addCleanup(widget.close)
        return widget

    def test_stopped_launch_search_and_escape_restore_terminal(self):
        widget = self.widget()
        widget.wait_for(lambda: "Service stopped" in widget.text())
        self.assertIn("Start service to load locations", widget.text())
        self.assertIn("VPN is off", widget.text())
        self.assertNotIn("Technology", widget.text())
        self.assertNotIn("—", widget.text())
        self.assertNotIn("Search countries", widget.text())
        self.assertFalse(widget.screen.cursor_visible)
        self.assertFalse(any(command[0] == "nordvpn" for command in widget.commands()))
        widget.send(b"iq")
        widget.wait_for(lambda: "No matches" in widget.text())
        self.assertTrue(widget.screen.cursor_visible)
        widget.send(b"\x1b")
        widget.wait_for(lambda: "VPN is off" in widget.text())
        self.assertFalse(widget.screen.cursor_visible)
        widget.send(b"q")
        self.assertEqual(widget.process.wait(timeout=3), 0)
        self.assertEqual(termios.tcgetattr(widget.slave)[3], widget.original_flags)

    def test_cached_city_authenticates_in_cooked_mode_then_connects(self):
        widget = self.widget(cached=True)
        widget.wait_for(lambda: "Service stopped" in widget.text())
        widget.send(b"/new york\r")
        widget.wait_for(lambda: b"AUTH_COOKED" in widget.raw)
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

    def test_slow_connection_keeps_search_responsive_and_exit_reaps_child(self):
        widget = self.widget(service="active", cached=True, NORTUI_CONNECT_DELAY="10")
        widget.wait_for(lambda: "Disconnected" in widget.text())
        widget.send(b"/new york\r")
        widget.wait_for(lambda: (widget.root / "connect.pid").exists())
        pid = int((widget.root / "connect.pid").read_text())
        widget.wait_for(lambda: "Connecting" in widget.footer())
        self.assertIn(widget.footer().lstrip()[0], SPINNER_FRAMES)
        widget.send(b"cc/Tokyo\r\r")
        widget.wait_for(lambda: widget.query() == "Tokyo")
        self.assertEqual(sum(command[:2] == ["nordvpn", "connect"] for command in widget.commands()), 1)
        widget.send(b"\x03")
        self.assertEqual(widget.process.wait(timeout=3), 0)
        self.assertFalse(Path(f"/proc/{pid}").exists())
        self.assertFalse(any(command[:2] == ["nordvpn", "disconnect"] for command in widget.commands()))
        self.assertEqual(termios.tcgetattr(widget.slave)[3], widget.original_flags)

    def test_external_service_stop_and_refresh_do_not_query_vpn(self):
        widget = self.widget(service="active")
        widget.wait_for(lambda: "Disconnected" in widget.text())
        widget.wait_for(lambda: (widget.root / "cache/nortui/locations.json").exists())
        (widget.root / "service").write_text("inactive")
        widget.wait_for(lambda: "Service stopped" in widget.text())
        before = sum(command[0] == "nordvpn" for command in widget.commands())
        widget.send(b"r")
        widget.pump(0.5)
        self.assertEqual(sum(command[0] == "nordvpn" for command in widget.commands()), before)
        self.assertIn("Status refreshed", widget.footer())
        self.assertNotIn("Refreshing", widget.footer())
        self.assertFalse(any(frame in widget.footer() for frame in SPINNER_FRAMES))

    def test_city_loading_spinner_animates_while_searching_and_disappears_when_done(self):
        widget = self.widget(service="active", cached=True, NORTUI_CITY_DELAY="1")
        widget.wait_for(lambda: "Loading cities" in widget.footer())
        first = widget.footer().lstrip()[0]
        self.assertIn(first, SPINNER_FRAMES)
        self.assertIn("/3 countries", widget.footer())
        self.assertTrue(widget.footer().endswith("● Disconnected "))

        widget.send(b"/new")
        widget.wait_for(lambda: widget.query() == "new")
        widget.wait_for(lambda: widget.footer().lstrip()[0] != first, timeout=0.5)
        self.assertIn(widget.footer().lstrip()[0], SPINNER_FRAMES)
        self.assertIn("Loading cities", widget.footer())

        widget.wait_for(lambda: "Service running" in widget.footer())
        self.assertFalse(any(frame in widget.footer() for frame in SPINNER_FRAMES))
        self.assertEqual(widget.query(), "new")
        self.assertTrue(widget.footer().endswith("● Disconnected "))

    def test_sigterm_restores_terminal(self):
        widget = self.widget()
        widget.wait_for(lambda: "Service stopped" in widget.text())
        widget.process.send_signal(signal.SIGTERM)
        self.assertEqual(widget.process.wait(timeout=3), 0)
        self.assertEqual(termios.tcgetattr(widget.slave)[3], widget.original_flags)

    def test_closing_terminal_exits_without_hangup_signal(self):
        widget = self.widget()
        widget.wait_for(lambda: "Service stopped" in widget.text())
        # There is no controlling terminal in this fixture, so closing the
        # master supplies EOF without SIGHUP. The input reader must handle it.
        widget.pump(0.1)
        os.close(widget.master)
        widget.master = None
        self.assertEqual(widget.process.wait(timeout=3), 0)

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

    def test_service_and_connection_commands_complete_a_full_lifecycle(self):
        widget = self.widget()
        (widget.root / "credentials").touch()
        widget.wait_for(lambda: "Service stopped" in widget.text())
        widget.send(b"s")
        widget.wait_for(lambda: "Disconnected" in widget.text())
        cache = widget.root / "cache/nortui/locations.json"
        widget.wait_for(lambda: cache.exists() and "Tokyo" in cache.read_text())
        widget.send(b"/united states\r")
        widget.wait_for(lambda: "Connected" in widget.text() and "You are connected" in widget.text())
        self.assertIn(["nordvpn", "connect", "United_States"], widget.commands())
        widget.send(b"c")
        widget.wait_for(lambda: ["nordvpn", "connect"] in widget.commands())
        widget.wait_for(lambda: "You are connected" in widget.text() and "Connecting to recommended" not in widget.text())
        widget.send(b"d")
        widget.wait_for(lambda: "Disconnected" in widget.text())
        self.assertEqual((widget.root / "service").read_text(), "active")
        self.assertNotIn("192.0.2.10", widget.text())
        self.assertIn("Ready to connect", widget.text())
        self.assertNotIn("Technology", widget.text())
        widget.send(b"s")
        widget.wait_for(lambda: "Service stopped" in widget.text())
        self.assertEqual((widget.root / "service").read_text(), "inactive")
        self.assertFalse(any("enable" in command or "disable" in command for command in widget.commands()))

    def test_search_letters_never_execute_panel_actions_or_exit(self):
        widget = self.widget(cached=True)
        widget.wait_for(lambda: "Service stopped" in widget.text())
        for trigger in (b"i", b"/"):
            widget.send(trigger)
            widget.wait_for(lambda: "Search countries and cities" in widget.text())
            self.assertIn("Country", widget.text())
            self.assertIn("City", widget.text())
            widget.send(b"cdsrqi/")
            widget.wait_for(lambda: widget.query() == "cdsrqi/")
            self.assertIsNone(widget.process.poll())
            self.assertEqual((widget.root / "service").read_text(), "inactive")
            self.assertFalse(any(command[0] in ("nordvpn", "sudo") for command in widget.commands()))
            widget.send(b"\x1b")
            widget.wait_for(lambda: "VPN is off" in widget.text())
            self.assertFalse(widget.screen.cursor_visible)
        widget.send(b"q")
        self.assertEqual(widget.process.wait(timeout=3), 0)

    def test_picker_navigation_connects_a_city_and_returns_to_panel(self):
        widget = self.widget(cached=True)
        (widget.root / "credentials").touch()
        widget.wait_for(lambda: "Service stopped" in widget.text())
        widget.send(b"i")
        widget.wait_for(lambda: "Search countries and cities" in widget.text())
        # Cached results are country then city; Ctrl-J/K move without typing.
        widget.send(b"\x0a\x0b\x0a\r")
        widget.wait_for(lambda: ["nordvpn", "connect", "United_States", "New_York"] in widget.commands())
        widget.wait_for(lambda: "Connected" in widget.text())
        self.assertIn("Technology", widget.text())
        self.assertFalse(widget.screen.cursor_visible)

    def test_connect_shortcut_starts_stopped_service_and_connects_without_location(self):
        widget = self.widget()
        (widget.root / "credentials").touch()
        widget.wait_for(lambda: "Service stopped" in widget.text())
        widget.send(b"c")
        widget.wait_for(lambda: "Connected" in widget.text())
        commands = widget.commands()
        self.assertIn(["systemctl", "start", "nordvpnd.service"], commands)
        self.assertIn(["nordvpn", "connect"], commands)
        self.assertLess(commands.index(["systemctl", "start", "nordvpnd.service"]), commands.index(["nordvpn", "connect"]))
        self.assertNotIn("Search countries", widget.text())

    def test_connected_hierarchy_and_footer_stay_cohesive_when_searching(self):
        widget = self.widget(service="active", cached=True, vpn="Connected")
        widget.wait_for(lambda: "Connected" in widget.footer())
        lines = widget.text().splitlines()
        city_row = next(index for index, line in enumerate(lines) if line.strip() == "New York")
        self.assertEqual(lines[city_row + 1].strip(), "United States")
        self.assertIn("192.0.2.10", widget.text())
        self.assertIn("NORDLYNX", widget.text())
        self.assertNotIn("Location", widget.text())
        self.assertTrue(widget.footer().endswith("● Connected "))
        self.assertNotIn("United States", widget.footer())
        widget.send(b"/")
        widget.wait_for(lambda: widget.query() == "Search countries and cities…")
        self.assertTrue(widget.footer().endswith("● Connected "))
        self.assertNotIn("192.0.2.10", widget.text())
        widget.send(b"\x1b")
        widget.wait_for(lambda: "Technology" in widget.text())
        self.assertFalse(widget.screen.cursor_visible)

    def test_narrow_footer_activity_preserves_status_and_child_cleanup(self):
        widget = self.widget(service="active", cached=True, width=24, height=8, NORTUI_CONNECT_DELAY="10")
        widget.wait_for(lambda: "Disconnected" in widget.footer())
        widget.send(b"c")
        widget.wait_for(lambda: (widget.root / "connect.pid").exists())
        pid = int((widget.root / "connect.pid").read_text())
        widget.wait_for(lambda: "…" in widget.footer())
        self.assertTrue(widget.footer().endswith("● Disconnected "))
        self.assertEqual(widget.footer().count("●"), 1)
        widget.send(b"\x03")
        self.assertEqual(widget.process.wait(timeout=3), 0)
        self.assertFalse(Path(f"/proc/{pid}").exists())


if __name__ == "__main__":
    if not BINARY.exists():
        raise SystemExit("Build the binary first with cargo build")
    unittest.main(verbosity=2)
