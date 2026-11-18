# VM, subagents and dedicated ChatGPT Browser — initial Rust implementation

**Status: PARTIAL.** These optional NovaGPT capabilities require further live platform testing. This page names what actually works and what remains unfinished.

## Module ownership

| Component | Rust crate | Current behavior |
| --- | --- | --- |
| VM storage/lifecycle | `novagpt-vm` | Approved immutable imported/promoted bases, QEMU copy-on-write forks with NAT/isolated multi-NIC, Linux UEFI, private QMP display, bounded lifecycle, offline QCOW2 snapshots, separate approved ISO registry and conservative store quotas. Windows UEFI/TPM QEMU fork support remains pending. |
| VM guest control | `novagpt-vm` | Unix-domain QEMU Guest Agent ping, bounded guest exec, upload/download inside Linux-hosted QEMU forks; PowerShell Direct for Windows Hyper-V forks (24 KiB). A real installed Linux guest control smoke remains pending. |
| Subagent manager | `novagpt-agents` | Async spawn, message, status, result, cancel, resume after human confirmation, browser diagnostics; roles explorer/reviewer/tester/implementer; bounded shared registry |
| Chrome/ChromeDriver driver | `novagpt-browser-driver` | Rust-native, visible Chrome process and exclusively owned persistent profile; patched ChromeDriver, W3C sessions/tabs, physical input, screenshots, bounded timeouts, crash/relaunch recovery, atomic click/Enter transactions; real Chrome and local adapter fixtures tested on Linux |
| ChatGPT Web adapter | `novagpt-browser` | Dedicated browsing tab with URL/origin guards, visible UI inspection, responsive viewport tests, form interactions and bounded diagnostics; ChatGPT selectors and human login/challenge detection |
| MCP wiring | `novagpt-mcp` | Official `rmcp` schemas/structured results; `vm`, `vm_image`, `vm_iso`, `vm_snapshot`, `vm_guest_status`, `vm_exec`, `vm_upload`, `vm_download`, `vm_desktop_screenshot`, `vm_desktop_input`; existing subagent and browser tools preserved. |
| CLI wiring | `novagpt-cli` | Existing flags and installer workflows, plus `vm-import-iso`, `vm-init-base`, `vm-fork` NIC/ISO options, `vm-fork-start`, `vm-fork-stop`, `vm-fork-snapshot`. |

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

On Linux the local CLI accepts approved `.qcow2` bases and ISO images. QEMU forks now support per-VM guest-agent and QMP Unix sockets, bounded virtual networking, boot-time Linux UEFI variables, and an exclusive store lease. Existing QEMU child handles remain process-local: after a server crash/restart, a previously running guest is reported failed/unowned rather than killing an unverified host PID. Automatic orphan adoption and Windows UEFI/TPM forks remain pending. The new Linux QEMU path has not been validated on macOS.

## Linux QEMU managed VM lab (2026-11-18, partially verified)

The optional VM store is explicitly selected with `--vm-home`; it must be **dedicated, outside agent-writable workspace roots, and never a personal or production VM directory**. A cross-process exclusive lease blocks two NovaGPT managers from claiming the same home simultaneously. To run the human-only import/blank-base CLI commands, first stop the MCP process using that home, then restart it with the same home. Existing bases are checksummed and immutable; QCOW2 bases with external backing/data file references are rejected.

### Human-approved installation media and blank bases

The following commands are run by the human on the NovaGPT host, **not** by an MCP agent. `vm-import-iso` makes an immutable, SHA-256-verified copy of a locally approved ISO; `vm-init-base` creates a separate sparse QCOW2 base for installation. Use distinct base names for Ubuntu, Arch and CachyOS. The default blank base uses **Linux UEFI** (OVMF), while `--legacy` creates a BIOS base suitable for disk-only environment snapshots. Windows 11 Linux-hosted raw QEMU UEFI/TPM install forks are **not supported by this new workflow**; use the existing Windows installer procedure below or a Windows-hosted Hyper-V disposable test VM.

```bash
novagpt --vm-home "$HOME/.local/state/NovaGPT/vms" vm-import-iso \
  --name ubuntu-approved --source /path/to/human-approved-ubuntu.iso
novagpt --vm-home "$HOME/.local/state/NovaGPT/vms" vm-init-base \
  --name ubuntu-empty --disk-gib 50 --cpus 4 --memory-mb 8192
# Optional BIOS-based installation if disk-only QCOW2 snapshots are required:
novagpt --vm-home "$HOME/.local/state/NovaGPT/vms" vm-init-base \
  --name ubuntu-bios-empty --disk-gib 50 --cpus 4 --memory-mb 8192 --legacy
```

