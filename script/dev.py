#!/usr/bin/env python3
"""Run NovaGPT in development; rebuild and restart only on explicit MCP request.

Requires Python 3 and Cargo. POSIX only: the owned server runs in a process
group so unexpected children can be stopped on timeout.
"""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time


def sources(root: Path) -> tuple[tuple[str, int, int], ...]:
    """Snapshot Rust sources and Cargo configuration to detect a build race."""
    files = [root / "Cargo.toml", root / "Cargo.lock", root / "rust-toolchain.toml"]
    files.extend((root / "crates").rglob("*.rs"))
    files.extend((root / "crates").rglob("Cargo.toml"))
    files.extend((root / "crates").rglob("build.rs"))
    files.extend((root / ".cargo").glob("config*"))
    found = []
    for path in set(files):
        try:
            stat = path.stat()
            found.append((str(path.relative_to(root)), stat.st_mtime_ns, stat.st_size))
        except FileNotFoundError:
            continue
    return tuple(sorted(found))


def stop_owned(process: subprocess.Popen[bytes], grace: float = 8.0) -> None:
    """Ask NovaGPT to drain and stop its ngrok; forcibly stop its group on timeout."""
    if process.poll() is not None:
        process.wait()
        return
    try:
        process.send_signal(signal.SIGTERM)
        process.wait(timeout=grace)
    except subprocess.TimeoutExpired:
        print("[dev] shutdown timed out; killing owned process group", flush=True)
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
    except ProcessLookupError:
        process.wait()


