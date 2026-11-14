# VM, subagents and dedicated ChatGPT Browser — initial Rust implementation

**Status: PARTIAL.** These optional NovaGPT capabilities require further live platform testing. This page names what actually works and what remains unfinished.

## Module ownership

| Component | Rust crate | Current behavior |
| --- | --- | --- |
| VM storage/lifecycle | `novagpt-vm` | Dedicated ownership-marked image/instance directories; human-only local disk import; image listing; create/status/destroy host VM via Hyper-V (Windows) or QEMU (Linux/macOS); guarded IDs/resources and isolated overlays |
| VM guest control | `novagpt-vm` | `vm_guest_status` reports **unavailable** for registered VMs; guest exec/upload/download not implemented or registered |
| Subagent manager | `novagpt-agents` | Async spawn, message, status, result, cancel, resume after human confirmation, browser diagnostics; roles explorer/reviewer/tester/implementer; bounded shared registry |
| Chrome/ChromeDriver driver | `novagpt-browser-driver` | Rust-native, visible Chrome process and exclusively owned persistent profile; patched ChromeDriver, W3C sessions/tabs, physical input, screenshots, bounded timeouts, crash/relaunch recovery, atomic click/Enter transactions; real Chrome and local adapter fixtures tested on Linux |
| ChatGPT Web adapter | `novagpt-browser` | Dedicated browsing tab with URL/origin guards, visible UI inspection, responsive viewport tests, form interactions and bounded diagnostics; ChatGPT selectors and human login/challenge detection |
| MCP wiring | `novagpt-mcp` | Official `rmcp` schemas/structured results; 2 VM tools, 9 subagent tools and 10 dedicated browser tools; explicit errors when optional subsystems are disabled |
| CLI wiring | `novagpt-cli` | Flags `--vm-home`, `--chatgpt-chrome`, `--chatgpt-profile`; human-only `vm-import` command |

The crates follow the direction `novagpt-cli -> novagpt-mcp -> optional domain adapters`; `novagpt-core` stays independent of browser and VM. Subagent records are server-scoped, **not tied to individual ChatGPT transports**.

## VM setup (Windows Hyper-V)

Requires supported Windows, Hyper-V PowerShell commands, sufficient privileges and an **already prepared, trusted and human-approved VHDX base image**. This implementation does not download an OS image, accept arbitrary host image paths through MCP or reuse another application's VM home. The import command copies the entire file to the separate NovaGPT VM store and hashes it.

```powershell
# Use a fresh, dedicated directory; never your another application's VM home.
cargo run -p novagpt-cli -- --vm-home C:\NovaGPT-VMs vm-import --name win-test --source C:\ApprovedImages\win-test.vhdx --cpus 2 --memory-mb 4096

# Start the MCP server with VM actions enabled.
cargo run -p novagpt-cli -- --root C:\Projects\your-repo --vm-home C:\NovaGPT-VMs stdio
```

Then call `vm` with `{"action":"images"}` to inspect approved images; `{"action":"create","image":"win-test"}`, followed by `{"action":"status","id":"vm-..."}` and, when **explicitly requested**, `{"action":"destroy","id":"vm-..."}`. NovaGPT names Hyper-V guests `NovaGPT-vm-<16-hex>`; instances use a differencing disk, preserving imported bases. Destroy stops/removes **only the named managed VM**, then deletes its expected owned instance files, never arbitrary paths. If unknown files or symlinks appear in an instance directory, cleanup refuses rather than deleting them.

On Linux/macOS the same CLI import accepts a `.qcow2` base image and the host backend runs `qemu-img` and `qemu-system-x86_64`. A running QEMU process is owned by the live server process; after a restart, an orphaned or unowned process is not killed by guessing a PID. QEMU guest control, architecture/accelerator options and reliable restart recovery remain pending.

### Windows ISO provisioning on Windows (experimental)

On Windows, NovaGPT uses **Hyper-V Generation 2** for disposable installer VMs. The local CLI accepts only explicitly selected ISO files, never remote MCP paths. Use a fresh, empty, NovaGPT-owned VM directory on a local NTFS drive. Guests have no external virtual switch by default.

