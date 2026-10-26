# VM, subagents and dedicated ChatGPT Browser — initial Rust implementation

**Status (2026-10-24): INITIAL, NOT full CodexPro parity.** These are independently optional capabilities of NovaGPT. The original CodexPro TypeScript implementation is the oracle, not a shared runtime or data directory. This page names what actually works and what remains unfinished.

## Module ownership

| Component | Rust crate | Current behavior |
| --- | --- | --- |
| VM storage/lifecycle | `novagpt-vm` | Dedicated ownership-marked image/instance directories; human-only local disk import; image listing; create/status/destroy host VM via Hyper-V (Windows) or QEMU (Linux/macOS); guarded IDs/resources and isolated overlays |
| VM guest control | `novagpt-vm` | `vm_guest_status` reports **unavailable** for registered VMs; guest exec/upload/download not implemented or registered |
| Subagent manager | `novagpt-agents` | Async spawn, message, status, result, cancel, resume after human confirmation, browser diagnostics; roles explorer/reviewer/tester/implementer; bounded shared registry |
| Browser driver | `novagpt-browser` | Dedicated visible Chrome profile over Chrome DevTools Protocol; separate interactive browsing tab (open/navigate/read/close) alongside the ChatGPT subagent tabs; bounded visible-text reads; human login/challenge detection; stop/cancel/close |
| MCP wiring | `novagpt-mcp` | Official `rmcp` schemas/structured results; 2 VM tools, 7 subagent tools and 4 dedicated browser tools; errors returned when relevant subsystem is disabled |
| CLI wiring | `novagpt-cli` | Flags `--vm-home`, `--chatgpt-chrome`, `--chatgpt-profile`; human-only `vm-import` command |

The crates follow the direction `novagpt-cli -> novagpt-mcp -> optional domain adapters`; `novagpt-core` stays independent of browser and VM. Subagent records are server-scoped, **not tied to individual ChatGPT transports**.

## VM setup (Windows Hyper-V)

Requires supported Windows, Hyper-V PowerShell commands, sufficient privileges and an **already prepared, trusted and human-approved VHDX base image**. This implementation does not download an OS image, accept arbitrary host image paths through MCP or reuse CodexPro VM home. The import command copies the entire file to the separate NovaGPT VM store and hashes it.

```powershell
# Use a fresh, dedicated directory; never your CodexPro VM home.
cargo run -p novagpt-cli -- --vm-home C:\NovaGPT-VMs vm-import --name win-test --source C:\ApprovedImages\win-test.vhdx --cpus 2 --memory-mb 4096

# Start the MCP server with VM actions enabled.
cargo run -p novagpt-cli -- --root C:\Projects\your-repo --vm-home C:\NovaGPT-VMs stdio
```

Then call `vm` with `{"action":"images"}` to inspect approved images; `{"action":"create","image":"win-test"}`, followed by `{"action":"status","id":"vm-..."}` and, when **explicitly requested**, `{"action":"destroy","id":"vm-..."}`. NovaGPT names Hyper-V guests `NovaGPT-vm-<16-hex>`; instances use a differencing disk, preserving imported bases. Destroy stops/removes **only the named managed VM**, then deletes its expected owned instance files, never arbitrary paths. If unknown files or symlinks appear in an instance directory, cleanup refuses rather than deleting them.

On Linux/macOS the same CLI import accepts a `.qcow2` base image and the host backend runs `qemu-img` and `qemu-system-x86_64`. A running QEMU process is owned by the live server process; after a restart, an orphaned or unowned process is not killed by guessing a PID. QEMU guest control, architecture/accelerator options and reliable restart recovery remain pending.

**Do not use production or personal guest disks for validation.** No real Hyper-V creation/destruction, QEMU boot, guest agent or PowerShell Direct operation has been run as part of this work. Only tests using disposable fixture files were run. Hyper-V privileges, Windows command behavior, secure boot, guest login and native disk provisioning need controlled hardware/VM tests.

## ChatGPT Browser subagent setup

Requires a real local Chrome-compatible executable (pass its exact path) and a **fresh, dedicated** Chrome user-data directory. NovaGPT refuses a populated non-NovaGPT profile, to avoid accidentally attaching to your personal or CodexPro login state. You must complete ChatGPT login/Cloudflare checks in the **visible** Chrome window. NovaGPT never automatically solves challenges or extracts cookies, bearer tokens, passwords or browser storage.

```powershell
cargo run -p novagpt-cli -- `
  --root C:\Projects\your-repo `
  --chatgpt-chrome 'C:\Program Files\Google\Chrome\Application\chrome.exe' `
  --chatgpt-profile 'C:\NovaGPT-ChatGPT-Browser' `
  stdio
```

Supply paths that exist on your machine; the Chrome path above is illustrative, not detected. To use Streamable HTTP instead, provide `NOVAGPT_HTTP_TOKEN` and change `stdio` to `http --port 8787` (the HTTP listener remains loopback-only).

