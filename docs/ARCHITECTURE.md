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

- **novagpt-core:** owned workspaces, allowed-root and path guards, file reads and writes, optimistic SHA-256 updates, safe Git operations, commands and structured workspace operations. The MCP `grep` tool uses ripgrep's `grep-regex`, `grep-searcher` and `ignore` crates for bounded regex matching and ignore-aware traversal; the existing literal `search` contract is unchanged. These dependencies are pinned to Rust 1.88-compatible releases. It must not depend on CLI, HTTP or MCP.
- **novagpt-mcp:** tools, schemas, protocol dispatch, stdio and HTTP transport, and request authentication through the official Rust MCP SDK (`rmcp`).
- **novagpt-cli:** argument parsing, shared private per-user configuration and token, terminal UI, process ownership, ngrok tunnel management, and optional integrations.
- **novagpt-vm:** optional approved VM images and platform-specific VM controls; real guest execution support is incomplete.
- **novagpt-agents** and **novagpt-browser:** optional bounded ChatGPT Web subagent management, manual-login handling, visible UI inspection and protected browser navigation.
- **novagpt-browser-driver:** optional Rust-native owner of the dedicated Chrome process/profile, exact-target background CDP commands, serialized W3C WebDriver setup/navigation, bounded DOM/input primitives, and crash invalidation. No Node.js, Patchright runtime or shared personal browser profile.

## Workspace and security model

One server can serve multiple simultaneous MCP clients on the same filesystem. MCP session IDs represent transport sessions, not ChatGPT conversations. Each operation uses the default configured root unless another explicitly approved workspace ID is supplied.

Canonical workspace roots and path guards reject disallowed paths and selected sensitive files. Optimistic SHA-256 changes are checked under per-file locks. Git index operations coordinate per canonical root. OS-level hostile path races still require further testing.

The MCP HTTP server binds to loopback and requires the shared user token. It accepts a bearer header or the private URL query parameter `novagpt_token`; the query secret is removed before MCP protocol handling. The token is private user state outside the repository. A previously issued query key is accepted temporarily for existing private connections, but is not emitted in new URLs.

When the enabled MCP server is running, the TUI can explicitly raise one owned subagent Chrome tab (or a separate blank browsing tab when no subagent is open). This user-initiated action reuses the server's browser handle and never navigates a worker tab. The manual-login Chrome flow still requires stopping MCP to avoid sharing an occupied browser profile.

The CLI/TUI can manage its own ngrok process and verify the matching upstream port and hostname before disclosing the HTTPS endpoint. The ngrok inspector and tunnel logs are disabled. The full connector URL is shown only on user request. Credentials and full URLs are not written to audit logs.

**Host-shell execution is not sandboxed.** A client with the full private connector URL can run commands with the server user's OS permissions. Explicitly allowed workspaces are not a sandbox around shell commands. Browser, VM and subagent features are opt-in.

## User experience and lifecycle

`novagpt` opens a four-page terminal UI: Overview, Activity, Settings and Chat. Guided `--setup` requires confirmation before it saves settings or creates a workspace-local launcher. The default UI language is Automatic, resolved from the system locale, with English fallback; all logs and diagnostics are in English.

The Chat page asks the model through the same owned `ChatGptBrowser` instead of an HTTP client: prompts are composed in the visible, authenticated Chrome tab `tui-chat` and replies are read back, so no cookie or token is handled by NovaGPT. That tab is opened at the account home with `open_home()`, outside the optional subagent project. Prompt and reply text stay in memory for the current run only; ChatGPT remains the owner of history.

Conversation URLs are recorded in a bounded, owner-private session index (`chat-sessions.json`, 0600 on Unix, atomic replace) containing only opaque ids, kinds, validated `https://chatgpt.com/c/<id>` URLs and timestamps. An adjacent `chat-sessions.lock` coordinates read-modify-replace updates across local processes to avoid lost concurrent writes; readers continue to use the atomic index snapshot without locking. Subagent conversations reach it through a narrow `AgentManager` observer that receives just `(agent id, URL)`. `Ctrl+O` lists the index; reopening a TUI chat continues that conversation in the chat page, while a subagent conversation opens in the existing browsing tab. A tampered or oversized index degrades to an empty list and is never used to navigate.