~~~powershell
cargo run -p novagpt-cli -- --root C:\Projects\novagpt --vm-home E:\NovaGPT-VM-Tests vm-install-iso --name win11-test --iso C:\OS\Win11_25H2_English_x64_v2.iso --os windows --cpus 4 --memory-mb 8192 --disk-gib 80
cargo run -p novagpt-cli -- --root C:\Projects\novagpt --vm-home E:\NovaGPT-VM-Tests vm-iso-status --name win11-test

cargo run -p novagpt-cli -- --root C:\Projects\novagpt --vm-home E:\NovaGPT-VM-Tests vm-install-iso --name cachyos-test --iso C:\OS\cachyos-desktop-linux-260809.iso --os linux --cpus 4 --memory-mb 8192 --disk-gib 80

# Run only when the guest is powered off:
cargo run -p novagpt-cli -- --root C:\Projects\novagpt --vm-home E:\NovaGPT-VM-Tests vm-iso-snapshot --name cachyos-test --snapshot baseline
~~~

Each guest receives a dynamic VHDX of the requested *virtual capacity* in a dedicated installer directory on E:. Windows 11 additionally receives a small ISO9660 disc containing Autounattend.xml, virtual TPM, and Windows Secure Boot. NovaGPT sends an initial virtual-keyboard Space key to confirm the EFI DVD boot prompt. Linux ISO guests default to Secure Boot off. No source ISO is modified.

The unattended Windows account currently uses the same **weak disposable-only password** noted in the Linux provisioning section below; never connect it to an untrusted network. ISO VM status/checkpoints validate the registered VM's configuration location. **Not implemented for these ISO guests:** Windows PowerShell Direct guest execution, deletion/rollback of partially created VMs, arbitrary network connections, or exposed MCP installer operations. An active Hyper-V VM does not alone prove that its guest OS installed successfully.

**Live Windows-host evidence (2026-11-14):** CachyOS reached its graphical live desktop under Hyper-V with 4 vCPUs, 8192 MiB and an 80 GiB virtual disk on E:. A powered-on snapshot was denied and a powered-off baseline snapshot succeeded. Windows 11 25H2 completed unattended installation and reached the NovaTester desktop; the Hyper-V Heartbeat reported OK, and an off-guest baseline checkpoint succeeded. A second fresh Windows 11 ISO VM reached setup automatically without manual keyboard input and was then removed.

### Windows 11 ISO provisioning on Linux (experimental)

NovaGPT can now provision a persistent Windows 11 installer guest through user-session libvirt. This path is **separate** from the existing MCP `vm` image registry until Windows finishes installing and a qcow2 disk is imported. The CLI uses the installed host packages `virt-install`, `xorriso`, QEMU/KVM, `swtpm`, and OVMF. Run:

```bash
cargo run -p novagpt-cli -- \
  --root /path/to/novagpt \
  --vm-home "$HOME/.local/state/NovaGPT/vms" \
  vm-install-windows \
  --name win11-ace-test \
  --iso "$HOME/Downloads/Windows11_Client_x64_en-us_26300_9457.iso" \
  --username AceTester --cpus 4 --memory-mb 16384 --disk-gib 80
```

This command creates a new qcow2 disk and a second ISO containing `Autounattend.xml`; it never modifies the original Windows ISO. The answer file configures virtual Disk 0, installs image index 1, accepts the EULA, creates a local Administrator account for **disposable testing only**, and hides selected setup/OOBE screens. **Security limitation:** the current experimental answer file encodes a fixed, weak account password (`Password`) rather than generating a unique credential. It is only suitable for a throwaway guest behind QEMU user-mode NAT with no inbound ports; change the password before any external access or production use. The guest has a SPICE display bound to loopback. OOBE can still show an optional error that needs `Skip`, and Windows may perform normal updates after installation.

The libvirt domain name is `novagpt-install-<name>`. Use `virsh -c qemu:///session list --all` and `virt-viewer --connect qemu:///session novagpt-install-<name>` to inspect it. Boot media may display `Press any key to boot from CD or DVD`; use the VM console or `virsh send-key` to select the installer. The installation stays registered across NovaGPT server restarts. After Windows finishes and you verify that the guest boots, shut it down and create the baseline snapshot with `cargo run -p novagpt-cli -- --root /path/to/novagpt --vm-home "$HOME/.local/state/NovaGPT/vms" vm-snapshot-windows --name win11-ace-test --snapshot baseline`. The command refuses a running guest. Do not run a second import until the installed disk is known bootable.

