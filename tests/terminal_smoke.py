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
        delay = root / "status-delay"
        time.sleep(float(delay.read_text() if delay.exists() else os.environ.get("NORTUI_STATUS_DELAY", "0")))
        print((root / "service").read_text())
    elif args[0] in ("start", "stop"):
        if os.environ.get("NORTUI_TRANSITIONS"):
            (root / "service").write_text("activating" if args[0] == "start" else "deactivating")
        time.sleep(float(os.environ.get("NORTUI_SERVICE_DELAY", "0")))
        (root / "service").write_text("active" if args[0] == "start" else "inactive")
        if args[0] == "stop":
            (root / "vpn").write_text("Disconnected")
    else:
        sys.exit(2)
elif name == "sudo":
    if args == ["-n", "-v"]:
        time.sleep(float(os.environ.get("NORTUI_CREDENTIAL_DELAY", "0")))
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
        telemetry = bool(os.environ.get("NORTUI_TELEMETRY"))
        sample = 0
        def status_event(phase):
            fd = os.open(root / "status-events.jsonl", os.O_CREAT | os.O_WRONLY | os.O_APPEND, 0o600)
            os.write(fd, (json.dumps({"phase": phase, "sample": sample, "pid": os.getpid(), "time": time.monotonic()}) + "\n").encode())
            os.close(fd)
        if telemetry:
            counter = root / "status-count"
            sample = int(counter.read_text()) + 1 if counter.exists() else 1
            counter.write_text(str(sample))
            status_event("start")
        time.sleep(float(os.environ.get("NORTUI_VPN_STATUS_DELAY", "0")))
        if (root / "status-failure").exists():
            print("Temporary VPN status failure", file=sys.stderr)
            sys.exit(1)
        state = (root / "vpn").read_text()
        print("Status: " + state)
        if state == "Connected":
            city = (root / "vpn-city").read_text() if (root / "vpn-city").exists() else "New York"
            print(f"Country: United States\nCity: {city}\nIP: 192.0.2.10\nCurrent technology: NORDLYNX")
            if telemetry:
                print("Server: United States #8373\nCurrent protocol: UDP")
                print(f"Uptime: 4 minutes {9 + sample} seconds")
                print(f"Transfer: {98.94 + sample * 0.01:.2f} MiB received, {15.95 + sample * 0.01:.2f} MiB sent")
        if telemetry:
            status_event("end")
    elif args[0] == "countries":
        print("United_States\nJapan\nMonaco")
        if os.environ.get("NORTUI_STOP_AFTER_COUNTRIES") and not (root / "discovery-stopped").exists():
            (root / "service").write_text("inactive")
            (root / "discovery-stopped").touch()
    elif args[0] == "cities":
        if args[1] == "Monaco":
            print("Servers by city are not available for this country.", file=sys.stderr)
            sys.exit(1)
        (root / f"city-{args[1]}.pid").write_text(str(os.getpid()))
        time.sleep(float(os.environ.get(f"NORTUI_{args[1].upper()}_CITY_DELAY", os.environ.get("NORTUI_CITY_DELAY", "0"))))
        print("New_York\nLos_Angeles" if args[1] == "United_States" else "Tokyo")
    elif args[0] == "connect":
        (root / "connect.pid").write_text(str(os.getpid()))
        if os.environ.get("NORTUI_TRANSITIONS"):
            (root / "vpn").write_text("Connecting")
        time.sleep(float(os.environ.get("NORTUI_CONNECT_DELAY", "0.1")))
        if (root / "next-city").exists():
            (root / "vpn-city").write_text((root / "next-city").read_text())
        (root / "vpn").write_text("Connected")
        print("You are connected to NordVPN.")
    elif args[0] == "disconnect":
        (root / "vpn").write_text("Disconnected")
        time.sleep(float(os.environ.get("NORTUI_DISCONNECT_DELAY", "0")))
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
        self.original_flags = self.original_attributes[3]
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

    def body(self):
        return self.text().splitlines()[:-2]

    def send(self, data):
        os.write(self.master, data)

    def commands(self):
        path = self.root / "commands.jsonl"
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def location_commands(self):
        return [command for command in self.commands() if command[0] == "nordvpn" and command[1] in ("countries", "cities")]

    def status_events(self):
        path = self.root / "status-events.jsonl"
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

    def test_connected_telemetry_updates_each_second_without_layout_or_footer_activity(self):
        widget = self.widget(service="active", vpn="Connected", cached=True, width=80, height=14, NORTUI_TELEMETRY="1")
        widget.wait_for(lambda: "New York" in widget.text())
        first_body = widget.body()
        first_raw_end = len(widget.raw)
        widget.wait_for(lambda: len([event for event in widget.status_events() if event["phase"] == "end"]) >= 2, timeout=1.75)
        widget.wait_for(lambda: "04:11" in widget.text(), timeout=0.5)
        self.assertIn("Server      United States #8373", widget.text())
        self.assertIn("Technology  NORDLYNX · UDP", widget.text())
        self.assertIn("↓ 98.96 MiB", widget.text())
        self.assertIn("↑ 15.97 MiB", widget.text())
        dynamic_rows = [index for index, row in enumerate(first_body) if "Uptime" in row or "Transfer" in row]
        self.assertEqual(len(dynamic_rows), 2)
        for index, row in enumerate(first_body):
            if index not in dynamic_rows:
                self.assertEqual(widget.body()[index], row)
        self.assertNotEqual(widget.body(), first_body)
        self.assertEqual(widget.footer().strip(), "⦁ Connected")
        self.assertFalse(any(frame.encode() in widget.raw[first_raw_end:] for frame in SPINNER_FRAMES))
        starts = [event["time"] for event in widget.status_events() if event["phase"] == "start"]
        self.assertGreaterEqual(starts[1] - starts[0], 0.85)
        self.assertLess(starts[1] - starts[0], 1.75)
        self.assertEqual(widget.location_commands(), [])
        self.assertFalse(any(command[0] == "sudo" for command in widget.commands()))

    def test_connected_slow_status_reads_never_overlap(self):
        widget = self.widget(service="active", vpn="Connected", cached=True, NORTUI_TELEMETRY="1", NORTUI_VPN_STATUS_DELAY="1.35")
        widget.wait_for(lambda: "04:10" in widget.text())
        widget.wait_for(lambda: "04:11" in widget.text())
        events = widget.status_events()
        self.assertEqual([event["phase"] for event in events[:4]], ["start", "end", "start", "end"])
        active = None
        for event in events:
            if event["phase"] == "start":
                self.assertIsNone(active, "Status requests overlapped")
                active = event["pid"]
            else:
                self.assertEqual(active, event["pid"])
                active = None
        self.assertGreaterEqual(events[1]["time"] - events[0]["time"], 1.3)
        self.assertGreaterEqual(events[2]["time"], events[1]["time"])
        self.assertEqual(widget.footer().strip(), "⦁ Connected")
        self.assertEqual(widget.location_commands(), [])

    def test_initial_status_check_uses_only_footer_progress(self):
        widget = self.widget(cached=True, NORTUI_STATUS_DELAY="0.6")
        widget.wait_for(lambda: widget.screen.alternate_active and bool(widget.commands()))
        self.assertTrue(all(not row.strip() for row in widget.body()))
        self.assertIn(widget.footer().lstrip()[0], SPINNER_FRAMES)
        self.assertTrue(widget.footer().endswith("⦁ Checking… "))
        widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
        self.assertIn("VPN is off", widget.text())
        self.assertNotIn(b"Checking connection", widget.raw)
        self.assertFalse(any(frame in widget.footer() for frame in SPINNER_FRAMES))

    def test_authentication_preparation_does_not_change_panel_or_footer(self):
        widget = self.widget(cached=True, NORTUI_CREDENTIAL_DELAY="0.5", NORTUI_CONNECT_DELAY="0.5")
        widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
        body, footer = widget.body(), widget.footer()
        widget.send(b"c")
        widget.wait_for(lambda: ["sudo", "-n", "-v"] in widget.commands())
        widget.pump(0.15)
        self.assertEqual(widget.body(), body)
        self.assertEqual(widget.footer(), footer)
        widget.wait_for(lambda: b"AUTH_COOKED" in widget.raw)
        before_prompt = widget.raw.split(b"AUTH_COOKED")[0]
        self.assertNotIn(b"Connecting to", before_prompt)
        self.assertNotIn(b"Starting service", before_prompt)
        self.assertNotIn(b"38;2;229;200;144", before_prompt)
        widget.send(b"password\n")
        widget.wait_for(lambda: (widget.root / "connect.pid").exists() and "Connecting to" in widget.footer())
        self.assertEqual(widget.body(), body)
        self.assertIn(widget.footer().lstrip()[0], SPINNER_FRAMES)
        widget.wait_for(lambda: widget.footer().endswith("⦁ Connected "))
        self.assertIn("New York", widget.text())
        self.assertNotIn("VPN is off", widget.text())

    def test_service_actions_keep_body_until_final_status_is_known(self):
        widget = self.widget(cached=True, NORTUI_SERVICE_DELAY="0.35", NORTUI_TRANSITIONS="1")
        (widget.root / "credentials").touch()
        widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
        (widget.root / "status-delay").write_text("0.4")
        for final_state, activity, completion, headline in [
            ("Disconnected", "Starting service…", "Service started", "Ready to connect"),
            ("Service stopped", "Stopping service…", "Service stopped", "VPN is off"),
        ]:
            body = widget.body()
            widget.send(b"s")
            widget.wait_for(lambda: activity in widget.footer())
            self.assertEqual(widget.body(), body)
            self.assertIn(widget.footer().lstrip()[0], SPINNER_FRAMES)
            widget.wait_for(lambda: completion in widget.footer() and activity not in widget.footer())
            self.assertEqual(widget.body(), body, "Command completion must wait for the resulting status reading")
            widget.wait_for(lambda: widget.footer().endswith(f"⦁ {final_state} "))
            self.assertIn(headline, widget.text())
            self.assertNotEqual(widget.body(), body)
        self.assertEqual(widget.location_commands(), [])

    def test_connection_actions_keep_confirmed_details_through_completion(self):
        widget = self.widget(service="active", vpn="Connected", cached=True,
                             NORTUI_CONNECT_DELAY="0.35", NORTUI_DISCONNECT_DELAY="0.35", NORTUI_TRANSITIONS="1")
        widget.wait_for(lambda: "New York" in widget.text())
        (widget.root / "status-delay").write_text("0.4")
        for key, activity, completion, city, status, headline in [
            (b"c", "Connecting to", "You are connected", "Los Angeles", "Connected", "Los Angeles"),
            (b"d", "Disconnecting…", "You are disconnected", "", "Disconnected", "Ready to connect"),
            (b"c", "Connecting to", "You are connected", "Chicago", "Connected", "Chicago"),
        ]:
            if city:
                (widget.root / "next-city").write_text(city)
            body = widget.body()
            widget.send(key)
            widget.wait_for(lambda: activity in widget.footer())
            self.assertEqual(widget.body(), body)
            widget.wait_for(lambda: completion in widget.footer() and activity not in widget.footer())
            self.assertEqual(widget.body(), body, "Completed command must not restore or replace the confirmed body early")
            widget.wait_for(lambda: widget.footer().endswith(f"⦁ {status} ") and headline in widget.text())
            self.assertNotEqual(widget.body(), body)
            if key == b"d":
                self.assertNotIn("192.0.2.10", widget.text())
                self.assertNotIn("Technology", widget.text())
        self.assertEqual(widget.location_commands(), [])

    def test_external_transitions_and_read_failures_retain_confirmed_panel(self):
        widget = self.widget(service="active", vpn="Connected", cached=True, width=80, height=14)
        widget.wait_for(lambda: "New York" in widget.text())
        body = widget.body()
        for service, vpn, status in [
            ("activating", "Connected", "Service starting…"),
            ("deactivating", "Connected", "Service stopping…"),
            ("active", "Connecting", "Connecting…"),
        ]:
            (widget.root / "service").write_text(service)
            (widget.root / "vpn").write_text(vpn)
            widget.send(b"r")
            widget.wait_for(lambda: widget.footer().endswith(f"⦁ {status} "))
            self.assertEqual(widget.body(), body)
            self.assertIn(widget.footer().lstrip()[0], SPINNER_FRAMES)
        (widget.root / "status-failure").touch()
        widget.send(b"r")
        widget.wait_for(lambda: "Temporary VPN status failure" in widget.text())
        self.assertTrue(widget.footer().endswith("⦁ VPN unavailable "))
        self.assertEqual(widget.body()[:-1], body[:-1], "The error row must not move the confirmed panel")
        (widget.root / "status-failure").unlink()
        (widget.root / "service").write_text("inactive")
        widget.send(b"r")
        widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
        self.assertIn("VPN is off", widget.text())
        self.assertNotIn("New York", widget.text())
        self.assertNotIn("192.0.2.10", widget.text())
        self.assertNotIn("Technology", widget.text())

    def test_shortcut_help_is_dismissible_and_does_not_execute_actions_or_load_locations(self):
        widget = self.widget(service="active", cached=True)
        widget.wait_for(lambda: widget.footer().endswith("⦁ Disconnected "))
        for close in (b"\x1b", b"?"):
            widget.send(b"?")
            widget.wait_for(lambda: "Shortcuts" in widget.text())
            self.assertFalse(widget.screen.cursor_visible)
            self.assertTrue(widget.footer().endswith("⦁ Disconnected "))
            widget.send(b"cdsri/")
            widget.pump(0.15)
            self.assertIn("Shortcuts", widget.text())
            self.assertEqual(widget.location_commands(), [])
            self.assertEqual((widget.root / "service").read_text(), "active")
            self.assertFalse(any(command[0] == "sudo" or command[:2] in (["nordvpn", "connect"], ["nordvpn", "disconnect"]) for command in widget.commands()))
            widget.send(close)
            widget.wait_for(lambda: "Ready to connect" in widget.text())
            self.assertIsNone(widget.process.poll())
            self.assertNotIn("Shortcuts", widget.text())

        widget.send(b"/?q")
        widget.wait_for(lambda: widget.query() == "?q")
        self.assertNotIn("Shortcuts", widget.text())
        self.assertIsNone(widget.process.poll())
        widget.send(b"\x1b")
        widget.wait_for(lambda: "Ready to connect" in widget.text())
        widget.send(b"?")
        widget.wait_for(lambda: "Shortcuts" in widget.text())
        widget.send(b"q")
        self.assertEqual(widget.process.wait(timeout=3), 0)
        self.assertEqual(termios.tcgetattr(widget.slave)[3], widget.original_flags)

    def test_shortcut_help_scrolls_in_small_widget_and_returns_to_panel(self):
        widget = self.widget(width=40, height=9)
        widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
        widget.send(b"?")
        widget.wait_for(lambda: "Shortcuts" in widget.text())
        initial = widget.text().splitlines()[:-2]
        widget.send(b"\x1b[F")
        widget.wait_for(lambda: widget.text().splitlines()[:-2] != initial)
        self.assertFalse(widget.screen.cursor_visible)
        self.assertTrue(widget.footer().endswith("⦁ Service stopped "))
        end = widget.text().splitlines()[:-2]
        widget.send(b"\x0b")
        widget.wait_for(lambda: widget.text().splitlines()[:-2] != end)
        widget.send(b"\x1b[H")
        widget.wait_for(lambda: "Shortcuts" in widget.text())
        widget.send(b"\x1b")
        widget.wait_for(lambda: "VPN is off" in widget.text())
        self.assertEqual(widget.location_commands(), [])

    def test_query_start_end_keys_move_actual_cursor_and_insert_or_paste_there(self):
        widget = self.widget(cached=True)
        widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
        widget.send(b"/york")
        widget.wait_for(lambda: widget.query() == "york" and widget.screen.x == 8)
        prompt_row = widget.screen.y
        widget.send(b"\x01")
        widget.wait_for(lambda: (widget.screen.x, widget.screen.y) == (4, prompt_row))
        self.assertEqual(widget.query(), "york")
        widget.send(b"new ")
        widget.wait_for(lambda: widget.query() == "new york" and widget.screen.x == 8)
        self.assertIn("New York / United States", widget.text())
        widget.send(b"\x05")
        widget.wait_for(lambda: (widget.screen.x, widget.screen.y) == (12, prompt_row))
        self.assertEqual(widget.query(), "new york")
        widget.send(b"!\x7f\x1b[D\x7f")
        widget.wait_for(lambda: widget.query() == "new yok" and widget.screen.x == 10)
        widget.send(b"\x1b[200~r\x1b[201~")
        widget.wait_for(lambda: widget.query() == "new york" and widget.screen.x == 11)
        widget.send(b"\x05")
        widget.wait_for(lambda: widget.screen.x == 12)
        widget.send(b"\x15")
        widget.wait_for(lambda: widget.query() == "Search countries and cities…" and widget.screen.x == 4)
        self.assertEqual(widget.location_commands(), [])

    def test_long_query_viewport_follows_start_end_and_wide_character_insertions(self):
        widget = self.widget(cached=True, width=24, height=8)
        widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
        widget.send(b"i\x1b[200~start" + b"x" * 60 + b"end\x1b[201~")
        widget.wait_for(lambda: widget.query() is not None and widget.query().endswith("end"))
        self.assertLess(widget.screen.x, 22)
        self.assertGreater(widget.screen.x, 4)
        widget.send(b"\x01")
        widget.wait_for(lambda: widget.query().startswith("start") and widget.screen.x == 4)
        widget.send("東京".encode())
        widget.wait_for(lambda: widget.query().startswith("東") and widget.screen.x == 8)
        self.assertNotIn("�", widget.text())
        widget.send(b"\x05")
        widget.wait_for(lambda: widget.query().endswith("end") and widget.screen.x > 4)
        self.assertLess(widget.screen.x, 22)
        widget.send(b"\x7f")
        widget.wait_for(lambda: widget.query().endswith("en"))
        widget.send(b"\x01")
        widget.wait_for(lambda: widget.query().startswith("東") and widget.screen.x == 4)
        self.assertTrue(widget.footer().endswith("⦁ Service stopped "))
        self.assertEqual(widget.location_commands(), [])

    def test_stopped_launch_search_and_escape_restore_terminal(self):
        widget = self.widget()
        widget.wait_for(lambda: "Service stopped" in widget.text())
        self.assertIn("Start service, then open search to load locations.", widget.text())
        self.assertIn("VPN is off", widget.text())
        self.assertNotIn("Technology", widget.text())
        self.assertNotIn("—", widget.text())
        self.assertNotIn("Search countries", widget.text())
        self.assertFalse(widget.screen.cursor_visible)
        self.assertFalse(any(command[0] == "nordvpn" for command in widget.commands()))
        widget.send(b"iq")
        widget.wait_for(lambda: "No matches" in widget.text())
        self.assertTrue(widget.screen.cursor_visible)
        self.assertFalse(any(command[0] == "nordvpn" for command in widget.commands()))
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
        widget.send(b"i")
        cache = widget.root / "cache/nortui/locations.json"
        widget.wait_for(lambda: cache.exists() and "Tokyo" in cache.read_text() and "Loading" not in widget.footer())
        widget.send(b"\x1b")
        widget.wait_for(lambda: "Ready to connect" in widget.text())
        (widget.root / "service").write_text("inactive")
        widget.wait_for(lambda: "Service stopped" in widget.text())
        before = sum(command[0] == "nordvpn" for command in widget.commands())
        widget.send(b"r")
        widget.pump(0.5)
        self.assertEqual(sum(command[0] == "nordvpn" for command in widget.commands()), before)
        self.assertIn("Status refreshed", widget.footer())
        self.assertNotIn("Refreshing", widget.footer())
        self.assertFalse(any(frame in widget.footer() for frame in SPINNER_FRAMES))

    def test_delayed_refresh_keeps_progress_until_status_finishes_and_clears_on_failure(self):
        widget = self.widget(service="active", vpn="Connected", cached=True)
        widget.wait_for(lambda: "New York" in widget.text())
        original_body = widget.body()
        delay = widget.root / "status-delay"
        delay.write_text("3.5")
        widget.send(b"r")
        widget.wait_for(lambda: "Refreshing status…" in widget.footer())
        raw_start = len(widget.raw)
        widget.pump(3.1)
        self.assertIn("Refreshing status…", widget.footer())
        self.assertIn(widget.footer().lstrip()[0], SPINNER_FRAMES)
        self.assertEqual(widget.body(), original_body)
        frames = {frame for frame in SPINNER_FRAMES if frame.encode() in widget.raw[raw_start:]}
        self.assertGreaterEqual(len(frames), 2)
        widget.wait_for(lambda: "Status refreshed" in widget.footer())
        self.assertFalse(any(frame in widget.footer() for frame in SPINNER_FRAMES))
        delay.write_text("0.2")
        failure = widget.root / "status-failure"
        failure.touch()
        widget.send(b"r")
        widget.wait_for(lambda: "Refreshing status…" in widget.footer())
        widget.wait_for(lambda: "VPN unavailable" in widget.footer())
        self.assertIn("Temporary VPN status failure", widget.text())
        self.assertNotIn("Refreshing", widget.footer())
        self.assertFalse(any(frame in widget.footer() for frame in SPINNER_FRAMES))
        self.assertEqual(widget.body()[:-1], original_body[:-1], "Only the error row should change")
        failure.unlink()
        widget.send(b"r")
        widget.wait_for(lambda: "Status refreshed" in widget.footer() and "Connected" in widget.footer())
        self.assertNotIn("Temporary VPN status failure", widget.text())
        self.assertEqual(widget.location_commands(), [])

    def test_city_loading_spinner_animates_while_searching_and_disappears_when_done(self):
        widget = self.widget(service="active", cached=True, NORTUI_CITY_DELAY="1")
        widget.wait_for(lambda: "Disconnected" in widget.footer())
        self.assertEqual(widget.location_commands(), [])
        widget.send(b"i")
        widget.wait_for(lambda: "Loading cities" in widget.footer())
        first = widget.footer().lstrip()[0]
        self.assertIn(first, SPINNER_FRAMES)
        self.assertIn("/3 countries", widget.footer())
        self.assertTrue(widget.footer().endswith("⦁ Disconnected "))

        widget.send(b"new")
        widget.wait_for(lambda: widget.query() == "new")
        widget.wait_for(lambda: widget.footer().lstrip()[0] != first, timeout=0.5)
        self.assertIn(widget.footer().lstrip()[0], SPINNER_FRAMES)
        self.assertIn("Loading cities", widget.footer())

        widget.wait_for(lambda: widget.footer().strip() == "⦁ Disconnected")
        self.assertFalse(any(frame in widget.footer() for frame in SPINNER_FRAMES))
        self.assertEqual(widget.query(), "new")
        self.assertTrue(widget.footer().endswith("⦁ Disconnected "))

    def test_panel_launch_service_start_and_refresh_defer_locations_until_search(self):
        for service, trigger in (("active", b"i"), ("inactive", b"/")):
            with self.subTest(initial_service=service, search_trigger=trigger):
                widget = self.widget(service=service, cached=True, NORTUI_CITY_DELAY="1")
                (widget.root / "credentials").touch()
                if service == "inactive":
                    widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
                    widget.send(b"s")
                widget.wait_for(lambda: widget.footer().endswith("⦁ Disconnected "))
                widget.pump(0.15)
                self.assertEqual(widget.location_commands(), [])
                widget.send(b"r")
                widget.wait_for(lambda: "Status refreshed" in widget.footer())
                self.assertEqual(widget.location_commands(), [])
                self.assertNotIn("Loading", widget.footer())

                widget.send(trigger + b"new")
                widget.wait_for(lambda: widget.query() == "new" and "Loading cities" in widget.footer())
                self.assertIn("New York / United States", widget.text())
                cache = widget.root / "cache/nortui/locations.json"
                self.assertNotIn("Tokyo", cache.read_text())
                widget.wait_for(lambda: "Tokyo" in cache.read_text() and "Loading" not in widget.footer())
                self.assertEqual(widget.query(), "new")
                self.assertCountEqual(widget.location_commands(), [
                    ["nordvpn", "countries"],
                    ["nordvpn", "cities", "United_States"],
                    ["nordvpn", "cities", "Japan"],
                    ["nordvpn", "cities", "Monaco"],
                ])

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

    def test_completed_search_catalog_survives_reopening_restart_and_refresh(self):
        widget = self.widget(service="active")
        (widget.root / "credentials").touch()
        widget.wait_for(lambda: widget.footer().endswith("⦁ Disconnected "))
        widget.send(b"i")
        cache = widget.root / "cache/nortui/locations.json"
        widget.wait_for(lambda: cache.exists() and "Tokyo" in cache.read_text() and "Loading" not in widget.footer())
        first = widget.location_commands()
        self.assertEqual(len(first), 4)

        widget.send(b"\x1b")
        widget.wait_for(lambda: "Ready to connect" in widget.text())
        widget.send(b"/tokyo")
        widget.wait_for(lambda: widget.query() == "tokyo" and "Tokyo / Japan" in widget.text())
        widget.pump(0.15)
        self.assertEqual(widget.location_commands(), first)
        widget.send(b"\x1b")
        widget.wait_for(lambda: "Ready to connect" in widget.text())
        widget.send(b"s")
        widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
        widget.send(b"s")
        widget.wait_for(lambda: widget.footer().endswith("⦁ Disconnected "))
        widget.pump(0.15)
        self.assertEqual(widget.location_commands(), first)
        widget.send(b"i")
        widget.wait_for(lambda: widget.query() == "Search countries and cities…")
        widget.pump(0.15)
        self.assertEqual(widget.location_commands(), first)

        widget.send(b"\x12")
        widget.wait_for(lambda: len(widget.location_commands()) == 8 and "Loading" not in widget.footer())
        self.assertCountEqual(widget.location_commands(), first * 2)
        widget.send(b"\x1b")
        widget.wait_for(lambda: "Ready to connect" in widget.text())
        widget.send(b"r")
        widget.wait_for(lambda: "Status refreshed" in widget.footer())
        widget.pump(0.15)
        self.assertCountEqual(widget.location_commands(), first * 2)
        widget.send(b"/")
        widget.wait_for(lambda: len(widget.location_commands()) == 12 and "Loading" not in widget.footer())
        self.assertCountEqual(widget.location_commands(), first * 3)

    def test_service_stop_before_discovery_or_between_queries_retries_without_refresh(self):
        for when in ("before", "between"):
            with self.subTest(when=when):
                environment = {"NORTUI_STOP_AFTER_COUNTRIES": "1"} if when == "between" else {}
                widget = self.widget(service="active", **environment)
                widget.wait_for(lambda: "Ready to connect" in widget.text())
                if when == "before":
                    (widget.root / "service").write_text("inactive")
                widget.send(b"i")
                widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
                self.assertNotIn("Loading", widget.footer())
                self.assertFalse(any(command[1] == "cities" for command in widget.location_commands()))
                widget.send(b"\x1b")
                widget.wait_for(lambda: "VPN is off" in widget.text())
                (widget.root / "service").write_text("active")
                widget.wait_for(lambda: widget.footer().endswith("⦁ Disconnected "))
                widget.send(b"i")
                cache = widget.root / "cache/nortui/locations.json"
                widget.wait_for(lambda: cache.exists() and "Tokyo" in cache.read_text() and "Loading" not in widget.footer())
                self.assertIn("New York / United States", widget.text())
                self.assertEqual(sum(command[1] == "countries" for command in widget.location_commands()), 1 if when == "before" else 2)

    def test_interrupted_city_loading_retries_only_when_search_opens_again(self):
        for stop_source, delay in (("shortcut", "1.5"), ("external", "3")):
            with self.subTest(stop_source=stop_source):
                widget = self.widget(service="active", cached=True, NORTUI_CITY_DELAY=delay)
                (widget.root / "credentials").touch()
                widget.wait_for(lambda: widget.footer().endswith("⦁ Disconnected "))
                widget.send(b"/new")
                workers = [widget.root / f"city-{country}.pid" for country in ("United_States", "Japan")]
                widget.wait_for(lambda: all(path.exists() for path in workers) and len(widget.location_commands()) == 4 and "Loading cities" in widget.footer())
                pids = [int(path.read_text()) for path in workers]
                widget.send(b"\x1b")
                widget.wait_for(lambda: "Ready to connect" in widget.text())
                self.assertIn("Loading cities", widget.footer())
                if stop_source == "shortcut":
                    widget.send(b"s")
                else:
                    (widget.root / "service").write_text("inactive")
                widget.wait_for(lambda: widget.footer().endswith("⦁ Service stopped "))
                widget.wait_for(lambda: all(not Path(f"/proc/{pid}").exists() for pid in pids))
                before = widget.location_commands()
                widget.send(b"s")
                widget.wait_for(lambda: widget.footer().endswith("⦁ Disconnected "))
                widget.pump(0.15)
                self.assertEqual(widget.location_commands(), before)
                self.assertNotIn("Loading", widget.footer())

                widget.send(b"/new")
                widget.wait_for(lambda: widget.query() == "new" and "Loading cities" in widget.footer())
                self.assertIn("New York / United States", widget.text())
                cache = widget.root / "cache/nortui/locations.json"
                widget.wait_for(lambda: "Tokyo" in cache.read_text() and "Loading" not in widget.footer())
                self.assertCountEqual(widget.location_commands(), before * 2)

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
        self.assertEqual(widget.location_commands(), [])
        widget.send(b"/")
        cache = widget.root / "cache/nortui/locations.json"
        widget.wait_for(lambda: cache.exists() and "Tokyo" in cache.read_text() and "Loading" not in widget.footer())
        widget.send(b"united states\r")
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
        self.assertEqual(widget.location_commands(), [])

    def test_connected_hierarchy_and_footer_stay_cohesive_when_searching(self):
        widget = self.widget(service="active", cached=True, vpn="Connected")
        widget.wait_for(lambda: "Connected" in widget.footer())
        lines = widget.text().splitlines()
        city_row = next(index for index, line in enumerate(lines) if line.strip() == "New York")
        self.assertEqual(lines[city_row + 1].strip(), "United States")
        self.assertIn("192.0.2.10", widget.text())
        self.assertIn("NORDLYNX", widget.text())
        self.assertNotIn("Location", widget.text())
        self.assertTrue(widget.footer().endswith("⦁ Connected "))
        self.assertNotIn("United States", widget.footer())
        widget.send(b"/")
        widget.wait_for(lambda: widget.query() == "Search countries and cities…")
        self.assertTrue(widget.footer().endswith("⦁ Connected "))
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
        self.assertTrue(widget.footer().endswith("⦁ Disconnected "))
        self.assertEqual(widget.footer().count("⦁"), 1)
        widget.send(b"\x03")
        self.assertEqual(widget.process.wait(timeout=3), 0)
        self.assertFalse(Path(f"/proc/{pid}").exists())


if __name__ == "__main__":
    if not BINARY.exists():
        raise SystemExit("Build the binary first with cargo build")
    unittest.main(verbosity=2)