class Runner:
    def __init__(self, root: Path, port: int, ngrok: bool, domain: str | None):
        self.root = root
        self.port = port
        self.ngrok = ngrok
        self.domain = domain
        self.target = root / "target"
        self.versions = self.target / "novagpt-dev"
        self.process: subprocess.Popen[bytes] | None = None
        self.current_binary: Path | None = None
        self.build_process: subprocess.Popen[bytes] | None = None
        self.stopping = False
        self.counter = 0
        self.control_directory: tempfile.TemporaryDirectory[str] | None = None
        self.control_path: Path | None = None
        self.control_listener: socket.socket | None = None
        self.restart_due_at: float | None = None

    def request_stop(self, _signal: int, _frame: object) -> None:
        self.stopping = True

    def open_manual_control(self) -> None:
        """Create an owner-private local socket; only the owned MCP child gets its path."""
        self.control_directory = tempfile.TemporaryDirectory(prefix="novagpt-dev-control-")
        self.control_path = Path(self.control_directory.name) / "restart.sock"
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            listener.bind(str(self.control_path))
            os.chmod(self.control_path, 0o600)
            listener.listen(4)
            listener.setblocking(False)
        except OSError:
            listener.close()
            self.close_manual_control()
            raise
        self.control_listener = listener
        print("[dev] restart only on explicit MCP debug_restart request", flush=True)

    def close_manual_control(self) -> None:
        if self.control_listener is not None:
            self.control_listener.close()
            self.control_listener = None
        if self.control_directory is not None:
            self.control_directory.cleanup()
            self.control_directory = None
        self.control_path = None

    def poll_manual_control(self) -> None:
        """Acknowledge a bounded local request before scheduling a graceful restart."""
        if self.control_listener is None:
            return
        try:
            connection, _ = self.control_listener.accept()
        except BlockingIOError:
            return
        with connection:
            connection.settimeout(0.25)
            try:
                command = connection.recv(32)
                if command != b"restart\n":
                    connection.sendall(b"invalid\n")
                elif self.restart_due_at is not None:
                    connection.sendall(b"pending\n")
                else:
                    # Let the MCP success response leave the process before stopping it.
                    self.restart_due_at = time.monotonic() + 2.0
                    connection.sendall(b"accepted\n")
                    print("[dev] authenticated MCP requested a manual rebuild", flush=True)
            except (OSError, TimeoutError):
                pass

    def compile(self) -> Path | None:
        print("[dev] building novagpt-cli (current server stays online)...", flush=True)
        self.build_process = subprocess.Popen(
            ["cargo", "build", "--target-dir", str(self.target), "-p", "novagpt-cli", "--bin", "novagpt"],
            cwd=self.root,
            start_new_session=True,
        )
        try:
            while self.build_process.poll() is None:
                if self.stopping:
                    stop_owned(self.build_process, 2)
                    return None
                time.sleep(0.2)
            if self.build_process.returncode:
                print("[dev] build failed; keeping the last working NovaGPT running", flush=True)
                return None
            original = self.target / "debug" / "novagpt"
            if not original.is_file():
                print("[dev] build succeeded but executable is missing; keeping old server", flush=True)
                return None
            self.versions.mkdir(parents=True, exist_ok=True)
            self.counter += 1
            staged = self.versions / f"novagpt-{os.getpid()}-{self.counter}"
            shutil.copy2(original, staged)
            return staged
        finally:
            self.build_process = None

    def start(self, binary: Path) -> subprocess.Popen[bytes] | None:
        args = [str(binary), "--root", str(self.root), "http", "--port", str(self.port)]
        args.append("--ngrok" if self.ngrok else "--no-ngrok")
        if self.domain:
            args.extend(["--ngrok-domain", self.domain])
        environment = os.environ.copy()
        environment.pop("NOVAGPT_DEV_RESTART_SOCKET", None)
        if self.control_path is not None:
            environment["NOVAGPT_DEV_RESTART_SOCKET"] = str(self.control_path)
        process = subprocess.Popen(args, cwd=self.root, env=environment, start_new_session=True)
        print(f"[dev] starting build {binary.name} on port {self.port}", flush=True)
        deadline = time.monotonic() + 16.0
        while not self.stopping and time.monotonic() < deadline:
            if process.poll() is not None:
                print("[dev] server exited before becoming ready", flush=True)
                process.wait()
                return None
            try:
                check = subprocess.run(
                    [str(binary), "--root", str(self.root), "connection-test", "--port", str(self.port)],
                    cwd=self.root,
                    stdin=subprocess.DEVNULL,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    timeout=2,
                    check=False,
                )
                if check.returncode == 0 and process.poll() is None:
                    print("[dev] authenticated local MCP is ready", flush=True)
                    return process
            except (OSError, subprocess.TimeoutExpired):
                pass
            time.sleep(0.35)
        stop_owned(process)
        if not self.stopping:
            print("[dev] server readiness timed out", flush=True)
        return None

    def rebuild(self) -> bool:
        before = sources(self.root)
        candidate = self.compile()
        if self.stopping:
            if candidate:
                candidate.unlink(missing_ok=True)
            return False
        if sources(self.root) != before:
            print("[dev] sources changed during compilation; skipping this build until the next explicit request", flush=True)
            if candidate:
                candidate.unlink(missing_ok=True)
            return False
        if candidate is None:
            return False
        previous_binary = self.current_binary
        if self.process is not None:
            print("[dev] build successful; stopping previous server gracefully", flush=True)
            stop_owned(self.process)
            self.process = None
        next_process = self.start(candidate)
        if next_process is not None:
            self.process = next_process
            self.current_binary = candidate
            if previous_binary:
                previous_binary.unlink(missing_ok=True)
            return False
        candidate.unlink(missing_ok=True)
        if not self.stopping and previous_binary:
            print("[dev] new server failed to start; restoring previous working build", flush=True)
            self.process = self.start(previous_binary)
            if self.process is None:
                print("[dev] rollback could not start; check port and ngrok configuration", flush=True)
        return False

    def run(self) -> None:
        signal.signal(signal.SIGINT, self.request_stop)
        signal.signal(signal.SIGTERM, self.request_stop)
        try:
            self.open_manual_control()
            self.rebuild()  # Startup build only; no automatic retries.
            while not self.stopping:
                self.poll_manual_control()
                if self.restart_due_at is not None and time.monotonic() >= self.restart_due_at:
                    self.restart_due_at = None
                    self.rebuild()
                elif self.process is not None and self.process.poll() is not None:
                    print("[dev] server exited unexpectedly; no automatic restart", flush=True)
                    self.process.wait()
                    self.process = None
                time.sleep(0.2)
        finally:
            self.close_manual_control()
            if self.build_process is not None:
                stop_owned(self.build_process, 2)
            if self.process is not None:
                print("[dev] stopping NovaGPT and owned ngrok...", flush=True)
                stop_owned(self.process)
            if self.current_binary is not None:
                self.current_binary.unlink(missing_ok=True)
            print("[dev] stopped", flush=True)


def main() -> int:
    if os.name != "posix":
        print("This dev runner currently supports Linux/macOS only.", file=sys.stderr)
        return 2
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8787)
    parser.add_argument("--ngrok", action="store_true", default=True)
    parser.add_argument("--no-ngrok", action="store_false", dest="ngrok")
    parser.add_argument("--ngrok-domain", metavar="HOSTNAME")
    args = parser.parse_args()
    if not 1 <= args.port <= 65535 or (args.ngrok_domain and not args.ngrok):
        parser.error("invalid port or --ngrok-domain used with --no-ngrok")
    root = Path(__file__).resolve().parent.parent
    Runner(root, args.port, args.ngrok, args.ngrok_domain).run()
    return 0


if __name__ == "__main__":
    sys.exit(main())