A QEMU Guest Agent channel is configured for new installer VMs. Install the official VirtIO guest tools **inside Windows with Administrator privileges** using `virtio-win-guest-tools.exe /install /quiet /norestart`; `/s /qn` is not a working substitute. Verify `virsh -c qemu:///session qemu-agent-command novagpt-install-<name> '{"execute":"guest-ping"}'`. The installer path has a separate, local-only `vm-exec-windows` CLI; it verifies that the named libvirt domain uses a disk in the owned VM directory, then runs a bounded program through QEMU Guest Agent. Example:

```bash
cargo run -p novagpt-cli -- \
  --root /path/to/novagpt --vm-home "$HOME/.local/state/NovaGPT/vms" \
  vm-exec-windows --name win11-ace-test \
  --program 'C:\Windows\System32\cmd.exe' \
  --arg /c --arg 'whoami && sc query QEMU-GA'
```

The existing MCP `vm_guest_status` still reports guest control unavailable; this installed-libvirt-guest workflow is not registered in the MCP `vm` lifecycle.

**Live validation (2026-11-13):** Windows 11 build 26300.9457 installed to a disposable KVM/UEFI/TPM VM with 4 vCPU, 16 GiB RAM and 80 GiB virtual disk. The guest reached the desktop; `baseline` is an internal qcow2 snapshot verified under libvirt. QEMU Guest Agent `guest-ping` and a real `vm-exec-windows` call returned successfully. Ace Blocker 0.1.0's historical November 1 Windows MSI installed with exit code 0. Its engine and watchdog services started, and Windows created the Ace Blocker Tunnel interface. However, the *historical* watchdog repeatedly rejected the heartbeat with `deserialize_any is unsupported`, restarted the engine and left the UI intermittently offline. The protocol defect was already corrected in Ace Blocker source commit `393414c` (November 3); the historical installer predates that fix. A current Windows candidate, baseline revert and end-to-end blocked-site test are **not yet validated**. Do not use production or personal guest disks for validation.

## ChatGPT Browser subagent setup

Requires a real local Chrome-compatible executable (pass its exact path) and a **fresh, dedicated** Chrome user-data directory. NovaGPT refuses a populated non-NovaGPT profile, to avoid accidentally attaching to your personal or other applications' login state. You must complete ChatGPT login/Cloudflare checks in the **visible** Chrome window. NovaGPT never automatically solves challenges or extracts cookies, bearer tokens, passwords or browser storage.

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

This mode does **not** add `--remote-debugging-port`, WebDriver, headless, or injection switches. The CLI waits for Chrome to exit. Log in personally in the Chrome window, complete any authentication or verification yourself, and **close that dedicated Chrome** before starting NovaGPT with `stdio` or `http`. If you previously configured browser support in the TUI, `--root C:\Projects\your-repo browser-login` uses the same per-user NovaGPT browser directory and detected/configured Chrome automatically. The profile is kept outside the repository and reused by later launches on that machine. Use exactly the same profile path in both modes; never use a personal or another application's profile.

A cross-process OS file lock prevents two NovaGPT instances from claiming the same owned profile at the same time. The profile marker and existing Chrome data are preserved. A stale DevTools endpoint after Chrome exits is discarded instead of treating it as a healthy browser. The Rust-native `novagpt-browser-driver` creates tabs through ChromeDriver and waits for the page composer before input. Every tab command switches to the caller's exact window handle under one session lock; physical click and Enter sequences are single transactions because real Chrome drops mouse input on hidden tabs. Response polling remains concurrent. Restart invalidates old tab handles and never implicitly resends a submitted prompt.

