"""Regression tests for the development process supervisor (no Cargo or ngrok needed)."""
import os
from pathlib import Path
import shutil
import socket
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from dev import Runner, sources, stop_owned


FAKE_SERVER = """#!/usr/bin/env python3
import signal
import socket
import sys
import time

port = int(sys.argv[sys.argv.index('--port') + 1])
if 'connection-test' in sys.argv:
    try:
        with socket.create_connection(('127.0.0.1', port), timeout=0.3):
            pass
        sys.exit(0)
    except OSError:
        sys.exit(1)
if 'http' not in sys.argv:
    sys.exit(2)
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
listener = socket.socket()
listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
listener.bind(('127.0.0.1', port))
listener.listen()
while True:
    time.sleep(0.1)
"""


@unittest.skipUnless(os.name == "posix", "POSIX process groups required")
class DevRunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "crates").mkdir()
        (self.root / "Cargo.toml").write_text("[workspace]\n", encoding="utf-8")
        (self.root / "crates" / "a.rs").write_text("fn main() {}\n", encoding="utf-8")
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.runner = Runner(self.root, port, False, None)
        self.addCleanup(self.cleanup_server)

    def cleanup_server(self):
        if self.runner.process:
            stop_owned(self.runner.process, 2)

    def binary(self, name):
        path = self.root / name
        path.write_text(FAKE_SERVER, encoding="utf-8")
        path.chmod(0o700)
        return path

    def test_build_snapshot_tracks_sources_not_generated_files(self):
        initial = sources(self.root)
        (self.root / "target").mkdir()
        (self.root / "target" / "generated.rs").write_text("changed", encoding="utf-8")
        self.assertEqual(initial, sources(self.root))
        (self.root / "crates" / "new.rs").write_text("different", encoding="utf-8")
        self.assertNotEqual(initial, sources(self.root))

    def test_manual_restart_requires_an_explicit_socket_request(self):
        self.runner.open_manual_control()
        self.addCleanup(self.runner.close_manual_control)
        path = self.runner.control_path
        self.assertIsNotNone(path)
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)
        self.assertIsNone(self.runner.restart_due_at)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(1)
            connection.connect(str(path))
            connection.sendall(b"restart\n")
            self.runner.poll_manual_control()
            self.assertEqual(connection.recv(32), b"accepted\n")
        self.assertIsNotNone(self.runner.restart_due_at)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as duplicate:
            duplicate.settimeout(1)
            duplicate.connect(str(path))
            duplicate.sendall(b"restart\n")
            self.runner.poll_manual_control()
            self.assertEqual(duplicate.recv(32), b"pending\n")

    def test_manual_restart_rejects_unknown_commands(self):
        self.runner.open_manual_control()
        self.addCleanup(self.runner.close_manual_control)
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(1)
            connection.connect(str(self.runner.control_path))
            connection.sendall(b"not-restart\n")
            self.runner.poll_manual_control()
            self.assertEqual(connection.recv(32), b"invalid\n")
        self.assertIsNone(self.runner.restart_due_at)

    def test_manual_restart_is_available_by_default(self):
        self.runner.open_manual_control()
        self.addCleanup(self.runner.close_manual_control)
        self.assertIsNotNone(self.runner.control_listener)
        self.assertIsNotNone(self.runner.control_path)
        self.assertEqual(self.runner.control_path.stat().st_mode & 0o777, 0o600)
        self.assertIsNone(self.runner.restart_due_at)

    def test_source_edit_does_not_restart_until_explicit_request(self):
        builds = 0

        def compile_fixture():
            nonlocal builds
            builds += 1
            return self.binary(f"version-{builds}")

        self.runner.compile = compile_fixture
        with patch("dev.signal.signal"):
            worker = threading.Thread(target=self.runner.run, daemon=True)
            worker.start()
            try:
                deadline = time.monotonic() + 5
                while (self.runner.process is None or self.runner.control_path is None) and time.monotonic() < deadline:
                    time.sleep(0.02)
                self.assertIsNotNone(self.runner.process)
                first = self.runner.process
                self.assertEqual(builds, 1)
                (self.root / "crates" / "a.rs").write_text("fn edited() {}\n", encoding="utf-8")
                time.sleep(0.7)
                self.assertEqual(builds, 1, "source changes cannot trigger a rebuild")
                self.assertIs(self.runner.process, first)
                self.assertIsNone(first.poll())

                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
                    connection.settimeout(2)
                    connection.connect(str(self.runner.control_path))
                    connection.sendall(b"restart\n")
                    self.assertEqual(connection.recv(32), b"accepted\n")

                deadline = time.monotonic() + 5
                while (builds < 2 or self.runner.process is first) and time.monotonic() < deadline:
                    time.sleep(0.03)
                self.assertEqual(builds, 2)
                self.assertIsNot(self.runner.process, first)
                self.assertIsNotNone(first.poll())
            finally:
                self.runner.stopping = True
                worker.join(timeout=5)
                self.assertFalse(worker.is_alive(), "development supervisor should stop cleanly")

    def test_failed_build_keeps_original_server_running(self):
        first = self.binary("first")
        self.runner.process = self.runner.start(first)
        self.assertIsNotNone(self.runner.process)
        before = self.runner.process.pid
        self.runner.current_binary = first
        self.runner.compile = lambda: None
        self.assertFalse(self.runner.rebuild())
        self.assertEqual(self.runner.process.pid, before)
        self.assertIsNone(self.runner.process.poll())

    def test_successful_build_restarts_and_reaps_previous_process(self):
        first = self.binary("first")
        second = self.binary("second")
        self.runner.process = self.runner.start(first)
        self.assertIsNotNone(self.runner.process)
        old = self.runner.process
        self.runner.current_binary = first
        staged = self.root / "staged"
        shutil.copy2(second, staged)
        self.runner.compile = lambda: staged
        self.assertFalse(self.runner.rebuild())
        self.assertIsNotNone(self.runner.process)
        self.assertNotEqual(old.pid, self.runner.process.pid)
        self.assertIsNotNone(old.poll())
        self.assertFalse(first.exists())

    def test_failed_new_server_rolls_back_last_working_build(self):
        first = self.binary("first")
        self.runner.process = self.runner.start(first)
        self.assertIsNotNone(self.runner.process)
        old = self.runner.process
        self.runner.current_binary = first
        failing = self.root / "failing"
        failing.write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
        failing.chmod(0o700)
        self.runner.compile = lambda: failing
        self.assertFalse(self.runner.rebuild())
        self.assertIsNotNone(old.poll())
        self.assertIsNotNone(self.runner.process)
        self.assertIsNone(self.runner.process.poll())
        self.assertEqual(self.runner.current_binary, first)


if __name__ == "__main__":
    unittest.main()
