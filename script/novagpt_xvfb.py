#!/usr/bin/env python3
"""Run an existing NovaGPT CLI command under a private, owned Linux Xvfb.

Opt-in service wrapper. Chrome/ChromeDriver/browser automation remain owned by
NovaGPT, not this script. Requires Xvfb, xauth and a window manager (openbox by
default). No listening VNC/DevTools endpoint is created.
"""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import re
import secrets
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time

STARTUP_SECONDS = 8.0
STOP_SECONDS = 5.0


class DisplayError(Exception):
    """Stable error without subprocess stderr or authentication material."""


def validate_display(value: str) -> str:
    if re.fullmatch(r":[0-9]{2,4}", value) is None:
        raise ValueError("display must be :90 through :4095")
    number = int(value[1:])
    if value != f":{number}":
        raise ValueError("display must use a canonical decimal number")
    if not 90 <= number <= 4095:
        raise ValueError("display must be :90 through :4095")
    return f":{number}"


def socket_path(display: str) -> Path:
    return Path(f"/tmp/.X11-unix/X{display[1:]}")


def executable(name: str) -> str:
    resolved = shutil.which(name)
    if resolved is None:
        raise DisplayError("required executable is unavailable")
    return resolved


def create_authority(path: Path, display: str, xauth: str) -> None:
    # Never pass the cookie in argv, logs or the environment. The authority
    # file lives under a mode-0700 temporary directory.
    cookie = secrets.token_hex(16)
    try:
        subprocess.run(
            [xauth, "-q", "-f", str(path), "-"],
            input=f"add {display} . {cookie}\n",
            text=True, stdin=None,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=5, check=True,
        )
        os.chmod(path, 0o600)
    except (OSError, subprocess.SubprocessError) as error:
        raise DisplayError("could not create private Xauthority") from error


def owned_spawn(args: list[str], *, env: dict[str, str] | None = None) -> subprocess.Popen:
    try:
        return subprocess.Popen(
            args, env=env, start_new_session=True, close_fds=True,
        )
    except OSError as error:
        raise DisplayError("a required owned process failed to start") from error


def stop_owned(process: subprocess.Popen | None) -> None:
    """Signal only the process group created by this wrapper.

    A reaped leader cannot safely establish group ownership, so never use
    pid-based global cleanup or kill a group after its leader has exited.
    """
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=STOP_SECONDS)
    except subprocess.TimeoutExpired:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass


def wait_for_display(process: subprocess.Popen, display: str) -> None:
    path = socket_path(display)
    deadline = time.monotonic() + STARTUP_SECONDS
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise DisplayError("owned Xvfb exited before readiness")
        # Do not use filesystem existence as proof of display ownership:
        # a local peer could create a look-alike socket.
        if path.is_socket():
            sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            try:
                sock.settimeout(0.4)
                sock.connect(str(path))
                if process.poll() is None:
                    return
            except OSError:
                pass
            finally:
                sock.close()
        time.sleep(0.1)
    raise DisplayError("owned Xvfb did not become ready")


def validate_vnc_auth(value: str) -> str:
    path = Path(value).resolve(strict=True)
    original = Path(value)
    if original.is_symlink() or not path.is_file():
        raise DisplayError("VNC password file must be a regular private file")
    meta = path.stat()
    if meta.st_uid != os.getuid() or stat.S_IMODE(meta.st_mode) & 0o077:
        raise DisplayError("VNC password file must be owned by this user and mode 0600")
    return str(path)


