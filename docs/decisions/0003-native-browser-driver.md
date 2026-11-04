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

**Verified:** `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, `cargo test --workspace` (one opt-in Chrome test ignored by default) and a separately executed `cargo test -p novagpt-browser-driver --features test-support --test real_chrome -- --ignored --nocapture`. The live test passed against a disposable loopback HTML fixture on Linux, Chrome 156.0.8078.47, with four tabs, text input, clicks, PNG screenshots, isolated close, owned profile contention and simulated process crash/restart. A separate fake backend validates four out-of-order agent results with one isolated cancellation.

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