### Manual-login mode (dedicated Chrome, no CDP)

Before starting the MCP server, run an ordinary, visible Chrome instance using the **same dedicated NovaGPT profile**:

```powershell
cargo run -p novagpt-cli -- `
  --root C:\Projects\your-repo `
  --chatgpt-chrome 'C:\Program Files\Google\Chrome\Application\chrome.exe' `
  --chatgpt-profile 'C:\NovaGPT-ChatGPT-Browser' `
  browser-login
```

This mode does **not** add `--remote-debugging-port`, WebDriver, headless, or injection switches. The CLI waits for Chrome to exit. Log in personally in the Chrome window, complete any authentication or verification yourself, and **close that dedicated Chrome** before starting NovaGPT with `stdio` or `http`. If you previously configured browser support in the TUI, `--root C:\Projects\your-repo browser-login` uses the TUI's `.novagpt/browser` directory and detected/configured Chrome automatically. Use exactly the same profile path in both modes; never use a personal or CodexPro profile.

A cross-process OS file lock prevents two NovaGPT instances from claiming the same owned profile at the same time. The profile marker and existing Chrome data are preserved. A stale CDP endpoint after Chrome exits is discarded instead of treating it as a healthy browser.

**Important:** A login accepted in ordinary Chrome may still be rejected after Chrome restarts with CDP enabled, depending on the identity provider's policy. This is a compatibility workflow, **not** a bypass of authentication/antifraud controls or a guarantee of a successful login. If the provider does not permit the browser to be automated, use an officially supported authentication or API workflow. Chrome login and session persistence have not been tested against a real account in this change.

To open Chrome directly from ChatGPT after enabling the browser and restarting the server, call `browser_open` (launches the visible, dedicated Chrome profile); then call `browser_navigate` with `{"url":"https://www.rust-lang.org/"}` and `browser_read` to retrieve bounded visible page text/title/URL. `browser_close` closes only the interactive browsing tab, **not** the shared Chrome process or any subagent conversation. These browser tools have no arbitrary JavaScript, login automation, or cookie access. Navigation currently accepts public HTTPS hostnames without credentials, IP addresses or custom ports; redirect destinations are checked before returning page text. Network-level DNS rebinding and real-browser navigation must still be verified independently. Web page content is untrusted data, not instructions to follow. Use an explicit URL and do not expose sensitive private pages through this tool. Chrome is launched only after the first explicit `browser_open` or subagent request, not at server startup.

Call `subagent_spawn`:

```json
{"role":"explorer","task":"Inspect the HTTP session lifecycle and identify concurrency risks.","paths":["crates/novagpt-mcp/src/lib.rs"]}
```

The call responds promptly with an `agent-...` ID. Every delegated task includes a **private six-character URL-safe completion code** in its prompt. On finishing, the subagent calls the MCP tool `subagent_finished` using its own `id`, `code` and full `result`. NovaGPT verifies the current task's code and immediately marks it `completed`; this wakes the parent waiting with `subagent_wait` without requiring Chrome to recognize a finished response. The code is **not** returned by spawn/status/wait, is accepted only once, and changes for each new task (manual verification resume retains the same code). Five failed code attempts disable this completion callback for that task; the browser may still provide the result normally. A six-character code provides only 36 bits of entropy, so the callback also relies on authenticated MCP access and the failure limit. Treat the code as private and avoid passing it through logs or regular messages. The result remains **untrusted**, and the parent must verify claims independently.

Prefer **`subagent_wait`** with `{"id":"agent-...","max_wait_seconds":20}` to await a change without repeated polling; it returns `{"agent":{...},"timed_out":false}` as soon as the subagent reaches `completed`, `failed`, `cancelled`, or `waiting` for human action. If 20 seconds elapse while it is still `running`, the response has `timed_out:true` and the subagent **keeps running**; call `subagent_wait` again. Allowed timeout: 1–25 seconds per call. Alternatively, `subagent_status` is a nonblocking snapshot of `running`, `waiting`, `completed`, `failed`, or `cancelled`. `subagent_wait` includes a completed **untrusted** result; `subagent_result` remains available to fetch it independently. Never treat agent claims as proof of edits, tests or browser actions. Continue the same worker tab with `subagent_message`. If it is `waiting` due to manual verification or inspection, finish the visible step yourself and only then call `subagent_resume` with `manual_check_completed: true`. `subagent_cancel` stops the worker and its tab without terminating the entire Chrome instance. `subagent_browser_diagnostics` shows safe browser state without cookies or challenge tokens.

`implementer` currently **returns proposed diffs only**; no automatic worktree creation or patch application occurs. This is safer than applying untrusted web output to the real repository without verification. Selected `paths` are validated and sent as **names only**. No local file contents are automatically uploaded; explicit `task` and `context` text are sent to the browser/account. Keep them free of secrets.

