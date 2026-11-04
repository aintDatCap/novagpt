# ADR 0003 — Rust-native persistent browser driver

**Date:** 2026-11-04
**Status:** Accepted; Rust driver and adapter migration implemented. Real ChatGPT verification pending.

## Context and evidence

NovaGPT's `novagpt-browser` currently combines Chrome process ownership, a
dedicated profile lease, the CDP websocket transport, generic DOM/input
primitives, and ChatGPT-specific selectors and response polling. Its
`novagpt-agents` backend delegates each agent to a separate logical page.
The current CDP launch uses a visible Chrome window with a dedicated
`--user-data-dir`, `--remote-debugging-port=0`, and `about:blank`.
Target creation calls `/json/new?https://chatgpt.com/`, then waits for a
visible composer before writing via `Input.insertText`.

Read-only comparison with `aintDatCap/codexpro` at its public `main`
branch (files `src/chatgptBrowserManager.ts`,
`src/chatgptBrowserBackend.ts`, `src/agentManager.ts`) found:

| Concern | CodexPro | NovaGPT before this ADR |
| --- | --- | --- |
| Engine | `patchright-core` `chromium.launchPersistentContext(profile, { executablePath, headless: false, viewport: null })` | Direct CDP websocket + HTTP target endpoints |
| Page initialization | `context.newPage()` followed by `goto(..., waitUntil: "domcontentloaded")` and composer check | Target created with ChatGPT URL, then ready-state polling |
| Startup | Shared in-flight start promise; lifecycle epoch checks | Async startup mutex; process and CDP endpoint health checks |
| Page concurrency | Serialized creation with interval, parallel response work | Serialized creation with interval, parallel tab send |
| Context | Optional ChatGPT project and explicit fresh-conversation preparation | New tab at ChatGPT home; no project auto-create |
| Source modifications | Worktree and Git patch validation | Proposed diffs only; guarded parent application |

CodexPro uses Node.js and `patchright-core`; its manifest declares an
MIT license. Patchright's public repository advertises Apache-2.0 licensing.
We do not copy Patchright implementation or bundled browser patches. Its
Playwright-like operations (`launchPersistentContext`, `newPage`,
`goto`, `locator`, `click`, `fill`, `press`, `waitFor`,
`evaluate`, `screenshot`, `close`) are generic behaviors; its
browser-protocol adjustments are not evidence of any bypass guarantee.

**Hypotheses, not verified causes:** distinct launch flags, target creation
order, navigation readiness semantics, browser version and profile state
may affect session compatibility. Cloudflare behavior has not been
experimentally compared on a live account. No bypass, cookie transfer,
CAPTCHA solving, or stealth/fingerprint changes will be implemented.

## Decision

Add `novagpt-browser-driver` as an optional, Rust-native, Tokio-based
browser owner and CDP automation library. Separate it from ChatGPT semantics:

- `profile`: reuse the existing NovaGPT-owned marker and cross-process OS
  lease; reject arbitrary populated or redirected profiles.
- `lifecycle`: own one visible Chrome process, health-checked local CDP
  endpoint, startup/restart mutex, target creation mutex, epoch invalidation,
  and explicit shutdown.
- `cdp`: typed errors, bounded request/reply transport, page handles, CDP
  commands and selected DOM/input/screenshot/wait primitives.
- `novagpt-browser`: keep ChatGPT selectors, challenge detection, manual
  login workflow, conversation polling, the public browsing safety guard
  and MCP-facing data shapes. Migrate process and page transport ownership
  instead of running two overlapping engines.
- `novagpt-agents`: retain distinct IDs, per-agent state, independently
  cancellable workers and protected Git workflow. No MCP schema changes.

Only trusted internal Rust call sites may supply evaluation expressions.
Never accept scripts from an MCP request or surface raw CDP errors, headers,
URLs, prompt text, DOM dumps or credentials in diagnostics. Browser profile
data stays on the user's device. The separately approved read-only
`BrowserBridge` remains isolated because it attaches to another
user-managed Chrome endpoint with a different trust model.

## Invariants, failure and tests

- A process/profile pair is owned by one driver; Chrome must not be
  force-killed unless it was spawned by that driver.
- Serialize startup and target creation, **not** unrelated per-page work.
- Each page tracks its own CDP request IDs and task state; failure, timeout,
  cancellation and out-of-order completion must not cross page boundaries.
