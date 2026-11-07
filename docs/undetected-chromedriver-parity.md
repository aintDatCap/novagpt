# undetected-chromedriver source audit and Rust parity

**Reference inspected:** `ultrafunkamsterdam/undetected-chromedriver` commit
[`757ed6a22052f0674bb7c9f765ca884d15dcb780`](https://github.com/ultrafunkamsterdam/undetected-chromedriver/commit/757ed6a22052f0674bb7c9f765ca884d15dcb780)
(2025-07-05), Python package version `3.5.5`. The shallow clone's `master`
tip identifies itself as `__version__ = "3.5.5"` in
`undetected_chromedriver/__init__.py`; there are no GitHub releases or tags in
the repository, so the commit hash is the reference. PyPI reports `3.5.5` as
the latest published version, uploaded 2024-02-17 (the repository commit is
later, 2025-07-05, and still declares 3.5.5).

**Upstream license:** GPL-3.0 (`LICENSE`, and `license="GPL-3.0"` in
`setup.py`). NovaGPT is `MIT OR Apache-2.0`. See
[licensing analysis](#7-licensing-analysis); no GPL source was copied or
translated.

**Research date:** 2026-11-04, Europe/Rome.

**Reference environment used for real tests:**

| Component | Version |
| --- | --- |
| Google Chrome (installed, stable channel) | `156.0.8078.47` |
| ChromeDriver (Chrome for Testing, installed and patched by NovaGPT) | `156.0.8078.80` |
| Platform | Linux x86_64, Wayland/X11 desktop session |
| Chrome for Testing Stable at research time | `157.0.8103.38` |
| Chrome for Testing Beta / Dev / Canary | `158.0.8127.2` / `159.0.8130.0` / `159.0.8142.0` |
| Rust | `rustc 1.98.1` |

Sources: the Chrome for Testing availability page and its JSON feeds
(`last-known-good-versions-with-downloads.json`,
`latest-versions-per-milestone-with-downloads.json`, fetched 2026-11-04),
the [Chrome 136 remote-debugging change](https://developer.chrome.com/blog/remote-debugging-port),
the W3C WebDriver and DevTools protocols, and the upstream files listed below.

## 1. What upstream actually does

The package is a Selenium WebDriver wrapper. It is not a CDP automation
library. Its important mechanisms are:

| Upstream file | Observed behavior |
| --- | --- |
| `__init__.py` (`Chrome`) | Builds `ChromeOptions`; selects/creates a user-data directory; sets `--remote-debugging-host/port` and `--user-data-dir`; appends locale, window, log-level, `--no-sandbox`/`--test-type`, welcome-suppression and headless arguments; writes `prefs`; repairs `profile.exit_type`; launches **Chrome** itself (optionally detached); starts a Selenium `ChromiumService` around a patched **ChromeDriver**; creates a WebDriver session; exposes `get`, `tab_new`, `window_new`, `reconnect`, `quit`, CDP event listeners and recursive element search. |
| `__init__.py` (`_configure_headless`) | After the first navigation, injects scripts that redefine `navigator.webdriver`, strip `Headless` from the user agent, synthesize `window.chrome`, change touch/network values, and wrap `Function.prototype.toString`/permissions. This is headless-only identity evasion. |
| `patcher.py` (`Patcher`) | Chooses a per-user data directory and platform name; resolves a ChromeDriver release (stable, a requested milestone, or legacy ≤114); downloads a ZIP; extracts it; rewrites the injected `{window.cdc…;}` JavaScript block to a same-length inert `console.log` expression; detects its own patch by the literal `undetected chromedriver`; can force-kill running drivers; removes its binary at object destruction. |
| `options.py` (`ChromeOptions`) | Selenium Chromium options plus a `user_data_dir` property, dotted-preference "undot" and deep merge, and `handle_prefs` writing `Default/Preferences`. |
| `devtool.py` (`Structure`, `timeout`) | Dict-with-attribute-access helper and a thread-timer decorator used by examples/test code; not part of the WebDriver lifecycle. |
| `reactor.py` (`Reactor`) | Background thread polling Selenium `performance` logs every second and dispatching CDP messages to listeners, with wildcard support. |
| `cdp.py` (`CDP`, `CDPObject`, `PageElement`) | Thin `/json` HTTP helpers and a one-command-per-WebSocket CDP sender. |
| `webelement.py` (`WebElement`, `UCWebElement`) | `click_safe` (click then reconnect), `children(tag, recursive)`, cached `attrs`, and a compact `repr`. |
| `dprocess.py` (`start_detached`) | Launches Chrome in a detached session and registers the PID for `atexit` SIGTERM. |

Dependencies declared by `setup.py`: `selenium>=4.18.1`, `requests>=2.31.0`,
`websockets>=12.0`, `packaging>=23.0`. Chrome support is version-driven:
legacy Google Storage for ChromeDriver ≤114, Chrome for Testing for ≥115.
Upstream's own README notes results vary, says it does not hide IP addresses,
and gives no bypass guarantee.

## 2. Rust implementation map

All managed-driver code lives in the existing optional
`novagpt-browser-driver` crate under `undetected/`; the CLI exposes it as
`novagpt chrome-driver`. **It is the only browser transport:** the former
native CDP process/WebSocket modules were deleted and the ChatGPT adapter now
runs on managed WebDriver tabs. No Python, Node.js, Selenium, or upstream
source is embedded.

| Rust location | Responsibility |
| --- | --- |
| `undetected/platform.rs` | Strict four-part versions, milestone compatibility, CfT/legacy platform names, archive entry names. |
| `undetected/manifest.rs` | Bounded parsing of CfT stable/milestone JSON and legacy `LATEST_RELEASE_*`; URL allowlists. |
| `undetected/install.rs` | Owned, versioned ChromeDriver store; install lock; HTTPS bounded fetch; SHA-256 verification/pinning; safe ZIP extraction; exact-version check; atomic install; verified reuse; legacy layout. |
| `undetected/patch.rs` | Bounded `{window.cdc…;}` block discovery, same-length inert replacement, idempotence, foreign-patch refusal, binary version scan, atomic rewrite. |
| `undetected/options.rs` | Bounded options, dedicated-profile enforcement, locale, launch arguments, dotted `prefs` merge, `exit_type` repair. |
| `undetected/chrome.rs` | Owned visible Chrome launch, ephemeral loopback DevTools port, `/json/version` identity validation, version detection, termination. |
| `undetected/service.rs` | Owned ChromeDriver child, loopback port, `/status.ready` wait, liveness recheck, shutdown. |
| `undetected/webdriver.rs` | Bounded W3C client, session, per-session command serialization, isolated tabs, elements, screenshots, atomic click/Enter input, CDP-execute passthrough, browser/performance logs, recursive frame search, typed errors. |
| `undetected/managed.rs` | `UndetectedChrome`: install/launch/session/reconnect/shutdown, 4-tab support, test-only crash hooks. |
| `crates/novagpt-cli/src/driver.rs` | `chrome-driver status\|install` with redacted human/JSON output. |
| `undetected/profile.rs` | Dedicated-profile owner marker and OS lease used by the managed backend. |
| `crates/novagpt-browser/src/lib.rs` | Re-exports `managed`; the ChatGPT adapter now uses WebDriver tabs, native WebDriver script execution, and ChromeDriver's `goog/cdp/execute` passthrough for physical input/screenshot commands. |

## 3. Source-to-implementation parity matrix

Status values: **parity** = behavior reproduced; **parity, different
mechanism** = same observable contract with a Rust/security-appropriate
mechanism; **not implemented** = consciously omitted, with reason.

| Upstream source | Original behavior | Rust location | Difference | Automated test | Status |
| --- | --- | --- | --- | --- | --- |
| `__init__.py` `find_chrome_executable` | Search PATH and standard install paths; check executable bit. | `undetected::discover_chrome`, `cli::chrome::discover` | Same candidates plus stable/Chromium; no Brave. | `chrome.rs`, real tests | parity |
| `__init__.py` constructor: options / binary location | Build Selenium options, set `binary_location`. | `ChromeOptions`, `ManagedConfig`, `managed::launch` | Typed Rust; explicit paths. | `options.rs`, `managed.rs` tests | parity |
| Constructor: debug host/port | Pick free port; set `debuggerAddress`. | `chrome.rs` + `ChromeOptions::launch_arguments` + `create_session` | Chrome uses an **ephemeral** port (`0`) and NovaGPT reads `DevToolsActivePort`; avoids free-port race. | `real_chromedriver.rs` | parity, different mechanism |
| Constructor: user-data-dir | Use supplied profile, extract from args, or create a temp directory. | `managed::launch`, `undetected::profile::acquire` | Only an absolute, NovaGPT-owned dedicated profile; never a temp/default/personal profile. | `undetected/profile.rs`, `managed.rs`, `real_chromedriver.rs` | parity, intentional security difference |
| Constructor: language | Derive locale, fallback `en-US`. | `options::detected_language` | Same environment precedence, sanitized; passed as both `--lang` (UI) and `--accept-lang` (web locale), because `--lang` alone does not change `navigator.languages`/`Accept-Language` on Linux. | `options.rs` | parity plus locale consistency |
| Constructor: welcome suppression | `--no-first-run`, `--no-default-browser-check`. | `ChromeOptions::launch_arguments` | Same. | `options.rs` | parity |
| Constructor: `no_sandbox=True` default | Always adds `--no-sandbox`, `--test-type`. | `ChromeOptions.no_sandbox` | **Opt-in only**, Linux-only. Default sandbox remains enabled. | `options.rs` | parity, intentional security difference |
| Constructor: headless | `--headless=new` for ≥108. | `ChromeOptions.headless` | Same flag. | `options.rs` | parity |
| Constructor: window/log flags | `--window-size`, `--start-maximized`, `--log-level`. | `options.rs` | Same. | `options.rs` | parity |
| Constructor: `prefs` | Undot keys, deep-merge `Default/Preferences`. | `ChromeOptions::handle_prefs` | Same merge semantics; atomic write; malformed file is refused rather than overwritten. | `options.rs` | parity, safer failure |
| Constructor: `exit_type` repair | Set `profile.exit_type = null`. | `ChromeOptions::fix_exit_type` | Same; atomic; absent file is a no-op. | `options.rs` | parity |
| Constructor: launch Chrome | `Popen` (optionally detached). | `OwnedChrome::launch` | Direct owned child with `kill_on_drop`; no detach. | `managed.rs`, real tests | parity, deliberate ownership difference |
| `dprocess.start_detached` | Orphan-capable detached browser + atexit kill. | — | **Not implemented.** NovaGPT must not leave an unowned Chrome process; it may not kill processes it did not spawn. | `managed.rs` duplicate-profile test | not implemented (ownership) |
| `Chrome.quit` | Kill driver, stop reactor, SIGTERM Chrome, delete only temp profile, drop patcher. | `UndetectedChrome::shutdown` | Closes session, stops owned driver, terminates owned Chrome; never deletes a persistent profile. | real tests; `managed.rs` | parity, intentional profile-retention difference |
| `Chrome.__del__` / finalize / `_ensure_close` | Best-effort driver kill on GC. | `kill_on_drop` + explicit `shutdown` | Rust destructors cannot await; explicit shutdown is required for a clean session. | real tests | parity, different mechanism |
| `Chrome.reconnect` | Stop service, sleep, start service, new session. | `UndetectedChrome::reconnect` | Same sequence; invalidates old session/tabs. If Chrome itself exited, relaunches Chrome with the same profile. Never replays commands. | `real_chromedriver.rs` | parity plus crash extension |
| `Chrome.start_session` | Recreate capabilities and call Selenium. | `WebDriverSession::create` + `create_session` | Bounded W3C POST, timeouts. | `webdriver.rs`, real test | parity |
| `Chrome.tab_new` / `window_new` | CDP/WebDriver new tab. | `UndetectedChrome::new_tab`, `WebDriverSession::new_tab` | WebDriver `window/new`, then explicit navigation. | `webdriver.rs` four-worker test, real test | parity |
| `Chrome.add_cdp_listener` / `clear_cdp_listeners` | Background Reactor thread; wildcard callbacks. | `WebDriverSession::performance_log`, `drain_cdp_events`, `browser_log` | Pull-based; caller drains bounded events. No background thread or wildcard registry. Browser console uses bounded, URL-redacted ChromeDriver logs. | `webdriver.rs`, `novagpt-browser` console test | parity, different mechanism |
| `Chrome.find_elements_recursive` | Generator over main frame and iframes, restoring the default frame. | `WebDriverTab::find_elements_recursive` | Returns a bounded `Vec`; documented staleness after frame changes. | `webdriver.rs` | parity, API difference |
| `keep_alive` | Selenium remote connection keep-alive. | `WebDriverClient` reqwest pool | HTTP connection reuse is the client default; no toggle. | real test | parity, different mechanism |
| `suppress_welcome` / `log_level` | Launch flags. | `ChromeOptions::launch_arguments` | Fixed `--no-first-run`, `--no-default-browser-check`, `--log-level=0`; welcome suppression is always on. | `options.rs` | parity, fixed policy |
| Browser automation flag | Upstream relies on the patched CDriver injection and, in headless mode, rewrites `navigator.webdriver` from JavaScript. | `ChromeOptions::launch_arguments` | Chrome is started with `--disable-blink-features=AutomationControlled`. Current Chrome sets Blink's automation feature for any remote-debugging launch, so the `{window.cdc…}` patch alone leaves `navigator.webdriver === true`; the launch switch clears it without patching JavaScript. | `options.rs` | parity, launch-time mechanism |
| `advanced_elements` | Select `UCWebElement` with `attrs`. | `WebElement::attrs` | Attribute snapshot available on every element. | `webdriver.rs` | parity |
| `Chrome.get` | Selenium navigation. | `WebDriverTab::goto` | Restricts to http(s)/`about:blank`, no credentials/controls. | `webdriver.rs` | parity, safer URL policy |
| `Chrome.debug` method wrapper | Logs every method call/args. | — | **Not implemented**, to avoid leaking prompts, URLs and DOM data. | — | not implemented (security) |
| `_configure_headless` | Rewrites `navigator.webdriver`, UA, `window.chrome`, permissions/toString. | — | **Not implemented.** Falsifying identity is outside NovaGPT's visible, user-verified flow; no stealth guarantee is claimed. | — | not implemented (security/ethics) |
| `WebElement.click_safe` | Click then reconnect. | `WebElement::click`; `UndetectedChrome::reconnect` | No hidden reconnect after every click; caller reconnects explicitly. | real test (`reconnect`) | parity, explicit API difference |
| `WebElement.children(tag, recursive)` | Direct/recursive children via script. | `WebElement::children` | Same semantics, bounded, typed. | `webdriver.rs` | parity |
| `UCWebElement.attrs` | Snapshot attribute map. | `WebElement::attrs` | Same one-shot snapshot; no stale caching. | `webdriver.rs` | parity |
| `WebElement` text/tag/click/type | Standard WebDriver element operations. | `WebElement::{text, tag_name, click, send_keys, attribute}` | Bounded; W3C element IDs accepted. | real test; WebDriver tests | parity |
| `options.user_data_dir` property | Absolute, normalized path. | `ChromeOptions.user_data_dir`, `validate_profile` | Rejects mismatch with the owned profile. | `options.rs` | parity |
| `options._undot_key` / `_merge_nested` | Dotted key expansion and recursive merge. | `options::merge_dotted` | Nested objects; rejects non-object intermediate values. | `options.rs` | parity |
| `options.from_options` | Copy Selenium option attributes. | — | Not needed in a typed Rust API. | — | not implemented (irrelevant) |
| `devtool.Structure` | Dict/attribute convenience. | — | No equivalent; JSON `Value` is used. | — | not implemented (test utility) |
| `devtool.timeout` | Thread-based function timeout. | Per-operation Tokio timeouts | No decorator; every network/process wait is bounded. | timeout tests | parity, different mechanism |
| `cdp.CDP` `/json` helpers | List/new/activate/close tabs, one-shot WebSocket send. | `WebDriverSession`/`WebDriverTab`; `cdp_execute` for page-level CDP commands | No public CDP object, no separate WebSocket. ChromeDriver's documented `goog/cdp/execute` (Selenium `execute_cdp_cmd` equivalent) carries input/emulation/screenshot commands. | `webdriver.rs`, `real_browser.rs`, `real_chromedriver.rs` | parity, different mechanism |
| `reactor.Reactor` | 1-second performance-log poller. | `drain_cdp_events` | Pull-based. | `webdriver.rs` | parity, different mechanism |
| `patcher.Patcher.auto` | Delete cached driver, fetch release, unzip, patch. | `ChromeDriverStore::install` | Reuses a verified versioned install; never deletes a working driver while another process may use it. | `install.rs` | parity, safer caching |
| `Patcher.fetch_release_number` | Stable, milestone, legacy endpoint. | `manifest::{parse_stable, parse_milestone, parse_legacy_release}` | Same endpoints; strict bounds and milestone checks. | `manifest.rs` | parity |
| `Patcher.fetch_package` | Build CfT/legacy ZIP URL and download. | `manifest::{cft_download_url, legacy_download_url}`, `HttpFetcher` | HTTPS allowlist, no redirects, 64 MiB cap, optional SHA-256 pin. | `manifest.rs`, `install.rs` | parity, hardened |
| `Patcher.unzip_package` | `extractall`, then move executable, chmod. | `install::extract_driver` | Extracts only the exact binary; rejects traversal, absolute paths, backslashes, symlinks, duplicates, oversized/unexpected entries; atomic install. | `install.rs` | parity, hardened |
| `Patcher.patch_exe` | Regex-replace `{window.cdc…;}` with inert padded JS. | `patch::patch_driver` | Same functional transformation; bounded scan; own marker; refuses foreign patches; atomic rewrite. | `patch.rs` | parity |
| `Patcher.is_binary_patched` | Search literal upstream marker. | `patch::is_patched`, `contains_version` | Uses NovaGPT’s own marker; foreign upstream patch is not executed. | `patch.rs` | parity, provenance difference |
| `Patcher.parse_exe_version` | Scan `platform_handle\0content\0<version>`. | `binary_version`, `detect_driver_version` | Marker scan first; ChromeDriver 156 no longer contains the contiguous marker, so a bounded `--version` probe is the fallback for explicit drivers. | `patch.rs`, `chrome.rs` | parity, compatibility fix |
| `Patcher.driver_binary_in_use` | Heuristic open/read test. | — | Not needed; no file deletion while in use, install lock instead. | — | not implemented (irrelevant) |
| `Patcher.force_kill_instances` | `pidof`/`taskkill` all drivers. | — | **Not implemented.** NovaGPT kills only its own child. | `service.rs` | not implemented (security) |
| `Patcher.cleanup_unused_files` / `__del__` | Delete cached/unpacked driver. | Store retains verified versions | Retention is deliberate for reproducibility/offline reuse; no user data. | `install.rs` | parity, different lifecycle |
| `Patcher.user_multi_procs` | Global multiprocessing lock + latest patched binary. | Store install lock + immutable version directories | Concurrent installers serialize; running drivers are never modified. | `install.rs` lock test | parity, hardened |
| `Patcher.gen_random_cdc` | Random string helper (unused). | — | Dead upstream helper; no equivalent. | — | not implemented (unused) |
| `setup.py` dependencies | Selenium/requests/websockets/packaging. | Tokio/reqwest/zip/sha2 etc. | No Selenium; native W3C client. | — | parity, native reimplementation |

## 4. Binary management

- **ChromeDriver and Chrome are distinct.** NovaGPT downloads only
  ChromeDriver from Chrome for Testing (or the legacy Google Storage layout
  for ≤114). It discovers the installed Chrome and does not download a
  browser. The CLI can optionally target a milestone (`--milestone`).
- **Version matching.** Chrome's major milestone is the compatibility
  boundary. The CfT milestone feed selects an exact driver build; the
  embedded release string is checked and the driver must report
  `/status.ready` before a session is created. No silent "disable build
  check".
- **Integrity.** CfT does not publish per-ZIP SHA-256 sidecars (verified:
  `chromedriver-linux64.zip.sha256` returns HTTP 404 on 2026-11-04).
  NovaGPT therefore (a) requires HTTPS and the two documented hosts,
  (b) disables redirects, (c) caps the archive at 64 MiB, (d) accepts an
  explicit archive SHA-256 pin, and (e) records the archive and patched
  binary SHA-256 at install time and re-verifies both on reuse. The patched
  binary digest, exact version string, and patch marker are checked before
  every launch.
- **Safe extraction.** Only `chromedriver-<platform>/chromedriver[.exe]`
  (or root `chromedriver` for legacy) is extracted. Traversal, absolute
  paths, backslashes, empty components, symlinks, duplicate expected
  entries, directories, and archive bombs above the size limits are
  rejected.
- **Atomicity and concurrency.** Downloads go to a temporary directory in
  the owned store, are fsynced, and are renamed into
  `<root>/<version>/<platform>/`. An exclusive file lock serializes
  installers across processes. A verified installation is reused without
  network access.
- **Store ownership.** The store root has its own marker. NovaGPT refuses a
  non-empty directory without that marker and refuses symlinked roots.
- **Legacy support.** Milestones ≤114 use `LATEST_RELEASE_<n>` and the
  legacy ZIP layout. This path is unit-tested with injected bytes; it was
  not exercised against an old live browser.

## 5. Session, tabs, recovery, and duplicate safety

- One owned Chrome process and one owned ChromeDriver child per managed
  instance. ChromeDriver attaches to Chrome through `debuggerAddress`; it
  does not launch a second browser.
- Every **explicit WebDriver** tab operation takes one session-wide async mutex,
  switches to the tab's exact window handle, and then issues the command.
  This remains useful for setup and manual navigation but was responsible for
  repeated Chrome foreground activation when used during 300-ms worker polls.
  Ordinary ChatGPT subagent read/poll/input now uses `BackgroundTarget` in the
  browser-driver layer: a specific tab ID is matched to the owned loopback
  Chrome DevTools target and addressed via its own page WebSocket, without a
  WebDriver window switch or `Page.bringToFront`. Mouse/key pairs are kept
  ordered on the same socket. Concurrent workers cannot switch one another's
  current target or mix DOM results.
- `reconnect()` stops and restarts ChromeDriver, creates a new WebDriver
  session, increments the epoch, and invalidates the old session and all old
  tabs. It never re-navigates or replays input. If Chrome is still alive,
  its tabs and page state survive (verified by the real test); if Chrome
  exited, the same dedicated profile is reused to launch a new Chrome, whose
  tabs are necessarily empty.
- `ensure_session()` probes the current session (`window/handles`) and
  reconnects only when ChromeDriver reports it expired. It performs no
  navigation/input replay; callers must re-find tabs and decide whether the
  last action was idempotent. The ChatGPT adapter's send wrapper retries an
  unsent prompt once after recovery and fails closed after a possible
  submission.
- `shutdown()` is explicit and idempotent. Dropped objects rely on
  `kill_on_drop` only as a last resort.
- Process spawning retries a transient `ETXTBSY` ("text file busy") for a
  few hundred milliseconds, and profile locking retries a transient
  inherited-descriptor window before reporting contention. Both races can
  occur when another thread forks while a freshly written executable or
  lock descriptor is briefly inherited.
- The ChatGPT adapter now runs on this backend. It recreates an **unsent**
  stale tab, retries the unsent prompt once after driver/session recovery,
  and refuses to replay once a submission may have occurred. These behaviors
  are covered by tests in `novagpt-browser` and `novagpt-agents`.
- The transport exposes errors rather than retrying non-idempotent
  commands. A driver crash mid-submit is ambiguous; callers must not replay.
- Physical click and Enter sequences remain ordered per target
  (`BackgroundTarget::commands` for worker input; legacy WebDriver
  `click_at`/`press_enter` for explicit/manual operations), preventing four
  workers from interleaving press/release or key-down/up.
- Replay policy in the ChatGPT adapter is explicit:
  `tab_recovered_before_send` is the only automatic tab recreation;
  `recovery_blocked_after_send` and `BrowserError::AmbiguousSubmission`
  stop the workflow and require human inspection. A manual verification
  interstitial can resume the retained unsent prompt only because no input
  was dispatched before it.

## 6. Security boundaries

- Loopback only: Chrome debugging and ChromeDriver bind/validate
  `127.0.0.1`; WebDriver URLs are constructed from a validated port.
- The profile is a NovaGPT-owned directory with an owner marker and an OS
  file lock. Existing populated or redirected profiles are refused. The
  managed backend never launches Chrome against the default user profile.
- Browser launches reject `--disable-web-security`,
  `--ignore-certificate-errors`, `--allow-running-insecure-content`,
  `--remote-debugging-pipe`, and caller-supplied user-data/remote-debugging
  flags. `--no-sandbox` is opt-in only.
- Errors carry categories, never raw WebDriver messages, URLs, titles,
  prompts, cookies, CDP endpoints, or DOM text. The CLI prints only
  versions, store/path metadata, patch state, and install state.
- Navigation accepts only http(s) or `about:blank`, without embedded
  credentials or control characters. Scripts are trusted internal code;
  no MCP request supplies JavaScript.
- Element attribute/children helpers can read page data; they are library
  APIs, not MCP tools, and the existing adapter does not expose them.

## 7. Licensing analysis

Upstream is **GPL-3.0**. NovaGPT is **MIT OR Apache-2.0**. The following is
an engineering/legal-risk summary, not legal advice.

1. **Copying or translating upstream source would create a derivative
   work.** That includes porting Python functions line-by-line, copying
   regexes/comments/strings, or mechanically transliterating classes. A
   GPL-3.0 derivative cannot be distributed as part of an MIT/Apache-2.0
   work without making the combined distribution GPL-compatible.
2. **This implementation is independent.** It was written from observed
   behavior and public protocol facts (W3C WebDriver, DevTools `/json`
   endpoints, Chrome for Testing manifests, Chrome release notes). No
   upstream code, comments, names, or literal strings were copied. The
   patch marker (`novagpt-chromedriver-patch`) deliberately differs from
   upstream's marker; the replacement expression is NovaGPT's own.
3. **Functional behavior is not the same as copied expression.** The
   `{window.cdc…;}` byte pattern and the CfT URL layout are protocol/
   compatibility facts, not creative source text. An independent
   implementation that performs the same transformation is the standard
   basis for a clean-room compatible tool.
4. **Residual risk.** Because the implementation was informed by reading
   GPL source, a copyright holder could still allege similarity. The
   mitigation is documented independent design, no copied text, own
   markers, and a review of the diff. If NovaGPT ever incorporates any
   upstream expression, it must either relicense the combined work under
   GPL-3.0 or remove that expression.
5. **Binaries are downloaded, not redistributed.** Chrome and ChromeDriver
   remain Google's; ChromeDriver's bundled license is BSD-3-Clause. The
   store keeps Google's license notices untouched. NovaGPT does not ship
   the binaries in its source tree.
6. **Purpose and provider terms.** The patch removes ChromeDriver's
   automation signal; that is an evasion mechanism and may conflict with
   provider terms or local law depending on use. The launch also disables
   Blink's `AutomationControlled` feature because current Chrome sets it for
   any remote-debugging browser; without it the page reports
   `navigator.webdriver === true` before any input. NovaGPT does not solve
   CAPTCHAs, bypass interactive verification, or hide IP addresses, and the
   ChatGPT adapter now uses this transport by default.

## 8. Verification evidence

Deterministic suite (2026-11-04):

```text
cargo fmt --all -- --check                         passed
cargo clippy --workspace --all-targets -- -D warnings   passed
cargo check --workspace                            passed
cargo test --workspace                             201 passed, 0 failed, 2 ignored
```

Crate-level breakdown: `novagpt-browser-driver` 39 passed, `novagpt-browser`
15, `novagpt-agents` 18, plus core/MCP/CLI/VM suites. The two ignored tests
are the opt-in real ChromeDriver and real adapter browsing tests.

Real-browser, disposable profile, loopback HTML fixture, Linux:

```text
cargo test -p novagpt-browser-driver --features test-support \
  --test real_chromedriver -- --ignored --nocapture    1 passed
cargo test -p novagpt-browser \
  --test real_browser -- --ignored --nocapture         1 passed
```

The managed ChromeDriver test performed: ChromeDriver download/reuse,
version match, patch, Chrome launch, WebDriver session, local navigation,
element input/click, PNG screenshot, four isolated tabs with per-tab
outputs, reconnect (driver service + session) with surviving tabs and
cookie, tab close while Chrome stayed usable, shutdown, and a **separate
application launch against the same profile** that still saw the cookie.
No account, credential, personal profile, or remote service was used.

The real ChromeDriver test additionally covers native WebDriver evaluation,
CDP passthrough, driver reconnect, Chrome crash/relaunch, and a separate
application launch against the same profile. The real adapter test covers
the ChatGPT adapter's loopback browsing path over the same transport.

The CLI was exercised manually against the real environment:
`novagpt chrome-driver status`, `install --milestone 156`, and a repeat
`status`; it reported Chrome `156.0.8078.47`, ChromeDriver
`156.0.8078.80` patched and verified, and did not create a browser.

## 9. Deliberate gaps and unsupported cases

| Gap | Reason | Consequence |
| --- | --- | --- |
| `_configure_headless` identity rewriting | Falsifies `navigator.webdriver`, UA, `window.chrome`, permissions; outside visible, user-verified automation. | Headless parity is not claimed; headless is an explicit option but not stealth-tested. |
| Detached/browser-survives-parent mode | Process ownership and cleanup; NovaGPT must not orphan unowned Chrome. | Closing the NovaGPT process can close its owned Chrome; use the persistent profile for login state. |
| `force_kill_instances` | Would kill unrelated ChromeDriver processes. | A locked foreign binary yields a clear error; the caller must close it. |
| Upstream GPL source reuse | License incompatibility. | Behavior is reimplemented independently; literal parity of internals is not claimed. |
| Write-capable attach to an existing user Chrome (Chrome 144+ consent flow) | Separate permission/ownership design; the current bridge is read-only. | Managed backend always uses NovaGPT's dedicated profile. |
| Background Reactor thread | Rust async pull model avoids a long-lived thread and leaked callbacks. | Users call `drain_cdp_events`; no wildcard handler registry. |
| `click_safe` | Hidden reconnect after each click is surprising and can replay/lose state. | Reconnect is explicit. |
| Chrome browser auto-download | Upstream discovers Chrome; NovaGPT does the same. | No browser binary management, by design. |
| macOS/Windows execution | Only Linux x86_64 was available. | Code has platform mappings but is not runtime-verified there. |
| Live authenticated ChatGPT end-to-end | No authorized account was available to this task. | Real login, Cloudflare challenge, and conversation completion remain unverified. |
| Old ChromeDriver ≤114 live download | Legacy path unit-tested only. | URL/layout logic is covered; old binaries are not executed. |

## 10. Production-readiness assessment

**Experimental, now the sole browser transport.** The managed backend has
deterministic coverage for binary selection, integrity, extraction,
patching, service startup, WebDriver tabs, isolation, atomic input,
reconnection, Chrome crash recovery and profile persistence. Real Chrome
156/ChromeDriver 156 runs passed for the raw managed lifecycle and for the
ChatGPT adapter's browsing path. It is not yet validated against a live
authenticated ChatGPT account, the headless stealth layer is intentionally
absent, and Windows/macOS, legacy ChromeDriver downloads and the Chrome
144+ consent-based attach flow are unverified. ChromeDriver's CDP passthrough
is required for physical input/screenshot commands. Treat this as a
functionally integrated but not provider-validated implementation, not as a
guarantee of complete upstream parity or of reliable bypass of provider
defenses.
