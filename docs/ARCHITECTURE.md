# NovaGPT architecture

NovaGPT is an independent Rust MCP coding server. Product design, protocol support and release decisions belong to this repository; backward compatibility is not guaranteed for every change. See [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md), [EXTENSION_PLAYBOOK.md](EXTENSION_PLAYBOOK.md), and [QUALITY_GATES.md](QUALITY_GATES.md).

## Crate boundaries

```text
novagpt-cli
  ├─ novagpt-mcp ── novagpt-core
  │    ├─ novagpt-vm
  │    └─ novagpt-agents ── novagpt-browser ── novagpt-browser-driver
  ├─ novagpt-vm, novagpt-agents, novagpt-browser
  └─ novagpt-core
```

- **novagpt-core:** owned workspaces, allowed-root and path guards, file reads and writes, optimistic SHA-256 updates, safe Git operations, commands and structured workspace operations. It must not depend on CLI, HTTP or MCP.
- **novagpt-mcp:** tools, schemas, protocol dispatch, stdio and HTTP transport, and request authentication through the official Rust MCP SDK (`rmcp`).
- **novagpt-cli:** argument parsing, shared private per-user configuration and token, terminal UI, process ownership, ngrok tunnel management, and optional integrations.
- **novagpt-vm:** optional approved VM images and platform-specific VM controls; real guest execution support is incomplete.
- **novagpt-agents** and **novagpt-browser:** optional bounded ChatGPT Web subagent management, manual-login handling, visible UI inspection and protected browser navigation.
- **novagpt-browser-driver:** optional Rust-native owner of the dedicated Chrome process/profile, exact-target background CDP commands, serialized W3C WebDriver setup/navigation, bounded DOM/input primitives, and crash invalidation. No Node.js, Patchright runtime or shared personal browser profile.

## Workspace and security model

One server can serve multiple simultaneous MCP clients on the same filesystem. MCP session IDs represent transport sessions, not ChatGPT conversations. Each operation uses the default configured root unless another explicitly approved workspace ID is supplied.

Canonical workspace roots and path guards reject disallowed paths and selected sensitive files. Optimistic SHA-256 changes are checked under per-file locks. Git index operations coordinate per canonical root. OS-level hostile path races still require further testing.

The MCP HTTP server binds to loopback and requires the shared user token. It accepts a bearer header or the private URL query parameter `novagpt_token`; the query secret is removed before MCP protocol handling. The token is private user state outside the repository. A previously issued query key is accepted temporarily for existing private connections, but is not emitted in new URLs.

The CLI/TUI can manage its own ngrok process and verify the matching upstream port and hostname before disclosing the HTTPS endpoint. The ngrok inspector and tunnel logs are disabled. The full connector URL is shown only on user request. Credentials and full URLs are not written to audit logs.

**Host-shell execution is not sandboxed.** A client with the full private connector URL can run commands with the server user's OS permissions. Explicitly allowed workspaces are not a sandbox around shell commands. Browser, VM and subagent features are opt-in.

## User experience and lifecycle

`novagpt` opens a three-page terminal UI: Overview, Activity and Settings. Guided `--setup` requires confirmation before it saves settings or creates a workspace-local launcher. The default UI language is Automatic, resolved from the system locale, with English fallback; all logs and diagnostics are in English.

The saved `auto_start_mcp` preference is disabled by default. If enabled, normal TUI launch requests MCP startup using saved HTTP and ngrok preferences; guided setup does not. Missing ngrok prompts for explicit installer consent, with No as the default answer. The installer uses only supported system package managers and does not handle ngrok account credentials.

Graceful HTTP shutdown handles Ctrl+C and SIGTERM on Unix and flushes audit events. CLI-owned ngrok is stopped and reaped on normal termination. Forced process termination, power failures and all in-flight requests are not guaranteed to be recoverable.