**Google sign-in compatibility:** Google explicitly may reject sign-in from browsers controlled by automation (see [Google Account Help](https://support.google.com/accounts/answer/7675428)). Before using subagents, close the automation Chrome, run `browser-login` to authenticate **manually in normal visible Chrome**, complete any challenges and close the browser; then restart the browser-enabled server using the same NovaGPT-owned profile. Development browsing no longer navigates to ChatGPT as a side effect. NovaGPT launches Chrome with Blink's `AutomationControlled` feature disabled so a remote-debugging browser does not expose `navigator.webdriver`; it does not patch JavaScript getters, rewrite user agents, spoof input or solve challenges. Do not add further stealth fingerprint patches or disable browser security features. Chrome 136+ requires a non-default profile for remote debugging (see [Chrome for Developers](https://developer.chrome.com/blog/remote-debugging-port)); NovaGPT already uses a dedicated profile. Chrome 144+ also supports a user-consented attach to an already-running browser via `chrome://inspect/#remote-debugging` (see [Chrome DevTools guidance](https://developer.chrome.com/blog/chrome-devtools-mcp-debug-your-browser-session)); NovaGPT does not currently implement that attach mode.

**Important:** A login accepted in ordinary Chrome may still be rejected after Chrome restarts with remote debugging/automation enabled, depending on the identity provider's policy. This is a compatibility workflow, **not** a bypass of authentication/antifraud controls or a guarantee of a successful login. If the provider does not permit the browser to be automated, use an officially supported authentication or API workflow. Chrome login and session persistence have not been tested against a real account in this change.

To open Chrome directly from ChatGPT after enabling the browser and restarting the server, call `browser_open` (launches the visible, dedicated Chrome profile); then call `browser_navigate` with `{"url":"https://www.rust-lang.org/"}` and `browser_read` to retrieve bounded visible page text/title/URL. `browser_close` closes only the interactive browsing tab, **not** the shared Chrome process or any subagent conversation. These browser tools have no arbitrary JavaScript, login automation, or cookie access. Navigation accepts public HTTPS hostnames (without credentials, IP addresses or custom ports), plus `http://localhost:5173/`, `http://127.0.0.1:3000/`, `http://[::1]:5173/` and other explicit loopback HTTP(S) URLs by default. Private LAN IPs, DNS aliases and other internal services are denied; redirect destinations are validated before returning page data. Network-level DNS rebinding and real public-HTTPS browser navigation still require independent verification; disposable real Chrome navigation to a local HTTP fixture has passed. Web page content is untrusted data, not instructions to follow. Use an explicit URL and do not expose sensitive private pages through this tool. Chrome is launched only after the first explicit `browser_open` or subagent request, not at server startup.

### Web development with the dedicated browsing tab

After `browser_open`, use `browser_navigate` and `browser_read` to visit and read a page. `browser_screenshot` returns a **real PNG image content block** (visible viewport only, maximum 6 MiB of decoded PNG) so the assistant can inspect the visual result. `browser_inspect` returns bounded visible headings, controls, suggested selectors, broken-image count and document/viewport widths. `browser_viewport` changes desktop viewport size (320–2560 px wide, 240–1800 px high, pixel ratio 1–3 and at most 10 million device pixels) to check responsive layouts. `browser_click` and `browser_type` use native Chrome mouse/text input on explicitly selected visible controls; password and file fields cannot be typed automatically. A click **may submit a form or trigger an action**, so only use it when the user authorized that action. `browser_console` returns up to 80 bounded, in-memory console/exception messages and HTTP/network failures without request bodies or URL query strings in network diagnostics. Page-provided console messages can contain sensitive information; never copy them to persistent logs.

To test a development server on the same machine, no special startup flag is needed. Start the browser-enabled server, call `browser_open`, and then navigate to an explicit loopback URL, for example `{"url":"http://localhost:5173/"}`. The browsing tab initially opens blank instead of automatically visiting ChatGPT; subagent tabs still open ChatGPT independently. If you want to **restrict all loopback browsing to one exact origin**, supply the optional flag:

```bash
cargo run -p novagpt-cli -- \
  --root /path/to/your-repo \
  --browser-dev-origin http://127.0.0.1:3000 \
  stdio
```

With this scope, `browser_navigate` can access `http://127.0.0.1:3000/` and its paths. Other loopback ports, hostnames, HTTP-to-HTTPS changes and redirects to unapproved origins are rejected. **Without the flag, explicit localhost/127.0.0.1/[::1] HTTP(S) addresses on any port are permitted.** The port is on the **machine running NovaGPT**, not the ChatGPT client's device; if Vite binds only to `::1`, use `localhost` or `[::1]` rather than `127.0.0.1`. The same dedicated per-user Chrome profile is used for subagents and browsing; its data survives a NovaGPT restart, but cookies/session expiration and identity-provider rules may require renewed **manual** authentication. This does not expose a personal Chrome profile and does not authorize arbitrary JavaScript, local files or internal services. Public HTTPS DNS rebinding remains a known limitation.

### Automatic grouping of subagent conversations

With ChatGPT Browser enabled, NovaGPT uses the **first worker's own tab** to find or create the ChatGPT project **NovaGPT - subagents**. No separate setup tab opens. If a validated project URL is already known, new worker tabs open directly at that project; persisted URLs are reverified in the first worker tab after a server restart. The URL cache stays within the dedicated browser profile, and setup is serialized to prevent duplicate creation under concurrent spawns. An explicit **ChatGPT subagent project URL** in TUI Settings overrides the automatic project. Login and challenges remain manual: if the project cannot be verified, worker startup stops without sending the task. This does not move existing conversations or affect the built-in Chat page. Delete the project yourself in ChatGPT when appropriate. See [ADR 0005](decisions/0005-automatic-subagent-project.md).

Call `subagent_spawn`:

```json
{"role":"explorer","connector_name":"NovaGPT Laptop","task":"Inspect the HTTP session lifecycle and identify concurrency risks.","paths":["crates/novagpt-mcp/src/lib.rs"]}
```

The call responds promptly with an `agent-...` ID. Every delegated task includes a **private six-character URL-safe completion code** in its prompt. On finishing, the subagent calls the MCP tool `subagent_finished` using its own `id`, `code` and full `result`. NovaGPT verifies the current task's code, stops the previous tab's response poller before marking it `completed`, and wakes the parent waiting with `subagent_wait` without requiring Chrome to recognize a finished response. Subsequent messages wait until this cleanup finishes, preventing overlapping sends. The code is **not** returned by spawn/status/wait, is accepted only once, and changes for each new task (manual verification resume retains the same code). Five failed code attempts disable this completion callback for that task; the browser may still provide the result normally. A six-character code provides only 36 bits of entropy, so the callback also relies on authenticated MCP access and the failure limit. Treat the code as private and avoid passing it through logs or regular messages. The result remains **untrusted**, and the parent must verify claims independently.

Prefer **`subagent_wait`** with `{"id":"agent-...","max_wait_seconds":20}` to await a change without repeated polling; it returns `{"agent":{...},"timed_out":false}` as soon as the subagent reaches `completed`, `failed`, `cancelled`, or `waiting` for human action. If 20 seconds elapse while it is still `running`, the response has `timed_out:true` and the subagent **keeps running**; call `subagent_wait` again. Allowed timeout: 1–25 seconds per call. Alternatively, `subagent_status` is a nonblocking snapshot of `running`, `waiting`, `completed`, `failed`, or `cancelled`. `subagent_wait` includes a completed **untrusted** result; `subagent_result` remains available to fetch it independently. Never treat agent claims as proof of edits, tests or browser actions. Continue the same worker tab with `subagent_message`. Completed conversations are retained up to a live-tab cap (8); the oldest completed tab is closed when a new spawn would exceed it, and a later follow-up to an evicted agent opens a new conversation instead of targeting the closed tab. Failed executions close their tab immediately. If it is `waiting` due to manual verification or inspection, finish the visible step yourself and only then call `subagent_resume` with `manual_check_completed: true`. `subagent_cancel` stops the worker and its tab without terminating the entire Chrome instance. `subagent_browser_diagnostics` shows safe browser state without cookies or challenge tokens.

### Connector access and single-turn worker dispatch

A new subagent receives its **complete assignment in the first browser prompt**. NovaGPT does not send a separate capability-preflight message or automatically follow it with a second coding prompt. Follow-ups likewise send one message to the worker's existing dedicated tab. A `connector_name` is optional client-owned metadata and does not establish authorization, a tool namespace, or a ChatGPT conversation identity.

For implementers, `capability_status="unchecked"` means there is no mandatory upfront tool handshake; it does **not** mean write permissions have been granted or verified. The optional `subagent_apply_patch(probe=true)` and `subagent_create_file(probe=true)` calls still return authenticated no-write receipts, but they are never required before the assignment is sent. Real changes require the live worker's private per-execution code, appropriate role, assigned path where updating an existing file, workspace safety validation, and a successful server receipt. `subagent_finished` is also credential-bound, and follow-ups rotate the code.

If tools are unavailable in the worker's ChatGPT conversation, the agent must report **NOT APPLIED**; the server does not silently apply its proposed edits. A parent may explicitly review the proposal and use its own guarded file tools, with that action reported as parent-mediated. NovaGPT cannot install or expose connected tools in a remote ChatGPT tab by prompt wording. An uncertain submission or timeout is not automatically replayed to another tab. Dedicated-tab routing is tested deterministically with three simultaneous workers, but live ChatGPT UI behavior and delegated write callbacks still require real-browser validation.

### Applying real changes from implementer subagents

An `implementer` can now **explicitly apply** a patch to an existing file in its real assigned workspace using `subagent_apply_patch`. Supply the exact existing file names in `subagent_spawn.paths`; without assignment, pre-existing files cannot be edited (new files created by the same worker can be). The subagent is instructed to use its private current six-character execution code. Example callback argument shape (placeholders only, not a real code):

```json
{"id":"agent-...","code":"<private-current-code>","patch":"--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1 +1 @@\n-old\n+new\n"}
```

The tool returns a **server-generated receipt** with `workspace_id`, `paths`, `bytes` and `patches_applied` only after a guarded Git application succeeds. The parent can read `subagent_status` / `subagent_result` (`patches_applied`, `patched_paths`) and independently inspect `git_status`, `git_diff` and tests. `result.applied_to_workspace` is set only if a patch or new-file creation succeeded during the current execution. Merely claiming in chat that a patch was applied never changes files or this flag.

Each `subagent_apply_patch` call accepts **one existing-file update**, either an ordinary unified diff or a single-file `diff --git` with unchanged paths. For **new files**, use `subagent_create_file` with the same `id` and private `code`, plus `path` and UTF-8 `content`:

```json
{"id":"agent-...","code":"<private-current-code>","path":"src/new_module/generated.rs","content":"// generated code\n"}
```

New files may be created **anywhere within the assigned allowed workspace**, including nested directories, without being present in `subagent_spawn.paths`. Creation uses exclusive file creation and **never overwrites an existing path**. The receipt includes the relative path, SHA-256, byte count and `files_created`; status/result additionally expose `files_created` and `created_paths`. Files created in the current run can then be edited by that same implementer with `subagent_apply_patch`, without earlier path selection. Editing other existing files still requires explicit assignment at spawn. Multiple calls are **not atomic as a group**. Deletion, rename, binary patches, mode changes and implicit application of chat text are not supported.

At most 16 existing-file patch attempts (including conflicts), 256 file-creation attempts, 1,024 bytes per new path and 200,000 bytes per patch are allowed per execution. New-file UTF-8 content is limited to 1,000,000 bytes per call by the core workspace write-size cap. Parent directories are created where needed; incomplete writes and directory-creation races are not transactional. Existing core path/blocklist/symlink checks, revalidation and `git apply --check` reject unsafe or stale updates. An existing-file patch target must match a file explicitly assigned at spawn or created by that agent in the current run; new-file paths only need to pass the normal workspace safety checks. The immutable workspace is determined by that agent's record, not a caller override. The six-character credential shares the five-failure limit with completion and requires authenticated MCP access; a tab or transport session alone is not trusted provenance.

File creation, patch execution, cancellation, completion and follow-up are serialized **per agent**. A cancelled, completed or stale run cannot begin another patch. Follow-ups rotate the code and reset current-run counters; already applied changes remain. Patches go directly to the real working tree, **without an isolated worktree, second approval, multi-file transaction or rollback**. Delegate only files you intend to allow the subagent to modify. General-purpose `apply_patch` calls do not count as authenticated agent receipts. Assigned `paths` are transmitted as **names only**; no local file content is uploaded automatically, and `task`/`context` must not contain secrets.

After deploying, rebuild and restart NovaGPT, then spawn a **new** implementer to receive the updated prompt and tool. A live authenticated ChatGPT Web patch-callback test has not yet been performed; local tests cannot prove compatibility with every ChatGPT interface.

### Authenticated worker activity (separate from lifecycle)

A ChatGPT Web browser backend can stop waiting for a reply at its configured timeout while the remote worker continues composing. Neither that backend timeout nor `subagent_wait` is proof of remote inactivity or task completion. `subagent_wait` **continues to wait only for a lifecycle transition** (`waiting`, `completed`, `failed`, `cancelled`).

A worker can call `subagent_progress` with its current `id`, private `code`, and one of the exact English `kind` values `working`, `reading`, `editing`, `testing`, `reviewing`, or `blocked`. This explicit callback is authenticated using the **same per-execution code and five-failure limit** as completion and patch callbacks. It is accepted while the current worker is ready and either `running` or `waiting`; a completed, failed, cancelled or rotated run cannot report. A backend `waiting` state does not invalidate the current execution credential. The callback carries no free-form description. NovaGPT stores server-owned fixed descriptions, never prompts, credentials, paths, URLs, raw tool arguments or results. A reported event remains an **untrusted worker claim**, not proof that an action occurred.

The read-only `subagent_activity` takes `{ "id": "agent-...", "after_sequence": 0 }` (cursor optional) and returns an `epoch`, `state`, monotonic `cursor`, `dropped_before` cursor and `events`. Each event includes agent `id`, execution `epoch`, strictly increasing sequence, UTC Unix `timestamp_ms`, English `kind` and fixed safe description. Only the latest **32 events** are retained per agent, with at most **64 accepted progress callbacks per execution**. Follow-ups clear the ring and reset the per-run allowance but preserve the monotonically increasing cursor; they rotate the private code and epoch. Manual verification resume retains its credential and activity allowance while incrementing the backend epoch.

The separate `subagent_activity_wait` takes `id`, optional `after_sequence`, optional last-seen `epoch`, and `max_wait_seconds` (1–25, default 20). It returns `{ "activity": { ... }, "timed_out": false }` when new events arrive, an epoch changes or lifecycle becomes completed, failed or cancelled, or `{ "activity": { ... }, "timed_out": true }` if still running or waiting without a matching change. Events from other workers do not wake this wait. A timed-out wait never cancels the worker. The epoch and cursor are safe to share with the parent; the private completion code is not.

### Diagnosing ChatGPT subagent browser failures

After rebuilding and **restarting** NovaGPT, launch a subagent and check three sources:

1. The dedicated Chrome **subagent tab** remains visible for manual verification, but routine status polling no longer injects `console.log` into the page (that former diagnostic side-effect repeatedly switched WebDriver windows). Check the NovaGPT server's structured logs or the parent-accessible diagnostics for fixed stage names such as `page_state`, `composer_ready`, `tab_interactive`, `text_input_typed`, `text_verified`, `send_mouse_dispatched`, `manual_before_submit`, `recovery_blocked_by_verification`, and `manual_check_required`. A newly opened tab may be `about:blank` briefly; the adapter waits up to 20 seconds for navigation. Manual login or challenge details remain in the visible browser, never the logs.
2. In NovaGPT's **Activity** tab (when launched from its TUI), look for `Browser: ...` events. When NovaGPT runs directly using `novagpt http` or `novagpt stdio`, the same stage names are written to Rust's structured **stderr** diagnostics at INFO level (default filter includes `novagpt=info`).
3. Call the MCP tool `subagent_browser_diagnostics` with the subagent ID. Its `recent_events` field contains up to **24** sanitized stage/state lines for that page and `last_step` identifies the most recent line. The page and event history disappear when its owning server is restarted; these are diagnostic events, not the persistent metadata-only tool audit.

Only fixed event names, page-state booleans and bounded assistant counts are logged. **No prompts, responses, raw URLs, DOM text, headers, tokens or cookies** enter these traces. If the worker reports `manual_check_required verification`, check whether the *visible* ChatGPT tab really asks for verification before calling `subagent_resume`; never attempt to bypass the check. Collect the stage names plus the **visible page condition**, without copying account or authentication details.

### Prompt entry and submission

The Rust-native driver and ChatGPT Web adapter use a visible, genuinely editable ChatGPT composer (including an accessible textbox). Before any input the adapter raises that exact worker tab and requires the page to report itself visible and focused; if it cannot, the send stops with `InteractionBlocked` and nothing is typed. Short ASCII prompts are entered as individual key events with jittered timing (`Shift+Enter` inserts a newline, so it cannot submit early); longer or non-ASCII prompts are inserted as one trusted text event because Chrome offers no user-permitted clipboard paste to an automation client. The pointer follows a short jittered path before the Send press, and the adapter re-checks the page, the editor focus and the topmost element at the click point immediately before submitting. The Send button uses a serialized physical mouse press/release transaction rather than a synthetic DOM click, with Enter as the fallback. Hidden login/verification widgets do not block an otherwise usable composer; a **visible** or structural Cloudflare challenge does, and a tab last seen on a challenge is not recreated automatically. A local mock-WebDriver send/response test checks these commands, and a real ChromeDriver browsing test covers the adapter transport. **This is not a successful live ChatGPT account smoke test.** Restart the NovaGPT server with the rebuilt binary before testing the updated driver.

## Important technical gaps

1. **Guest operations:** `vm_exec`, `vm_upload`, `vm_download`, QEMU guest agent and credential-scoped Hyper-V PowerShell Direct must be implemented before guest control. No credential transport is implemented in this initial slice.
2. **Native VM interoperability:** image import intentionally does not claim other image manifest formats, boot-tested images, real Hyper-V/QEMU integration, recovery or secure boot. Windows disk creation and cleanup need smoke tests on isolated hardware.
3. **ChatGPT Browser:** `novagpt-browser-driver` uses Rust, ChromeDriver and WebDriver rather than Node.js/Patchright. Four-tab isolation, input, screenshot, close, driver reconnect and Chrome crash/relaunch passed against disposable local HTML fixtures on Linux/Chrome 156; the ChatGPT adapter's local browsing path also passed; agent result association and cancellation were additionally checked by deterministic mocks. It has **not** been validated against a real ChatGPT account, authenticated profile persistence across separate server invocations, dynamic ChatGPT UI changes, project auto-creation, relogin, anti-bot gates, multi-tab races against live ChatGPT or actual response completion. Human verification remains manual. Chrome is launched with Blink's automation feature disabled so the page does not report `navigator.webdriver`; this is a launch switch, not a JavaScript identity patch, and it does not guarantee that a provider accepts the session.
4. **Tool visibility:** optional tool names are present in `tools/list` even when a subsystem is disabled; invocation fails explicitly. Disabled-tool visibility needs its own policy and tests.
5. **Subagent completeness:** no DeepSeek backend, scoped file-context snippets, isolated implementer worktree, automated approval/rollback or browser project auto-create. Explicit authenticated single-existing-file patch application is implemented; full multi-file transactions and a live ChatGPT Web patch-callback test remain pending. Subagent registry is bounded to 64 entries, six running/waiting agents; no persistence across NovaGPT process restarts.
6. **Reliability:** browser profile ownership is checked but full symlink/reparse-point and process-handoff audits remain; VM image checksum is checked before create, but launch + metadata commit is not transactionally atomic. No real guest credentials should ever be passed to a tool until their secure ephemeral handling is implemented.
7. **Performance/limits:** browser polls every ~300 ms with a 180-second response bound, a maximum prompt/reply size and serialized tab creation. These are provisional defaults, not verified in every environment.

## Reproducible real-Chrome smoke test

On Linux with an installed visible Chrome and an available desktop session, the following test uses only a new, disposable NovaGPT-owned profile and a loopback HTML server. It checks four independent tabs, input/click/PNG screenshot, profile contention, targeted tab closure, owned-process crash/restart and invalidation of old tab handles. The process is owned by the test; the fixture does not contact ChatGPT or access a personal profile.

```bash
cargo test -p novagpt-browser-driver --features test-support --test real_chromedriver -- --ignored --nocapture
cargo test -p novagpt-browser --test real_browser -- --ignored --nocapture
# Opt-in focused-window smoke test on a live niri Wayland compositor only:
cargo test -p novagpt-browser-driver --lib real_niri_focus_remains_elsewhere_during_concurrent_background_input -- --ignored --nocapture
```

**Observed 2026-11-04:** PASS on Linux with Google Chrome 156.0.8078.47. The test is ignored during normal `cargo test --workspace` unless invoked explicitly with `--ignored`. Full workspace formatting, Clippy, check and test commands also passed. Tests against real ChatGPT authentication, Cloudflare checks, browser accounts, OAuth and session persistence across independent NovaGPT invocations remain **NOT RUN**; do not infer that a browser able to navigate local HTML can successfully sign in to ChatGPT.

## Development and checks

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
cargo run -p novagpt-cli -- --help
```

For a release milestone, add real protocols tests for enabled VM tools; mock PowerShell/qemu command tests; isolated Hyper-V/QEMU provisioning and cleanup; browser WebDriver/ChromeDriver integration against a consented throwaway account/page; manual-check/resume/cancel races; explicit role-specific worktree tests. Update [CAPABILITIES.md](CAPABILITIES.md) only with actual evidence; never modify unrelated applications as a shortcut.