The new MCP tools `vm_image` and `vm_iso` list approved names only. From the restarted MCP server an agent can issue:

```json
{"action":"create","image":"ubuntu-empty","installer_iso":"ubuntu-approved",
 "cpus":4,"memory_mb":8192,"networks":["none"],"display":true,"guest_agent":true}
```

This is an example of the **`vm` tool arguments**, not a shell command. `networks:["none"]` provides no NIC and no internet egress; select `["nat"]` explicitly when installation needs network access. New QEMU forks run with a private Unix QMP monitor, virtual USB keyboard/tablet, optional VNC socket and a virtio Guest Agent channel. The guest OS must still be installed via its own installer; install and enable `qemu-guest-agent` **inside** the Linux guest (Ubuntu/Arch/CachyOS package instructions vary). The channel being present does not mean the guest service is ready. System ISO installation, user account creation, guest-agent package installation, desktop login and guest network configuration are **not yet unattended**.

Once the OS installer finishes, call `vm` `{"action":"stop","id":"vm-..."}` to request ACPI power-off (up to 10 seconds). If it does not stop, a deliberate `{"action":"stop","id":"vm-...","force":true}` terminates the disposable QEMU process but gives **only crash-consistent disks**. While stopped, call `{"action":"eject_iso","id":"vm-..."}` and then `{"action":"start","id":"vm-..."}`. Retain the instance ID returned by `create`.

### Guest control and desktop tools

`vm_guest_status` calls a real QGA `guest-ping`; it does not infer readiness from a socket pathname. `vm_exec` on Linux-hosted QEMU takes `id`, an **absolute guest executable path** (such as `/usr/bin/true`), bounded `args` and optional `timeout_ms` (100–300000); `vm_upload` and `vm_download` take `id`, absolute `guest_path` and canonical `data_base64` for uploads. These are guest paths only; no host filesystem paths, host shell or arbitrary QMP commands are accepted. Upload/download are bounded to 1 MiB with 12 KiB guest-agent chunks. Uploaded data may be partial if a transfer fails. **QGA cannot cancel `guest-exec` on timeout**; the fork is marked failed/tainted until an explicit stop.

`vm_desktop_screenshot` returns a bounded base64 PNG plus dimensions of the *guest* display, never the host screen. `vm_desktop_input` accepts `{"action":"keys","keys":["ctrl","a"],"id":"vm-..."}` or `{"action":"pointer","x":10000,"y":10000,"button":"left","id":"vm-..."}` (virtual tablet coordinates 0–32767; optional left/right/middle button). Guest screen resolution is still controlled by the guest OS or virtual GPU and does not have a dedicated runtime MCP setter. VNC, QMP and QGA sockets are private within the owned VM directory and must not be forwarded to public interfaces.

### Offline snapshots and reusable environments

`vm_snapshot` supports `create`, `list`, `restore`, `delete` on **stopped legacy-BIOS QEMU QCOW2 forks**. Names should identify environment configurations (`clean-os`, `wireguard-ready`, `tor-proxy-ready`), not individual test cases. `promote` additionally converts one stopped named snapshot into an immutable, SHA-256-hashed approved base, for example:

```json
{"action":"promote","id":"vm-...","name":"wireguard-ready","image":"ubuntu-wireguard"}
```

A worker can then request `subagent_spawn` with `vm_image="ubuntu-wireguard"`, causing a disposable QCOW2 fork of that approved image with cleanup on worker completion/cancellation (cleanup failure remains visible). The snapshot conversion is deliberately full/flattened once; subsequent forks share the immutable base incrementally. **UEFI VARS and TPM state are not captured by QCOW2 internal snapshots**: UEFI/TPM snapshot/restore/promote requests are rejected. A forced stop is crash-consistent, not a guest filesystem quiesce, so reboot and verify the filesystem/application state before promoting a meaningful environment.

### Network policy and resource boundaries