- Closing a page does not close Chrome or other pages. A stale endpoint
  invalidates all page handles; reconnect by opening new pages, never
  silently replay a prompt after a crash.
- Timeouts are finite. Check profile leases and release resources on
  launch failure, shutdown and driver drop.
- Existing browser and MCP contracts remain functional during migration.
- Unit/mock CDP tests cover commands, page state, cancellation, profile
  contention, idempotent cleanup, four concurrent pages and response
  association; real Chrome tests are required before declaring browser
  compatibility. A real ChatGPT session requires explicit authorized login.

## Rejected alternatives

- Wrapping or bundling Node.js, Patchright or a Playwright runtime.
- Duplicating the current CDP implementation indefinitely.
- Making browser support mandatory for core filesystem/Git tools.
- Switching subagents to Codex CLI or an OpenAI API backend.
- Automating manual verification, disabling browser security or sharing
  another application's profile.
- Automatically applying subagent edits to the shared Git tree.

## Implementation evidence (2026-11-04)

The migration now uses `novagpt-browser-driver` for the single owned Chrome process, profile lease, CDP sessions and native tab actions. `novagpt-browser` retains ChatGPT-specific policy, manual-authentication detection, safe browsing origins, mock UI tests and existing MCP-visible types; `novagpt-agents` retains task state and bounded worker coordination. One short-lived foreground input lock serializes `Page.bringToFront` and native input sequences, not response waits. This was required by a real Chrome finding: hidden tabs accepted CDP requests but did not dispatch physical mouse clicks. Recovery also copies the current CDP port out of its mutex before invalidation to avoid a deadlock after process exit.

**Verified at that stage (historical; the CDP test target was later removed):** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, `cargo test --workspace` and a separately executed `cargo test -p novagpt-browser-driver --features test-support --test real_chrome -- --ignored --nocapture`. The live test passed against a disposable loopback HTML fixture on Linux, Chrome 156.0.8078.47, with four tabs, text input, clicks, PNG screenshots, isolated close, owned profile contention and simulated process crash/restart. A separate fake backend validates four out-of-order agent results with one isolated cancellation.

**Not verified:** live ChatGPT account sign-in, Cloudflare behavior, real ChatGPT completion and session retention across independent server processes, Windows/macOS Chrome execution or guarantees against provider detection. No Node.js or Patchright runtime is embedded; the existing `fs2`, Tokio, reqwest, tungstenite and JSON dependencies are reused. The authorized browsing and MCP tool contract is unchanged.

## Migration and acceptance

Migration stages implemented: ADR; crate API; process/profile lifecycle; page
primitives; replacement of duplicate CDP plumbing in `novagpt-browser`;
agent concurrency regression tests; disposable real-Chrome smoke and workspace/MCP
regressions; documentation. Further live ChatGPT account and cross-platform checks
are outstanding.
Preserve manual login, a single shared browser, existing MCP method names,
and the independent review/approval workflow. If the real Chrome binary,
account or service is unavailable, record an explicitly untested gate;
compilation alone is not a Cloudflare or ChatGPT success claim.

## Follow-up: explicit readiness phases and conservative recovery (2026-11-04)

An observed Cloudflare `Just a moment...` page was misreported as authentication because a full-page interstitial lacked visible challenge selectors and the old classifier treated every missing composer as a login requirement. The adapter now prioritizes a recognized verification title or visible challenge, checks explicit authentication indications, distinguishes navigation from an unavailable composer, and waits a bounded interval for delayed rendering. The title is evaluated in memory and is never forwarded to event logs. The page stays unchanged until a human completes any provider-required verification; only then can the agent resume its retained prompt. Tests use synthetic CDP responses, never real challenge interaction.