The optional ChatGPT project URL makes every new subagent tab start on that project page, so the group can be deleted in ChatGPT at once. Before the first prompt the adapter refuses to submit if the page is no longer on the project (`ProjectMismatch`); reopened conversations and TUI chat tabs intentionally skip that guard. NovaGPT never creates or deletes the project itself. See [ADR 0004](decisions/0004-browser-mediated-tui-chat.md).

The TUI Activity page can display up to 2,000 recent persisted audit events (H switches back to live events); E prompts for an owner-private ZSTD metadata export. Both actions are also in the advanced Overview menu. Export creates a new file without upload or automatic clipboard sharing. The CLI supports logs tail and logs export, using the same validated records.

The TUI initializes the shared user HTTP token automatically on launch using the existing private, concurrency-safe token resolver; it reuses an existing token and does not reveal its value. Startup token preparation is independent of the saved MCP auto-start preference.

The saved `auto_start_mcp` preference is disabled by default. If enabled, normal TUI launch requests MCP startup using saved HTTP and ngrok preferences; guided setup does not. Missing ngrok prompts for explicit installer consent, with No as the default answer. The installer uses only supported system package managers and does not handle ngrok account credentials.

Subagent activity is attributed only when an execution-specific credential authenticates a worker callback. Ordinary MCP calls are recorded in the global tool audit, but do not identify a worker: an MCP session is not a ChatGPT conversation. The worker prompt requests a separate authenticated `subagent_progress` callback after each ordinary tool call, while credentialed probe/patch/create requests are recorded directly in the bounded activity feed. This is cooperative reporting, not a guarantee that every ordinary tool call is attributed or that an absent heartbeat means the worker stopped. The ChatGPT browser reply deadline for long turns is five minutes; a timeout after possible submission remains **uncertain** and never causes an automatic resend.

Subagent lifecycle audit events are server-observed, use fixed English stage labels and optionally include only the opaque minted agent ID. Create-tab failures, unavailable, blocked, manual and uncertain browser replies, waiting states and completions are distinguishable in both the CLI and the support export without logging prompts, private browser content or error text. An uncertain reply stays waiting without automatic resubmission, because the prompt may already have been sent. The implementer capability preflight is a narrow exception to *waiting for textual completion*, not to safe send ordering: all three authenticated probe receipts plus four stable submitted-tab idle observations may mark the preflight verified after stopping and joining its response poller. Unreachable/busy tabs remain waiting; neither browser text nor an attempted send authorizes coding-task delivery. Events are emitted by a narrow observer installed in both TUI and standalone MCP server compositions.

The MCP audit keeps a 128-event in-memory fast view, while private disk history rotates plain-text active segments into Zstandard-compressed archives. The compressed history and active/legacy segments share a 100 MiB workspace budget with a 90-day archive age limit; audit data remains fixed-schema metadata only, never tool arguments, prompts, URLs or tokens. Legacy rotated raw log segments are not supported or imported. Archived events may be dropped after age or quota expiry, and a saturated bounded writer queue records a dropped-event counter.

Graceful HTTP shutdown handles Ctrl+C and SIGTERM on Unix and flushes audit events. CLI-owned ngrok is stopped and reaped on normal termination. Forced process termination, power failures and all in-flight requests are not guaranteed to be recoverable.

The native driver boundary follows [ADR 0003](decisions/0003-native-browser-driver.md); the preceding opt-in architecture remains described in [ADR 0002](decisions/0002-optional-vm-and-browser-agents.md). Live ChatGPT account compatibility is outside the disposable browser fixture's coverage.

### Chrome readiness, consent, and safe recovery

The ChatGPT adapter classifies a bounded page snapshot as **ready**, **navigation**, **authentication**, **verification**, or **unavailable**. Verification is recognized from a visible Cloudflare/Turnstile widget, a structural challenge marker (`window._cf_chl_opt`, the challenge running/form/stage/error elements, or a `cf-chl-widget-*` container), or a localized Cloudflare interstitial title; structural markers count even when a composer is already rendered, while a hidden widget alone does not. A missing composer is *not* proof of authentication. The adapter waits up to 20 seconds for an initially blank or delayed page, then reports a navigation timeout or an unavailable composer rather than inventing a login requirement. Bounded `page_state` and lifecycle diagnostics contain phase/flags only, not page titles, URLs, prompts, cookies or challenge tokens. Provider verification is performed manually in the visible tab; `subagent_resume` rechecks the same tab before any input, and a tab last seen on a challenge is never recreated automatically.