`vm(create)` accepts up to four virtual NICs: `nat` (QEMU user-mode outbound NAT), `none` (only as a single entry, no NIC), `listen:segment-name` and `connect:segment-name` (private Unix point-to-point Ethernet stream). A pair of owned VMs may share an isolated segment without editing host TAP devices, routing, DNS or firewall. These connections are **point-to-point, not a multi-VM switch or a routed/VPN gateway product**; creating VPN gateways, dynamic routing policies, leak-proof enforcement, per-interface packet capture and paid VPN credential injection remain future work. For VPN no-leak phases, provision a completely disconnected or isolated environment, and intentionally enable NAT only in a separate test environment. Never interpret QEMU NAT alone as a VPN kill switch.

The owned store enforces at most 32 registered forks, four active forks, 16 active vCPUs and 32 GiB total configured active guest RAM; these are conservative software admission limits, **not** host cgroup or disk-growth quotas. The VM-home lock protects against concurrent independent NovaGPT processes. VM process handles remain process-local: an abrupt server restart may leave an unowned QEMU process, and NovaGPT refuses to kill it by guessing a PID. A stopped guest is safe to restart through MCP; orphan process adoption, automated cleanup of every interrupted launch, Windows QEMU swtpm state, full snapshot transactions, runtime guest resolution changes and complete Windows/Linux installer automation are **not yet implemented**.

### Tests actually executed for this upgrade

On the local Linux host, the new QEMU test suites exercised a blank disposable QCOW2/ISO pair, QEMU start/stop/destroy, offline snapshot round-trip and promotion, actual private QMP screenshot/keyboard/tablet input, guest-status denial without a QGA, ISO detach, exclusive VM-home lease, backing-file denial, quotas, and mock-Unix-socket QGA command/file transfer. An MCP stdio integration test called `vm`, `vm_image`, `vm_snapshot`, `vm_desktop_screenshot` and `vm_desktop_input` end to end against a new disposable blank fork and verified cleanup. A **real installed Linux guest with QGA was not booted**, and none of these tests demonstrates full Windows 11/Ubuntu/Arch/CachyOS installation, VPN leak detection, real Tor networking, paid VPN setup or server-crash recovery.