Driver health is retried briefly before invalidation, and startup waits for the live CDP endpoint. Each tab records whether a send event could have been delivered. After a Chrome epoch change, an unsent tab may be recreated in the same dedicated profile; a tab with ambiguous post-submission state fails closed instead of replaying a prompt. The existing read-only, origin-limited external Chrome bridge is retained. Write-capable attachment to an existing user-authorized Chrome browser is a separate permission/ownership design, not a substitute for the dedicated profile. Chrome 144+ user-consented remote-debugging APIs and Chrome 136+ profile restrictions are documented in [Chrome DevTools MCP session attachment](https://developer.chrome.com/blog/chrome-devtools-mcp-debug-your-browser-session) and [Chrome remote-debugging changes](https://developer.chrome.com/blog/remote-debugging-port).

No new dependencies, stealth mechanisms or CDP command capabilities are introduced. Fixture tests confirm classifier transitions, manual resumption without challenge input, and refusal to replay an already submitted prompt. A disposable real-Chrome test and end-to-end authorized ChatGPT sign-in remain separate acceptance gates.

## Upstream undetected-chromedriver source review and lifecycle hardening (2026-11-04)

**Primary sources reviewed:** [upstream README](https://github.com/ultrafunkamsterdam/undetected-chromedriver), [Chrome wrapper](https://github.com/ultrafunkamsterdam/undetected-chromedriver/blob/master/undetected_chromedriver/__init__.py), [patcher](https://github.com/ultrafunkamsterdam/undetected-chromedriver/blob/master/undetected_chromedriver/patcher.py), [options](https://github.com/ultrafunkamsterdam/undetected-chromedriver/blob/master/undetected_chromedriver/options.py), and [package metadata](https://github.com/ultrafunkamsterdam/undetected-chromedriver/blob/master/setup.py). The upstream distribution identifies itself as GPL-3.0.

The source demonstrates a **Selenium/WebDriver** wrapper: a patcher downloads and modifies *ChromeDriver* (not necessarily the Chrome executable), while the Chrome wrapper manages browser/profile paths, a separate driver service, CDP events and WebDriver session restart. Its `reconnect()` stops/restarts the ChromeDriver service and creates a new WebDriver session. At that time NovaGPT had no ChromeDriver service and owned per-tab CDP WebSockets directly; the replacement section below changes that architecture. Upstream claims about bot detection do not establish reliable access to an individual authenticated provider.

**Implemented independently at that stage (historical; superseded by the replacement section):** `/json/version` health was bounded and verified a Chrome-family browser identifier plus an exact local browser WebSocket URI; a live owned process was not force-killed merely because health checks failed; a disconnected tab socket could reattach to the same Chrome target while retaining its DOM and profile. The ChatGPT adapter retried only its read-only state probe after that transport reconnection. Non-idempotent input, navigation and prompt submission were never replayed on a connection error. The dedicated-profile lock, process crash epochs, and four-tab concurrency remained in place.

**Verification at that stage (historical):** deterministic tests covered wrong-endpoint responses and live-child preservation. The opt-in Linux real-Chrome test disconnected a tab WebSocket and reattached to the same target. No user profile, cookie, password or provider challenge was accessed.

**At the time of that review, not implemented:** automatic download of a ChromeDriver binary, Selenium/W3C session management, driver binary patching, full Chrome 144 user-consented write-capable attach, unattended provider verification or guaranteed ChatGPT login. The next section records the subsequent managed ChromeDriver implementation and supersedes those items.


## Managed ChromeDriver compatibility backend (2026-11-04)

**Reference:** upstream commit `757ed6a22052f0674bb7c9f765ca884d15dcb780`, package `3.5.5`, GPL-3.0. Full audit and the row-by-row parity matrix are in [undetected-chromedriver-parity.md](../undetected-chromedriver-parity.md).

**Decision (superseded in part by the replacement section below):** add a native Rust ChromeDriver backend in `novagpt-browser-driver/src/undetected/`. It reproduces the upstream *lifecycle*: platform/version selection, Chrome for Testing or legacy ChromeDriver download, safe extraction, patched-driver installation, owned Chrome launch, preferences and `exit_type` repair, owned ChromeDriver service, W3C session creation, isolated tabs, and `reconnect()` by stopping/restarting only ChromeDriver and creating a new session against the same Chrome/profile. The default ChatGPT conversation transport remains CDP because it gives NovaGPT per-tab ownership and the unsent-only recovery policy described above.

**Independent implementation under GPL-3.0:** no upstream source, comments, strings or regexes were copied or translated. The binary patch uses NovaGPT's own marker and replacement, refuses binaries patched by another implementation, and does not adopt upstream's marker. Download integrity is strengthened with bounded responses, host allowlists, no redirects, an optional SHA-256 pin, recorded archive/binary digests, exact-version checks, and atomic installation. CfT publishes no per-archive checksum sidecars; this limitation is recorded rather than hidden. Legacy ≤114 storage and modern CfT layouts are both handled.

**Deliberate conflicts with exact upstream behavior, documented rather than silently substituted:**

- `--no-sandbox` is opt-in only (upstream defaults it to true); keeping the Chrome sandbox is a security requirement.
- Detached unowned Chrome is not supported; NovaGPT kills only processes it spawned.
- `force_kill_instances` is not ported; it can kill unrelated browser automation.
- A temporary profile is never created implicitly; only a persistent, NovaGPT-owned dedicated profile is accepted.
- Headless identity rewriting (`navigator.webdriver`, user agent, `window.chrome`, permissions/`toString`) is not implemented. It falsifies browser identity, conflicts with the visible human-verification workflow, and cannot be validated without provider interaction. Headless mode is available but no stealth guarantee is made.
- The Reactor is replaced by bounded pull-based performance-log draining; `click_safe` and hidden reconnect-after-click are replaced by explicit `reconnect()`.
- `find_elements_recursive` returns a bounded `Vec` (with documented staleness) instead of a Python generator.

**Verification:** `cargo test --workspace` passed 201 tests with 2 opt-in real-browser tests ignored. The managed backend adds deterministic tests for version parsing/mismatch, download/traversal/symlink/size rejection, missing or wrong checksums, patch idempotence and foreign-patch refusal, profile ownership and locking, install-lock contention, service startup/timeout/early-exit, session expiry, tab isolation under four concurrent workers, and element/frame operations. A disposable Linux real-browser test passed against Chrome 156.0.8078.47 and patched ChromeDriver 156.0.8078.80: download/reuse, launch, local fixture input/click, PNG screenshot, four isolated tabs, reconnect with surviving tabs and cookie, tab close, shutdown, and a separate application launch that retained profile data. No account, personal profile, or remote provider was accessed. The CLI `novagpt chrome-driver status|install` was also exercised.

**Remaining limitations at that stage:** macOS/Windows runtime execution, live ChromeDriver ≤114 download, Chrome 144+ consent-based attach, and live ChatGPT login/Cloudflare/conversation completion were not verified or not implemented. The backend was not yet the ChatGPT transport.


## Replacement of the native CDP driver (2026-11-04, final)

**Decision:** the ChromeDriver/WebDriver backend is now the only browser transport in NovaGPT. The old native CDP process/socket implementation was removed rather than retained as a parallel path:

- Deleted `novagpt-browser-driver/src/{cdp,config,error,lifecycle,profile}.rs` and `tests/real_chrome.rs`.
- Moved the dedicated-profile lease into `undetected/profile.rs` with typed profile errors.
- `ChatGptBrowser` now lazily launches `UndetectedChrome`, creates one WebDriver tab per worker, and keeps its existing classifier, manual-verification policy, reply polling, and unsent-only replay guard on top of that transport.
- `Page` operations use native WebDriver script execution plus ChromeDriver's documented `goog/cdp/execute` passthrough for physical input, emulation, and screenshots. There is no second DevTools WebSocket, no raw CDP target manager, and no separate Chrome process owner.
- `reconnect()` stops the driver service, invalidates all old tabs, starts a new service/session, and relaunches Chrome with the same profile if the process exited. The adapter retries an **unsent** prompt once after recovery; if `submission_attempted` is set, it returns `AmbiguousSubmission` and never replays.
- Input sequences are atomic at the tab level (`click_at`, `press_enter`), so four workers cannot interleave a press/release or key-down/up.
- Browser console capture uses ChromeDriver browser logs; URLs are redacted and entries are bounded.
- `novagpt chrome-driver status|install` remains the explicit driver management interface.

**Verification:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, and `cargo test --workspace` pass with **201 passed, 0 failed, 2 ignored**. The opt-in real tests also pass:

- `cargo test -p novagpt-browser-driver --features test-support --test real_chromedriver -- --ignored --nocapture` — Chrome 156 launch, patch, CDP execute, four isolated tabs, driver reconnect, Chrome crash/relaunch, profile persistence across a separate application launch, screenshot and shutdown.
- `cargo test -p novagpt-browser --test real_browser -- --ignored --nocapture` — the ChatGPT adapter's browsing transport against real Chrome/ChromeDriver and a loopback fixture, including navigation/read/type/click/screenshot/console/close.

**Remaining limitations:** live ChatGPT account login, Cloudflare/manual verification, and live response completion are still not verified; macOS/Windows, live ChromeDriver ≤114, Chrome 144+ consent-based attach, and the headless identity-evasion layer remain unimplemented or untested. ChromeDriver must support `goog/cdp/execute` for the adapter's physical-input/screenshot commands; this is tested on ChromeDriver 156 and documented in the parity report.