### Diagnosing ChatGPT subagent browser failures

After rebuilding and **restarting** NovaGPT, launch a subagent and check three sources:

1. In the dedicated Chrome **subagent tab**, open Developer Tools (**F12 → Console**) and filter for `[NovaGPT/browser]`. Lifecycle lines such as `page_state page_kind=initial_blank composer=false`, `page_state page_kind=chatgpt composer=true`, `composer_ready`, `text_verified`, `send_mouse_dispatched`, and `manual_check_required verification` show exactly where the flow stopped. A just-created Chrome tab may be `about:blank` briefly; NovaGPT now waits up to 20 seconds for navigation instead of treating this transient page as a manual CAPTCHA. Persistent external redirects still require manual review. These logs are emitted by NovaGPT's CDP driver; they are not messages from ChatGPT itself. A navigation may clear older console lines; enable **Preserve log** if needed.
2. In NovaGPT's **Activity** tab (when launched from its TUI), look for `Browser: ...` events. When NovaGPT runs directly using `novagpt http` or `novagpt stdio`, the same events are written to Rust's structured **stderr** diagnostics at INFO level (default filter includes `novagpt=info`).
3. Call the MCP tool `subagent_browser_diagnostics` with the subagent ID. Its `recent_events` field contains up to **24** sanitized stage/state lines for that page and `last_step` identifies the most recent line. The page and event history disappear when its owning server is restarted; these are diagnostic events, not the persistent metadata-only tool audit.

Only fixed event names, page-state booleans and bounded assistant counts are logged. **No prompts, responses, raw URLs, DOM text, headers, tokens or cookies** enter these traces. If the worker reports `manual_check_required verification`, check whether the *visible* ChatGPT tab really asks for verification before calling `subagent_resume`; never attempt to bypass the check. Collect the stage names plus the **visible page condition**, without copying account or authentication details.

### Prompt entry and submission

The Rust CDP adapter is modeled on the interaction pattern in CodexPro's `src/chatgptBrowserManager.ts`: look for a visible, genuinely editable ChatGPT composer (including an accessible textbox), insert text using Chrome input events, prefer a visible enabled Send button, and fall back to Enter. The Send button is activated with CDP mouse events rather than a synthetic DOM click. Hidden login/verification widgets do not block an otherwise usable composer; a **visible** challenge still requires manual user action. A local mock-CDP send/response test checks these commands and the initial page-navigation retry. **This is not a successful live ChatGPT account smoke test.** Restart the NovaGPT server with the rebuilt binary before testing the updated driver.

## Important technical gaps

1. **Guest operations:** `vm_exec`, `vm_upload`, `vm_download`, QEMU guest agent and credential-scoped Hyper-V PowerShell Direct must be implemented before guest-control parity. No credential transport is implemented in this initial slice.
2. **Native VM interoperability:** image import intentionally does not claim CodexPro image manifest parity, boot-tested images, real Hyper-V/QEMU integration, recovery or secure boot. Windows disk creation and cleanup need smoke tests on isolated hardware.
3. **ChatGPT Browser:** the Rust implementation uses Chrome CDP rather than CodexPro's Patchright engine. It has **not** been validated against a live ChatGPT account, dynamic UI changes, project auto-creation, browser relogin, anti-bot gates, multi-tab races or actual response completion. It may require selector/API adjustments. Human verification must remain manual.
4. **Tool visibility:** optional tool names are present in `tools/list` even when a subsystem is disabled; invocation fails explicitly. CodexPro suppresses some disabled tools; mode/visibility parity is not yet implemented.
5. **Subagent parity:** no DeepSeek backend, scoped file-context snippets, isolated implementer worktree, patch validation/application or browser project auto-create. Subagent registry is bounded to 64 entries, six running/waiting agents; no persistence across NovaGPT process restarts.
6. **Reliability:** browser profile ownership is checked but full symlink/reparse-point and process-handoff audits remain; VM image checksum is checked before create, but launch + metadata commit is not transactionally atomic. No real guest credentials should ever be passed to a tool until their secure ephemeral handling is implemented.
7. **Performance/limits:** browser polls every ~300 ms with a 180-second response bound, a maximum prompt/reply size and serialized tab creation. These are provisional defaults, not proven CodexPro parity.

## Development and checks

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
cargo run -p novagpt-cli -- --help
```

For a release or parity milestone, add real protocols tests for enabled VM tools; mock PowerShell/qemu command tests; isolated Hyper-V/QEMU provisioning and cleanup; browser CDP integration against a consented throwaway account/page; manual-check/resume/cancel races; explicit role-specific worktree tests. Update [PARITY.md](PARITY.md) only with actual evidence and never overwrite CodexPro as a shortcut.