def run(args: argparse.Namespace) -> int:
    if not sys.platform.startswith("linux"):
        raise DisplayError("the virtual display wrapper supports Linux only")
    if os.geteuid() == 0:
        raise DisplayError("run as a dedicated unprivileged Linux user, not root")
    display = validate_display(args.display)
    if socket_path(display).exists() or socket_path(display).is_symlink():
        raise DisplayError("display is already occupied; never attach to a foreign X server")
    command = args.command[1:] if args.command and args.command[0] == "--" else args.command
    if not command:
        raise DisplayError("provide an explicit NovaGPT command after --")
    if args.vnc_port is not None and args.vnc_auth is None:
        raise DisplayError("--vnc-port requires --vnc-auth (private x11vnc password file)")
    if args.vnc_port is None and args.vnc_auth is not None:
        raise DisplayError("--vnc-auth requires --vnc-port")
    xvfb = executable(args.xvfb)
    xauth = executable(args.xauth)
    wm = executable(args.window_manager) if args.window_manager else None
    vnc = executable(args.vnc_executable) if args.vnc_port is not None else None
    vnc_auth = validate_vnc_auth(args.vnc_auth) if args.vnc_auth else None
    command[0] = executable(command[0])

    stopped_signal: list[int] = []
    def handle_signal(signum: int, _frame: object) -> None:
        stopped_signal.append(signum)

    old_handlers = {
        sig: signal.signal(sig, handle_signal)
        for sig in (signal.SIGINT, signal.SIGTERM)
    }
    display_process = window_process = vnc_process = service_process = None
    try:
        with tempfile.TemporaryDirectory(prefix="novagpt-xvfb-") as directory:
            authority = Path(directory) / "Xauthority"
            create_authority(authority, display, xauth)
            environment = os.environ.copy()
            environment.update({"DISPLAY": display, "XAUTHORITY": str(authority)})
            display_process = owned_spawn(
                [xvfb, display, "-screen", "0", "1920x1080x24",
                 "-nolisten", "tcp", "-auth", str(authority)],
                env=environment,
            )
            wait_for_display(display_process, display)
            if wm is not None:
                window_process = owned_spawn([wm], env=environment)
                time.sleep(0.25)
                if window_process.poll() is not None:
                    raise DisplayError("owned window manager exited before readiness")
            if vnc is not None:
                # Explicitly loopback-only, authenticated; use SSH forwarding.
                vnc_process = owned_spawn(
                    [vnc, "-display", display, "-auth", str(authority),
                     "-localhost", "-rfbport", str(args.vnc_port),
                     "-rfbauth", vnc_auth, "-forever", "-shared", "-quiet",
                     "-noshm"],
                    env=environment,
                )
                time.sleep(0.25)
                if vnc_process.poll() is not None:
                    raise DisplayError("private VNC inspection server failed to start")
            service_process = owned_spawn(command, env=environment)
            while not stopped_signal:
                status = service_process.poll()
                if status is not None:
                    return status
                if display_process.poll() is not None:
                    raise DisplayError("owned Xvfb exited while NovaGPT was running")
                if window_process is not None and window_process.poll() is not None:
                    raise DisplayError("owned window manager exited while NovaGPT was running")
                if vnc_process is not None and vnc_process.poll() is not None:
                    raise DisplayError("private VNC inspection server exited")
                time.sleep(0.25)
            return 128 + stopped_signal[0]
    finally:
        # Reverse startup order; never pkill by name or remove global X sockets.
        for child in (service_process, vnc_process, window_process, display_process):
            stop_owned(child)
        for sig, handler in old_handlers.items():
            signal.signal(sig, handler)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--display", default=":99", type=validate_display)
    parser.add_argument("--xvfb", default="Xvfb")
    parser.add_argument("--xauth", default="xauth")
    parser.add_argument("--window-manager", default="openbox",
                        help="Window manager executable; set empty to opt out")
    parser.add_argument("--vnc-port", type=int, choices=range(5900, 6000),
                        metavar="5900..5999", help="Opt-in localhost VNC inspection port")
    parser.add_argument("--vnc-auth", help="Owner-private x11vnc password file")
    parser.add_argument("--vnc-executable", default="x11vnc")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    try:
        return run(args)
    except (DisplayError, ValueError) as error:
        print(f"Virtual display unavailable: {error}", file=sys.stderr)
        return 70


if __name__ == "__main__":
    raise SystemExit(main())
