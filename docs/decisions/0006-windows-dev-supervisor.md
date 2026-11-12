# ADR 0006 — Windows development supervisor

**Date:** 2026-11-12
**Status:** Accepted. Windows tests and debug CLI integration; manual console/terminal smoke tests remain useful.

## Context

`script/dev.py` previously required Unix-domain sockets and POSIX process groups. The Rust MCP `debug_restart` tool and CLI fallback were also compiled only on Unix. Simply making the Python launcher accept Windows would leave its authenticated restart feature unusable there.

## Decision

- Retain the owner-private `AF_UNIX` control socket and existing request format on POSIX.
- On Windows, bind a local TCP listener exclusively to `127.0.0.1` on an ephemeral port. Generate a fresh 256-bit random hexadecimal capability per supervisor launch. The owned MCP debug child receives the port and capability as `NOVAGPT_DEV_RESTART_PORT` and `NOVAGPT_DEV_RESTART_TOKEN`. Require an exact capability-bearing `restart\n` message before scheduling a rebuild. Compare tokens in constant time and cap request bytes and receive time. Do not log or persist the capability.
- Expose `debug_restart` through the MCP and CLI only on debug builds for Unix and Windows. Keep release builds unable to accept manual restart requests, even when the environment variables are present. Ordinary MCP clients still require authenticated access before invoking the control.
- Use `CREATE_NEW_PROCESS_GROUP` for Windows build/server children. Send `CTRL_BREAK_EVENT` to request a graceful exit. If that fails or times out, terminate the owned process tree rather than unrelated processes. The Windows HTTP signal handlers listen for Ctrl+Break as well as Ctrl+C.
- Preserve startup-only builds, explicit-only restarts, previous-build rollback and temporary versioned executables (with `.exe` on Windows).

## Consequences and risks

The capability guards a loopback socket against other local clients, but another process with sufficient access to the supervised MCP child's environment may retrieve it. The supervisor must never expose the control to a public interface or print its token. The listener is local to the machine and is not independently routed through ngrok. The MCP authentication token remains separate.

Windows console hosts can differ in delivery of `CTRL_BREAK_EVENT`. The process-tree termination fallback is intentionally forceful and may not flush in-flight events. Live testing with Windows Terminal, detached sessions and active ngrok processes remains necessary.

This changes no stable public MCP schema; debug-only `debug_restart` becomes available on Windows. Existing POSIX behavior is preserved.
