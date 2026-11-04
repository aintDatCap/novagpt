# NovaGPT capabilities and verification

This document tracks observable NovaGPT functionality and test coverage. Statuses apply to **NovaGPT's own contracts**, not to compatibility with any other product.

**Status definitions:** `IMPLEMENTED` means working Rust code and unit or integration tests exist; `PARTIAL` means some intended operations are missing or insufficiently validated; `PLANNED` means not yet delivered. No status guarantees production security or a successful live ChatGPT integration.

| Area | Current status | Evidence and open questions |
| --- | --- | --- |
| Multiple MCP clients, shared workspace | IMPLEMENTED / PARTIAL | Real two-client HTTP tests cover isolated sessions, shared files, CAS conflicts, explicit root IDs; restart, load and TTL need further testing |
| File reads, tree/search, guarded writes | IMPLEMENTED / PARTIAL | Unit and stdio/HTTP fixtures; cross-platform crash-atomicity and TOCTOU protection incomplete |
| Git status/diff, selected index and worktree operations | IMPLEMENTED / PARTIAL | Temporary Git tests including concurrency and denial; state ownership after crashes requires improvement |
| MCP stdio and Streamable HTTP | IMPLEMENTED / PARTIAL | Local POST/GET/SSE/DELETE, authorization, host validation, audit integration tests; full public client test needed |
| HTTP private connector URLs | IMPLEMENTED | URL builder uses `novagpt_token`, token authorization tests cover query and bearer credentials; keep URLs secret |
| ngrok CLI/TUI lifecycle | IMPLEMENTED / PARTIAL | Port ownership, public hostname matching, tunnel cleanup and mocked ngrok tests; live end-to-end public client behavior is environment dependent |
| TUI setup, language, settings and auto-start | IMPLEMENTED | UI-model tests cover default Automatic locale, English fallback, explicit save, opt-in normal startup, no startup during setup |
| ngrok installation consent | IMPLEMENTED / PARTIAL | TUI confirmation tests and deterministic package-manager selection; platform installers still need interactive OS tests |
| HTTP token management, private audit | IMPLEMENTED / PARTIAL | Token migration/reuse, redaction and bounded log tests; platform ACL and abrupt-exit guarantees incomplete |
| Browser and subagents | PARTIAL | The Rust-native `novagpt-browser-driver` passed 39 unit/mock tests, the ChatGPT adapter 15 tests, and the agent manager 18 tests (including four out-of-order workers, isolated cancellation, scoped patch callbacks, and unassigned new-file creation); MCP stdio registration and workspace tests pass. The ChatGPT adapter now uses the managed ChromeDriver/WebDriver transport for every tab; there is no parallel native CDP driver. A real Linux Chrome 156 + patched ChromeDriver 156 test passed four isolated tabs, driver reconnect, Chrome crash/relaunch, profile persistence across a separate application launch, screenshots and shutdown. A separate real-browser test passed the adapter's loopback navigation/read/type/click/screenshot/console/close path. Deterministic adapter tests cover Cloudflare `Just a moment...` recognition, navigation/authentication/verification/unavailable states, manual-verification resumption without challenge input, and a stale post-submission tab that refuses automatic replay. The unsent-only recovery wrapper is the sole automatic prompt retry. The separate user-consented external Chrome bridge remains read-only and independent. **Not verified:** actual ChatGPT account login/Cloudflare challenge behavior, live ChatGPT response completion, write-capable attachment to existing personal sessions, and other OS platforms. |
| Managed ChromeDriver compatibility | PARTIAL | `novagpt-browser-driver::undetected` independently reproduces the relevant undetected-chromedriver 3.5.5 (commit `757ed6a`) lifecycle: Chrome for Testing / legacy ChromeDriver resolution by Chrome milestone, bounded HTTPS download, optional SHA-256 pin, recorded digests, safe ZIP extraction, atomic versioned store, `{window.cdc…;}` binary patch with idempotence and foreign-patch refusal, owned Chrome launch, `prefs`/`exit_type` repair, owned ChromeDriver service, W3C session/tabs/elements/screenshots, ChromeDriver CDP passthrough for physical input, explicit reconnect (including Chrome relaunch after a crash), and shutdown. `novagpt chrome-driver status\|install` exposes install/verification without launching a browser. Deterministic tests cover version mismatch, traversal/symlink/archive/digest rejection, install-lock contention, service timeout/early exit, session expiry, four-worker tab isolation, atomic input, and profile ownership. Real Linux tests cover Chrome 156/ChromeDriver 156 install/patch, four tabs, reconnect, Chrome restart, profile persistence and the ChatGPT adapter's browsing path. **Not implemented/verified:** headless identity rewriting, detached browser mode, force-kill, `--no-sandbox` default, write-capable attach to an existing user Chrome, live ChatGPT login/verification/completion, live ChromeDriver ≤114, macOS/Windows execution. GPL-3.0 source was not copied or translated; the parity matrix and licensing analysis are in `docs/undetected-chromedriver-parity.md`. |

| Authenticated implementer writes | IMPLEMENTED / PARTIAL | `subagent_apply_patch` applies an update to assigned existing files (or files created in the same run); `subagent_create_file` creates UTF-8 files at arbitrary safe paths within the agent's approved workspace, with automatic parent directory creation and exclusive no-overwrite behavior, even with no selected paths. Both require its current private execution credential and return verified receipts. Unit tests cover real writes, nested paths, symlink/blocked paths, conflicts, role/credential denial and multi-file edits by separate calls; MCP stdio registration and disabled-capability denial are tested. No atomic multi-file transactions, file deletion/renaming, worktree isolation, live ChatGPT callback test or complete filesystem-race guarantees. |
| VM images and host controls | PARTIAL | Guarded image operations; real Hyper-V/QEMU deployment and guest APIs need platform validation |
| Release packaging | PARTIAL | Installer and CI workflows exist; every platform still needs a published-release installation and upgrade smoke test |

## Product invariants

1. One server supports multiple simultaneous ChatGPT conversations operating on the same explicitly approved real filesystem.
2. An MCP session, IP address or token is not a ChatGPT conversation identity.
3. If `workspace_id` is omitted, the configured default root is used; other roots must be explicitly permitted.
4. Stale writers are rejected when their `expected_sha256` no longer matches.
5. Optional VM, browser and subagents are not prerequisites for ordinary file and Git operations.
6. Logs and documentation are in English; persistent diagnostics do not include credentials.
7. Connector URLs and shell access require clear user consent and warnings about host permissions.

## Verification process

For Rust changes, run `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, and `cargo test --workspace`. For the POSIX development watcher, also run `python3 -m unittest discover -s script -p 'test_dev.py'`.

When making a claim about a feature, record the executable test command, operating system, outcome and limits. Successful compilation alone is not validation of user-visible behavior. Consult [QUALITY_GATES.md](QUALITY_GATES.md) and [ARCHITECTURE.md](ARCHITECTURE.md) for security and release criteria.