The native driver boundary follows [ADR 0003](decisions/0003-native-browser-driver.md); the preceding opt-in architecture remains described in [ADR 0002](decisions/0002-optional-vm-and-browser-agents.md). Live ChatGPT account compatibility is outside the disposable browser fixture's coverage.

### Chrome readiness, consent, and safe recovery

The ChatGPT adapter classifies a bounded page snapshot as **ready**, **navigation**, **authentication**, **verification**, or **unavailable**. A visible verification widget or the full-page Cloudflare title `Just a moment...` is verification, even without a widget; a missing composer alone is *not* proof of authentication. The adapter waits up to 20 seconds for an initially blank or delayed page, then reports a navigation timeout or an unavailable composer rather than inventing a login requirement. Bounded `page_state` and lifecycle diagnostics contain phase/flags only, not page titles, URLs, prompts, cookies or challenge tokens. Provider verification is performed manually in the visible tab; `subagent_resume` rechecks the same tab before any input.

The Rust driver owns one dedicated, non-default Chrome user-data directory, guarded by an owner marker and an OS file lock. Chrome 136+ disallows remote debugging against the normal Chrome user-data directory (see [Chrome's security change](https://developer.chrome.com/blog/remote-debugging-port)); using a persistent **dedicated** profile is the intended supported configuration. Chrome is launched with an ephemeral loopback DevTools port; NovaGPT downloads and patch-installs a ChromeDriver matching the installed Chrome milestone, starts only its own driver child, waits for `/status.ready`, and creates a W3C WebDriver session through `debuggerAddress`. Legacy WebDriver tab operations are serialized under the session lock and can switch the active window, so the ChatGPT worker adapter avoids them during normal page observation, response polling, composer entry, mouse clicks, and Enter. Instead, the driver resolves the exact ChromeDriver-created tab ID in the dedicated browser's loopback CDP target list, checks the exact page WebSocket URL, and dispatches direct per-target commands with no window-activation request; mouse press/release and key down/up remain ordered on the same socket. Startup/new-tab navigation may initially activate Chrome, but routine background worker work does not. No compositor focus restoration loop is used. `reconnect()` restarts the driver service and, if Chrome has exited, relaunches Chrome with the same dedicated profile; old tabs are invalidated and no navigation, input, or prompt is replayed. The ChatGPT adapter keeps a submission-attempt flag: only an **unsent** tab may be recreated/resumed, and a possible submission after a crash fails closed as ambiguous.

The separately enabled `BrowserBridge` can inspect visible text from explicitly approved HTTPS-origin tabs in an already running, user-managed CDP Chrome, but cannot navigate, authenticate or type. Chrome 144+ documents a per-connection user-consent remote-debugging flow; NovaGPT does **not** yet implement that attachment protocol for subagent control. Extending the read-only bridge to write-capable automation would require explicit tab selection, narrower authorization, lifecycle and origin checks, and a dedicated review of exposure of personal browser data. No anti-bot spoofing or automated challenge solving is implemented.

### Background worker focus policy

Visible Chrome is permitted one initial window activation when the browser launches or a new tab is created; NovaGPT does **not** make it always-on-top, request repeated maximization, or fight the compositor for focus. Underlying WebDriver `POST /session/{session}/window` switches could repeatedly raise the browser while polling. Therefore, routine ChatGPT worker DOM reads, 300-ms response polls, status diagnostics, text insertion, mouse-click and Enter events use the owned Chrome's **exact matching per-page CDP WebSocket** in `novagpt-browser-driver::undetected::BackgroundTarget`. These operations never request `Page.bringToFront`, WebDriver `switch to window`, OS focus, or window-manager activation. Per-target selection and command ordering keep concurrent workers isolated. WebDriver remains available for infrequent explicit tab creation/navigation, manual browsing commands and controlled recovery. Chrome startup removes `--start-maximized`; the browser is neither always on top nor repeatedly refocused. The independent user-chosen foreground application can retain focus while worker page input continues.

A real opt-in Linux **niri Wayland** smoke test checks another application's compositor window ID while two disposable Chrome targets are repeatedly read and one is typed/clicked; it also verifies both targets' independent DOM results. Run `cargo test -p novagpt-browser-driver --lib real_niri_focus_remains_elsewhere_during_concurrent_background_input -- --ignored --nocapture`. This test restores the prior application once after launching the disposable browser (to disregard its unavoidable initial activation). X11 and other Wayland compositors require manual observation or a separately supported compositor API. It is not a live ChatGPT-account test, and it does not claim that **every** compositor supports reliable background input. If background input fails, report the error; never run a rapid focus-stealing/recovery loop.

### Managed ChromeDriver compatibility backend

`novagpt-browser-driver::undetected` reproduces the ChromeDriver lifecycle used by upstream undetected-chromedriver 3.5.5 without embedding Selenium or GPL source. It keeps an owned Chrome process, downloads and patch-installs a ChromeDriver matching the installed Chrome milestone into a versioned, digest-checked store, starts only its own ChromeDriver child, creates a W3C WebDriver session that attaches through `debuggerAddress`, serializes tab commands while switching to each worker's exact window handle, and reconnects by restarting the driver service/session or relaunching a crashed Chrome with the same profile. **This is now the ChatGPT adapter's transport.** The former native CDP process/WebSocket modules (`cdp.rs`, `lifecycle.rs`, `config.rs`, `error.rs`, root `profile.rs`) and the native `real_chrome` test were removed; the profile lease moved under `undetected/`. WebDriver remains the owner for session/tab creation and explicit manual browsing operations. **Worker page evaluation and physical input now use the driver-layer `BackgroundTarget`, an exact-target loopback DevTools WebSocket** (no separate browser process and no arbitrary MCP-exposed JavaScript); existing WebDriver CDP-execute remains available for explicit commands and test fixtures. This separation avoids repeated ChromeDriver `POST /window` activation during ordinary worker polling. Browser console capture uses ChromeDriver browser logs with URL redaction. The library and `novagpt chrome-driver status|install` expose driver management. See [the parity report](undetected-chromedriver-parity.md) for the source audit, licensing analysis, deliberate gaps (headless identity rewriting, detached processes, force-kill, and `--no-sandbox` default) and test evidence. There is no live authenticated ChatGPT coverage yet.

For local Rust iteration, `script/dev.py` watches source and manifests, builds without stopping the last working server, then restarts its owned server and tunnel on successful compilation. It attempts rollback if a new binary fails readiness. Each successful restart briefly interrupts active MCP sessions and may change a dynamically assigned ngrok hostname.

## Known limitations

- The full live public ChatGPT-to-ngrok integration has not been independently verified on every supported platform.
- TUI shutdown currently aborts some async server tasks; complete graceful resource cleanup needs an end-to-end audit.
- Shell commands run with host permissions; subprocess-tree cleanup and full sandboxing are not guaranteed.
- VM guest control and some optional integrations require live testing. The managed ChromeDriver transport passed disposable Chrome 156 tests covering launch, patch, four-tab isolation, reconnect, Chrome crash/restart, profile persistence across application restart, screenshots, and the ChatGPT adapter's local browsing path. Live ChatGPT authentication, challenge handling, and response completion remain unverified; this is not a fully managed cloud environment.
- Some filesystem writes are not crash-atomic on all platforms; path-swap defenses and adversarial concurrency require further review.
- Public MCP transport session TTL, reconnect and load boundaries need additional integration tests.

## Development checks

From the repository root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
python3 -m unittest discover -s script -p 'test_dev.py'
```

All runtime behavior and security changes require relevant positive and negative integration tests, plus recorded verification evidence. See [CAPABILITIES.md](CAPABILITIES.md), [QUALITY_GATES.md](QUALITY_GATES.md) and the [architecture decisions](decisions/README.md).