The Rust driver owns one dedicated, non-default Chrome user-data directory, guarded by an owner marker and an OS file lock. Chrome 136+ disallows remote debugging against the normal Chrome user-data directory (see [Chrome's security change](https://developer.chrome.com/blog/remote-debugging-port)); using a persistent **dedicated** profile is the intended supported configuration. Chrome is launched with an ephemeral loopback DevTools port; NovaGPT downloads and patch-installs a ChromeDriver matching the installed Chrome milestone, starts only its own driver child, waits for `/status.ready`, and creates a W3C WebDriver session through `debuggerAddress`. Legacy WebDriver tab operations are serialized under the session lock and can switch the active window, so the ChatGPT worker adapter avoids them during normal page observation and response polling. Instead, the driver resolves the exact ChromeDriver-created tab ID in the dedicated browser's loopback CDP target list, checks the exact page WebSocket URL, and dispatches direct per-target commands; mouse press/release and key down/up remain ordered on the same socket. Observation never requests activation. Immediately before composer input, however, the adapter raises that exact tab with `Page.bringToFront` and refuses to type unless the document reports `visibilityState === "visible"` and `hasFocus()`; a hidden or unfocused document cannot receive human input. No compositor focus restoration loop is used. `reconnect()` restarts the driver service and, if Chrome has exited, relaunches Chrome with the same dedicated profile; old tabs are invalidated and no navigation, input, or prompt is replayed. The ChatGPT adapter keeps a submission-attempt flag: only an **unsent** tab may be recreated/resumed, and a possible submission after a crash fails closed as ambiguous.

The separately enabled `BrowserBridge` can inspect visible text from explicitly approved HTTPS-origin tabs in an already running, user-managed CDP Chrome, but cannot navigate, authenticate or type. Chrome 144+ documents a per-connection user-consent remote-debugging flow; NovaGPT does **not** yet implement that attachment protocol for subagent control. Extending the read-only bridge to write-capable automation would require explicit tab selection, narrower authorization, lifecycle and origin checks, and a dedicated review of exposure of personal browser data. Chrome is launched with Blink's `AutomationControlled` feature disabled so that a remote-debugging browser does not report `navigator.webdriver`; that is a launch switch, not a JavaScript identity patch. No user-agent, WebGL, plugin or permission values are rewritten, and no CAPTCHA or challenge is solved automatically.

### Background worker focus policy

Visible Chrome is permitted one initial window activation when the browser launches or a new tab is created; NovaGPT does **not** make it always-on-top, request repeated maximization, or fight the compositor for focus. Routine ChatGPT worker DOM reads, response polls, status diagnostics and console reads use the owned Chrome's **exact matching per-page CDP WebSocket** in `novagpt-browser-driver::undetected::BackgroundTarget`. These operations never request `Page.bringToFront`, WebDriver `switch to window`, OS focus, or window-manager activation, which is what keeps six concurrent workers from repeatedly raising the browser.

Composer input is treated differently because a person cannot type into a hidden, unfocused document. Immediately before input, the adapter raises **that exact tab** with `Page.bringToFront`, then requires `document.visibilityState === "visible"` and `document.hasFocus()` within a bounded reveal window. If the compositor does not grant focus, the send aborts with `InteractionBlocked` and no keystroke, text insertion or click is dispatched. Short ASCII prompts are typed as individual key events with jittered delays (`Shift+Enter` for newlines); the pointer follows a short jittered path before the submit press; the page and the topmost element at the click point are re-checked immediately before submitting. Long or non-ASCII prompts are inserted as one trusted text event because Chrome exposes no user-permitted clipboard paste to an automation client. Production delays come from `InteractionPacing::human`; deterministic tests use `InteractionPacing::instant`.

The `BackgroundTarget` primitive itself still never activates unless asked, and the opt-in Linux **niri Wayland** smoke test continues to verify that repeated background reads and an explicitly commanded background input do not steal the foreground application. It exercises the driver primitive, not the adapter's input gate. X11 and other Wayland compositors require manual observation or a separately supported compositor API. It is not a live ChatGPT-account test, and a compositor that refuses focus makes the adapter report `InteractionBlocked` rather than fall back to hidden input.

### Managed ChromeDriver compatibility backend

`novagpt-browser-driver::undetected` reproduces the ChromeDriver lifecycle used by upstream undetected-chromedriver 3.5.5 without embedding Selenium or GPL source. It keeps an owned Chrome process, downloads and patch-installs a ChromeDriver matching the installed Chrome milestone into a versioned, digest-checked store, starts only its own ChromeDriver child, creates a W3C WebDriver session that attaches through `debuggerAddress`, serializes tab commands while switching to each worker's exact window handle, and reconnects by restarting the driver service/session or relaunching a crashed Chrome with the same profile. **This is now the ChatGPT adapter's transport.** The former native CDP process/WebSocket modules (`cdp.rs`, `lifecycle.rs`, `config.rs`, `error.rs`, root `profile.rs`) and the native `real_chrome` test were removed; the profile lease moved under `undetected/`. WebDriver remains the owner for session/tab creation and explicit manual browsing operations. **Worker page evaluation and physical input now use the driver-layer `BackgroundTarget`, an exact-target loopback DevTools WebSocket** (no separate browser process and no arbitrary MCP-exposed JavaScript); existing WebDriver CDP-execute remains available for explicit commands and test fixtures. This separation avoids repeated ChromeDriver `POST /window` activation during ordinary worker polling. Browser console capture uses ChromeDriver browser logs with URL redaction. The library and `novagpt chrome-driver status|install` expose driver management. See [the parity report](undetected-chromedriver-parity.md) for the source audit, licensing analysis, deliberate gaps (headless identity rewriting, detached processes, force-kill, and `--no-sandbox` default) and test evidence. There is no live authenticated ChatGPT coverage yet.

For local Rust iteration, `script/dev.py` builds once at startup and **never rebuilds or restarts automatically** on source changes or unexpected server exits. It creates an owner-private POSIX Unix socket, passes its path only to its owned MCP child, and accepts an explicit authenticated `debug_restart` MCP request, also reachable via `novagpt debug-restart` when the connected client tool list is stale. Both entrypoints are compiled only in POSIX debug builds (`cfg(all(debug_assertions, unix))`) and are not present in release builds. Even in debug builds, the supervisor must be running for the operation to be accepted. The supervisor acknowledges the request before rebuilding; a failed build preserves the existing process and a failed new process triggers a rollback attempt. Production and ordinary MCP processes cannot use this control, and the MCP server never attempts to restart itself. Each requested restart briefly interrupts MCP sessions and may change a dynamically assigned ngrok hostname.

## Known limitations

- The full live public ChatGPT-to-ngrok integration has not been independently verified on every supported platform.
- TUI shutdown currently aborts some async server tasks; complete graceful resource cleanup needs an end-to-end audit.
- Shell commands run with host permissions; subprocess-tree cleanup and full sandboxing are not guaranteed.
- VM guest control and some optional integrations require live testing. The managed ChromeDriver transport passed disposable Chrome 156 tests covering launch, patch, four-tab isolation, reconnect, Chrome crash/restart, profile persistence across application restart, screenshots, and the ChatGPT adapter's local browsing path. Live ChatGPT authentication, challenge handling, and response completion remain unverified; this is not a fully managed cloud environment.
- Adapter input requires a visible, focusable tab. A window manager that refuses focus produces `InteractionBlocked` (no prompt submitted) instead of hidden background input.
- The TUI chat requires the ChatGPT browser feature and a visible Chrome; its transcript is not restored after a restart, and the live login/Cloudflare/reply path is still unverified. Windows x64 Chrome 156 launch and the adapter's browsing path are now verified; macOS Chrome execution is untested. See the [managed backend notes](undetected-chromedriver-parity.md#11-windows-chrome-version-detection-2026-11-09).
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