```bash
cargo test -p novagpt-vm --lib
cargo test -p novagpt-cli --test protocol --test vm_mcp_live
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

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

### Promote a Windows installer and fork immutable bases (Windows host)

The local CLI can flatten a trusted powered-off installer VM into an approved image. To make this VM store available to the running MCP server and to VM-backed subagents, select it once with `novagpt vm-set-home --path E:\\NovaGPT-VM-Tests-20261114` and restart the server. This changes the per-user VM home setting without copying or moving any images. The explicit `--vm-home` CLI flag still overrides the saved selection for individual commands.

The local CLI can then promote and fork an installer:

~~~powershell
novagpt --vm-home E:\NovaGPT-VM-Tests vm-promote-iso --installer win11-live1 --image win11-base
novagpt --vm-home E:\NovaGPT-VM-Tests vm-fork --image win11-base --cpus 4 --memory-mb 8192
novagpt --vm-home E:\NovaGPT-VM-Tests vm-fork-status --id vm-0123456789abcdef
novagpt --vm-home E:\NovaGPT-VM-Tests vm-fork-destroy --id vm-0123456789abcdef
~~~

Promotion never overwrites existing images and does not delete the installer VM, snapshots or disk. It checks the owned VM name, configuration location, backing VHDX/AVHDX chain, and powered-off state. It converts the active disk to an independent dynamic VHDX, records its SHA-256 and boot profile, and marks the base read-only. A failed conversion may leave an unregistered image directory for inspection. The original installer remains for recovery and is retired manually only after validation; do not remove a parent disk while snapshots still depend on it.

A Windows fork receives a new differencing VHDX pointing at that immutable base, a distinct TPM/key protector, Windows Secure Boot, and the image's CPU/RAM defaults (overridable). Destroy validates the exact owned disk before removing the VM and only its instance files, never the base. Owned Hyper-V forks can be inspected and destroyed across NovaGPT restarts.

A parent can opt in to VM isolation by specifying vm_image on subagent_spawn. Both VM and subagent services must be enabled. NovaGPT creates a fork before the worker starts; the VM ID and base appear in the parent-visible agent record and task. The fork is cleaned up on completion, failure or cancellation. Lease states: active, cleaning, destroyed, cleanup_failed. Completed VM-backed agents reject follow-ups: spawn a new worker to get a new fork. Cleanup failures retain resources for explicit owner recovery; they are never silently reported as successful.

On Windows Hyper-V, the MCP tools vm_exec, vm_upload and vm_download operate inside owned Windows guests through PowerShell Direct with guest credentials supplied separately for each operation. The password is not saved to the image, manifest, logs or process command line. All operations check the registered attached VHDX before contacting the guest. The execution tool accepts an absolute Windows executable and bounded arguments for up to 300 seconds. Each transfer uses canonical base64 and a 24 KiB limit, with guest paths only. Credential-free vm_guest_status cannot authenticate the guest and does not claim guest execution works. A worker cannot obtain credentials merely by inheriting the MCP connection.

Live Windows-host verification: Windows 11 25H2 with an existing Hyper-V baseline checkpoint was flattened to a standalone 23.1 GB read-only base.vhdx. NovaGPT created a fresh 4-vCPU, 8192-MiB, 80-GiB differencing fork. The guest booted and Hyper-V Heartbeat reported OK. vm-fork-status and vm-fork-destroy succeeded; the instance directory was removed, while the approved base and installer snapshots remained. An additional ignored live integration test passed with `NOVAGPT_LIVE_VM_HOME=E:\\NovaGPT-VM-Tests-20261114` and `NOVAGPT_LIVE_VM_IMAGE=win11-base-20261114` using `cargo test --release -p novagpt-agents live_agent_vm_fork_is_released_after_backend_completion -- --ignored --nocapture`: the test backend completed and its subagent-owned Hyper-V fork was removed while the approved base remained registered. Guest command/file transfer still needs separate authenticated live testing.

Limits: On Linux, the existing QEMU fork backend does not configure UEFI/TPM for Windows guest profiles. Such images are rejected rather than silently booted with incorrect firmware. Legacy qcow2 images keep their existing behavior. Installer retirement, crash recovery, guest credential provisioning and cross-host base portability are not automated.

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

1. **Guest operations:** Windows Hyper-V forks retain credential-scoped PowerShell Direct `vm_exec`, `vm_upload`, and `vm_download` (24 KiB transfers). On Unix, these MCP tools now use a bounded QEMU Guest Agent protocol with up to 1 MiB file transfers and 48 KiB output. QGA operations have passed socket-level mock tests; live installed Ubuntu/Arch/CachyOS and Windows QGA end-to-end tests remain **NOT RUN**. Guest-exec timeout cannot cancel the guest process; the VM is tainted until stopped.
2. **Native VM interoperability:** image import intentionally does not claim other image manifest formats, boot-tested images, real Hyper-V/QEMU integration, recovery or secure boot. Windows disk creation and cleanup need smoke tests on isolated hardware.
3. **ChatGPT Browser:** `novagpt-browser-driver` uses Rust, ChromeDriver and WebDriver rather than Node.js/Patchright. Four-tab isolation, input, screenshot, close, driver reconnect and Chrome crash/relaunch passed against disposable local HTML fixtures on Linux/Chrome 156; the ChatGPT adapter's local browsing path also passed; agent result association and cancellation were additionally checked by deterministic mocks. It has **not** been validated against a real ChatGPT account, authenticated profile persistence across separate server invocations, dynamic ChatGPT UI changes, project auto-creation, relogin, anti-bot gates, multi-tab races against live ChatGPT or actual response completion. Human verification remains manual. Chrome is launched with Blink's automation feature disabled so the page does not report `navigator.webdriver`; this is a launch switch, not a JavaScript identity patch, and it does not guarantee that a provider accepts the session.
4. **Tool visibility:** optional tool names are present in `tools/list` even when a subsystem is disabled; invocation fails explicitly. Disabled-tool visibility needs its own policy and tests.
5. **Subagent completeness:** no DeepSeek backend, scoped file-context snippets, isolated implementer worktree, automated approval/rollback or browser project auto-create. Explicit authenticated single-existing-file patch application is implemented; full multi-file transactions and a live ChatGPT Web patch-callback test remain pending. Subagent registry is bounded to 64 entries, six running/waiting agents; no persistence across NovaGPT process restarts.
6. **Reliability:** VM home has an exclusive OS file lease, and approved bases and ISOs are checksummed; the QEMU fork's process/metadata lifecycle and snapshot promotion are still not crash-atomic. External crash/restart recovery, in-guest atomic uploads and private VPN secret injection remain incomplete. Do not pass real paid VPN credentials to LLM prompts. Browser profile symlink/reparse and process-handoff audits remain pending.
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
