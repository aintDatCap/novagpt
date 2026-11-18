"""Offline safety tests for the opt-in Linux Xvfb owner wrapper."""
import os
from pathlib import Path
import signal
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import novagpt_xvfb as display


class ValidationTests(unittest.TestCase):
    def test_display_number_is_strict_and_canonical(self):
        for invalid in ("", "99", ":0", ":89", ":4096", ":99.0",
                        ":99;rm", ":99\n", ":099", ":0099", ":-1"):
            with self.subTest(value=invalid), self.assertRaises(ValueError):
                display.validate_display(invalid)
        self.assertEqual(display.validate_display(":99"), ":99")
        self.assertEqual(display.validate_display(":4095"), ":4095")

    def test_existing_display_denied_without_spawning(self):
        args = mock.Mock(display=":99", command=["--", "novagpt", "http"],
                         vnc_port=None, vnc_auth=None)
        with mock.patch.object(display.sys, "platform", "linux"), \
                mock.patch.object(display.os, "geteuid", return_value=1000), \
                mock.patch.object(display, "socket_path") as path, \
                mock.patch.object(display, "owned_spawn") as spawn:
            path.return_value.exists.return_value = True
            with self.assertRaisesRegex(display.DisplayError, "occupied"):
                display.run(args)
            spawn.assert_not_called()

    def test_vnc_password_requires_private_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "auth"
            path.write_text("non-secret-test-fixture")
            os.chmod(path, 0o644)
            with self.assertRaisesRegex(display.DisplayError, "0600"):
                display.validate_vnc_auth(str(path))
            os.chmod(path, 0o600)
            self.assertEqual(display.validate_vnc_auth(str(path)), str(path))
            link = Path(directory) / "symlink"
            link.symlink_to(path)
            with self.assertRaisesRegex(display.DisplayError, "regular private"):
                display.validate_vnc_auth(str(link))

    def test_xauth_cookie_is_passed_only_on_stdin(self):
        with tempfile.TemporaryDirectory() as directory:
            authority = Path(directory) / "Xauthority"
            def xauth(argv, **kwargs):
                self.assertNotIn(kwargs["input"].strip().split()[-1], repr(argv))
                self.assertEqual(argv[-1], "-")
                self.assertNotIn("shell", kwargs)
                authority.write_bytes(b"dummy-xauth")
                return subprocess.CompletedProcess(argv, 0)
            with mock.patch.object(display.subprocess, "run", side_effect=xauth):
                display.create_authority(authority, ":99", "/usr/bin/xauth")
            self.assertEqual(stat.S_IMODE(authority.stat().st_mode), 0o600)

    def test_vnc_pair_is_required(self):
        args = mock.Mock(display=":99", command=["--", "novagpt", "http"],
                         vnc_port=5905, vnc_auth=None)
        with mock.patch.object(display.sys, "platform", "linux"), \
                mock.patch.object(display.os, "geteuid", return_value=1000), \
                mock.patch.object(display, "socket_path") as path:
            path.return_value.exists.return_value = False
            path.return_value.is_symlink.return_value = False
            with self.assertRaisesRegex(display.DisplayError, "requires"):
                display.run(args)


class ProcessOwnershipTests(unittest.TestCase):
    def test_no_kill_of_a_process_that_already_exited(self):
        child = mock.Mock()
        child.poll.return_value = 0
        with mock.patch.object(display.os, "killpg") as killpg:
            display.stop_owned(child)
            killpg.assert_not_called()

    def test_owned_process_group_is_terminated(self):
        child = subprocess.Popen(
            [sys.executable, "-c", "import time; time.sleep(60)"],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        self.addCleanup(lambda: child.poll() is None and child.kill())
        display.stop_owned(child)
        self.assertIsNotNone(child.poll())

    def test_wait_fails_when_owner_has_exited(self):
        process = mock.Mock()
        process.poll.return_value = 1
        with mock.patch.object(display, "socket_path") as path:
            path.return_value.is_socket.return_value = False
            with self.assertRaisesRegex(display.DisplayError, "before readiness"):
                display.wait_for_display(process, ":99")


if __name__ == "__main__":
    unittest.main()
