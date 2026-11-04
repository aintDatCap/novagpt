#!/usr/bin/env python3
"""Hot-reload NovaGPT without replacing the last working server on build failure.

Requires Python 3 and Cargo, no external file watcher. POSIX only: the owned
server is given a process group so unexpected children can be stopped on timeout.
"""
from __future__ import annotations

import argparse
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time


def sources(root: Path) -> tuple[tuple[str, int, int], ...]:
    """Watch Rust sources and Cargo configuration, never generated target files."""
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

    def request_stop(self, _signal: int, _frame: object) -> None:
        self.stopping = True

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
        process = subprocess.Popen(args, cwd=self.root, start_new_session=True)
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
            print("[dev] sources changed during compilation; rebuilding latest revision", flush=True)
            if candidate:
                candidate.unlink(missing_ok=True)
            return True
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
        last = sources(self.root)
        try:
            needs_rebuild = True
            while not self.stopping:
                if needs_rebuild:
                    needs_rebuild = self.rebuild()
                    last = sources(self.root)
                    continue
                new = sources(self.root)
                if new != last:
                    last = new
                    # Debounce editor save/rename bursts.
                    quiet_until = time.monotonic() + 0.45
                    while not self.stopping and time.monotonic() < quiet_until:
                        time.sleep(0.15)
                        current = sources(self.root)
                        if current != last:
                            last = current
                            quiet_until = time.monotonic() + 0.45
                    needs_rebuild = not self.stopping
                elif self.process is not None and self.process.poll() is not None:
                    print("[dev] server exited unexpectedly; waiting for a source change", flush=True)
                    self.process.wait()
                    self.process = None
                else:
                    time.sleep(0.4)
        finally:
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
