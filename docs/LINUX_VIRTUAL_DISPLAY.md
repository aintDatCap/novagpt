# Optional Linux virtual display for NovaGPT

**Scope:** Opt-in Xvfb service wrapper, not a new browser driver or headless
ChatGPT implementation. The existing `novagpt-browser-driver` still owns
Chrome, its dedicated profile, ChromeDriver and exact-tab CDP operations. The
wrapper only owns Xvfb, an optional window manager, an optional VNC inspection
process and the child NovaGPT command. Nothing changes for ordinary NovaGPT
launches, Windows, macOS, MCP transport or the TUI.

## Prerequisites and trust boundary

Install **Xvfb**, **xauth**, **openbox** and Chrome/Chromium on the Linux host.
`x11vnc` is optional for manual remote inspection. Use an **unprivileged,
dedicated OS account** with a private persistent Chrome profile directory
and a separate approved workspace. The wrapper refuses root. Linux desktop
libraries, shared memory, seccomp and the Chrome sandbox must work normally;
do not supply `--no-sandbox` to work around host misconfiguration.

An Xvfb display is **not an isolation or multi-tenant security boundary**.
Keep NovaGPT HTTP MCP bound to loopback and protect the existing full MCP
token: a caller with full MCP authority can execute host-shell commands.
Never publish Chrome DevTools, the display, VNC or the MCP listener directly.
The wrapper uses `-nolisten tcp`, a temporary private MIT-MAGIC-COOKIE
Xauthority file, and never attaches to a pre-existing X socket.

## Start an owned virtual display

Run as the dedicated Linux user. Paths must match this host:

```bash
python3 script/novagpt_xvfb.py --display :99 -- \
  /usr/local/bin/novagpt \
  --root /srv/novagpt-workspace \
  --chatgpt-chrome /usr/bin/google-chrome \
  --chatgpt-profile /home/novagpt/.local/share/novagpt-browser \
  http --no-ngrok
```

If display `:99` is occupied, choose a different explicit number (90–4095).
Never remove a foreign `/tmp/.X11-unix/X*` socket to force startup.
Xvfb and the window manager are started only for this opt-in invocation.
The wrapper waits for the owned display socket; it exits without starting
NovaGPT when dependencies, authorization or readiness fail. It forwards the
X11 environment only to its owned child processes, and returns NovaGPT's exit
status. On SIGINT/SIGTERM or normal exit, it stops *only* its still-owned
process groups in reverse startup order. An OS SIGKILL or power loss cannot
guarantee cleanup; inspect orphaned processes before manually recovering.

The Chrome profile is leased by NovaGPT itself; the wrapper does not modify
that profile or bypass its owner lock. NovaGPT browser recovery still never
replays a prompt after possible submission. An interrupted send may be
**ambiguous** and requires manual reconciliation.

## Manual login and optional remote inspection

Use the **same dedicated browser profile** for initial, interactive login.
Do not run the login command concurrently with the MCP server holding the
profile. Start the command with the same wrapper and opt-in inspection flags:

```bash
x11vnc -storepasswd /home/novagpt/.config/novagpt-vnc-pass
chmod 600 /home/novagpt/.config/novagpt-vnc-pass

python3 script/novagpt_xvfb.py --display :99 \
  --vnc-port 5905 --vnc-auth /home/novagpt/.config/novagpt-vnc-pass -- \
  /usr/local/bin/novagpt \
  --chatgpt-profile /home/novagpt/.local/share/novagpt-browser \
  browser-login
```

From an administrator's workstation, use SSH port forwarding, then connect
a VNC viewer to **localhost:5905**:

```bash
ssh -N -L 5905:127.0.0.1:5905 novagpt@YOUR-VPS
```

The optional `x11vnc` process is bound to localhost, requires a private
password file owned by the service user and is stopped by the wrapper.
Legacy VNC authentication is **not** strong transport encryption: never
connect directly over an untrusted network; use an authenticated SSH tunnel
or a comparably secure private channel. Close the login process before
starting the managed server. During later troubleshooting, inspection can
be enabled explicitly on the server invocation with the same flags.
A manual Cloudflare/login challenge must be completed by a human in the
visible browser. The wrapper never answers or bypasses challenges.

## Operational validation

1. Run `python3 -m unittest discover -s script -p test_novagpt_xvfb.py -v`
   to check bounds, foreign-display refusal, private VNC passwords, Xauth
   cookie handling, and owned child process termination.
2. Start a disposable server under Xvfb; confirm owned Chrome can navigate
   to an approved localhost fixture, receive focus and complete one ordinary
   interaction. Check that it can stop/restart with the same profile and that
   no owned Xvfb/ChromeDriver child remains after graceful shutdown.
3. Check manual login and a real ChatGPT completion **separately**. Local
   browser tests cannot establish that provider authentication, challenges,
   UI stability or long-running focus work on a VPS.

If a display cannot become ready or a process exits, inspect package
installation, browser dependencies, X11 permissions and the *operator's*
system journal. Errors from this wrapper are intentionally bounded and do
not include cookies, tokens, Xauthority contents, browser URLs or prompts.

## Deliberate limitations

The wrapper is a process supervisor, not an integrated Rust lifecycle API.
It does not yet appear in NovaGPT CLI flags, TUI settings or Rust managed
Chrome reconnect status. A crashed Xvfb ends the supervised service instead
of silently attaching to another display. It neither schedules work nor
exposes a new chat API. Live account interactions and focus-gate behavior
on each VPS compositor must be verified before unattended use.
